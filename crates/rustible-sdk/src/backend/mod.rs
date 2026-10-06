//! The only thing that touches reality. One method per primitive.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod elevated;
pub(crate) use elevated::HelperGone;
mod fake;
mod local;

pub(crate) use elevated::Limits;
pub use elevated::{Elevated, Spawner, helper_argv, serve_helper};
pub use fake::{AttrCall, Fake, FakeFile, ReadCall};
pub use local::Local;
pub(crate) use local::Staged;

/// What one path looks like on the target: the part of `stat(2)` the ops
/// care about. Produced by [`Backend::stat`] and [`Backend::stat_follow`],
/// which answer `None` for a path that is not there rather than failing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    /// Permission and setuid/setgid/sticky bits only, masked to `0o7777`, so
    /// it compares straight against the `0o644` a playbook wrote. The file
    /// type is in [`kind`](Self::kind), not here.
    pub mode: u32,
    /// Numeric owner. Nothing in the backend resolves it to a name; an op
    /// that wants one looks it up itself.
    pub uid: u32,
    /// Numeric owning group, likewise unresolved.
    pub gid: u32,
    /// Bytes of content for a regular file. For a directory it is whatever
    /// the OS reports for the directory entry itself, not the size of the
    /// tree under it.
    pub size: u64,
    /// Whether the entry is a file, a directory, a symlink or something
    /// else, decided by which of the two `stat` calls produced this.
    pub kind: FileKind,
}

/// What a path turned out to be. [`Backend::stat`] reports a symlink as
/// [`Symlink`](Self::Symlink); [`Backend::stat_follow`] resolves it first
/// and reports what it landed on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    /// A regular file, or, from [`Backend::stat_follow`], a link that ends
    /// at one.
    File,
    /// A directory. [`Backend::remove`] refuses a populated one;
    /// [`Backend::remove_all`] is the only primitive that takes a tree.
    Dir,
    /// A symbolic link, and only ever from [`Backend::stat`].
    /// [`Backend::stat_follow`] never reports it: it follows the link, and a
    /// link with nothing at the end of it makes that call answer `None`.
    Symlink,
    /// A socket, a fifo, a device node, anything else. No op creates one;
    /// the variant exists so that a `stat` of an unexpected path is
    /// representable and an op can refuse it by name.
    Other,
}

/// One command, resolved down to what `execve` needs. Ops build these with
/// [`System::cmd`](crate::system::System::cmd) rather than by hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CmdSpec {
    /// The executable, looked up on `PATH` when it has no `/`. No shell is
    /// involved anywhere, so this is a program name and never a command line.
    pub program: String,
    /// One element per argument, passed through untouched: no word
    /// splitting, no globbing, no `$VAR`, no `|` or `>`. An op that really
    /// wants a shell runs `sh -c` and says so.
    pub args: Vec<String>,
    /// Environment entries added on top of the parent's. The backend forces
    /// `LANG=C` and `LC_ALL=C` first, so a locale-dependent message cannot
    /// break a parser; naming either of them here overrides that.
    pub env: BTreeMap<String, String>,
    /// Working directory for the child. `None` inherits the calling
    /// process's, which for an escalated command is the helper's.
    pub cwd: Option<PathBuf>,
    /// Bytes to feed the child on stdin. `None` gives it `/dev/null`, not
    /// the terminal, so a command that decides to prompt gets EOF instead of
    /// hanging the run.
    #[serde(default, with = "crate::protocol::b64_opt")]
    pub stdin: Option<Vec<u8>>,
    /// Non-empty means "run via this prefix", e.g. ["sudo", "-n", "-u", "root"].
    pub prefix: Vec<String>,
}

impl CmdSpec {
    /// The argument vector actually executed: [`prefix`](Self::prefix), then
    /// [`program`](Self::program), then [`args`](Self::args). Error messages
    /// and [`Fake::argvs`] show this, so it is what a test asserts on.
    pub fn argv(&self) -> Vec<String> {
        let mut v = self.prefix.clone();
        v.push(self.program.clone());
        v.extend(self.args.iter().cloned());
        v
    }
}

/// Everything a finished command left behind. [`Backend::spawn`] waits for
/// the child, so there is no partial `Output`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// The exit code, or `-1` when the child was killed by a signal and had
    /// none — in which case [`Output::signal`] names it, so `-1` here is
    /// never ambiguous with a command that genuinely exited `-1`.
    pub status: i32,
    /// The signal that killed the child, when one did. `None` for a process
    /// that exited on its own, whatever its code. Defaulted on deserialize,
    /// so a frame from an older peer that does not carry it still parses.
    #[serde(default)]
    pub signal: Option<i32>,
    /// Everything the command wrote to stdout, captured whole rather than
    /// streamed.
    #[serde(with = "crate::protocol::b64")]
    pub stdout: Vec<u8>,
    /// Everything it wrote to stderr. Captured separately, so a failure
    /// message can be reported without the data on stdout.
    #[serde(with = "crate::protocol::b64")]
    pub stderr: Vec<u8>,
}

