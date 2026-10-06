//! `file::Copy`: Ansible's `copy` (with `src` or `content`).

use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};

use rustible_sdk::backend::{FileKind, WriteAttrs};
use rustible_sdk::prelude::*;

use super::{AttrPlan, Owner, cleared_by_chown, plan_attrs};

/// Above this size a content change is reported as a byte-count summary
/// instead of a unified diff, whatever the encoding.
pub const TEXT_DIFF_LIMIT: usize = 64 * 1024;

/// How much of each side `check` holds at once while it compares them.
const COMPARE_CHUNK: usize = 64 * 1024;

/// Where the bytes come from.
#[derive(Debug, Clone)]
pub enum CopySource {
    /// Bytes baked into the binary (`include_bytes!`, `include_str!`, or a
    /// string built at run time). Vision 5.6 mechanism 1.
    Bytes(Vec<u8>),
    /// A path read **on the target**, by `check` to compare and by `apply`
    /// again to write: a source that changes in between is written as it
    /// is then, not as the diff showed it (vision 5.1: the playbook
    /// runs there, so this is not the operator's machine). It must be a
    /// regular file, or a symlink to one. For a file that only exists on
    /// the controller, embed it or stream it with `ctx.local_file` instead.
    LocalPath(PathBuf),
}

/// Ensure `dest` holds exactly these bytes, optionally with a mode and owner.
/// Ansible's `copy` with `src`/`content`, `mode`, `owner`/`group`, `backup`.
///
/// ```no_run
/// # use rustible_std::file;
/// let op = file::Copy::from_str("net.ipv4.ip_forward = 1\n")
///     .to("/etc/sysctl.d/99-forward.conf")
///     .mode(0o644);
/// ```
///
/// `check` compares bytes and, when given, mode and owner. A content change
/// is shown as a unified diff when both old and new content are UTF-8 and
/// at most `TEXT_DIFF_LIMIT`, otherwise as `<n> bytes -> <m> bytes`. Fails
/// if `dest` exists and is not a regular file (vision 6.7: use
/// [`super::Absent`] first to replace a directory or a link), and if a
/// [`Copy::from_local_path`] source is not a regular file.
///
/// Files of any size stream, as root or as any account: `check` compares
/// the two a chunk at a time and stops at the first difference, and
/// `apply` writes the source into `dest` as it reads it, so neither is
/// held whole in memory. Only a text diff reads both whole, and only when
/// both are within `TEXT_DIFF_LIMIT`. New content is given `.mode()` and
/// `.owner()` before it is renamed into place, so it is never readable at
/// a wider mode, and a refused owner leaves `dest` as it was.
#[derive(Debug, Clone)]
pub struct Copy {
    source: CopySource,
    dest: PathBuf,
    mode: Option<u32>,
    owner: Option<Owner>,
    backup: bool,
}

/// A `Copy` with a source but no destination yet; `.to(dest)` finishes it.
#[derive(Debug, Clone)]
pub struct CopyBuilder {
    source: CopySource,
}

/// Output of [`struct@Copy`], as `apply` left the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyReport {
    /// Whether the bytes were (or would be) rewritten, as opposed to an
    /// attributes-only change.
    pub content_changed: bool,
    /// The destination, as given to `.to(..)`.
    pub path: PathBuf,
    /// Set only when `.backup(true)` and a previous version was saved.
    pub backup_path: Option<PathBuf>,
    /// Size of the content now at `path`.
    pub bytes: usize,
}

