//! File operations: the Rustible side of Ansible's `copy`, `file`,
//! `lineinfile` and `blockinfile` modules. One type per desired state
//! (vision 6.3):
//!
//! | Ansible | Rustible |
//! |---|---|
//! | `copy` (with `src` or `content`) | [`struct@Copy`] |
//! | `file: state=directory` | [`Directory`] |
//! | `file: state=link` | [`Symlink`] |
//! | `file: state=absent` | [`Absent`] |
//! | `file: state=file` (attributes only) | [`Attrs`] |
//! | `lineinfile: state=present` | [`Line`] |
//! | `blockinfile` | [`Block`] |
//!
//! All I/O goes through [`System`] (vision 7). A step that would change has
//! no output in check mode, and a file or directory an earlier step could
//! create is not refused there (vision 12).

use std::path::{Path, PathBuf};

use regex::Regex;
use rustible_sdk::backend::{FileKind, Stat};
use rustible_sdk::prelude::*;

mod absent;
mod attrs;
mod block;
mod copy;
mod directory;
mod line;
mod symlink;

pub use absent::{Absent, AbsentIntent, AbsentReport};
pub use attrs::{Attrs, AttrsIntent, AttrsReport};
pub use block::{Block, BlockBuilder, BlockReport, DEFAULT_MARKER, plan_block};
pub use copy::{
    Copy, CopyBuilder, CopyIntent, CopyReport, CopySource, TEXT_DIFF_LIMIT, content_diff,
};
pub use directory::{DirReport, Directory, DirectoryIntent};
pub use line::{Line, LineBuilder, LineReport, plan_line};
pub use symlink::{Symlink, SymlinkBuilder, SymlinkIntent, SymlinkReport};

/// Where to put a line (or block) that is not present yet. Only consulted
/// when nothing matched: a line or a marked block that is already in the
/// file is edited where it stands and never moved. Both [`Line`] and
/// [`Block`] default to `Append`.
#[derive(Debug, Clone)]
pub enum Insert {
    /// After the last line of the file, which is where Ansible puts a line
    /// given neither `insertafter` nor `insertbefore`.
    Append,
    /// Before the first line of the file. Ansible's `insertbefore: BOF`.
    Prepend,
    /// After the *last* line the regex matches, as Ansible's `insertafter`.
    /// A regex that matches nothing appends.
    After(Regex),
    /// Before the *first* line the regex matches, as Ansible's
    /// `insertbefore`. A regex that matches nothing appends; it does not
    /// prepend.
    Before(Regex),
}

impl Insert {
    /// Index at which to insert into `lines` when the thing is absent.
    /// `After` takes the last match (Ansible's `insertafter`), `Before` the
    /// first (`insertbefore`); no match falls back to the end of the file.
    pub(crate) fn position(&self, lines: &[String]) -> usize {
        match self {
            Insert::Append => lines.len(),
            Insert::Prepend => 0,
            Insert::After(re) => lines
                .iter()
                .rposition(|l| re.is_match(l))
                .map(|i| i + 1)
                .unwrap_or(lines.len()),
            Insert::Before(re) => lines
                .iter()
                .position(|l| re.is_match(l))
                .unwrap_or(lines.len()),
        }
    }
}

/// Numeric owner, as `chown uid:gid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    /// Numeric user id. Names are never resolved here, so a playbook that
    /// wants `www-data` reads the uid off a [`user::Account`](crate::user::Account) first.
    pub uid: u32,
    /// Numeric group id, the half after the colon in `chown uid:gid`. It is
    /// independent of the user: nothing forces it to be that user's primary
    /// group.
    pub gid: u32,
}

impl Owner {
    fn label(&self) -> String {
        format!("{}:{}", self.uid, self.gid)
    }

    fn of(s: &Stat) -> Owner {
        Owner {
            uid: s.uid,
            gid: s.gid,
        }
    }
}

/// One attribute an op was given: the value `check` found on the path
/// (`None` when the path does not exist yet) and the value wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Wanted<T> {
    pub(crate) now: Option<T>,
    pub(crate) want: T,
}

impl<T: PartialEq> Wanted<T> {
    fn differs(&self) -> bool {
        self.now.as_ref() != Some(&self.want)
    }
}

