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
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_file(self, p: impl Into<PathBuf>, content: impl AsRef<[u8]>) -> Self {
        self.with_file_mode(p, content, 0o644)
    }

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
        self.files.lock().unwrap().insert(
            p.into(),
            FakeFile {
                bytes: target.into().as_os_str().as_encoded_bytes().to_vec(),
                mode: 0o777,
                uid: 0,
                gid: 0,
                kind: FileKind::Symlink,
            },
        );
        self
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

    pub fn file(&self, p: impl AsRef<Path>) -> Option<FakeFile> {
        self.files.lock().unwrap().get(p.as_ref()).cloned()
    }

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
        self.files
            .lock()
            .unwrap()
            .get(p)
            .map(|f| f.bytes.clone())
            .ok_or_else(|| not_found(p))
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut files = self.files.lock().unwrap();
        let entry = files.entry(p.to_path_buf()).or_insert(FakeFile {
            bytes: vec![],
            mode: 0o644,
            uid: 0,
            gid: 0,
            kind: FileKind::File,
        });
        entry.bytes = bytes.to_vec();
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
        // The fake has no symlinks, so following changes nothing.
        self.stat(p)
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
        files.retain(|k, _| !k.starts_with(p));
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
        self.write(to, &bytes)
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
        match files.get(p) {
            Some(f) if f.kind == FileKind::Dir => Ok(files
                .keys()
                .filter(|k| k.parent() == Some(p))
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