impl Copy {
    /// Content from bytes, typically `include_bytes!("../files/x")`.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> CopyBuilder {
        CopyBuilder {
            source: CopySource::Bytes(bytes.as_ref().to_vec()),
        }
    }

    /// Content from a string, typically `include_str!` or a `format!`.
    /// Ansible's `content:`.
    #[allow(
        clippy::should_implement_trait,
        reason = "not a parser; mirrors from_bytes and the vision's naming"
    )]
    pub fn from_str(text: &str) -> CopyBuilder {
        Self::from_bytes(text.as_bytes())
    }

    /// Content read from a file on the target, in `check` and again in
    /// `apply` (see [`CopySource::LocalPath`]). A source that is not a
    /// regular file (a directory, a FIFO, a device), after following
    /// symlinks, is refused before anything reads it, as Ansible's `copy`
    /// refuses one.
    pub fn from_local_path(path: impl Into<PathBuf>) -> CopyBuilder {
        CopyBuilder {
            source: CopySource::LocalPath(path.into()),
        }
    }

    /// Permission bits as an octal literal (`0o644`). Only the low twelve
    /// bits are compared and set. Left unset, the mode is neither checked
    /// nor changed: an existing file keeps its own, and a file this op
    /// creates keeps whatever the write gave it.
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Numeric owner (`chown uid:gid`). Changing the owner clears setuid,
    /// and setgid with group execute, as `chown` does; give `.mode(..)` too
    /// to keep them.
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some(Owner { uid, gid });
        self
    }

    /// Keep a copy of the previous content next to the file before
    /// overwriting it (`<name>.~rustible.<unix-ts>`). A write that fails
    /// removes the copy it took.
    pub fn backup(mut self, on: bool) -> Self {
        self.backup = on;
        self
    }

    /// The source's size, refusing a path that is not a regular file before
    /// anything reads it (decision 25). Exact for bytes; for a path it is
    /// what `stat` says, which is not the content's size for a file under
    /// `/proc` or `/sys`, so it is used only to choose how a change is
    /// shown. `None` for a path that does not exist, which opening it
    /// reports.
    fn source_size(&self, sys: &System) -> Result<Option<u64>> {
        let p = match &self.source {
            CopySource::Bytes(b) => return Ok(Some(b.len() as u64)),
            CopySource::LocalPath(p) => p,
        };
        match sys
            .stat_follow(p)
            .with_context(|| format!("reading copy source {}", p.display()))?
        {
            None => Ok(None),
            Some(s) if s.kind == FileKind::File => Ok(Some(s.size)),
            Some(s) => bail!(
                "{} is not a regular file ({}), so not copied; file::Copy copies regular files",
                p.display(),
                kind_name(s.kind)
            ),
        }
    }

    /// A reader over the source.
    fn open_source<'s>(&'s self, sys: &'s System) -> Result<Box<dyn Read + Send + 's>> {
        match &self.source {
            CopySource::Bytes(b) => Ok(Box::new(Cursor::new(b.as_slice()))),
            CopySource::LocalPath(p) => sys
                .open_read(p)
                .with_context(|| format!("reading copy source {}", p.display())),
        }
    }

    /// The change to `dest`'s content as the report shows it, `dest` being
    /// `before` bytes (`None`: missing) and the source `after`. The two are
    /// read, up to `TEXT_DIFF_LIMIT`, only when both claim to fit in it.
    fn content_change(
        &self,
        sys: &System,
        before: Option<u64>,
        after: u64,
    ) -> Result<ContentChange> {
        let fits = |n: u64| n <= TEXT_DIFF_LIMIT as u64;
        if before.is_none_or(fits) && fits(after) {
            let old = match before {
                None => Some(vec![]),
                Some(_) => read_at_most(sys.open_read(&self.dest)?, TEXT_DIFF_LIMIT)?,
            };
            let new = read_at_most(self.open_source(sys)?, TEXT_DIFF_LIMIT)?;
            if let (Some(old), Some(new)) = (old, new) {
                return Ok(ContentChange::of(Some(&old), &new));
            }
        }
        Ok(ContentChange::Sizes {
            before: size(before.unwrap_or(0)),
            after: size(after),
        })
    }

    /// The mode and owner new content is written with. `.mode()` and
    /// `.owner()` as given; without `.mode()`, a write that changes the
    /// owner gives the content the mode `dest` has less what a `chown`
    /// clears (setuid, and setgid with group execute), as a `chown` of the
    /// existing file would leave it, where the write alone would carry them
    /// over to the new owner.
    fn write_attrs(&self, attrs: &AttrPlan, existing: Option<u32>) -> Option<WriteAttrs> {
        let kept = existing
            .filter(|_| self.mode.is_none() && attrs.owner_to_set().is_some())
            .map(|m| m & 0o7777)
            .filter(|m| cleared_by_chown(*m) != 0)
            .map(|m| m & !cleared_by_chown(m));
        let w = WriteAttrs {
            mode: self.mode.map(|m| m & 0o7777).or(kept),
            owner: self.owner.map(|o| (o.uid, o.gid)),
        };
        (w.mode.is_some() || w.owner.is_some()).then_some(w)
    }
}

impl CopyBuilder {
    /// The destination path. Finishes the builder.
    pub fn to(self, dest: impl Into<PathBuf>) -> Copy {
        Copy {
            source: self.source,
            dest: dest.into(),
            mode: None,
            owner: None,
            backup: false,
        }
    }
}

/// How a refusal names a kind of file that is not a regular one.
fn kind_name(kind: FileKind) -> &'static str {
    match kind {
        FileKind::File => "a regular file",
        FileKind::Dir => "a directory",
        FileKind::Symlink => "a symlink",
        FileKind::Other => "a FIFO, socket or device",
    }
}

/// A size as the report's `usize`.
fn size(n: u64) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

/// Fill `buf` from `r` as far as it goes; fewer bytes only at the end.
fn fill(r: &mut dyn Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Whether `a` and `b` yield the same bytes, read a chunk of each at a
/// time and stopping at the first chunk that differs: the length when they
/// do, `None` when they do not.
pub(crate) fn same_content(a: &mut dyn Read, b: &mut dyn Read) -> io::Result<Option<u64>> {
    let (mut x, mut y) = (vec![0u8; COMPARE_CHUNK], vec![0u8; COMPARE_CHUNK]);
    let mut total = 0u64;
    loop {
        let n = fill(a, &mut x)?;
        let m = fill(b, &mut y)?;
        if x[..n] != y[..m] {
            return Ok(None);
        }
        total += n as u64;
        if n < COMPARE_CHUNK {
            return Ok(Some(total));
        }
    }
}

/// All of `r` when it holds at most `limit` bytes, `None` when it holds
/// more; never more than `limit + 1` bytes are read.
fn read_at_most(r: impl Read, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut buf = Vec::new();
    r.take(limit as u64 + 1).read_to_end(&mut buf)?;
    Ok((buf.len() <= limit).then_some(buf))
}

/// A content change as the report shows it: the two texts when both are
/// small UTF-8, their sizes otherwise. Holds only what the rendering needs,
/// so a large file is never carried twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContentChange {
    /// Both sides within [`TEXT_DIFF_LIMIT`] and valid UTF-8.
    Text { before: String, after: String },
    /// Anything else: binary, or too large to diff.
    Sizes { before: usize, after: usize },
}

