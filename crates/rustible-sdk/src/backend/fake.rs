use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::{Backend, CmdSpec, FileKind, Output, Stat};

/// One path inside a [`Fake`], as [`Fake::file`] hands it back: the whole
/// of what the fake knows about it. A test reaches for this when
/// [`Fake::content`] is not enough — asserting the mode a `chmod` produced,
/// or that an op wrote a symlink where a file was expected.
#[derive(Debug, Clone)]
pub struct FakeFile {
    /// The file's contents. For a [`FileKind::Symlink`] this is the link
    /// target as bytes, which is how the fake stores one.
    pub bytes: Vec<u8>,
    /// Permission bits, low twelve only, as the ops compare them.
    pub mode: u32,
    /// Owning user id.
    pub uid: u32,
    /// Owning group id.
    pub gid: u32,
    /// Whether this path is a regular file, a directory or a symlink.
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

/// One `chmod` or `chown` an op asked the backend for, as [`Fake::attr_calls`]
/// records it: the path as the op passed it (a symlink is recorded as the
/// link, though the call changes its target), whether or not the call found
/// anything to change.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AttrCall {
    /// [`Backend::set_mode`] with these permission bits.
    Chmod {
        /// The path passed.
        path: PathBuf,
        /// The mode passed.
        mode: u32,
    },
    /// [`Backend::set_owner`] with these ids.
    Chown {
        /// The path passed.
        path: PathBuf,
        /// The uid passed.
        uid: u32,
        /// The gid passed.
        gid: u32,
    },
}

/// The mode a `chown(2)` by **root** (`CAP_FSETID`) leaves on an inode,
/// measured on Linux (debian:12; `docs/plan/DECISIONS.md` `[FAKE-CHOWN]` has
/// the table): on anything that is not a directory, every successful `chown`
/// clears `S_ISUID`, and clears `S_ISGID` when group execute is set (setgid
/// without group execute is mandatory locking, and stays). It does so even
/// when the owner and group are the ones the file already has, and even for
/// `chown(-1, -1)`. The sticky bit stays, and a directory keeps every bit.
///
/// The `Fake` models root, as it does elsewhere. An unprivileged owner who is
/// not in the file's group loses setgid without group execute as well
/// (`2745` became `0745`), which this does not model.
pub(crate) fn mode_after_chown(kind: FileKind, mode: u32) -> u32 {
    if kind == FileKind::Dir {
        return mode;
    }
    let mut mode = mode & !0o4000;
    if mode & 0o010 != 0 {
        mode &= !0o2000;
    }
    mode
}

/// In-memory backend for unit tests. Plant files, can commands, then assert
/// on what the op read, wrote, and ran.
///
/// `chown` is modelled as Linux does it, not as a plain assignment: see
/// [`Backend::set_owner`] here, and [`Fake::attr_calls`] for the record of
/// every `chmod` and `chown` an op made. The fake does not know which user
/// the op runs as, so every `chown` succeeds, as it does for root.
#[derive(Default)]
pub struct Fake {
    files: Mutex<BTreeMap<PathBuf, FakeFile>>,
    canned: Mutex<Vec<Canned>>,
    ran: Mutex<Vec<CmdSpec>>,
    attr_calls: Mutex<Vec<AttrCall>>,
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

    /// Every [`Backend::set_mode`] and [`Backend::set_owner`] call, in the
    /// order the op made them. The final state is in [`Fake::file`]; this is
    /// for asserting on what was *asked*: that a step issued no `chown` when
    /// the owner was already right (one that changes the owner needs root;
    /// one that doesn't is a needless call that also clears setuid), or that
    /// it set the owner before the mode. A fixture's own `Backend::set_*`
    /// calls are recorded too; take the length first to skip past them. The `chown` a rewrite does inside
    /// [`Backend::write`] is the backend's own, not an op's call, and is not
    /// recorded.
    pub fn attr_calls(&self) -> Vec<AttrCall> {
        self.attr_calls.lock().unwrap().clone()
    }

    /// The `chown` calls alone, as `(path, uid, gid)`, in order.
    pub fn chowns(&self) -> Vec<(PathBuf, u32, u32)> {
        self.attr_calls()
            .into_iter()
            .filter_map(|c| match c {
                AttrCall::Chown { path, uid, gid } => Some((path, uid, gid)),
                AttrCall::Chmod { .. } => None,
            })
            .collect()
    }