/// The attribute half of a file op's intent, shared by every op that takes
/// `.mode()` and `.owner()`: each attribute the op was given, with what
/// `check` found on the path. Built by [`plan_attrs`]; an op's intent holds
/// it, its `diff` renders the rows that differ with [`AttrPlan::changes`],
/// and its `apply` sets the attributes with `AttrPlan::apply`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AttrPlan {
    /// The permission bits wanted, file type bits already masked off.
    pub(crate) mode: Option<Wanted<u32>>,
    pub(crate) owner: Option<Wanted<Owner>>,
    /// The path is a directory, whose mode is not cleared before the
    /// `chown`: see [`set_mode_and_owner`].
    pub(crate) dir: bool,
}

impl AttrPlan {
    /// True when an attribute the op was given differs from the path, or
    /// the path does not exist yet. False when nothing was asked.
    pub fn differs(&self) -> bool {
        self.mode.is_some_and(|m| m.differs()) || self.owner.is_some_and(|o| o.differs())
    }

    /// The rows the report shows: one per differing attribute, mode first,
    /// with `-` for a path that does not exist yet.
    pub fn changes(&self) -> Vec<AttrChange> {
        let mut changes = vec![];
        if let Some(m) = self.mode.filter(Wanted::differs) {
            let from = m.now.map_or_else(|| "-".into(), |now| format!("{now:04o}"));
            changes.push(AttrChange::new("mode", from, format!("{:04o}", m.want)));
        }
        if let Some(o) = self.owner.filter(Wanted::differs) {
            let from = o.now.map_or_else(|| "-".into(), |now| now.label());
            changes.push(AttrChange::new("owner", from, o.want.label()));
        }
        changes
    }

    /// Set every attribute the op was given, differing or not: `file::Attrs`
    /// and `file::Directory` call this when their step changes. `chmod` is
    /// idempotent; `chown` is not, even to the owner the file already has,
    /// because it clears setuid (and setgid with group execute) on anything
    /// but a directory. A wanted mode is set again after it when it carries
    /// those bits, so they come back; a mode nobody asked for does not. An
    /// op that rewrote the file, whose replacement already has the old owner
    /// and mode, calls [`AttrPlan::apply_differing`] instead. The order of
    /// the calls is [`set_mode_and_owner`]'s.
    pub(crate) fn apply(&self, sys: &System, path: &Path) -> Result<()> {
        set_mode_and_owner(
            sys,
            path,
            self.dir,
            self.mode.map(|m| m.want),
            self.owner.map(|o| o.want),
        )
    }

    /// Set only the attributes that differ: what an op uses when `check`
    /// found the rest already right and must not touch them.
    /// `ssh::authorized_keys` relies on it, and so do `file::Copy` and
    /// `http::Download`: a rewrite keeps the old owner and mode (as root;
    /// see `System::write_atomic`), so what `check` found right is still
    /// right after it. A `chown` that changes the owner needs root and is
    /// issued only when the owner is wrong; one to the owner the file
    /// already has would be a needless call that also clears setuid.
    ///
    /// When it does `chown`, it sets a wanted mode around it even if the
    /// mode already matched, in [`set_mode_and_owner`]'s order, because the
    /// `chown` clears setuid (and setgid with group execute). With no mode
    /// wanted, a `chown` that changes the owner leaves the file without
    /// those bits: that is the kernel's rule, and Ansible's `owner:` does
    /// the same. An op that wants them kept asks for the mode too.
    pub(crate) fn apply_differing(&self, sys: &System, path: &Path) -> Result<()> {
        match self.owner_to_set() {
            Some(o) => set_mode_and_owner(sys, path, self.dir, self.mode.map(|m| m.want), Some(o)),
            None => set_mode_and_owner(
                sys,
                path,
                self.dir,
                self.mode.filter(Wanted::differs).map(|m| m.want),
                None,
            ),
        }
    }