impl ContentChange {
    pub(crate) fn of(old: Option<&[u8]>, new: &[u8]) -> Self {
        let old = old.unwrap_or_default();
        if old.len() <= TEXT_DIFF_LIMIT
            && new.len() <= TEXT_DIFF_LIMIT
            && let (Ok(before), Ok(after)) = (std::str::from_utf8(old), std::str::from_utf8(new))
        {
            return ContentChange::Text {
                before: before.into(),
                after: after.into(),
            };
        }
        ContentChange::Sizes {
            before: old.len(),
            after: new.len(),
        }
    }

    pub(crate) fn diff(&self, path: &Path) -> Diff {
        match self {
            ContentChange::Text { before, after } => {
                Diff::text(path, before.as_str(), after.as_str())
            }
            ContentChange::Sizes { before, after } => Diff::summary(format!(
                "{}: {before} bytes -> {after} bytes",
                path.display()
            )),
        }
    }
}

/// The diff for a content change: unified text when both sides are small
/// UTF-8, a byte-count summary otherwise.
pub fn content_diff(path: &Path, old: Option<&[u8]>, new: &[u8]) -> Diff {
    ContentChange::of(old, new).diff(path)
}

/// What [`Copy`](struct@Copy)'s `check` decided: whether to rewrite the
/// content, with what mode and owner, and the attributes to set otherwise.
/// The bytes to write stay on the op, or in the source file.
#[derive(Debug)]
pub struct CopyIntent {
    dest: PathBuf,
    content: Content,
    attrs: AttrPlan,
}

/// The content half of a [`CopyIntent`].
#[derive(Debug)]
enum Content {
    /// `dest` already holds the source's bytes, this many of them; only the
    /// attributes change.
    Same(u64),
    /// Write the source over `dest` (after a backup, when asked), with
    /// these attributes on the new content before it is renamed into place.
    Rewrite {
        /// What the report shows of the change.
        change: ContentChange,
        attrs: Option<WriteAttrs>,
    },
}

impl Intent for CopyIntent {
    fn diff(&self) -> Diff {
        match &self.content {
            // A content change is reported as the content; the attributes
            // that come with it are set, and not listed.
            Content::Rewrite { change, .. } => change.diff(&self.dest),
            Content::Same(_) => Diff::attrs(self.dest.display().to_string(), self.attrs.changes()),
        }
    }
}

