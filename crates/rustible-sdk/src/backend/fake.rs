use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{AttrStep, Backend, CmdSpec, FileKind, Output, Stat, WriteAttrs, attr_steps};

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

/// One read an op made, as [`Fake::reads`] records it: the path as the op
/// passed it, and how many bytes it was served. A [`Backend::read`] is served
/// the whole file at once; a reader from [`Backend::open_read`] is recorded
/// when it is opened and counts the bytes as they are read from it, so a
/// test can tell that an op compared a large file without reading it whole.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ReadCall {
    /// The path passed.
    pub path: PathBuf,
    /// The bytes served so far.
    pub bytes: u64,
    /// Whether it came from [`Backend::open_read`] rather than
    /// [`Backend::read`].
    pub streamed: bool,
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
    reads: Mutex<Vec<ReadCall>>,
    /// Names the staged file of each [`Backend::write_from`] with attributes.
    staged: AtomicU64,
    /// Every `chown` fails with `EPERM` ([`Fake::with_chown_refused`]).
    chown_refused: bool,
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

    /// Make every `chown` fail with `EPERM`, as it does for an unprivileged
    /// login or a root without `CAP_CHOWN`; the fake otherwise models root,
    /// whose `chown` always succeeds. That includes the owner a
    /// [`Backend::write_from`] is given, so a test can show that a refused
    /// owner fails the write and leaves the target as it was. An owner the
    /// write only keeps from the file it replaces (a rewrite given just a
    /// mode) is best effort, as on `Local`: its refused `chown` is ignored
    /// and the write goes on. A refused call changes nothing and is still
    /// recorded in [`Fake::attr_calls`]: it was asked for.
    ///
    /// Not modelled: an unprivileged `Local` rewrite whose `chown` back to
    /// the old owner is refused leaves the file the writer's. The fake has
    /// no writer's identity, so its rewrites, with or without attributes,
    /// keep the old owner.
    pub fn with_chown_refused(mut self) -> Self {
        self.chown_refused = true;
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
    /// one that doesn't is a needless call that also clears setuid), or the
    /// order of its `set_mode` and `set_owner` calls. A fixture's own
    /// `Backend::set_*` calls are recorded too; take the length first to
    /// skip past them. The `chown` a rewrite does inside [`Backend::write`]
    /// is the backend's own, not an op's call, and is not recorded; the
    /// attributes a [`Backend::write_from`] was given are recorded, on the
    /// staged `.rustible-fake-<n>` path beside the target, before the
    /// rename that puts it in place. Those calls include what the write
    /// keeps: a rewrite given only a mode also records a `chown` to the
    /// owner it keeps, as `Local` makes one.
    pub fn attr_calls(&self) -> Vec<AttrCall> {
        self.attr_calls.lock().unwrap().clone()
    }

    /// Every [`Backend::read`] and [`Backend::open_read`] that found a file,
    /// in order, with the bytes each was served ([`ReadCall`]). A fixture's
    /// own reads are recorded too; take the length first to skip past them.
    pub fn reads(&self) -> Vec<ReadCall> {
        self.reads.lock().unwrap().clone()
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

impl Fake {
    /// What a read of `p` is served, following symlinks as `std::fs::read`
    /// does.
    fn contents(&self, p: &Path) -> io::Result<Vec<u8>> {
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

    /// Record a read of `p`, served `bytes` so far; its index in the log.
    fn log_read(&self, p: &Path, bytes: u64, streamed: bool) -> usize {
        let mut reads = self.reads.lock().unwrap();
        reads.push(ReadCall {
            path: p.to_path_buf(),
            bytes,
            streamed,
        });
        reads.len() - 1
    }

    /// [`Backend::write_from`] with attributes, as `Local`'s staged writer
    /// does it: the content goes to a `.rustible-fake-<n>` file beside `p`,
    /// at 0600, which gets its mode and owner through this backend's own
    /// `set_mode` and `set_owner` (so they are in [`Fake::attr_calls`],
    /// naming the staged path) and is then renamed over `p`. A field left
    /// `None` keeps the existing file's mode or owner (a symlink at `p` is
    /// followed for them), or is a new file's 0644 and nothing.
    fn write_staged(&self, p: &Path, bytes: Vec<u8>, attrs: WriteAttrs) -> io::Result<()> {
        let (mode, owner) = {
            let files = self.files.lock().unwrap();
            if files.get(p).is_some_and(|f| f.kind == FileKind::Dir) {
                return Err(io::Error::new(
                    io::ErrorKind::IsADirectory,
                    format!("{}: is a directory (fake)", p.display()),
                ));
            }
            let old = match files.get(&Self::resolve(&files, p)) {
                Some(f) if f.kind != FileKind::Symlink => Some((f.mode, (f.uid, f.gid))),
                _ => None,
            };
            (
                attrs
                    .mode
                    .map(|m| m & 0o7777)
                    .or(old.map(|o| o.0))
                    .unwrap_or(0o644),
                attrs.owner.or(old.map(|o| o.1)),
            )
        };
        let n = self.staged.fetch_add(1, Ordering::SeqCst);
        let tmp = p.with_file_name(format!(".rustible-fake-{n}"));
        self.files.lock().unwrap().insert(
            tmp.clone(),
            FakeFile {
                bytes,
                mode: 0o600,
                uid: 0,
                gid: 0,
                kind: FileKind::File,
            },
        );
        // An owner asked for must be given; one kept from the file being
        // replaced is best effort, as `Local` keeps it.
        let required = attrs.owner.is_some();
        let attributed = attr_steps(mode, owner)
            .into_iter()
            .try_for_each(|step| match step {
                AttrStep::Mode(m) => self.set_mode(&tmp, m),
                AttrStep::Owner(uid, gid) => match self.set_owner(&tmp, uid, gid) {
                    Err(e) if required => Err(e),
                    _ => Ok(()),
                },
            });
        match attributed.and_then(|()| self.rename(&tmp, p)) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.files.lock().unwrap().remove(&tmp);
                Err(e)
            }
        }
    }
}

/// [`Backend::open_read`] on a [`Fake`]: the file as it was when opened,
/// counting what is read from it into the read log.
struct FakeReader<'a> {
    data: io::Cursor<Vec<u8>>,
    reads: &'a Mutex<Vec<ReadCall>>,
    index: usize,
}

