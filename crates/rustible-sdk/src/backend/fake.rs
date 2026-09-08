use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{Backend, CmdSpec, FileKind, Output, Stat};

#[derive(Debug, Clone)]
pub struct FakeFile {
    pub bytes: Vec<u8>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub kind: FileKind,
}

/// A canned response for a command. Matched by program name and, if given,
/// by the exact argument list.
#[derive(Debug, Clone)]
pub struct Canned {
    pub program: String,
    pub args: Option<Vec<String>>,
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// In-memory backend for unit tests. Plant files, can commands, then assert
/// on what the op read, wrote, and ran.
#[derive(Default)]
pub struct Fake {
    files: Mutex<BTreeMap<PathBuf, FakeFile>>,
    canned: Mutex<Vec<Canned>>,
    ran: Mutex<Vec<CmdSpec>>,
}

impl Fake {
    /// An empty machine: no files, no directories, no canned commands.
    ///
    /// Nothing exists until a `with_*` builder plants it, and a command
    /// nobody canned fails with `no canned response`. That is the point: a
    /// test learns when the op under test reaches for something the test did
    /// not think about, instead of getting a plausible default.
    pub fn new() -> Self {
        Self::default()
    }

    /// Plant a regular file with these contents, mode `0o644`, owned by
    /// uid 0 / gid 0.
    ///
    /// Parent directories are not created. The fake keeps a flat map of
    /// paths, so an op that stats or lists a parent needs it planted with
    /// [`with_dir`](Self::with_dir) as well.
    pub fn with_file(self, p: impl Into<PathBuf>, content: impl AsRef<[u8]>) -> Self {
        self.with_file_mode(p, content, 0o644)
    }

    /// [`with_file`](Self::with_file) with the mode spelled out, for an op
    /// that asserts on permissions: `0o600` on a key, `0o440` on a sudoers
    /// drop-in.
    pub fn with_file_mode(
        self,
        p: impl Into<PathBuf>,
        content: impl AsRef<[u8]>,
        mode: u32,
    ) -> Self {
        self.files.lock().unwrap().insert(
            p.into(),
            FakeFile {
                bytes: content.as_ref().to_vec(),
                mode,
                uid: 0,
                gid: 0,
                kind: FileKind::File,
            },
        );
        self
    }

    /// Plant a directory, mode `0o755`, owned by uid 0 / gid 0.
    ///
    /// Only the one directory: parents are not implied, and neither are
    /// children. [`Backend::read_dir`] and [`Backend::stat`] look the path
    /// up directly, so a directory nobody planted is absent even when files
    /// beneath it are there.
    pub fn with_dir(self, p: impl Into<PathBuf>) -> Self {
        self.files.lock().unwrap().insert(
            p.into(),
            FakeFile {
                bytes: vec![],
                mode: 0o755,
                uid: 0,
                gid: 0,
                kind: FileKind::Dir,
            },
        );
        self
    }

    /// Plant a symbolic link at `p` pointing at `target`.
    pub fn with_symlink(self, p: impl Into<PathBuf>, target: impl Into<PathBuf>) -> Self {
        Backend::symlink(&self, target.into().as_path(), p.into().as_path())
            .expect("planting a symlink in the fake");
        self
    }

    /// Follow symlinks from `p` the way the kernel would (relative targets
    /// resolve against the link's parent), with a hop limit. Returns the
    /// final path, which may not exist (a dangling link).
    fn resolve(files: &BTreeMap<PathBuf, FakeFile>, p: &Path) -> PathBuf {
        let mut cur = p.to_path_buf();
        for _ in 0..16 {
            match files.get(&cur) {
                Some(f) if f.kind == FileKind::Symlink => {
                    let target = PathBuf::from(String::from_utf8_lossy(&f.bytes).into_owned());
                    cur = if target.is_absolute() {
                        target
                    } else {
                        cur.parent().map(|d| d.join(&target)).unwrap_or(target)
                    };
                }
                _ => return cur,
            }
        }
        cur
    }

    /// Can a command: any invocation of `program` (optionally with exactly
    /// these args) returns this output.
    pub fn with_cmd(self, program: &str, args: Option<&[&str]>, status: i32, stdout: &str) -> Self {
        self.canned.lock().unwrap().push(Canned {
            program: program.to_string(),
            args: args.map(|a| a.iter().map(|s| s.to_string()).collect()),
            status,
            stdout: stdout.to_string(),
            stderr: String::new(),
        });
        self
    }

    /// The planted entry at `p`, or `None` when nothing is there.
    ///
    /// The path is taken literally: symlinks are not followed, so a link
    /// comes back as itself with its target held in its bytes. This is the
    /// assertion side of the fake, and it never runs an op's logic.
    pub fn file(&self, p: impl AsRef<Path>) -> Option<FakeFile> {
        self.files.lock().unwrap().get(p.as_ref()).cloned()
    }