    /// The `chmod` calls alone, as `(path, mode)`, in order.
    pub fn chmods(&self) -> Vec<(PathBuf, u32)> {
        self.attr_calls()
            .into_iter()
            .filter_map(|c| match c {
                AttrCall::Chmod { path, mode } => Some((path, mode)),
                AttrCall::Chown { .. } => None,
            })
            .collect()
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
        // untouched. New files get 0644. An existing file keeps its owner,
        // and its mode as `Local::write` leaves it: that copies the mode onto
        // the temporary file and then `chown`s it to the same owner, and the
        // `chown` clears setuid (and setgid with group execute) as any does
        // (`mode_after_chown`). An op that wants those bits after a rewrite
        // has to set the mode again.
        let mut files = self.files.lock().unwrap();
        match files.get_mut(p) {
            Some(f) if f.kind == FileKind::File => {
                f.bytes = bytes.to_vec();
                f.mode = mode_after_chown(f.kind, f.mode);
            }
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

    // `chmod` and `chown` follow symlinks, which `System::set_mode` and
    // `System::set_owner` both document. Operating on the link entry itself
    // instead would let an op pass its tests here and change the wrong
    // inode on a real machine.
    fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()> {
        self.attr_calls.lock().unwrap().push(AttrCall::Chmod {
            path: p.to_path_buf(),
            mode,
        });
        let mut files = self.files.lock().unwrap();
        let real = Self::resolve(&files, p);
        files
            .get_mut(&real)
            .map(|f| f.mode = mode)
            .ok_or_else(|| not_found(p))
    }

    // `chown` clears setuid, and setgid with group execute, on anything but
    // a directory, whether or not the ids change: `mode_after_chown` has the
    // measured rule. Assigning the ids alone let an op that `chmod`s and then
    // `chown`s a setuid file pass here and lose the bit on a real machine.
    fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> io::Result<()> {
        self.attr_calls.lock().unwrap().push(AttrCall::Chown {
            path: p.to_path_buf(),
            uid,
            gid,
        });
        let mut files = self.files.lock().unwrap();
        let real = Self::resolve(&files, p);
        files
            .get_mut(&real)
            .map(|f| {
                f.uid = uid;
                f.gid = gid;
                f.mode = mode_after_chown(f.kind, f.mode);
            })
            .ok_or_else(|| not_found(p))
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        // The real `copy` (`std::fs::copy`) never `chown`s. Going through
        // `write` here would clear setuid on an existing destination, which
        // `Local` does not; harmless today, because the one caller,
        // `System::backup`, always copies to a new path.
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
                // A canned response is a process that exited; the `Fake` has
                // no way to express one that was signalled, and no op needs
                // it to, so this is `None` rather than a builder knob.
                signal: None,
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

    /// The table measured as root on debian:12 (`[FAKE-CHOWN]`), one row per
    /// case: the mode before a `chown` and the mode after it. The ids do not
    /// matter, so the `Fake` takes none into account: the same-owner rows
    /// cleared exactly like the others.
    #[test]
    fn chown_clears_the_bits_linux_clears() {
        for (kind, before, after) in [
            (FileKind::File, 0o4755, 0o755),
            (FileKind::File, 0o2755, 0o755),
            (FileKind::File, 0o2745, 0o2745),
            (FileKind::File, 0o6755, 0o755),
            (FileKind::File, 0o6745, 0o2745),
            (FileKind::File, 0o4700, 0o700),
            (FileKind::File, 0o1755, 0o1755),
            (FileKind::File, 0o644, 0o644),
            (FileKind::Dir, 0o2775, 0o2775),
            (FileKind::Dir, 0o1777, 0o1777),
            (FileKind::Dir, 0o6775, 0o6775),
        ] {
            assert_eq!(mode_after_chown(kind, before), after, "{kind:?} {before:o}");
        }
    }

    /// Through the backend: a `chown` to the owner the file already has
    /// still clears setuid, a `chmod` after it puts the bit back, and a
    /// directory keeps its setgid. Both calls are recorded, in order, with
    /// the path as passed.
    #[test]
    fn set_owner_clears_setuid_and_every_call_is_recorded() {
        let fake = Fake::new()
            .with_file_mode("/bin/x", "", 0o4755)
            .with_dir("/srv/shared");
        fake.set_owner(Path::new("/bin/x"), 0, 0).unwrap();
        assert_eq!(fake.file("/bin/x").unwrap().mode, 0o755);
        fake.set_mode(Path::new("/bin/x"), 0o4755).unwrap();
        assert_eq!(fake.file("/bin/x").unwrap().mode, 0o4755);

        fake.set_mode(Path::new("/srv/shared"), 0o2775).unwrap();
        fake.set_owner(Path::new("/srv/shared"), 5, 6).unwrap();
        assert_eq!(fake.file("/srv/shared").unwrap().mode, 0o2775);

        assert_eq!(
            fake.attr_calls(),
            vec![
                AttrCall::Chown {
                    path: "/bin/x".into(),
                    uid: 0,
                    gid: 0
                },
                AttrCall::Chmod {
                    path: "/bin/x".into(),
                    mode: 0o4755
                },
                AttrCall::Chmod {
                    path: "/srv/shared".into(),
                    mode: 0o2775
                },
                AttrCall::Chown {
                    path: "/srv/shared".into(),
                    uid: 5,
                    gid: 6
                },
            ]
        );
        assert_eq!(fake.chowns().len(), 2);
        assert_eq!(
            fake.chmods(),
            vec![
                (PathBuf::from("/bin/x"), 0o4755),
                (PathBuf::from("/srv/shared"), 0o2775)
            ]
        );

        // Through a symlink: the target changes, and the call is recorded
        // with the path as passed, the link, not the target it resolved to.
        let fake = Fake::new()
            .with_file_mode("/opt/real", "", 0o4755)
            .with_symlink("/opt/link", "/opt/real");
        fake.set_owner(Path::new("/opt/link"), 7, 8).unwrap();
        fake.set_mode(Path::new("/opt/link"), 0o4755).unwrap();
        assert_eq!(
            fake.attr_calls(),
            vec![
                AttrCall::Chown {
                    path: "/opt/link".into(),
                    uid: 7,
                    gid: 8
                },
                AttrCall::Chmod {
                    path: "/opt/link".into(),
                    mode: 0o4755
                },
            ]
        );
        let real = fake.file("/opt/real").unwrap();
        assert_eq!((real.mode, real.uid, real.gid), (0o4755, 7, 8));
    }

    /// A rewrite of an existing file keeps its owner and loses setuid, as
    /// `Local::write` does: it copies the mode onto the replacement and then
    /// `chown`s it to the same owner. A new file is 0644.
    #[test]
    fn a_rewrite_clears_setuid_as_local_write_does() {
        let fake = Fake::new().with_file_mode("/bin/x", "v1", 0o4755);
        fake.write(Path::new("/bin/x"), b"v2").unwrap();
        let f = fake.file("/bin/x").unwrap();
        assert_eq!((f.mode, f.bytes.as_slice()), (0o755, b"v2".as_slice()));
        assert!(fake.attr_calls().is_empty(), "the backend's own chown");
    }

    /// `chmod` and `chown` follow symlinks, and `System::set_mode` and
    /// `System::set_owner` both say so. The fake operated on the link entry
    /// itself until `ssh::authorized_keys` learned to repair a symlinked
    /// `~/.ssh`: its test passed here while a real machine would have kept
    /// the wrong mode on the directory that actually holds the keys.
    #[test]
    fn set_mode_and_set_owner_follow_symlinks_as_chmod_does() {
        let fake = Fake::new()
            .with_dir("/srv/keys")
            .with_symlink("/home/a/.ssh", "/srv/keys");
        fake.set_mode(Path::new("/home/a/.ssh"), 0o700).unwrap();
        fake.set_owner(Path::new("/home/a/.ssh"), 1000, 1001)
            .unwrap();

        let target = fake.file("/srv/keys").unwrap();
        assert_eq!((target.mode, target.uid, target.gid), (0o700, 1000, 1001));
        // The link itself is untouched, and is still a link.
        assert_eq!(fake.file("/home/a/.ssh").unwrap().kind, FileKind::Symlink);
    }

    /// A dangling link has nothing to chmod, and says so rather than
    /// inventing the target.
    #[test]
    fn set_mode_through_a_dangling_symlink_is_not_found() {
        let fake = Fake::new().with_symlink("/home/a/.ssh", "/gone");
        assert!(fake.set_mode(Path::new("/home/a/.ssh"), 0o700).is_err());
        assert!(fake.file("/gone").is_none());
    }

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