impl Read for FakeReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.data.read(buf)?;
        self.reads.lock().unwrap()[self.index].bytes += n as u64;
        Ok(n)
    }
}

impl Backend for Fake {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        let bytes = self.contents(p)?;
        self.log_read(p, bytes.len() as u64, false);
        Ok(bytes)
    }

    /// The whole of `src` first, so a source that fails writes nothing,
    /// then [`write`](Backend::write) when there are no attributes, or the
    /// staged write `Local` does when there are (see [`Fake::attr_calls`]).
    fn write_from(
        &self,
        p: &Path,
        src: &mut dyn Read,
        attrs: Option<WriteAttrs>,
    ) -> io::Result<u64> {
        let mut bytes = Vec::new();
        src.read_to_end(&mut bytes)?;
        let n = bytes.len() as u64;
        match attrs.filter(|a| a.mode.is_some() || a.owner.is_some()) {
            None => self.write(p, &bytes)?,
            Some(attrs) => self.write_staged(p, bytes, attrs)?,
        }
        Ok(n)
    }

    fn open_read(&self, p: &Path) -> io::Result<Box<dyn Read + Send + '_>> {
        let data = io::Cursor::new(self.contents(p)?);
        let index = self.log_read(p, 0, true);
        Ok(Box::new(FakeReader {
            data,
            reads: &self.reads,
            index,
        }))
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        // Mirrors `Local::write` (tempfile + rename). An existing file keeps
        // its owner and its whole mode, setuid and setgid included: `Local`
        // sets setuid and setgid again after its `chown`, which cleared them
        // (`mode_after_chown`). Writing at a symlink's path replaces the link
        // itself with a regular file carrying the *target's* mode and owner,
        // because `Local` reads them with a `stat` that follows the link; the
        // target is untouched. A new file, or one at a dangling link, is
        // 0644 root.
        let mut files = self.files.lock().unwrap();
        if files.get(p).is_some_and(|f| f.kind == FileKind::Dir) {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                format!("{}: is a directory (fake)", p.display()),
            ));
        }
        let real = Self::resolve(&files, p);
        let (mode, uid, gid) = match files.get(&real) {
            Some(f) if f.kind != FileKind::Symlink => (f.mode, f.uid, f.gid),
            _ => (0o644, 0, 0),
        };
        files.insert(
            p.to_path_buf(),
            FakeFile {
                bytes: bytes.to_vec(),
                mode,
                uid,
                gid,
                kind: FileKind::File,
            },
        );
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
        if self.chown_refused {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("chown {}: Operation not permitted (fake)", p.display()),
            ));
        }
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
        // `Local::copy`'s rules (issue #75): the source is followed and must
        // be a regular file; anything at `to`, a symlink included, refuses
        // with `AlreadyExists` and stays as it was; the copy is new, carries
        // the source's mode without setuid, setgid and sticky, and is given
        // the source's owner and group, which always succeeds here because
        // the fake runs as root (`Local` ignores the `EPERM` an unprivileged
        // runner gets).
        let mut files = self.files.lock().unwrap();
        let real = Self::resolve(&files, from);
        let (bytes, mode, uid, gid) = match files.get(&real) {
            Some(f) if f.kind == FileKind::File => (f.bytes.clone(), f.mode, f.uid, f.gid),
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{}: not a regular file, so not copied (fake)",
                        from.display()
                    ),
                ));
            }
            None => return Err(not_found(from)),
        };
        if files.contains_key(to) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{}: already exists (fake)", to.display()),
            ));
        }
        files.insert(
            to.to_path_buf(),
            FakeFile {
                bytes,
                mode: mode & 0o777,
                uid,
                gid,
                kind: FileKind::File,
            },
        );
        Ok(())
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

    /// A rewrite of an existing file keeps its owner and its whole mode,
    /// setuid and setgid with group execute included, as `Local::write`
    /// does: it `chown`s the replacement to the same owner first and copies
    /// the mode onto it after, so the bits that `chown` clears come back
    /// (issue #51). Neither is recorded: they are the backend's own calls,
    /// not an op's.
    #[test]
    fn a_rewrite_keeps_owner_and_mode_setuid_included() {
        for mode in [0o4755, 0o2755, 0o6755, 0o2745, 0o1755, 0o600] {
            // Owner first, then mode, so the planted mode is the one asked.
            let fake = Fake::new().with_file("/bin/x", "v1");
            fake.set_owner(Path::new("/bin/x"), 5, 6).unwrap();
            fake.set_mode(Path::new("/bin/x"), mode).unwrap();
            let planted = fake.attr_calls().len();

            fake.write(Path::new("/bin/x"), b"v2").unwrap();
            let f = fake.file("/bin/x").unwrap();
            assert_eq!(
                (f.mode, f.uid, f.gid, f.bytes.as_slice()),
                (mode, 5, 6, b"v2".as_slice()),
                "{mode:o}"
            );
            assert_eq!(fake.attr_calls().len(), planted, "the backend's own calls");
        }
    }

    /// Writing at a symlink's path replaces the link with a regular file
    /// that has the target's mode and owner, as `Local::write` does (its
    /// `a_write_at_a_symlink_takes_the_targets_mode`); the target keeps its
    /// content. A dangling link gives a new file's 0644.
    #[test]
    fn a_write_at_a_symlink_takes_the_targets_mode() {
        let fake = Fake::new()
            .with_file("/opt/real", "t")
            .with_symlink("/opt/link", "/opt/real")
            .with_symlink("/opt/dangling", "/opt/gone");
        fake.set_owner(Path::new("/opt/real"), 5, 6).unwrap();
        fake.set_mode(Path::new("/opt/real"), 0o4750).unwrap();

        fake.write(Path::new("/opt/link"), b"new").unwrap();
        let f = fake.file("/opt/link").unwrap();
        assert_eq!(
            (f.kind, f.mode, f.uid, f.gid, f.bytes.as_slice()),
            (FileKind::File, 0o4750, 5, 6, b"new".as_slice())
        );
        assert_eq!(fake.content("/opt/real").unwrap(), "t");

        fake.write(Path::new("/opt/dangling"), b"x").unwrap();
        let f = fake.file("/opt/dangling").unwrap();
        assert_eq!((f.kind, f.mode, f.uid), (FileKind::File, 0o644, 0));
        assert!(fake.file("/opt/gone").is_none());
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

    /// Every read that found a file is logged with the bytes it was served:
    /// a `read` the whole file, a reader what was taken from it, so a reader
    /// dropped half way shows half. A miss is not logged.
    #[test]
    fn reads_are_logged_with_the_bytes_served() {
        let fake = Fake::new()
            .with_file("/big", vec![b'x'; 1000])
            .with_symlink("/link", "/big");
        assert_eq!(
            Backend::read(&fake, Path::new("/link")).unwrap().len(),
            1000
        );
        let mut r = fake.open_read(Path::new("/big")).unwrap();
        r.read_exact(&mut [0u8; 100]).unwrap();
        r.read_exact(&mut [0u8; 50]).unwrap();
        drop(r);
        assert!(fake.open_read(Path::new("/nope")).is_err());
        let mut whole = Vec::new();
        fake.open_read(Path::new("/big"))
            .unwrap()
            .read_to_end(&mut whole)
            .unwrap();
        let log = |path: &str, bytes, streamed| ReadCall {
            path: path.into(),
            bytes,
            streamed,
        };
        assert_eq!(
            fake.reads(),
            [
                log("/link", 1000, false),
                log("/big", 150, true),
                log("/big", 1000, true),
            ]
        );
    }

    /// `write_from` with attributes stages the content beside the target,
    /// gives the staged file its mode and owner in the safe order (each call
    /// recorded, naming the staged path), and only then renames it over the
    /// target; without attributes it is `write`. A source that fails writes
    /// nothing and makes no call.
    #[test]
    fn write_from_applies_attributes_to_the_staged_file_before_the_rename() {
        let fake = Fake::new()
            .with_dir("/d")
            .with_file_mode("/d/old", "old", 0o640);
        fake.set_owner(Path::new("/d/old"), 7, 8).unwrap();
        let planted = fake.attr_calls().len();

        let attrs = WriteAttrs {
            mode: Some(0o4750),
            owner: Some((5, 6)),
        };
        let n = fake
            .write_from(Path::new("/d/new"), &mut &b"new"[..], Some(attrs))
            .unwrap();
        assert_eq!(n, 3);
        let staged = PathBuf::from("/d/.rustible-fake-0");
        assert_eq!(
            fake.attr_calls()[planted..],
            [
                AttrCall::Chmod {
                    path: staged.clone(),
                    mode: 0o750
                },
                AttrCall::Chown {
                    path: staged.clone(),
                    uid: 5,
                    gid: 6
                },
                AttrCall::Chmod {
                    path: staged.clone(),
                    mode: 0o4750
                },
            ]
        );
        let f = fake.file("/d/new").unwrap();
        assert_eq!(
            (f.mode, f.uid, f.gid, f.bytes.as_slice()),
            (0o4750, 5, 6, &b"new"[..])
        );
        assert!(fake.file(&staged).is_none());

        // A rewrite given only a mode keeps the owner it had, with a `chown`
        // to it, as `Local` keeps it.
        let planted = fake.attr_calls().len();
        let only_mode = WriteAttrs {
            mode: Some(0o600),
            owner: None,
        };
        fake.write_from(Path::new("/d/old"), &mut &b"v2"[..], Some(only_mode))
            .unwrap();
        let staged = PathBuf::from("/d/.rustible-fake-1");
        assert_eq!(
            fake.attr_calls()[planted..],
            [
                AttrCall::Chmod {
                    path: staged.clone(),
                    mode: 0o600
                },
                AttrCall::Chown {
                    path: staged,
                    uid: 7,
                    gid: 8
                },
            ]
        );
        let f = fake.file("/d/old").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o600, 7, 8));

        // Without attributes, `write`: the mode and owner kept, no call.
        let planted = fake.attr_calls().len();
        fake.write_from(Path::new("/d/old"), &mut &b"v3"[..], None)
            .unwrap();
        assert_eq!(fake.attr_calls().len(), planted);
        assert_eq!(fake.content("/d/old").unwrap(), "v3");

        struct Failing;
        impl Read for Failing {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("broke"))
            }
        }
        let err = fake
            .write_from(Path::new("/d/old"), &mut Failing, Some(attrs))
            .unwrap_err();
        assert_eq!(err.to_string(), "broke");
        assert_eq!(fake.attr_calls().len(), planted);
        assert_eq!(fake.content("/d/old").unwrap(), "v3");
        assert_eq!(
            fake.read_dir(Path::new("/d")).unwrap(),
            [PathBuf::from("/d/new"), PathBuf::from("/d/old")]
        );
    }

    /// With `chown` refused, a `write_from` given an owner fails: the
    /// target keeps its content, mode and owner, and nothing is left beside
    /// it. The mode was set on the staged file first, then the `chown` was
    /// refused, so the calls stop there.
    #[test]
    fn a_refused_owner_fails_a_staged_write_and_leaves_the_target() {
        let fake = Fake::new()
            .with_dir("/d")
            .with_file_mode("/d/f", "before", 0o640)
            .with_chown_refused();
        let attrs = WriteAttrs {
            mode: Some(0o4750),
            owner: Some((5, 6)),
        };
        for target in ["/d/f", "/d/new"] {
            let planted = fake.attr_calls().len();
            let err = fake
                .write_from(Path::new(target), &mut &b"after"[..], Some(attrs))
                .unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
            let calls = fake.attr_calls()[planted..].to_vec();
            assert!(
                matches!(
                    &calls[..],
                    [AttrCall::Chmod { mode: 0o750, .. }, AttrCall::Chown { .. }]
                ),
                "{calls:?}"
            );
        }
        let f = fake.file("/d/f").unwrap();
        assert_eq!(
            (f.mode, f.uid, f.bytes.as_slice()),
            (0o640, 0, &b"before"[..])
        );
        assert_eq!(
            fake.read_dir(Path::new("/d")).unwrap(),
            [PathBuf::from("/d/f")]
        );
        assert!(fake.set_owner(Path::new("/d/f"), 1, 1).is_err());
    }

    /// With `chown` refused, a rewrite given only a mode still succeeds: the
    /// owner it keeps is best effort, as `Local` keeps it, so the refused
    /// `chown` to it is ignored. The file keeps its old owner, which is what
    /// the fake can show; an unprivileged `Local` leaves it the writer's.
    #[test]
    fn a_kept_owner_that_cannot_be_given_does_not_fail_a_staged_write() {
        let fake = Fake::new()
            .with_dir("/d")
            .with_file_mode("/d/f", "before", 0o640);
        fake.set_owner(Path::new("/d/f"), 7, 8).unwrap();
        let fake = Fake {
            chown_refused: true,
            ..fake
        };
        let only_mode = WriteAttrs {
            mode: Some(0o600),
            owner: None,
        };
        fake.write_from(Path::new("/d/f"), &mut &b"after"[..], Some(only_mode))
            .unwrap();
        let f = fake.file("/d/f").unwrap();
        assert_eq!((f.mode, f.bytes.as_slice()), (0o600, &b"after"[..]));
        assert_eq!(
            fake.read_dir(Path::new("/d")).unwrap(),
            [PathBuf::from("/d/f")]
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