    /// [`AttrPlan::apply_differing`] for an op that has just rewritten the
    /// path, with the owner read again when one is wanted. The rewrite keeps
    /// the old owner only when its `chown` succeeds, and it ignores a
    /// failure (`System::write_atomic`): an unprivileged identity, or a
    /// root without `CAP_CHOWN`, leaves the replacement with its own user or
    /// group. Planning from what `check` saw would then issue no `chown`
    /// and report the step changed with the wrong owner; read again, the
    /// owner differs, the `chown` is issued, and it fails the step as it
    /// failed in the rewrite. This is `apply` checking its own effect, not
    /// planning again: the wanted values are the intent's. Without a wanted
    /// owner there is nothing to verify, and no extra `stat`.
    ///
    /// That `chown` runs as the same identity as the rewrite's own, so it
    /// fails as that one did, and the step fails there, after the wanted
    /// mode (without setuid or setgid) and before anything else.
    ///
    /// With no mode wanted, the rewrite kept whatever mode the file it
    /// replaced had, setuid included. Before a `chown` that changes the
    /// owner, the bits that `chown` would clear are cleared with a `chmod`
    /// of the mode just read (setuid, and setgid with group execute; see
    /// [`cleared_by_chown`]), so a refused `chown` does not leave the new
    /// content setuid under the old owner. A `chown` that succeeds would
    /// have cleared the same bits, so the end state is the same.
    pub(crate) fn apply_after_rewrite(&self, sys: &System, path: &Path) -> Result<()> {
        if self.owner.is_none() {
            return self.apply_differing(sys, path);
        }
        let now = sys.stat(path)?;
        let plan = plan_attrs(
            now.as_ref(),
            self.mode.map(|m| m.want),
            self.owner.map(|o| o.want),
        );
        if self.mode.is_none()
            && plan.owner_to_set().is_some()
            && let Some(now) = now.filter(|s| s.kind != FileKind::Dir)
        {
            let mode = now.mode & 0o7777;
            let clear = cleared_by_chown(mode);
            if clear != 0 {
                sys.set_mode(path, mode & !clear)?;
            }
        }
        plan.apply_differing(sys, path)
    }

    /// The same plan for a path that is a directory, or will be once the op
    /// creates it: what `file::Directory` plans, since a missing path has
    /// no kind to read.
    pub(crate) fn on_a_directory(self) -> Self {
        AttrPlan { dir: true, ..self }
    }

    /// The owner this plan changes the path to, when it changes it.
    pub(crate) fn owner_to_set(&self) -> Option<Owner> {
        self.owner.filter(Wanted::differs).map(|o| o.want)
    }
}

/// Setuid and setgid.
const SETID: u32 = 0o6000;

/// The bits of `mode` a `chown(2)` of anything but a directory clears on
/// Linux: setuid, and setgid when group execute is set. Setgid without
/// group execute marks mandatory locking and survives it.
pub(crate) fn cleared_by_chown(mode: u32) -> u32 {
    let setgid = if mode & 0o010 != 0 { 0o2000 } else { 0 };
    mode & (0o4000 | setgid)
}

/// Set `mode` and `owner` on `path`, in the one order that leaves it no
/// more open than wanted whichever call fails and keeps the bits `chown`
/// clears. With an owner to set, on anything but a directory:
///
/// 1. the mode, with setuid and setgid cleared;
/// 2. the owner;
/// 3. the full mode again, only when it carries setuid or setgid.
///
/// A `chown` that fails (no root, no `CAP_CHOWN`) fails the step with the
/// restrictive mode already in place: a file wanted at `0600` is not left
/// at the `0644` it was created with. The owner before the mode would
/// leave it there. And a rewritten file that kept setuid or setgid from the
/// one it replaced loses them before the `chown`, so a refused `chown` does
/// not leave the new content setuid under the old owner. They come back
/// after it: `chown(2)` clears them, so a mode set only before it would
/// lose them. `keep_owner_and_mode` in the SDK's `Local` backend has the
/// same shape. Without an owner, the mode is one call.
///
/// A directory (`dir`) gets its full mode first, nothing cleared, then its
/// owner, then the full mode again only when it carries setuid or setgid.
/// On Linux `chown(2)` keeps setgid on a directory, so clearing it first
/// would only let a file created in the directory before the last call
/// take the creator's group, and a refused `chown` would leave the bit off.
/// Measured on macOS: `chown` clears setgid on directories too, even to
/// the owner the directory already has, so the last call puts it back
/// there. On Linux that call is idempotent.
///
/// What this cannot close is the time before the first call: a new file
/// exists at `0666 & ~umask`, and a rewrite carries the old mode, from the
/// write until the mode is set. That is closed once the ops move to
/// `write_from` (#85; #86 to #88).
pub(crate) fn set_mode_and_owner(
    sys: &System,
    path: &Path,
    dir: bool,
    mode: Option<u32>,
    owner: Option<Owner>,
) -> Result<()> {
    let Some(o) = owner else {
        if let Some(mode) = mode {
            sys.set_mode(path, mode)?;
        }
        return Ok(());
    };
    if let Some(mode) = mode {
        sys.set_mode(path, if dir { mode } else { mode & !SETID })?;
    }
    sys.set_owner(path, o.uid, o.gid)?;
    if let Some(mode) = mode.filter(|m| m & SETID != 0) {
        sys.set_mode(path, mode)?;
    }
    Ok(())
}