impl Output {
    /// [`stdout`](Self::stdout) as text, with invalid UTF-8 replaced instead
    /// of rejected: one stray byte in a command's output should not fail a
    /// step that only wanted the first line.
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    /// [`stderr`](Self::stderr) as text, lossy in the same way.
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
    /// True when the command exited 0. A child killed by a signal has
    /// [`status`](Self::status) `-1` and so is not a success.
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

/// The mode and owner a streamed write gives its file, applied to the staged
/// temporary file **before** it is renamed over the target, so the new
/// content is never readable at a wider mode than asked and never setuid
/// with the wrong owner, even for a moment.
///
/// A field left `None` keeps what a write without attributes would give: a
/// rewrite keeps the existing file's mode or owner, a new file gets the mode
/// any newly created file gets (0666 minus the umask) and the writer's
/// owner. A requested owner that cannot be given (`EPERM`, unprivileged)
/// fails the write and leaves the target as it was; an owner kept from the
/// existing file is best effort, as for [`Backend::write`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteAttrs {
    /// Permission bits as `chmod` takes them, setuid, setgid and sticky
    /// included: `0o600`, `0o4755`.
    pub mode: Option<u32>,
    /// Numeric `(uid, gid)`, both of them, as [`Backend::set_owner`] takes
    /// them. Handing a file to another user needs root.
    pub owner: Option<(u32, u32)>,
}

/// An `io::Error` worded here that keeps the errno of the failure it
/// describes. `io::Error::new` drops the raw OS code, and the escalation
/// helper sends the code across (`HelperResponse::Err`'s `code`) so the far
/// side rebuilds the same kind; this keeps it through a rewording.
#[derive(Debug)]
pub(crate) struct Coded {
    /// The errno.
    pub(crate) errno: i32,
    /// The whole message, which is what `Display` prints.
    pub(crate) message: String,
}

impl std::fmt::Display for Coded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Coded {}

/// An error with `errno`'s kind and code and this message.
pub(crate) fn coded(errno: i32, message: String) -> io::Error {
    io::Error::new(
        io::Error::from_raw_os_error(errno).kind(),
        Coded { errno, message },
    )
}

/// `e` reworded as `message`, keeping its errno when it has one and its
/// kind either way.
pub(crate) fn reworded(e: &io::Error, message: String) -> io::Error {
    match errno_of(e) {
        Some(errno) => coded(errno, message),
        None => io::Error::new(e.kind(), message),
    }
}

/// The errno `e` carries: the OS's own, or one kept by [`coded`].
pub(crate) fn errno_of(e: &io::Error) -> Option<i32> {
    e.raw_os_error().or_else(|| {
        e.get_ref()
            .and_then(|inner| inner.downcast_ref::<Coded>())
            .map(|c| c.errno)
    })
}

/// One attribute call, in the order [`attr_steps`] gives them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttrStep {
    /// `chmod` to these bits.
    Mode(u32),
    /// `chown` to these ids.
    Owner(u32, u32),
}

/// The calls that give a regular file `mode` and, when there is one,
/// `owner`, in the one order that is safe (`[ISSUE-51]`, `[ISSUE-79]`):
///
/// 1. the mode **without** setuid and setgid, while the writer still owns
///    the file, so a root without `CAP_FOWNER` can still do it, and the
///    content is never setuid to the writer when it is about to belong to
///    someone else;
/// 2. the owner;
/// 3. the full mode again, only when it carries setuid or setgid: every
///    successful `chown` of a non-directory clears setuid, and setgid with
///    group execute, on Linux (`[FAKE-CHOWN]`).
///
/// Without an owner it is the one `chmod`. Directories have their own order
/// (`file::set_mode_and_owner` in `rustible-std`); nothing that calls this
/// writes one.
pub(crate) fn attr_steps(mode: u32, owner: Option<(u32, u32)>) -> Vec<AttrStep> {
    let Some((uid, gid)) = owner else {
        return vec![AttrStep::Mode(mode)];
    };
    let mut steps = vec![AttrStep::Mode(mode & !0o6000), AttrStep::Owner(uid, gid)];
    if mode & 0o6000 != 0 {
        steps.push(AttrStep::Mode(mode));
    }
    steps
}