impl Op for Copy {
    type Output = CopyReport;
    type Intent = CopyIntent;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable. file::Copy writes a file through `sys` and sets mode and owner; nothing in it is a Linux concept.
        // The supported set is written out rather than left open, so a new
        // platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!("file::Copy has no implementation for {}", other.name()),
        }
        let src_size = self.source_size(sys)?;
        let stat = sys.stat(&self.dest)?;
        let dest_size = match &stat {
            None => None,
            Some(s) if s.kind == FileKind::File => Some(s.size),
            Some(s) => bail!(
                "{} exists and is not a regular file ({:?}); remove it first with file::Absent",
                self.dest.display(),
                s.kind
            ),
        };
        // Sizes first, where both are exact: bytes in memory against the
        // destination's `stat`. A path's `stat` is not its content's size
        // under `/proc` and `/sys`, so a path source is always compared. A
        // source that does not exist fails here or in `content_change`,
        // whichever opens it first.
        let same = match dest_size {
            None => None,
            Some(n) if matches!(self.source, CopySource::Bytes(_)) && Some(n) != src_size => None,
            Some(_) => same_content(&mut self.open_source(sys)?, &mut sys.open_read(&self.dest)?)?,
        };
        let attrs = plan_attrs(stat.as_ref(), self.mode, self.owner);
        let content = match same {
            Some(n) if !attrs.differs() => {
                return Ok(Plan::Satisfied(CopyReport {
                    content_changed: false,
                    path: self.dest.clone(),
                    backup_path: None,
                    bytes: size(n),
                }));
            }
            Some(n) => Content::Same(n),
            None => Content::Rewrite {
                change: self.content_change(sys, dest_size, src_size.unwrap_or(0))?,
                attrs: self.write_attrs(&attrs, stat.as_ref().map(|s| s.mode)),
            },
        };
        Ok(Plan::Change(CopyIntent {
            dest: self.dest.clone(),
            content,
            attrs,
        }))
    }

    fn apply(&self, sys: &System, intent: CopyIntent) -> Result<CopyReport> {
        match intent.content {
            // The bytes stay on the op, so a `LocalPath` source is read
            // again here: one that changed since `check` is written as it
            // is now, an accepted race like the other ops' (`[ISSUE-43]`).
            // The mode and owner are the new content's before the rename
            // (decision 24), and a requested owner that cannot be given
            // fails the write with `dest` as it was.
            Content::Rewrite { attrs, .. } => {
                let (backup_path, n) = super::write_from_with_backup(
                    sys,
                    &intent.dest,
                    self.backup,
                    self.open_source(sys)?,
                    attrs,
                )?;
                Ok(CopyReport {
                    content_changed: true,
                    path: intent.dest,
                    backup_path,
                    bytes: size(n),
                })
            }
            // Only what is wrong: a `chown` to the owner the file already
            // has would clear setuid with nothing to set it back when no
            // mode was asked for.
            Content::Same(n) => {
                intent.attrs.apply_differing(sys, &intent.dest)?;
                Ok(CopyReport {
                    content_changed: false,
                    path: intent.dest,
                    backup_path: None,
                    bytes: size(n),
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::{AttrCall, Fake, ReadCall};
    use rustible_sdk::event::Collect;

    use super::super::testing::{expect_change, fake_sys, not_regular_sys};
    use super::*;

    /// The other half of the platform work: a portable op has to keep
    /// working on a mac, and say so explicitly rather than by omission.
    /// Measured against a real mac — this op wrote `/etc/rustible-spike/marker`
    /// there as root.
    #[test]
    fn copy_runs_on_a_mac() {
        let fake = Arc::new(Fake::new().with_dir("/etc"));
        let base = fake_sys(&fake);
        let mut facts = base.facts().clone();
        facts.os = Os::Macos;
        facts.distro = Distro::Macos;
        let sys = base.with_facts(facts);

        let op = Copy::from_str("hello\n").to("/etc/x.conf");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/etc/x.conf").unwrap(), "hello\n");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// And a platform nobody has claimed is refused, not assumed.
    #[test]
    fn copy_refuses_an_unclaimed_platform() {
        let fake = Arc::new(Fake::new().with_dir("/etc"));
        let base = fake_sys(&fake);
        let mut facts = base.facts().clone();
        facts.os = Os::Other("freebsd".into());
        let sys = base.with_facts(facts);
        let err = Copy::from_str("x\n")
            .to("/etc/x.conf")
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("file::Copy has no implementation for freebsd"),
            "{err}"
        );
    }

    #[test]
    fn copy_is_satisfied_when_bytes_and_attrs_match() {
        let fake = Arc::new(Fake::new().with_file_mode("/etc/x.conf", "a=1\n", 0o600));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("a=1\n")
            .to("/etc/x.conf")
            .mode(0o600)
            .owner(0, 0);
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(
            r,
            CopyReport {
                content_changed: false,
                path: "/etc/x.conf".into(),
                backup_path: None,
                bytes: 4
            }
        );
    }

    #[test]
    fn copy_text_change_has_unified_diff() {
        let fake = Arc::new(Fake::new().with_file("/etc/x.conf", "a=1\nb=2\n"));
        let sys = fake_sys(&fake);
        let op = Copy::from_bytes(b"a=1\nb=3\n").to("/etc/x.conf");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().short(), "+1 -1 lines");
        assert!(
            c.diff().render().contains("-b=2\n+b=3"),
            "{}",
            c.diff().render()
        );

        let r = op.apply(&sys, c).unwrap();
        assert!(r.content_changed, "a text diff means a rewrite");
        assert_eq!(r.bytes, 8);
        assert_eq!(r.backup_path, None);
        assert_eq!(fake.content("/etc/x.conf").unwrap(), "a=1\nb=3\n");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn copy_creates_missing_file_with_mode() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Copy::from_str("hello\n").to("/new").mode(0o640);
        let c = expect_change(&op, &sys);
        // Creation is a text diff against an empty "before".
        assert_eq!(c.diff().short(), "+1 -0 lines");
        op.apply(&sys, c).unwrap();
        let f = fake.file("/new").unwrap();
        assert_eq!(
            (f.mode, String::from_utf8(f.bytes).unwrap()),
            (0o640, "hello\n".into())
        );
    }

    #[test]
    fn copy_binary_content_produces_summary_diff() {
        let fake = Arc::new(Fake::new().with_file("/bin/blob", [0u8, 159, 146, 150]));
        let sys = fake_sys(&fake);
        let op = Copy::from_bytes([0u8, 159, 146, 150, 1, 2]).to("/bin/blob");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().render(), "/bin/blob: 4 bytes -> 6 bytes");
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.file("/bin/blob").unwrap().bytes.len(), 6);
    }

    #[test]
    fn copy_large_text_falls_back_to_summary() {
        let big = "x".repeat(TEXT_DIFF_LIMIT + 1);
        let d = content_diff(Path::new("/f"), Some(b"small"), big.as_bytes());
        assert_eq!(d.render(), "/f: 5 bytes -> 65537 bytes");
        assert_eq!(
            ContentChange::of(Some(b"small"), big.as_bytes()),
            ContentChange::Sizes {
                before: 5,
                after: TEXT_DIFF_LIMIT + 1
            }
        );
        let d = content_diff(Path::new("/f"), None, b"small");
        assert_eq!(d.short(), "+1 -0 lines");
    }

    /// A rewrite of a setuid file with `.mode()` and `.owner()`, both
    /// already right, keeps the bit: the new content is given both before
    /// the rename, the bit set again after its `chown`.
    #[test]
    fn copy_rewrite_of_a_setuid_file_keeps_the_bit() {
        let fake = Arc::new(Fake::new().with_file_mode("/usr/local/bin/x", "v1\n", 0o4755));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("v2\n")
            .to("/usr/local/bin/x")
            .mode(0o4755)
            .owner(0, 0);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().short(), "+1 -1 lines");
        op.apply(&sys, c).unwrap();
        let f = fake.file("/usr/local/bin/x").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o4755, 0, 0));
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// Without `.mode()`, a rewrite keeps the mode the file had, setuid and
    /// setgid with group execute included, and the next `check` is
    /// `Satisfied`. Nothing in `apply` sets a mode here, so this is the
    /// backend's rewrite alone: one that `chown`ed the replacement after
    /// copying the old mode onto it would leave 0755, and `check`, which
    /// compares no mode, would never notice (issue #51).
    #[test]
    fn copy_rewrite_preserves_existing_mode_when_none_given() {
        for mode in [0o600, 0o4755, 0o2755, 0o6755] {
            let fake = Arc::new(Fake::new().with_file_mode("/usr/local/bin/x", "old\n", mode));
            let sys = fake_sys(&fake);
            let op = Copy::from_str("new\n").to("/usr/local/bin/x");
            let c = expect_change(&op, &sys);
            op.apply(&sys, c).unwrap();
            let f = fake.file("/usr/local/bin/x").unwrap();
            assert_eq!(
                (f.mode, String::from_utf8(f.bytes).unwrap()),
                (mode, "new\n".into()),
                "write_atomic keeps the whole mode of an existing file: {mode:o}"
            );
            assert!(fake.attr_calls().is_empty(), "no mode was asked for");
            assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        }
    }

    /// The `chmod` and `chown` calls since `planted`, and the one path they
    /// were all made on: the staged file `write_from` renames over `dest`,
    /// never `dest` itself (decision 24).
    fn staged_calls(fake: &Fake, planted: usize, dest: &str) -> Vec<AttrCall> {
        let calls = fake.attr_calls()[planted..].to_vec();
        let dest = Path::new(dest);
        for call in &calls {
            let (AttrCall::Chmod { path, .. } | AttrCall::Chown { path, .. }) = call else {
                unreachable!()
            };
            assert_ne!(
                path, dest,
                "set on the final path after the rename: {calls:?}"
            );
            assert_eq!(path.parent(), dest.parent(), "{calls:?}");
            assert!(
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".rustible-fake-"),
                "{calls:?}"
            );
        }
        calls
    }

    /// One call [`staged`] returns, its path left out.
    #[derive(Debug, PartialEq)]
    enum Set {
        Mode(u32),
        Owner(u32, u32),
    }

    /// `staged_calls`, with the staged path left out so a test can compare
    /// the calls themselves.
    fn staged(fake: &Fake, planted: usize, dest: &str) -> Vec<Set> {
        staged_calls(fake, planted, dest)
            .into_iter()
            .map(|c| match c {
                AttrCall::Chmod { mode, .. } => Set::Mode(mode),
                AttrCall::Chown { uid, gid, .. } => Set::Owner(uid, gid),
                _ => unreachable!(),
            })
            .collect()
    }

    /// With `.owner()` already right and no `.mode()`, a rewrite keeps
    /// setuid and setgid. The owner is given to the new content before the
    /// rename, so its `chown` clears the bits on the staged file, and the
    /// write sets the mode it kept again after it (found by review of #74
    /// when the `chown` came after the rename and nothing set them back).
    #[test]
    fn copy_rewrite_with_owner_already_right_keeps_setuid() {
        for mode in [0o4755, 0o2755] {
            let fake = Arc::new(Fake::new().with_file_mode("/usr/local/bin/x", "v1\n", mode));
            let planted = fake.attr_calls().len();
            let sys = fake_sys(&fake);
            let op = Copy::from_str("v2\n").to("/usr/local/bin/x").owner(0, 0);
            let c = expect_change(&op, &sys);
            op.apply(&sys, c).unwrap();
            let f = fake.file("/usr/local/bin/x").unwrap();
            assert_eq!((f.mode, f.uid, f.gid), (mode, 0, 0), "{mode:o}");
            assert_eq!(
                staged(&fake, planted, "/usr/local/bin/x"),
                [Set::Mode(0o755), Set::Owner(0, 0), Set::Mode(mode)],
                "{mode:o}"
            );
            assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        }
    }

    /// The owner asked for is given to the new content whatever the file
    /// had by the time of `apply`: here a `chown` planted between `check`
    /// and `apply`, as an unprivileged rewrite used to leave it (found by
    /// the second review of #74, when the owner was set after the rewrite
    /// and only where `check` had seen it wrong).
    #[test]
    fn a_rewrite_gives_the_owner_asked_for_whatever_the_file_has_by_then() {
        let fake = Arc::new(Fake::new().with_file_mode("/srv/app.conf", "v1\n", 0o640));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("v2\n").to("/srv/app.conf").owner(0, 0);
        let c = expect_change(&op, &sys);
        rustible_sdk::backend::Backend::set_owner(&*fake, Path::new("/srv/app.conf"), 1000, 1000)
            .unwrap();
        let planted = fake.attr_calls().len();
        op.apply(&sys, c).unwrap();
        let f = fake.file("/srv/app.conf").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o640, 0, 0));
        assert_eq!(
            staged(&fake, planted, "/srv/app.conf"),
            [Set::Mode(0o640), Set::Owner(0, 0)]
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// A `chown` that changes the owner, with no `.mode()`, leaves a setuid
    /// file without the bit: the kernel clears it, as Ansible's `owner:`
    /// leaves it cleared, and nothing was asked to set it back. Pinned so a
    /// change to that is a decision; `.mode(0o4755)` is how to keep it.
    #[test]
    fn copy_changing_the_owner_without_mode_clears_setuid_as_the_kernel_does() {
        let fake = Arc::new(Fake::new().with_file_mode("/usr/local/bin/x", "v1\n", 0o4755));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("v2\n").to("/usr/local/bin/x").owner(5, 6);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        let f = fake.file("/usr/local/bin/x").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o755, 5, 6));

        let fake = Arc::new(Fake::new().with_file_mode("/usr/local/bin/x", "v1\n", 0o4755));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("v2\n")
            .to("/usr/local/bin/x")
            .owner(5, 6)
            .mode(0o4755);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        let f = fake.file("/usr/local/bin/x").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o4755, 5, 6));
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// A new file wanted at 0600 under another owner gets both before it is
    /// renamed into place (#79, decision 24), mode before owner, and no
    /// third call, since 0600 has no setuid for the `chown` to clear.
    /// Nothing is set on the final path afterwards. A refused `chown`, as
    /// for an identity without `CAP_CHOWN`, fails the step with nothing
    /// created at all: not a file at 0644, nor one at 0600 under the wrong
    /// owner, nor a staged file beside it.
    #[test]
    fn copy_gives_new_content_its_mode_and_owner_before_the_rename() {
        let fake = Arc::new(Fake::new().with_dir("/etc/app"));
        let planted = fake.attr_calls().len();
        let sys = fake_sys(&fake);
        let op = Copy::from_bytes(b"secret")
            .to("/etc/app/key")
            .mode(0o600)
            .owner(5, 6);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(
            staged(&fake, planted, "/etc/app/key"),
            [Set::Mode(0o600), Set::Owner(5, 6)]
        );
        let f = fake.file("/etc/app/key").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o600, 5, 6));

        let fake = Arc::new(Fake::new().with_dir("/etc/app").with_chown_refused());
        let sys = fake_sys(&fake);
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("Operation not permitted"), "{err}");
        assert!(fake.file("/etc/app/key").is_none());
        assert_eq!(sys.read_dir("/etc/app").unwrap(), Vec::<PathBuf>::new());
    }

    /// A rewrite of a setuid file under a new owner, with `.mode(0o4755)`:
    /// the staged content is 0755 for its `chown` and 4755 after it, so a
    /// refused `chown` never leaves the new content setuid under the old
    /// owner, and the rename puts it in place finished.
    #[test]
    fn copy_rewrite_under_a_new_owner_clears_setuid_before_the_chown() {
        let fake = Arc::new(Fake::new().with_file_mode("/usr/local/bin/x", "v1\n", 0o4755));
        let planted = fake.attr_calls().len();
        let sys = fake_sys(&fake);
        let op = Copy::from_str("v2\n")
            .to("/usr/local/bin/x")
            .mode(0o4755)
            .owner(5, 6);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(
            staged(&fake, planted, "/usr/local/bin/x"),
            [Set::Mode(0o755), Set::Owner(5, 6), Set::Mode(0o4755)]
        );
        let f = fake.file("/usr/local/bin/x").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o4755, 5, 6));
    }

    /// A rewrite under a new owner with no `.mode()`: the new content gets
    /// the old mode less what a `chown` clears (setuid, and setgid with
    /// group execute; setgid without it survives a real `chown`, so it is
    /// kept), as a `chown` of the old file would have left it. When that
    /// `chown` is refused the step fails and the file is as it was, old
    /// content, mode and owner, where it used to be left with the new
    /// content under the old owner.
    #[test]
    fn an_owner_only_rewrite_gives_the_mode_a_chown_would_leave() {
        for (mode, left) in [
            (0o4755, 0o755),
            (0o2755, 0o755),
            (0o6750, 0o750),
            (0o2745, 0o2745),
            (0o6745, 0o2745),
        ] {
            let op = Copy::from_str("v2\n").to("/usr/local/bin/x").owner(5, 6);
            let fake = Arc::new(
                Fake::new()
                    .with_file_mode("/usr/local/bin/x", "v1\n", mode)
                    .with_chown_refused(),
            );
            let sys = fake_sys(&fake);
            let c = expect_change(&op, &sys);
            let err = op.apply(&sys, c).unwrap_err().chain();
            assert!(err.contains("Operation not permitted"), "{err}");
            let f = fake.file("/usr/local/bin/x").unwrap();
            assert_eq!(
                (f.mode, f.uid, f.bytes.as_slice()),
                (mode, 0, &b"v1\n"[..]),
                "{mode:04o}"
            );

            let fake = Arc::new(Fake::new().with_file_mode("/usr/local/bin/x", "v1\n", mode));
            let sys = fake_sys(&fake);
            let c = expect_change(&op, &sys);
            op.apply(&sys, c).unwrap();
            let f = fake.file("/usr/local/bin/x").unwrap();
            assert_eq!(
                (f.mode, f.uid, f.gid, f.bytes.as_slice()),
                (left, 5, 6, &b"v2\n"[..]),
                "{mode:04o}"
            );
        }
    }

    #[test]
    fn copy_attrs_only_change_does_not_rewrite() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/etc")
                .with_file_mode("/etc/x", "same\n", 0o644),
        );
        let sys = fake_sys(&fake);
        let op = Copy::from_str("same\n")
            .to("/etc/x")
            .mode(0o600)
            .owner(7, 8)
            .backup(true);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().short(), "mode=0600 owner=7:8");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.backup_path, None, "no content change, so no backup");
        assert!(
            !r.content_changed,
            "attributes only: the content is not rewritten"
        );
        let f = fake.file("/etc/x").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o600, 7, 8));
        // No backup file appeared next to it.
        assert_eq!(sys.read_dir("/etc").unwrap(), vec![PathBuf::from("/etc/x")]);
    }

    #[test]
    fn copy_backup_is_taken_before_rewrite() {
        let fake = Arc::new(Fake::new().with_file("/etc/x", "old\n"));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("new\n").to("/etc/x").backup(true);
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        let bp = r.backup_path.expect("backup path");
        assert!(
            bp.to_string_lossy().starts_with("/etc/x.~rustible."),
            "{}",
            bp.display()
        );
        assert_eq!(fake.content(&bp).unwrap(), "old\n");
        assert_eq!(fake.content("/etc/x").unwrap(), "new\n");
    }

    #[test]
    fn copy_from_local_path_reads_source_through_sys() {
        let fake = Arc::new(Fake::new().with_file("/opt/src.conf", "from source\n"));
        let sys = fake_sys(&fake);
        let op = Copy::from_local_path("/opt/src.conf").to("/etc/dst.conf");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/etc/dst.conf").unwrap(), "from source\n");

        let err = Copy::from_local_path("/opt/missing")
            .to("/etc/dst.conf")
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(err.contains("reading copy source /opt/missing"), "{err}");
    }

    #[test]
    fn copy_fails_when_dest_is_a_directory_or_link() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/etc/d")
                .with_symlink("/etc/l", "/etc/real"),
        );
        let sys = fake_sys(&fake);
        let err = Copy::from_str("x")
            .to("/etc/d")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file (Dir)"), "{err}");
        let err = Copy::from_str("x")
            .to("/etc/l")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file (Symlink)"), "{err}");
    }

    #[test]
    fn copy_in_check_mode_reports_would_change_and_writes_nothing() {
        let fake = Arc::new(Fake::new().with_file("/f", "a\n"));
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step("copy", Copy::from_str("bb\n").to("/f").mode(0o600))
            .unwrap();
        assert!(r.changed && !r.is_available(), "no apply, so no output");
        assert_eq!(r.diff.as_ref().unwrap().short(), "+1 -1 lines");
        let f = fake.file("/f").unwrap();
        assert_eq!((f.mode, f.bytes.as_slice()), (0o644, b"a\n".as_slice()));
    }

    // ---- streaming (#86) ----

    /// The reads of `path` the fake served since `from`.
    fn reads_of(fake: &Fake, from: usize, path: &str) -> Vec<ReadCall> {
        fake.reads()[from..]
            .iter()
            .filter(|r| r.path == Path::new(path))
            .cloned()
            .collect()
    }

    /// A destination over `TEXT_DIFF_LIMIT` that differs from the source is
    /// reported as a summary, and `check` does not read it whole: against
    /// bytes of another size it is not read at all (the sizes differ), and
    /// against a path of the same size only until the first chunk that
    /// differs.
    #[test]
    fn a_large_unequal_destination_is_summarised_without_reading_it_whole() {
        let big = 4 * TEXT_DIFF_LIMIT;
        let fake = Arc::new(
            Fake::new()
                .with_file("/srv/dest", vec![b'a'; big])
                .with_file("/srv/src", vec![b'b'; big]),
        );
        let sys = fake_sys(&fake);

        let op = Copy::from_bytes(vec![b'a'; big + 1]).to("/srv/dest");
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.diff().render(),
            format!("/srv/dest: {big} bytes -> {} bytes", big + 1)
        );
        assert_eq!(reads_of(&fake, 0, "/srv/dest"), []);

        let from = fake.reads().len();
        let op = Copy::from_local_path("/srv/src").to("/srv/dest");
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.diff().render(),
            format!("/srv/dest: {big} bytes -> {big} bytes")
        );
        let dest = reads_of(&fake, from, "/srv/dest");
        assert!(
            !dest.is_empty() && dest.iter().all(|r| r.streamed && r.bytes < big as u64),
            "{dest:?}"
        );
        let src = reads_of(&fake, from, "/srv/src");
        assert!(
            src.iter().all(|r| r.streamed && r.bytes < big as u64),
            "{src:?}"
        );

        op.apply(&sys, c).unwrap();
        assert_eq!(fake.file("/srv/dest").unwrap().bytes, vec![b'b'; big]);
    }

    /// Equal content is `Satisfied`, whatever its size, and comparing it is
    /// a streamed read of each side to its end, never a whole one.
    #[test]
    fn equal_large_content_is_satisfied_by_streaming_both_sides() {
        let big = 4 * TEXT_DIFF_LIMIT + 3;
        let fake = Arc::new(
            Fake::new()
                .with_file("/srv/dest", vec![7u8; big])
                .with_file("/srv/src", vec![7u8; big]),
        );
        let sys = fake_sys(&fake);
        for op in [
            Copy::from_local_path("/srv/src").to("/srv/dest"),
            Copy::from_bytes(vec![7u8; big]).to("/srv/dest"),
        ] {
            let from = fake.reads().len();
            let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
                panic!("expected satisfied")
            };
            assert_eq!((r.bytes, r.content_changed), (big, false));
            let reads = &fake.reads()[from..];
            assert!(reads.iter().all(|r| r.streamed), "{reads:?}");
            let dest = reads_of(&fake, from, "/srv/dest");
            assert_eq!(dest.iter().map(|r| r.bytes).sum::<u64>(), big as u64);
        }
    }

    /// A path source is read as a stream by `check` and `apply` alike, and
    /// written as one: nothing reads it, or the destination, whole.
    #[test]
    fn a_path_source_streams_into_the_destination() {
        let big = 3 * TEXT_DIFF_LIMIT;
        let content: Vec<u8> = (0..big).map(|i| (i % 251) as u8).collect();
        let fake = Arc::new(
            Fake::new()
                .with_file("/srv/src", &content)
                .with_file("/srv/dest", "old\n"),
        );
        let sys = fake_sys(&fake);
        let op = Copy::from_local_path("/srv/src")
            .to("/srv/dest")
            .mode(0o640);
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.diff().render(),
            format!("/srv/dest: 4 bytes -> {big} bytes")
        );
        let r = op.apply(&sys, c).unwrap();
        assert_eq!((r.bytes, r.content_changed), (big, true));
        let f = fake.file("/srv/dest").unwrap();
        assert_eq!((f.mode, f.bytes.as_slice()), (0o640, content.as_slice()));
        assert!(
            fake.reads().iter().all(|r| r.streamed),
            "{:?}",
            fake.reads()
        );
        let src = reads_of(&fake, 0, "/srv/src");
        assert_eq!(src.last().unwrap().bytes, big as u64, "{src:?}");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// A small change to a missing file reads only the source, and shows it
    /// as text; a large one reads nothing and shows the sizes.
    #[test]
    fn a_new_file_is_shown_as_text_when_small_and_as_a_size_when_not() {
        let big = TEXT_DIFF_LIMIT + 1;
        let fake = Arc::new(
            Fake::new()
                .with_file("/srv/small", "a\n")
                .with_file("/srv/big", vec![b'x'; big]),
        );
        let sys = fake_sys(&fake);
        let c = expect_change(&Copy::from_local_path("/srv/small").to("/srv/d1"), &sys);
        assert_eq!(c.diff().short(), "+1 -0 lines");
        let from = fake.reads().len();
        let c = expect_change(&Copy::from_local_path("/srv/big").to("/srv/d2"), &sys);
        assert_eq!(
            c.diff().render(),
            format!("/srv/d2: 0 bytes -> {big} bytes")
        );
        assert_eq!(fake.reads()[from..], []);
    }

    /// Decision 25: a source that is not a regular file is refused before
    /// anything reads it, in a real run and a dry one, as Ansible's `copy`
    /// refuses one (`modules/copy.py`, "Cannot copy invalid source"). A FIFO
    /// would block the read for a writer, and a directory has no content.
    /// A symlink to a regular file is followed.
    #[test]
    fn a_source_that_is_not_a_regular_file_is_refused_before_any_read() {
        // The fake cannot plant a FIFO: this one is a file it reports as one.
        let fake = Arc::new(
            Fake::new()
                .with_file("/run/fifo", "never read\n")
                .with_dir("/srv/dir")
                .with_file("/srv/real", "linked\n")
                .with_symlink("/srv/link", "/srv/real"),
        );
        for check_mode in [false, true] {
            let sys = not_regular_sys(&fake, "/run/fifo").with_check_mode(check_mode);
            let err = Copy::from_local_path("/run/fifo")
                .to("/srv/out")
                .check(&sys)
                .unwrap_err()
                .chain();
            assert!(
                err.contains(
                    "/run/fifo is not a regular file (a FIFO, socket or device), so not \
                     copied; file::Copy copies regular files"
                ),
                "{err}"
            );
            let err = Copy::from_local_path("/srv/dir")
                .to("/srv/out")
                .check(&sys)
                .unwrap_err()
                .chain();
            assert!(
                err.contains("/srv/dir is not a regular file (a directory), so not copied"),
                "{err}"
            );
            assert_eq!(fake.reads(), [], "nothing was read");
        }
        let sys = fake_sys(&fake);
        let op = Copy::from_local_path("/srv/link").to("/srv/out");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/srv/out").unwrap(), "linked\n");
    }

    /// Decision 26: a write that fails after its backup was taken removes
    /// that backup, so a failed run leaves no `.~rustible.` copy of the file
    /// it did not change. Here the write fails on its requested owner; a
    /// source that fails part way is `write_from_with_backup`'s own test.
    #[test]
    fn a_failed_write_leaves_no_backup() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/etc")
                .with_file("/etc/x", "old\n")
                .with_chown_refused(),
        );
        let sys = fake_sys(&fake);
        let op = Copy::from_str("new\n")
            .to("/etc/x")
            .owner(5, 6)
            .backup(true);
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("Operation not permitted"), "{err}");
        assert_eq!(sys.read_dir("/etc").unwrap(), [PathBuf::from("/etc/x")]);
        assert_eq!(fake.content("/etc/x").unwrap(), "old\n");
    }

    /// Through a real escalation helper (in process, as this user, over a
    /// temporary directory): a file of several helper chunks is compared,
    /// written with its mode and compared again, and comes out identical.
    #[test]
    fn copy_through_the_helper_streams_a_multi_chunk_file() {
        use rustible_sdk::protocol::CHUNK_SIZE;

        let sys = System::in_process_helper(Arc::new(Collect::default())).unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rustible-copy-helper-{}-{nanos}",
            std::process::id()
        ));
        struct Cleanup<'a>(&'a System, PathBuf);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = self.0.remove_all(&self.1);
            }
        }
        let _cleanup = Cleanup(&sys, dir.clone());
        sys.mkdir_all(&dir).unwrap();
        let (src, dest) = (dir.join("src"), dir.join("dest"));
        let content: Vec<u8> = (0..3 * CHUNK_SIZE + 5).map(|i| (i % 253) as u8).collect();
        sys.write_atomic(&src, &content).unwrap();
        sys.write_atomic(&dest, b"old").unwrap();

        let op = Copy::from_local_path(&src).to(&dest).mode(0o600);
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.diff().render(),
            format!("{}: 3 bytes -> {} bytes", dest.display(), content.len())
        );
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.bytes, content.len());
        assert_eq!(sys.read(&dest).unwrap(), content);
        assert_eq!(sys.stat(&dest).unwrap().unwrap().mode & 0o7777, 0o600);
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        assert_eq!(
            sys.read_dir(&dir).unwrap(),
            [dest.clone(), src.clone()],
            "nothing staged was left beside it"
        );
    }
}