/// Pure planning of the attribute part shared by every op that takes
/// `.mode()` and `.owner()`: each wanted attribute against `current` (`None`
/// when the path does not exist yet). Mode comparison ignores the file type
/// bits (`& 0o7777`).
pub fn plan_attrs(current: Option<&Stat>, mode: Option<u32>, owner: Option<Owner>) -> AttrPlan {
    AttrPlan {
        mode: mode.map(|want| Wanted {
            now: current.map(|s| s.mode & 0o7777),
            want: want & 0o7777,
        }),
        owner: owner.map(|want| Wanted {
            now: current.map(Owner::of),
            want,
        }),
        dir: current.is_some_and(|s| s.kind == FileKind::Dir),
    }
}

/// The line terminator a text uses, so a rewrite keeps CRLF files CRLF.
pub(crate) fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Read a text file for editing. Refuses a symlink: an atomic rewrite would
/// replace the link with a regular file and leave its target stale, which is
/// never what an edit meant. Missing is empty text when `create`, else an
/// error naming the `.create(true)` option — except under `--check`, where
/// it is `None`: an earlier step in the run may create the file (a package
/// shipping its config, a `file::Copy`), and a dry run verifies such a
/// prerequisite only when it is about to act (vision 12). The caller then
/// reports the edit it would make once the file exists.
pub(crate) fn read_text_or_empty(
    sys: &System,
    path: &Path,
    create: bool,
) -> Result<Option<String>> {
    use rustible_sdk::backend::FileKind;
    match sys.stat(path)? {
        Some(s) if s.kind == FileKind::Symlink => bail!(
            "{} is a symlink; edit its target instead (an atomic rewrite would replace the link)",
            path.display()
        ),
        Some(s) if s.kind == FileKind::Dir => bail!("{} is a directory", path.display()),
        Some(_) => sys.read_to_string(path).map(Some),
        None if create => Ok(Some(String::new())),
        None if sys.check_mode() => Ok(None),
        None => bail!(
            "{} does not exist (use .create(true) to create it)",
            path.display()
        ),
    }
}

/// The intent of an op that edits a text file in place, [`Line`] and
/// [`Block`]: the text `check` read and the text it decided to write. `apply`
/// writes exactly that text, so the diff a dry run shows is the edit a real
/// run makes, and a file that changed between `check` and `apply` is
/// overwritten with the planned text rather than merged again (the race is
/// accepted, as in Ansible). Its contents are private: only `check` builds
/// one.
#[derive(Debug)]
pub struct TextEdit(Edit);

#[derive(Debug)]
enum Edit {
    /// Write `after` over the file.
    Rewrite {
        /// The file, for the diff header and the write.
        path: PathBuf,
        /// The text `check` read; empty when `.create(true)` met no file.
        before: String,
        /// The whole text to write.
        after: String,
        /// 1-based line in `after` where the edit sits, for the report; 0
        /// for an edit that leaves nothing behind (an emptied block).
        line_no: usize,
    },
    /// The file does not exist yet and an earlier step may create it. Only
    /// `check` under `--check` produces this, so it is reported and never
    /// applied; a real run's `check` refuses the missing file instead.
    AwaitFile {
        /// The op's name as the report gives it, `file::Line`.
        op: &'static str,
        /// The file the op would edit.
        path: PathBuf,
    },
}

impl Intent for TextEdit {
    fn diff(&self) -> Diff {
        match &self.0 {
            Edit::Rewrite {
                path,
                before,
                after,
                ..
            } => Diff::text(path, before.as_str(), after.as_str()),
            // Worded so the reader knows why no text diff is shown.
            Edit::AwaitFile { op, path } => Diff::summary(format!(
                "{}: does not exist yet; {op} would edit it once an earlier step creates it \
                 (or use .create(true) to create it here)",
                path.display()
            )),
        }
    }
}

impl TextEdit {
    /// Write `after` over `path`, which `check` read as `before`.
    fn rewrite(path: PathBuf, before: String, after: String, line_no: usize) -> Self {
        TextEdit(Edit::Rewrite {
            path,
            before,
            after,
            line_no,
        })
    }

    /// Under `--check` only: `path` is not there yet.
    fn await_file(op: &'static str, path: PathBuf) -> Self {
        TextEdit(Edit::AwaitFile { op, path })
    }

    /// The text to write and where the edit sits, for a test to look at.
    #[cfg(test)]
    fn planned(&self) -> Option<(&str, usize)> {
        match &self.0 {
            Edit::Rewrite { after, line_no, .. } => Some((after.as_str(), *line_no)),
            Edit::AwaitFile { .. } => None,
        }
    }