    /// What is stored at `p` as text, lossily, for asserting on what an op
    /// wrote. `None` when the path is absent; a directory reads as the empty
    /// string, because a planted directory holds no bytes.
    pub fn content(&self, p: impl AsRef<Path>) -> Option<String> {
        self.file(p)
            .map(|f| String::from_utf8_lossy(&f.bytes).into_owned())
    }

    /// Every command spawned, in order.
    pub fn commands(&self) -> Vec<CmdSpec> {
        self.ran.lock().unwrap().clone()
    }

    /// Argv strings of every command spawned, for easy assertions.
    pub fn argvs(&self) -> Vec<Vec<String>> {
        self.commands().iter().map(|c| c.argv()).collect()
    }
}

fn not_found(p: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: no such file (fake)", p.display()),
    )
}

impl Backend for Fake {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        // Reads follow symlinks, as `std::fs::read` does.
        let files = self.files.lock().unwrap();
        let real = Self::resolve(&files, p);
        match files.get(&real) {
            Some(f) if f.kind == FileKind::Dir => Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{}: is a directory (fake)", p.display()),
            )),
            Some(f) => Ok(f.bytes.clone()),
            None => Err(not_found(p)),
        }
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        // Mirrors `Local::write` (tempfile + rename): writing at a symlink's
        // path replaces the link itself with a regular file; the target is
        // untouched. New files get 0644, an existing file keeps its attrs.
        let mut files = self.files.lock().unwrap();
        match files.get_mut(p) {
            Some(f) if f.kind == FileKind::File => f.bytes = bytes.to_vec(),
            Some(f) if f.kind == FileKind::Dir => {
                return Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    format!("{}: is a directory (fake)", p.display()),
                ));
            }
            _ => {
                files.insert(
                    p.to_path_buf(),
                    FakeFile {
                        bytes: bytes.to_vec(),
                        mode: 0o644,
                        uid: 0,
                        gid: 0,
                        kind: FileKind::File,
                    },
                );
            }
        }
        Ok(())
    }

    fn stat(&self, p: &Path) -> io::Result<Option<Stat>> {
        Ok(self.files.lock().unwrap().get(p).map(|f| Stat {
            mode: f.mode,
            uid: f.uid,
            gid: f.gid,
            size: f.bytes.len() as u64,
            kind: f.kind,
        }))
    }

    fn stat_follow(&self, p: &Path) -> io::Result<Option<Stat>> {
        let files = self.files.lock().unwrap();
        let real = Self::resolve(&files, p);
        Ok(files.get(&real).map(|f| Stat {
            mode: f.mode,
            uid: f.uid,
            gid: f.gid,
            size: f.bytes.len() as u64,
            kind: f.kind,
        }))
    }

    fn mkdir_all(&self, p: &Path) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        let mut cur = PathBuf::new();
        for comp in p.components() {
            cur.push(comp);
            files.entry(cur.clone()).or_insert(FakeFile {
                bytes: vec![],
                mode: 0o755,
                uid: 0,
                gid: 0,
                kind: FileKind::Dir,
            });
        }
        Ok(())
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        let Some(f) = files.get(p) else {
            return Ok(());
        };
        if f.kind == FileKind::Dir && files.keys().any(|k| k.parent() == Some(p)) {
            return Err(io::Error::new(
                io::ErrorKind::DirectoryNotEmpty,
                format!("{}: directory not empty (fake)", p.display()),
            ));
        }
        files.remove(p);
        Ok(())
    }

    fn remove_all(&self, p: &Path) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        files.retain(|k, _| !k.starts_with(p));
        Ok(())
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        if !files.contains_key(from) {
            return Err(not_found(from));
        }
        let moved: Vec<(PathBuf, FakeFile)> = files
            .iter()
            .filter(|(k, _)| k.starts_with(from))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        files.retain(|k, _| !k.starts_with(from) && !k.starts_with(to));
        for (k, v) in moved {
            let rel = k.strip_prefix(from).expect("under from");
            files.insert(to.join(rel), v);
        }
        Ok(())
    }

    fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        files
            .get_mut(p)
            .map(|f| f.mode = mode)
            .ok_or_else(|| not_found(p))
    }

    fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        files
            .get_mut(p)
            .map(|f| {
                f.uid = uid;
                f.gid = gid;
            })
            .ok_or_else(|| not_found(p))
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        let bytes = self.read(from)?;
        Backend::write(self, to, &bytes)
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        if files.contains_key(link) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: already exists (fake)", link.display()),
            ));
        }
        files.insert(
            link.to_path_buf(),
            FakeFile {
                bytes: target.as_os_str().as_encoded_bytes().to_vec(),
                mode: 0o777,
                uid: 0,
                gid: 0,
                kind: FileKind::Symlink,
            },
        );
        Ok(())
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        let files = self.files.lock().unwrap();
        match files.get(p) {
            Some(f) if f.kind == FileKind::Symlink => Ok(PathBuf::from(
                String::from_utf8_lossy(&f.bytes).into_owned(),
            )),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: not a symlink (fake)", p.display()),
            )),
            None => Err(not_found(p)),
        }
    }

    fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
        let files = self.files.lock().unwrap();
        let real = Self::resolve(&files, p);
        match files.get(&real) {
            Some(f) if f.kind == FileKind::Dir => Ok(files
                .keys()
                .filter(|k| k.parent() == Some(real.as_path()))
                .cloned()
                .collect()),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{}: not a directory (fake)", p.display()),
            )),
            None => Err(not_found(p)),
        }
    }

    fn spawn(&self, spec: &CmdSpec) -> io::Result<Output> {
        self.ran.lock().unwrap().push(spec.clone());
        let canned = self.canned.lock().unwrap();
        let hit = canned
            .iter()
            .find(|c| c.program == spec.program && c.args.as_ref().is_none_or(|a| *a == spec.args));
        match hit {
            Some(c) => Ok(Output {
                status: c.status,
                stdout: c.stdout.clone().into_bytes(),
                stderr: c.stderr.clone().into_bytes(),
            }),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("no canned response for `{}` (fake)", spec.argv().join(" ")),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symlink_read_link_and_stat_kind() {
        let fake = Fake::new().with_dir("/etc");
        fake.symlink(Path::new("/etc/real"), Path::new("/etc/link"))
            .unwrap();
        assert_eq!(
            fake.read_link(Path::new("/etc/link")).unwrap(),
            PathBuf::from("/etc/real")
        );
        assert_eq!(
            fake.stat(Path::new("/etc/link")).unwrap().unwrap().kind,
            FileKind::Symlink
        );
        // Creating over an existing path fails, like symlink(2).
        let err = fake
            .symlink(Path::new("/other"), Path::new("/etc/link"))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn symlinks_are_followed_by_read_and_stat_follow_but_not_stat() {
        let f = Fake::new()
            .with_dir("/real")
            .with_file("/real/f", "hi")
            .with_symlink("/link", "/real")
            .with_symlink("/real/rel", "f");
        assert_eq!(
            Backend::stat(&f, Path::new("/link")).unwrap().unwrap().kind,
            FileKind::Symlink
        );
        assert_eq!(
            f.stat_follow(Path::new("/link")).unwrap().unwrap().kind,
            FileKind::Dir
        );
        assert_eq!(
            Backend::read(&f, Path::new("/link/f")).unwrap_err().kind(),
            io::ErrorKind::NotFound,
            "no path components through links, like the real fs would need a resolver per component; direct links only"
        );
        assert_eq!(Backend::read(&f, Path::new("/real/rel")).unwrap(), b"hi");
        assert_eq!(f.read_dir(Path::new("/link")).unwrap().len(), 2);
        // Writing at a link's path replaces the link with a regular file.
        Backend::write(&f, Path::new("/real/rel"), b"new").unwrap();
        assert_eq!(
            Backend::stat(&f, Path::new("/real/rel"))
                .unwrap()
                .unwrap()
                .kind,
            FileKind::File
        );
        assert_eq!(
            Backend::read(&f, Path::new("/real/f")).unwrap(),
            b"hi",
            "target untouched"
        );
    }

    #[test]
    fn remove_is_not_recursive_but_remove_all_is() {
        let f = Fake::new().with_dir("/d").with_file("/d/x", "1");
        assert_eq!(
            Backend::remove(&f, Path::new("/d")).unwrap_err().kind(),
            io::ErrorKind::DirectoryNotEmpty
        );
        Backend::remove(&f, Path::new("/d/x")).unwrap();
        Backend::remove(&f, Path::new("/d")).unwrap();
        let f = Fake::new().with_dir("/d").with_file("/d/x", "1");
        f.remove_all(Path::new("/d")).unwrap();
        assert!(f.file("/d").is_none() && f.file("/d/x").is_none());
    }

    #[test]
    fn rename_moves_entries_and_replaces_the_target() {
        let f = Fake::new().with_file("/a", "1").with_file("/b", "2");
        f.rename(Path::new("/a"), Path::new("/b")).unwrap();
        assert!(f.file("/a").is_none());
        assert_eq!(f.content("/b").unwrap(), "1");
    }

    #[test]
    fn read_link_on_non_symlink_fails() {
        let fake = Fake::new().with_file("/f", "x");
        assert!(fake.read_link(Path::new("/f")).is_err());
        assert_eq!(
            fake.read_link(Path::new("/missing")).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn read_dir_lists_direct_children_only() {
        let fake = Fake::new()
            .with_dir("/d")
            .with_file("/d/a", "")
            .with_dir("/d/sub")
            .with_file("/d/sub/deep", "")
            .with_file("/other", "");
        let kids = fake.read_dir(Path::new("/d")).unwrap();
        assert_eq!(kids, vec![PathBuf::from("/d/a"), PathBuf::from("/d/sub")]);
        assert!(fake.read_dir(Path::new("/d/sub")).unwrap().len() == 1);
        assert!(fake.read_dir(Path::new("/d/a")).is_err());
        assert!(fake.read_dir(Path::new("/nope")).is_err());
    }
}
