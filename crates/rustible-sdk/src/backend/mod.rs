//! The only thing that touches reality. One method per primitive.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

mod fake;
mod local;

pub use fake::Fake;
pub use local::Local;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stat {
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub kind: FileKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CmdSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<Vec<u8>>,
    /// Non-empty means "run via this prefix", e.g. ["sudo", "-n", "-u", "root"].
    pub prefix: Vec<String>,
}

impl CmdSpec {
    pub fn argv(&self) -> Vec<String> {
        let mut v = self.prefix.clone();
        v.push(self.program.clone());
        v.extend(self.args.iter().cloned());
        v
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

impl Output {
    pub fn stdout_str(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_str(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
    pub fn success(&self) -> bool {
        self.status == 0
    }
}

pub trait Backend: Send + Sync {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>>;
    /// Must be atomic (temp file + rename) and preserve mode/owner of an existing file.
    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()>;
    /// `lstat`: a symlink reports `FileKind::Symlink`.
    fn stat(&self, p: &Path) -> io::Result<Option<Stat>>;
    /// `stat`: follows symlinks, so a link to a directory reports `Dir`.
    fn stat_follow(&self, p: &Path) -> io::Result<Option<Stat>>;
    fn mkdir_all(&self, p: &Path) -> io::Result<()>;
    /// Remove one entry: a file, a symlink, or an EMPTY directory. A
    /// populated directory is an error, so a typo cannot take a tree with it.
    /// Missing is Ok.
    fn remove(&self, p: &Path) -> io::Result<()>;
    /// Remove a directory tree (or a single file). The only recursive delete.
    fn remove_all(&self, p: &Path) -> io::Result<()>;
    /// Atomically move `from` to `to`, replacing `to` if it exists.
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()>;
    fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()>;
    fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> io::Result<()>;
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()>;
    /// Create the symbolic link `link` pointing at `target`. Fails if `link` exists.
    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()>;
    /// Where the symbolic link at `p` points. Fails if `p` is not a symlink.
    fn read_link(&self, p: &Path) -> io::Result<PathBuf>;
    /// Full paths of the direct children of the directory `p`.
    fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>>;
    fn spawn(&self, spec: &CmdSpec) -> io::Result<Output>;
}
