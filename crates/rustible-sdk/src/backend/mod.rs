//! The only thing that touches reality. One method per primitive.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod elevated;
mod fake;
mod local;

pub use elevated::{
    Elevated, HelperOp, HelperRequest, HelperResponse, Spawner, helper_argv, serve_helper,
};
pub use fake::Fake;
pub use local::Local;

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
    /// none.
    pub status: i32,
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

/// One method per primitive, nothing clever. `Local` is production, `Fake`
/// is for tests, `Elevated` proxies every call to a `Local` inside a helper
/// process running as another user (vision doc 7.2, 11.3).
pub trait Backend: Send + Sync {
    /// The whole file, in memory. There is no streaming read: through
    /// [`Elevated`] the contents cross the helper boundary in a single
    /// frame, so a file past that size is refused rather than truncated.
    fn read(&self, p: &Path) -> io::Result<Vec<u8>>;
    /// Must be atomic (temp file + rename) and preserve mode/owner of an existing file.
    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()>;
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
    /// Copy the contents of `from` onto `to`, creating or truncating it.
    /// Unlike [`write`](Backend::write) this is not atomic, so a reader can
    /// see a half-written `to`.
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