    /// Execute the edit: back up when asked, then write the planned text.
    /// Returns the line the edit sits on and the backup path.
    fn write(self, sys: &System, backup: bool) -> Result<(usize, Option<PathBuf>)> {
        match self.0 {
            Edit::Rewrite {
                path,
                after,
                line_no,
                ..
            } => {
                let backup_path = write_with_backup(sys, &path, backup, after.as_bytes())?;
                Ok((line_no, backup_path))
            }
            // The same refusal a real run's `check` gives.
            Edit::AwaitFile { path, .. } => bail!(
                "{} does not exist (use .create(true) to create it)",
                path.display()
            ),
        }
    }
}

/// Back up (when asked, and the file exists) then write atomically. Returns
/// the backup path.
pub(crate) fn write_with_backup(
    sys: &System,
    path: &Path,
    backup: bool,
    bytes: &[u8],
) -> Result<Option<std::path::PathBuf>> {
    let backup_path = if backup && sys.exists(path)? {
        Some(sys.backup(path)?)
    } else {
        None
    };
    sys.write_atomic(path, bytes)?;
    Ok(backup_path)
}

#[cfg(test)]
pub(crate) mod testing {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use rustible_sdk::backend::{Backend, CmdSpec, Fake, Output, Stat, WriteAttrs};
    use rustible_sdk::event::Collect;
    use rustible_sdk::prelude::*;

    pub fn fake_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    /// A `System` over `fake` whose `chown` fails with `EPERM`, as it does
    /// for an unprivileged login or a root without `CAP_CHOWN`. The `Fake`
    /// models `chown` as root does, so it never fails there. The refused
    /// call changes nothing and is not recorded in `fake.attr_calls()`;
    /// everything else passes through.
    pub fn chown_refused_sys(fake: &Arc<Fake>) -> System {
        let facts = fake_sys(fake).facts().clone();
        let backend = Arc::new(ChownRefused(fake.clone()));
        System::new(backend, facts, false, Arc::new(Collect::default()))
    }

    struct ChownRefused(Arc<Fake>);

