//! `file::Copy`: Ansible's `copy` (with `src` or `content`).

use std::path::{Path, PathBuf};

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

use super::{AttrPlan, Owner, plan_attrs};

/// Above this size a content change is reported as a byte-count summary
/// instead of a unified diff, whatever the encoding.
pub const TEXT_DIFF_LIMIT: usize = 64 * 1024;

/// Where the bytes come from.
#[derive(Debug, Clone)]
pub enum CopySource {
    /// Bytes baked into the binary (`include_bytes!`, `include_str!`, or a
    /// string built at run time). Vision 5.6 mechanism 1.
    Bytes(Vec<u8>),
    /// A path read **on the target**, by `check` to compare and by `apply`
    /// again to write: a source that changes in between is written as it
    /// is then, not as the diff showed it (vision 5.1: the playbook
    /// runs there, so this is not the operator's machine). For a file that
    /// only exists on the controller, embed it or stream it with
    /// `ctx.local_file` instead.
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
/// under `TEXT_DIFF_LIMIT`, otherwise as `<n> bytes -> <m> bytes`. Fails
/// if `dest` exists and is not a regular file (vision 6.7: use
/// [`super::Absent`] first to replace a directory or a link).
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
    /// `apply` (see [`CopySource::LocalPath`]).
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
    /// overwriting it (`<name>.~rustible.<unix-ts>`).
    pub fn backup(mut self, on: bool) -> Self {
        self.backup = on;
        self
    }

    fn source_bytes(&self, sys: &System) -> Result<Vec<u8>> {
        match &self.source {
            CopySource::Bytes(b) => Ok(b.clone()),
            CopySource::LocalPath(p) => sys
                .read(p)
                .with_context(|| format!("reading copy source {}", p.display())),
        }
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

/// What [`Copy`](struct@Copy)'s `check` decided: whether to rewrite the content, and the
/// attributes to set. The bytes to write stay on the op.
#[derive(Debug)]
pub struct CopyIntent {
    dest: PathBuf,
    /// `Some` when the content differs: rewrite the file (after a backup,
    /// when asked). Carries what the report shows of the change.
    rewrite: Option<ContentChange>,
    attrs: AttrPlan,
}

impl Intent for CopyIntent {
    fn diff(&self) -> Diff {
        match &self.rewrite {
            // A content change is reported as the content; the attributes
            // that come with it are set, and not listed.
            Some(content) => content.diff(&self.dest),
            None => Diff::attrs(self.dest.display().to_string(), self.attrs.changes()),
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
        let new = self.source_bytes(sys)?;
        let stat = sys.stat(&self.dest)?;
        let old = match &stat {
            None => None,
            Some(s) if s.kind == FileKind::File => Some(sys.read(&self.dest)?),
            Some(s) => bail!(
                "{} exists and is not a regular file ({:?}); remove it first with file::Absent",
                self.dest.display(),
                s.kind
            ),
        };
        let content_changed = old.as_deref() != Some(new.as_slice());
        let attrs = plan_attrs(stat.as_ref(), self.mode, self.owner);
        if !content_changed && !attrs.differs() {
            return Ok(Plan::Satisfied(CopyReport {
                content_changed,
                path: self.dest.clone(),
                backup_path: None,
                bytes: new.len(),
            }));
        }
        Ok(Plan::Change(CopyIntent {
            dest: self.dest.clone(),
            rewrite: content_changed.then(|| ContentChange::of(old.as_deref(), &new)),
            attrs,
        }))
    }

    fn apply(&self, sys: &System, intent: CopyIntent) -> Result<CopyReport> {
        // The bytes stay on the op, so a `LocalPath` source is read again
        // here: one that changed since `check` is written as it is now, an
        // accepted race like the other ops' (`[ISSUE-43]`).
        let bytes = self.source_bytes(sys)?;
        // Only what is wrong: a rewrite keeps the old owner and mode, and a
        // `chown` to the owner the file already has would clear setuid with
        // nothing to set it back when no mode was asked for. After a rewrite
        // the owner is read again, because a rewrite that could not keep it
        // does not fail.
        let backup_path = match intent.rewrite {
            Some(_) => {
                let backup = super::write_with_backup(sys, &intent.dest, self.backup, &bytes)?;
                intent.attrs.apply_after_rewrite(sys, &intent.dest)?;
                backup
            }
            None => {
                intent.attrs.apply_differing(sys, &intent.dest)?;
                None
            }
        };
        Ok(CopyReport {
            content_changed: intent.rewrite.is_some(),
            path: intent.dest,
            backup_path,
            bytes: bytes.len(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::super::testing::{expect_change, fake_sys};
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
    /// already right, keeps the bit: the rewrite keeps the old owner and
    /// mode, and `apply` sets only what `check` found wrong, here nothing.
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

    /// With `.owner()` already right and no `.mode()`, a rewrite keeps
    /// setuid and setgid. `apply` issues no `chown`: one to the owner the
    /// file already has clears those bits, and with no mode asked for
    /// nothing would set them back while the next `check`, which compares
    /// no mode, reported `Satisfied` (found by review of #74).
    #[test]
    fn copy_rewrite_with_owner_already_right_keeps_setuid() {
        for mode in [0o4755, 0o2755] {
            let fake = Arc::new(Fake::new().with_file_mode("/usr/local/bin/x", "v1\n", mode));
            let sys = fake_sys(&fake);
            let op = Copy::from_str("v2\n").to("/usr/local/bin/x").owner(0, 0);
            let c = expect_change(&op, &sys);
            op.apply(&sys, c).unwrap();
            let f = fake.file("/usr/local/bin/x").unwrap();
            assert_eq!((f.mode, f.uid, f.gid), (mode, 0, 0), "{mode:o}");
            assert!(fake.chowns().is_empty(), "{:?}", fake.attr_calls());
            assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        }
    }

    /// A rewrite that could not keep the owner does not fail
    /// (`System::write_atomic` ignores its `chown`'s `EPERM`), so `apply`
    /// reads the owner again after it and `chown`s back. On a real machine
    /// that `chown` fails as the rewrite's did, failing the step where it
    /// used to report `changed` with the wrong owner (found by the second
    /// review of #74). The `Fake`'s `chown` always succeeds and its rewrite
    /// keeps the owner, so the lost owner is planted between `check` and
    /// `apply`, and the test asserts the `chown` is issued.
    #[test]
    fn a_rewrite_that_did_not_keep_the_owner_is_chowned_back() {
        let fake = Arc::new(Fake::new().with_file_mode("/srv/app.conf", "v1\n", 0o640));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("v2\n").to("/srv/app.conf").owner(0, 0);
        let c = expect_change(&op, &sys);
        // What an unprivileged rewrite, or a root without CAP_CHOWN, leaves.
        rustible_sdk::backend::Backend::set_owner(&*fake, Path::new("/srv/app.conf"), 1000, 1000)
            .unwrap();
        op.apply(&sys, c).unwrap();
        let f = fake.file("/srv/app.conf").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o640, 0, 0));
        assert_eq!(
            fake.chowns().len(),
            2,
            "the planted chown and apply's: {:?}",
            fake.attr_calls()
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
}