/// One method per primitive, nothing clever. `Local` is production, `Fake`
/// is for tests, `Elevated` proxies every call to a `Local` inside a helper
/// process running as another user (vision doc 7.2, 11.3).
pub trait Backend: Send + Sync {
    /// The whole file, in memory. Through [`Elevated`] it crosses the
    /// helper boundary in chunks, so any size works.
    fn read(&self, p: &Path) -> io::Result<Vec<u8>>;
    /// Must be atomic (temp file + rename) and preserve mode/owner of an existing file.
    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()>;
    /// Like [`write`](Backend::write), from a reader: what `src` yields is
    /// staged beside `p` and renamed over it only once `src` reaches its
    /// end without an error, then the number of bytes is returned. An error
    /// from `src` (including one that a reader checking a digest returns at
    /// its end) leaves `p` as it was and nothing beside it, and is returned
    /// as `src` gave it, not reworded as a failure of `p`.
    ///
    /// `attrs` are applied to the staged file before the rename
    /// ([`WriteAttrs`]), so the content is never visible at a wider mode,
    /// or setuid with the wrong owner. `None` is [`write`](Backend::write)'s
    /// behaviour.
    ///
    /// The content streams: `Local` writes as it reads, and [`Elevated`]
    /// sends it to its helper a chunk at a time, taking its connection only
    /// per chunk, so `src` may itself read through the same helper. No
    /// default implementation, so a backend cannot quietly buffer the whole
    /// file.
    fn write_from(
        &self,
        p: &Path,
        src: &mut dyn Read,
        attrs: Option<WriteAttrs>,
    ) -> io::Result<u64>;
    /// A reader over the file at `p`, symlinks followed, as
    /// [`read`](Backend::read) without holding the whole file. Whatever `p`
    /// is gets opened and read, as `read` and `cat` do: a FIFO blocks for a
    /// writer, and `/dev/zero` never ends. Through [`Elevated`] each chunk is
    /// one request and no lock is held between them, so other primitives on
    /// the same identity may run while the reader is alive; dropping it
    /// before its end releases what the helper held for it.
    fn open_read(&self, p: &Path) -> io::Result<Box<dyn Read + Send + '_>>;
    /// `lstat`: a symlink reports `FileKind::Symlink`.
    fn stat(&self, p: &Path) -> io::Result<Option<Stat>>;
    /// `stat`: follows symlinks, so a link to a directory reports `Dir`.
    fn stat_follow(&self, p: &Path) -> io::Result<Option<Stat>>;
    /// Create `p` and every missing parent. An existing directory is Ok; an
    /// existing file in the way is an error. The mode is the umask's
    /// business, so an op that needs a particular one calls
    /// [`set_mode`](Backend::set_mode) after.
    fn mkdir_all(&self, p: &Path) -> io::Result<()>;
    /// Remove one entry: a file, a symlink, or an EMPTY directory. A
    /// populated directory is an error, so a typo cannot take a tree with it.
    /// Missing is Ok.
    fn remove(&self, p: &Path) -> io::Result<()>;
    /// Remove a directory tree (or a single file). The only recursive delete.
    fn remove_all(&self, p: &Path) -> io::Result<()>;
    /// Atomically move `from` to `to`, replacing `to` if it exists.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Set the permission bits of `p`, the octal `chmod` takes. Follows
    /// symlinks: aimed at a link, it changes the target.
    fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()>;
    /// `chown`: numeric ids only, both of them, no name lookup. Handing a
    /// file to another user needs root, which is the usual reason an op asks
    /// for `as_root`.
    fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> io::Result<()>;
    /// Copy the contents of the regular file `from` (symlinks followed) to a
    /// **new** file `to`, which must not exist in any form: an existing
    /// path, a symlink included (dangling or not), fails with
    /// [`AlreadyExists`](io::ErrorKind::AlreadyExists) and is left as it
    /// was, so nothing is ever written through a link planted at `to`. The
    /// copy gets `from`'s permission bits **without** setuid, setgid and
    /// sticky (`0o7000`), and `from`'s owner and group when the runner
    /// may give them (root); otherwise, `EPERM` ignored, the runner's. A
    /// source that is not a regular file (a directory, a FIFO) is refused
    /// before it is opened, with
    /// [`InvalidInput`](io::ErrorKind::InvalidInput). Unlike [`write`](Backend::write) this is not atomic, but a
    /// failure part-way removes the `to` it created, so it never leaves a
    /// half-written copy behind. [`System::backup`](crate::System::backup)
    /// is its one caller, and these rules are its safety (issue #75).
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Create the symbolic link `link` pointing at `target`. Fails if `link` exists.
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;
    /// Where the symbolic link at `p` points. Fails if `p` is not a symlink.
    fn read_link(&self, p: &Path) -> io::Result<PathBuf>;
    /// Full paths of the direct children of the directory `p`.
    fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>>;
    /// Run one command to completion and collect what it printed. Returns
    /// `Ok` for a command that ran and failed: a non-zero exit is in
    /// [`Output::status`], and only being unable to start the child at all
    /// is an `Err`.
    fn spawn(&self, spec: &CmdSpec) -> io::Result<Output>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Without an owner, one `chmod`. With one, the mode without setuid and
    /// setgid, the owner, and the full mode again only when it carries
    /// either; sticky is not cleared by a `chown`, so it is no reason for a
    /// third call.
    #[test]
    fn attr_steps_put_setuid_and_setgid_after_the_owner() {
        use AttrStep::{Mode, Owner};
        assert_eq!(attr_steps(0o4755, None), [Mode(0o4755)]);
        for (mode, steps) in [
            (0o600, vec![Mode(0o600), Owner(5, 6)]),
            (0o1755, vec![Mode(0o1755), Owner(5, 6)]),
            (0o4755, vec![Mode(0o755), Owner(5, 6), Mode(0o4755)]),
            (0o2750, vec![Mode(0o750), Owner(5, 6), Mode(0o2750)]),
            (0o6755, vec![Mode(0o755), Owner(5, 6), Mode(0o6755)]),
        ] {
            assert_eq!(attr_steps(mode, Some((5, 6))), steps, "{mode:o}");
        }
    }
}