    impl Backend for ChownRefused {
        fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
            self.0.read(p)
        }
        fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
            self.0.write(p, bytes)
        }
        fn write_from(
            &self,
            p: &Path,
            src: &mut dyn io::Read,
            attrs: Option<WriteAttrs>,
        ) -> io::Result<u64> {
            self.0.write_from(p, src, attrs)
        }
        fn open_read(&self, p: &Path) -> io::Result<Box<dyn io::Read + Send + '_>> {
            self.0.open_read(p)
        }
        fn stat(&self, p: &Path) -> io::Result<Option<Stat>> {
            self.0.stat(p)
        }
        fn stat_follow(&self, p: &Path) -> io::Result<Option<Stat>> {
            self.0.stat_follow(p)
        }
        fn mkdir_all(&self, p: &Path) -> io::Result<()> {
            self.0.mkdir_all(p)
        }
        fn remove(&self, p: &Path) -> io::Result<()> {
            self.0.remove(p)
        }
        fn remove_all(&self, p: &Path) -> io::Result<()> {
            self.0.remove_all(p)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.0.rename(from, to)
        }
        fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()> {
            self.0.set_mode(p, mode)
        }
        fn set_owner(&self, p: &Path, _uid: u32, _gid: u32) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("chown {}: Operation not permitted (test)", p.display()),
            ))
        }
        fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.0.copy(from, to)
        }
        fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
            self.0.symlink(target, link)
        }
        fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
            self.0.read_link(p)
        }
        fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
            self.0.read_dir(p)
        }
        fn spawn(&self, spec: &CmdSpec) -> io::Result<Output> {
            self.0.spawn(spec)
        }
    }

    /// Run `check`, insist on a change, return its intent.
    pub fn expect_change<O: Op>(op: &O, sys: &System) -> O::Intent {
        match op.check(sys).unwrap() {
            Plan::Change(c) => c,
            Plan::Satisfied(_) => panic!("expected a change, op was satisfied"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::{AttrCall, Fake, FileKind};

    use super::*;

    fn stat(mode: u32, uid: u32, gid: u32) -> Stat {
        Stat {
            mode,
            uid,
            gid,
            size: 0,
            kind: FileKind::File,
        }
    }

    #[test]
    fn plan_attrs_nothing_wanted_or_all_equal_is_empty() {
        let s = stat(0o644, 1000, 1000);
        assert!(!plan_attrs(Some(&s), None, None).differs());
        assert!(!plan_attrs(None, None, None).differs());
        assert!(
            !plan_attrs(
                Some(&s),
                Some(0o644),
                Some(Owner {
                    uid: 1000,
                    gid: 1000
                })
            )
            .differs()
        );
    }

    #[test]
    fn plan_attrs_reports_mode_and_owner_changes() {
        let s = stat(0o644, 0, 0);
        let plan = plan_attrs(Some(&s), Some(0o600), Some(Owner { uid: 33, gid: 33 }));
        assert!(plan.differs());
        assert_eq!(
            Diff::attrs("/f", plan.changes()).render(),
            "/f:\n  mode: 0644 -> 0600\n  owner: 0:0 -> 33:33\n"
        );
    }

    #[test]
    fn plan_attrs_ignores_file_type_bits() {
        // A stat that (wrongly) carries S_IFREG must still compare equal.
        let s = stat(0o100644, 0, 0);
        assert!(!plan_attrs(Some(&s), Some(0o644), None).differs());
        assert!(!plan_attrs(Some(&s), Some(0o100644), None).differs());
        let plan = plan_attrs(Some(&s), Some(0o100600), None);
        assert_eq!(
            plan.mode,
            Some(Wanted {
                now: Some(0o644),
                want: 0o600
            })
        );
    }

    #[test]
    fn plan_attrs_on_missing_path_renders_dash() {
        let plan = plan_attrs(None, Some(0o750), Some(Owner { uid: 1, gid: 2 }));
        assert_eq!(
            Diff::attrs("/d", plan.changes()).render(),
            "/d:\n  mode: - -> 0750\n  owner: - -> 1:2\n"
        );
    }

    /// Only the differing attributes are rows in the report, but every
    /// attribute the op was given is set when the step applies, in
    /// `set_mode_and_owner`'s order: the mode without setuid, the owner,
    /// then the mode with it, because `chown` clears setuid. The `Fake`
    /// models that clearing (measured on Linux; `[FAKE-CHOWN]` in
    /// `DECISIONS.md`), so an `apply` that set only what differed leaves
    /// 0755 here, and one that set the full mode only before the owner does
    /// too; `tests/it_file_ops.rs` holds the same on a real kernel.
    #[test]
    fn attr_plan_reports_what_differs_and_sets_everything_wanted() {
        let s = stat(0o4755, 0, 0);
        let plan = plan_attrs(Some(&s), Some(0o4755), Some(Owner { uid: 5, gid: 6 }));
        assert_eq!(Diff::attrs("/f", plan.changes()).short(), "owner=5:6");

        let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o4755));
        let sys = testing::fake_sys(&fake);
        plan.apply(&sys, Path::new("/f")).unwrap();
        let f = fake.file("/f").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o4755, 5, 6));
        assert_eq!(fake.attr_calls(), mode_owner_mode("/f", 0o4755, 5, 6));
    }

    /// The three calls `set_mode_and_owner` makes for a mode with setuid or
    /// setgid and an owner: the mode without them, the owner, the mode.
    fn mode_owner_mode(path: &str, mode: u32, uid: u32, gid: u32) -> Vec<AttrCall> {
        vec![
            AttrCall::Chmod {
                path: path.into(),
                mode: mode & 0o1777,
            },
            AttrCall::Chown {
                path: path.into(),
                uid,
                gid,
            },
            AttrCall::Chmod {
                path: path.into(),
                mode,
            },
        ]
    }

    /// With an owner to set, the mode goes first and the owner after it.
    /// Without setuid or setgid in the mode the `chown` clears nothing, so
    /// there is no third call; with either, the full mode comes back after
    /// the `chown`.
    #[test]
    fn set_mode_and_owner_sets_the_mode_first_and_the_setid_bits_last() {
        let owner = Some(Owner { uid: 5, gid: 6 });
        for (mode, calls) in [
            (
                0o600,
                vec![
                    AttrCall::Chmod {
                        path: "/f".into(),
                        mode: 0o600,
                    },
                    AttrCall::Chown {
                        path: "/f".into(),
                        uid: 5,
                        gid: 6,
                    },
                ],
            ),
            (0o4755, mode_owner_mode("/f", 0o4755, 5, 6)),
            (0o2750, mode_owner_mode("/f", 0o2750, 5, 6)),
            (0o6755, mode_owner_mode("/f", 0o6755, 5, 6)),
            (0o1777, {
                // Sticky is not one `chown` clears: two calls.
                let mut c = mode_owner_mode("/f", 0o1777, 5, 6);
                c.pop();
                c
            }),
        ] {
            let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o644));
            let sys = testing::fake_sys(&fake);
            set_mode_and_owner(&sys, Path::new("/f"), false, Some(mode), owner).unwrap();
            assert_eq!(fake.attr_calls(), calls, "{mode:04o}");
            let f = fake.file("/f").unwrap();
            assert_eq!((f.mode, f.uid, f.gid), (mode, 5, 6), "{mode:04o}");
        }
    }

    /// Without an owner the mode is one call, setuid and all: there is no
    /// `chown` to clear it.
    #[test]
    fn set_mode_and_owner_without_an_owner_sets_the_mode_once() {
        let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o644));
        let sys = testing::fake_sys(&fake);
        set_mode_and_owner(&sys, Path::new("/f"), false, Some(0o4755), None).unwrap();
        assert_eq!(fake.chmods(), vec![(PathBuf::from("/f"), 0o4755)]);
        assert!(fake.chowns().is_empty(), "{:?}", fake.attr_calls());
        set_mode_and_owner(&sys, Path::new("/f"), false, None, None).unwrap();
        assert_eq!(fake.attr_calls().len(), 1, "nothing asked, nothing set");
    }

    /// A directory gets its full mode, then its owner, with nothing cleared,
    /// then its full mode again only when it carries setuid or setgid. On
    /// Linux `chown(2)` keeps setgid on a directory, so clearing it first
    /// would only open a moment where a file created in it takes the
    /// creator's group, and a refused `chown` would leave the bit off. On
    /// macOS `chown` clears it on a directory too (measured), which the last
    /// call puts back; the `Fake` models Linux, so here it is idempotent. A
    /// plan reads the kind from the `stat` it was given.
    #[test]
    fn set_mode_and_owner_on_a_directory_sets_the_full_mode_around_the_owner() {
        let owner = Some(Owner { uid: 5, gid: 6 });
        let d = Stat {
            kind: FileKind::Dir,
            ..stat(0o755, 0, 0)
        };
        assert!(!plan_attrs(Some(&stat(0o755, 0, 0)), None, None).dir);
        let chmod = |mode| AttrCall::Chmod {
            path: "/d".into(),
            mode,
        };
        let chown = AttrCall::Chown {
            path: "/d".into(),
            uid: 5,
            gid: 6,
        };
        for (mode, calls) in [
            (0o2775, vec![chmod(0o2775), chown.clone(), chmod(0o2775)]),
            (0o0750, vec![chmod(0o0750), chown.clone()]),
        ] {
            let plan = plan_attrs(Some(&d), Some(mode), owner);
            assert!(plan.dir);
            let fake = Arc::new(Fake::new().with_dir("/d"));
            let planted = fake.attr_calls().len();
            plan.apply(&testing::fake_sys(&fake), Path::new("/d"))
                .unwrap();
            assert_eq!(fake.attr_calls()[planted..], calls, "{mode:04o}");
            let f = fake.file("/d").unwrap();
            assert_eq!((f.mode, f.uid, f.gid), (mode, 5, 6), "{mode:04o}");
        }

        // Refused, the `chown` is not recorded: the mode was set whole
        // before it and stays.
        let fake = Arc::new(Fake::new().with_dir("/d"));
        let planted = fake.attr_calls().len();
        let err = set_mode_and_owner(
            &testing::chown_refused_sys(&fake),
            Path::new("/d"),
            true,
            Some(0o2775),
            owner,
        )
        .unwrap_err()
        .chain();
        assert!(err.contains("Operation not permitted"), "{err}");
        assert_eq!(fake.attr_calls()[planted..], [chmod(0o2775)]);
        assert_eq!(fake.file("/d").unwrap().mode, 0o2775);
    }

    /// What a `chown` of a file clears, as the kernel does and the `Fake`
    /// models it: setuid always, setgid only with group execute.
    #[test]
    fn cleared_by_chown_is_setuid_and_setgid_with_group_execute() {
        assert_eq!(cleared_by_chown(0o4755), 0o4000);
        assert_eq!(cleared_by_chown(0o2755), 0o2000);
        assert_eq!(cleared_by_chown(0o6750), 0o6000);
        assert_eq!(cleared_by_chown(0o2745), 0);
        assert_eq!(cleared_by_chown(0o6745), 0o4000);
        assert_eq!(cleared_by_chown(0o1777), 0);
    }

    /// The point of the order (issue #79): a `chown` the identity may not
    /// make fails the step with the wanted mode already set, so a file
    /// created at 0644 and wanted at 0600 is not left readable by everyone.
    /// The owner first left it at 0644. A setuid mode is left without the
    /// bit, never with it under the wrong owner.
    #[test]
    fn a_refused_chown_leaves_the_wanted_mode_without_setid() {
        for (want, left) in [(0o600, 0o600), (0o4750, 0o750)] {
            let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o644));
            let s = stat(0o644, 0, 0);
            let plan = plan_attrs(Some(&s), Some(want), Some(Owner { uid: 5, gid: 6 }));
            let sys = testing::chown_refused_sys(&fake);
            let err = plan
                .apply_differing(&sys, Path::new("/f"))
                .unwrap_err()
                .chain();
            assert!(err.contains("Operation not permitted"), "{err}");
            let f = fake.file("/f").unwrap();
            assert_eq!((f.mode, f.uid, f.gid), (left, 0, 0), "{want:04o}");
        }
    }

    /// `apply_differing` sets only what `check` found wrong: with the owner
    /// already right, no `chown` is issued at all, only the `chmod`. (One
    /// that changes the owner needs root; one that doesn't would be a
    /// needless call that also clears setuid.)
    #[test]
    fn apply_differing_issues_no_chown_for_an_owner_already_right() {
        let s = stat(0o644, 5, 6);
        let plan = plan_attrs(Some(&s), Some(0o600), Some(Owner { uid: 5, gid: 6 }));
        let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o644));
        let sys = testing::fake_sys(&fake);
        plan.apply_differing(&sys, Path::new("/f")).unwrap();
        assert!(fake.chowns().is_empty(), "{:?}", fake.attr_calls());
        assert_eq!(fake.chmods(), vec![(PathBuf::from("/f"), 0o600)]);
    }

    /// With only the owner wrong, `apply_differing` still sets the wanted
    /// mode around its `chown`, because the `chown` cleared setuid: the
    /// mode without it, the owner, the mode, and the file ends as asked.
    #[test]
    fn apply_differing_sets_the_mode_again_after_a_chown() {
        let s = stat(0o4755, 0, 0);
        let plan = plan_attrs(Some(&s), Some(0o4755), Some(Owner { uid: 5, gid: 6 }));
        let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o4755));
        let sys = testing::fake_sys(&fake);
        plan.apply_differing(&sys, Path::new("/f")).unwrap();
        let f = fake.file("/f").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o4755, 5, 6));
        assert_eq!(fake.attr_calls(), mode_owner_mode("/f", 0o4755, 5, 6));
    }

    #[test]
    fn insert_positions() {
        let lines: Vec<String> = ["a", "Port 1", "b", "Port 2", "c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let re = Regex::new("^Port").unwrap();
        assert_eq!(Insert::Append.position(&lines), 5);
        assert_eq!(Insert::Prepend.position(&lines), 0);
        assert_eq!(Insert::After(re.clone()).position(&lines), 4);
        assert_eq!(Insert::Before(re).position(&lines), 1);
        let none = Regex::new("^zzz").unwrap();
        assert_eq!(Insert::After(none.clone()).position(&lines), 5);
        assert_eq!(Insert::Before(none).position(&lines), 5);
    }
}

#[cfg(test)]
mod reachability {
    //! Every public item this module hands a caller must be *nameable* by
    //! that caller. `Copy::from_str` returns a `CopyBuilder`, so a helper
    //! function returning one, or a struct holding one, needs the type in
    //! scope; until 2026-09-14 `CopyBuilder` was `pub` in `copy.rs` and
    //! absent from the `pub use` here, so it could be produced and never
    //! named. Same for `TEXT_DIFF_LIMIT`, `content_diff` and
    //! `DEFAULT_MARKER`. Nothing here asserts a value: the test is that it
    //! compiles through the public path a user has.

    use super::{CopyBuilder, DEFAULT_MARKER, TEXT_DIFF_LIMIT, content_diff};

    /// The shape that could not be written before: name the builder a
    /// finishing method returns.
    fn builder_for(body: &str) -> CopyBuilder {
        super::Copy::from_str(body)
    }

    #[test]
    fn every_public_item_is_nameable_through_the_module() {
        let _: CopyBuilder = builder_for("x\n");
        let _: usize = TEXT_DIFF_LIMIT;
        let _: &str = DEFAULT_MARKER;
        // `content_diff` is the rendering the reports show; a caller writing
        // their own op wants it for the same reason `file::Copy` does.
        let _ = content_diff(std::path::Path::new("/etc/x"), None, b"new\n");
    }
}
