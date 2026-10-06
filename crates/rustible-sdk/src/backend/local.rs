use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{AttrStep, Backend, CmdSpec, FileKind, Output, Stat, WriteAttrs, attr_steps, reworded};

/// The production backend: real filesystem, real processes.
pub struct Local;

impl Local {
    fn stat_with(m: io::Result<std::fs::Metadata>) -> io::Result<Option<Stat>> {
        match m {
            Ok(m) => {
                let ft = m.file_type();
                let kind = if ft.is_symlink() {
                    FileKind::Symlink
                } else if ft.is_dir() {
                    FileKind::Dir
                } else if ft.is_file() {
                    FileKind::File
                } else {
                    FileKind::Other
                };
                Ok(Some(Stat {
                    mode: m.permissions().mode() & 0o7777,
                    uid: m.uid(),
                    gid: m.gid(),
                    size: m.len(),
                    kind,
                }))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// A write in progress: the new content staged in a temporary file beside
/// its target, renamed over it only at [`commit`](Staged::commit). The one
/// implementation of an atomic write, shared by [`Local::write`],
/// [`Backend::write_from`] and the escalation helper's write streams, so
/// the rules below cannot drift between them (`[ISSUE-85]`).
///
/// The temporary file is `.rustible-<random>` in the target's directory, so
/// the rename never crosses a filesystem. It is created at `0600` for a
/// rewrite, and at `0600` for a new file given attributes, so the new
/// content is readable by nobody else while it streams in; a new file
/// without attributes is created as any new file is (0666 minus the umask),
/// which is the mode it keeps. Dropping a `Staged` that was not committed
/// removes the temporary file, so a failure part way, or a helper whose
/// parent went away, leaves the target exactly as it was and nothing beside
/// it.
///
/// At commit the content is synced, then given its mode and owner on the
/// descriptor, in [`attr_steps`]' order, before the rename:
///
/// - **With [`WriteAttrs`]**, the mode and owner asked for; a field left
///   `None` falls back as below. A requested owner that cannot be given fails
///   the write.
/// - **A rewrite** keeps the existing file's mode, setuid and setgid
///   included, and its owner, best effort: only root can give a file away,
///   and an unprivileged rewrite of another user's file goes on, the file
///   now the writer's. The existing file is found with a `stat` that follows
///   symlinks, so writing at a link's path takes the target's mode and owner
///   and the rename replaces the link itself.
/// - **A new file** without attributes is left as created.
///
/// The setuid and setgid bits go on last because a `chown` clears them: on
/// Linux every successful `chown` of a non-directory clears setuid, and
/// setgid when group execute is set, even to the ids the file already has
/// (`[FAKE-CHOWN]` in `docs/plan/DECISIONS.md` has the measurements). Copying
/// the whole mode before the `chown` rewrote a 4755 file as 0755 and reported
/// success (issue #51). That last `chmod` fails only for a root without
/// `CAP_FOWNER` whose `chown` succeeded, and then the write fails saying so,
/// rather than leaving the file without its bits. The first `chmod` comes
/// while the writer still owns the file, because `chmod` of a file someone
/// else owns takes `CAP_FOWNER`; it sets the group bits while the file still
/// has the writer's group, so for that moment members of the writer's group
/// could open the new content as the old file's group could. That is
/// accepted rather than masked: masking them means a `chmod` after the
/// `chown` for nearly every rewrite, which is exactly what a root without
/// `CAP_FOWNER` cannot do. For root the writer's group is gid 0, already
/// privileged; unprivileged, it is the writer's own group, and the content
/// is the writer's.
///
/// Then the mode the file ended up with is read back. A `chmod` that asks
/// for setgid does not fail when the caller is neither in the file's group
/// nor holds `CAP_FSETID`: the kernel drops the bit and reports success (a
/// root without `CAP_FSETID` rewriting a `2755` file another group owns got
/// `0755`). That too fails the write, before the rename, so the old file
/// stays as it was.
pub(crate) struct Staged {
    tmp: tempfile::NamedTempFile,
    target: PathBuf,
    /// The mode the file is given at commit. `None` for a new file without
    /// attributes, which keeps the mode it was created with.
    mode: Option<u32>,
    /// The owner it is given at commit, if any.
    owner: Option<(u32, u32)>,
    /// Whether [`owner`](Self::owner) was asked for, so failing to give it
    /// fails the write, or kept from the file being replaced, best effort.
    owner_required: bool,
    written: u64,
}

impl Staged {
    /// Stage a write of `p`: decide its mode and owner from `attrs` and from
    /// the file already there, and create the temporary file beside it.
    pub(crate) fn begin(p: &Path, attrs: Option<WriteAttrs>) -> io::Result<Staged> {
        let dir = p.parent().unwrap_or(Path::new("."));
        // Followed: writing at a symlink's path takes the target's mode and
        // owner, and the rename then replaces the link itself.
        let existing = std::fs::metadata(p).ok();
        let attrs = attrs.filter(|a| a.mode.is_some() || a.owner.is_some());
        // A new file is created as any newly created file is (0666 minus the
        // umask); tempfile's own default is 0600, which is not what an op
        // that creates a config file expects. A rewrite starts at 0600
        // instead, so the new content is never readable by more than the
        // writer until its own mode is set at commit.
        let create = if existing.is_some() { 0o600 } else { 0o666 };
        let tmp = tempfile::Builder::new()
            .prefix(".rustible-")
            .permissions(std::fs::Permissions::from_mode(create))
            .tempfile_in(dir)?;
        // What the umask made of 0666 is a new file's mode unless one is
        // asked for. Read before narrowing to 0600, which happens before the
        // first byte is written.
        let created = tmp.as_file().metadata()?.permissions().mode() & 0o7777;
        if attrs.is_some() && existing.is_none() {
            tmp.as_file()
                .set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        let old_mode = existing.as_ref().map(|m| m.permissions().mode() & 0o7777);
        let old_owner = existing.as_ref().map(|m| (m.uid(), m.gid()));
        let (mode, owner, owner_required) = match attrs {
            None => (old_mode, old_owner, false),
            Some(a) => (
                Some(a.mode.or(old_mode).unwrap_or(created)),
                a.owner.or(old_owner),
                a.owner.is_some(),
            ),
        };
        Ok(Staged {
            tmp,
            target: p.to_path_buf(),
            mode,
            owner,
            owner_required,
            written: 0,
        })
    }

    /// Append `bytes` to the staged content.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.tmp.write_all(bytes)?;
        self.written += bytes.len() as u64;
        Ok(())
    }

    /// How many bytes have been staged so far.
    pub(crate) fn written(&self) -> u64 {
        self.written
    }

    /// Sync, give the file its mode and owner, and rename it over the
    /// target. On any error the temporary file is removed and the target is
    /// as it was.
    pub(crate) fn commit(mut self) -> io::Result<()> {
        self.prepare()?;
        self.persist()
    }

    /// Everything [`commit`](Self::commit) does before the rename. Split out
    /// so a test can look at the temporary file between the two.
    fn prepare(&mut self) -> io::Result<()> {
        let f = self.tmp.as_file();
        f.sync_all()?;
        let Some(mode) = self.mode else {
            return Ok(());
        };
        let mut owned = None;
        for step in attr_steps(mode, self.owner) {
            match step {
                AttrStep::Mode(m) => {
                    f.set_permissions(std::fs::Permissions::from_mode(m))
                        .map_err(|e| match owned {
                            Some((uid, gid)) => reworded(
                                &e,
                                format!(
                                    "gave the new file owner {uid}:{gid} but could not set its \
                                     mode to {m:04o} ({e}); setting setuid or setgid on a file \
                                     this process does not own takes CAP_FOWNER"
                                ),
                            ),
                            None => e,
                        })?;
                }
                AttrStep::Owner(uid, gid) => {
                    match std::os::unix::fs::fchown(f, Some(uid), Some(gid)) {
                        Ok(()) => owned = Some((uid, gid)),
                        Err(e) if self.owner_required => {
                            return Err(reworded(
                                &e,
                                format!(
                                    "could not give the new file owner {uid}:{gid} ({e}); \
                                     giving a file to another user takes root. The file was \
                                     not replaced"
                                ),
                            ));
                        }
                        // Kept from the file being replaced: best effort.
                        Err(_) => {}
                    }
                }
            }
        }
        let got = f.metadata()?;
        let got_mode = got.permissions().mode() & 0o7777;
        if got_mode != mode {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "set mode {mode:04o} on the new file but the kernel left {got_mode:04o} \
                     (group {}); setting setgid on a file whose group this process is not in \
                     takes CAP_FSETID. The file was not replaced",
                    got.gid()
                ),
            ));
        }
        Ok(())
    }

    /// Rename the temporary file over the target.
    fn persist(self) -> io::Result<()> {
        self.tmp.persist(&self.target).map_err(|e| e.error)?;
        Ok(())
    }
}

impl Backend for Local {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(p)
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        let mut staged = Staged::begin(p, None)?;
        staged.write(bytes)?;
        staged.commit()
    }

    fn stat(&self, p: &Path) -> io::Result<Option<Stat>> {
        Self::stat_with(std::fs::symlink_metadata(p))
    }

    fn stat_follow(&self, p: &Path) -> io::Result<Option<Stat>> {
        Self::stat_with(std::fs::metadata(p))
    }

    fn mkdir_all(&self, p: &Path) -> io::Result<()> {
        std::fs::create_dir_all(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir(p),
            Ok(_) => std::fs::remove_file(p),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn remove_all(&self, p: &Path) -> io::Result<()> {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
            Ok(_) => std::fs::remove_file(p),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::rename(from, to)
    }

    fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()> {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }

    fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> io::Result<()> {
        std::os::unix::fs::chown(p, Some(uid), Some(gid))
    }

    // Not `std::fs::copy`: it opens `to` with create and truncate, which
    // follows a symlink planted there and writes through it, and `fchmod`s
    // the copy to the source's whole mode, so root copying another user's
    // setuid file made a root-owned setuid copy (issue #75).
    //
    // `create_new` is `O_CREAT | O_EXCL`, and POSIX makes that fail with
    // `EEXIST` when `to` is a symlink, "regardless of the contents of the
    // symbolic link", so it never follows one, dangling or not; that is what
    // stands in for `O_NOFOLLOW`, which `std` has no constant for. `EEXIST`
    // keeps its errno, so the `Elevated` helper hands `AlreadyExists` back
    // intact.
    //
    // The source is checked with a `stat` before it is opened, because
    // opening a FIFO for reading blocks until a writer appears, and again on
    // the descriptor, in case it was swapped in between.
    //
    // Once the data is in, through the descriptor: (1) `fchmod` to the
    // source's mode without setuid, setgid and sticky, while the runner
    // still owns the copy, so a root without `CAP_FOWNER` can still do it;
    // (2) `fchown` to the source's owner and group when they differ, so a
    // root-owned copy of another user's file never exists for something
    // that trusts root-owned files (logrotate did, in the review), with
    // `EPERM` ignored as Ansible's `preserved_copy` does: an unprivileged
    // runner keeps its own copy. With `0o7000` gone there is nothing for the
    // `chown` to clear, so this order loses nothing.
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        let not_regular = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: not a regular file, so not copied", from.display()),
            )
        };
        if !std::fs::metadata(from)?.is_file() {
            return Err(not_regular());
        }
        let mut src = std::fs::File::open(from)?;
        let meta = src.metadata()?;
        if !meta.is_file() {
            return Err(not_regular());
        }
        // Load-bearing: the copy is `0600` until the data is in, so nobody
        // else can read it in between; nothing outside this function can
        // observe that window, so no test holds it.
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(to)?;
        let mode = meta.permissions().mode() & 0o777;
        let filled = io::copy(&mut src, &mut dst)
            .and_then(|_| dst.set_permissions(std::fs::Permissions::from_mode(mode)))
            .and_then(|()| {
                let mine = dst.metadata()?;
                if (mine.uid(), mine.gid()) == (meta.uid(), meta.gid()) {
                    return Ok(());
                }
                match std::os::unix::fs::fchown(&dst, Some(meta.uid()), Some(meta.gid())) {
                    Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Ok(()),
                    other => other,
                }
            });
        if let Err(e) = filled {
            drop(dst);
            // `create_new` succeeded, so the path is ours to remove; unlink
            // does not follow a link either.
            let _ = std::fs::remove_file(to);
            return Err(e);
        }
        Ok(())
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        std::fs::read_link(p)
    }

    fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(p)?
            .map(|e| e.map(|e| e.path()))
            .collect::<io::Result<_>>()?;
        out.sort();
        Ok(out)
    }

    fn spawn(&self, spec: &CmdSpec) -> io::Result<Output> {
        let argv = spec.argv();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]);
        cmd.env("LANG", "C").env("LC_ALL", "C");
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        if let Some(cwd) = &spec.cwd {
            cmd.current_dir(cwd);
        }
        cmd.stdin(if spec.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        // Feed stdin from a thread while the parent drains stdout/stderr: a
        // child that writes more than a pipe buffer before reading its input
        // would otherwise deadlock against our blocking write.
        // The copy the thread owns is wiped when it is done: stdin may carry
        // a secret (a password piped to `chpasswd`).
        let feeder = spec.stdin.clone().map(|input| {
            let input = zeroize::Zeroizing::new(input);
            let mut stdin = child.stdin.take().expect("piped stdin");
            std::thread::spawn(move || {
                // EPIPE (the child exited without reading) is not an error
                // of ours; the exit status tells the story.
                let _ = stdin.write_all(&input);
            })
        });
        let out = child.wait_with_output()?;
        if let Some(f) = feeder {
            let _ = f.join();
        }
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            signal: std::os::unix::process::ExitStatusExt::signal(&out.status),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn symlink_read_link_read_dir_on_real_fs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let target = root.join("target.txt");
        let link = root.join("link");
        std::fs::write(&target, "x").unwrap();

        Local.symlink(&target, &link).unwrap();
        assert_eq!(Local.read_link(&link).unwrap(), target);
        assert_eq!(Local.stat(&link).unwrap().unwrap().kind, FileKind::Symlink);
        assert!(Local.read_link(&target).is_err(), "not a symlink");
        assert!(Local.symlink(&target, &link).is_err(), "link exists");

        let kids = Local.read_dir(root).unwrap();
        assert_eq!(kids, vec![link.clone(), target.clone()]);

        // Removing the link keeps the target.
        Local.remove(&link).unwrap();
        assert!(Local.stat(&link).unwrap().is_none());
        assert!(Local.stat(&target).unwrap().is_some());
    }

    /// `copy` creates a new file or nothing: a symlink at `to`, to a file
    /// or dangling, and a file already there, are refused with
    /// `AlreadyExists` and left exactly as they were (issue #75).
    /// `std::fs::copy` wrote through the link into its target.
    #[test]
    fn copy_never_writes_through_a_link_or_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name);
        std::fs::write(at("src"), "new").unwrap();
        std::fs::write(at("victim"), "victim").unwrap();
        Local.set_mode(&at("victim"), 0o600).unwrap();
        std::fs::write(at("taken"), "taken").unwrap();
        Local.symlink(&at("victim"), &at("link")).unwrap();
        Local.symlink(&at("nowhere"), &at("dangling")).unwrap();

        for to in ["link", "dangling", "taken"] {
            let err = Local.copy(&at("src"), &at(to)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::AlreadyExists, "{to}: {err}");
        }
        assert_eq!(std::fs::read_to_string(at("victim")).unwrap(), "victim");
        assert_eq!(Local.stat(&at("victim")).unwrap().unwrap().mode, 0o600);
        assert_eq!(std::fs::read_to_string(at("taken")).unwrap(), "taken");
        assert!(
            Local.stat(&at("nowhere")).unwrap().is_none(),
            "not created through the link"
        );
        for link in ["link", "dangling"] {
            assert_eq!(
                Local.stat(&at(link)).unwrap().unwrap().kind,
                FileKind::Symlink
            );
        }

        // A directory is not copied, and nothing is created for it.
        let err = Local.copy(dir.path(), &at("of-a-dir")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert!(Local.stat(&at("of-a-dir")).unwrap().is_none());
    }

    /// The copy has the source's mode without setuid, setgid and sticky.
    /// Linux only: a mac refuses an unprivileged sticky `chmod` on a file.
    #[cfg(target_os = "linux")]
    #[test]
    fn copy_drops_setuid_setgid_and_sticky() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o4755, 0o2755, 0o6755, 0o2745, 0o1755, 0o640, 0o600] {
            let (from, to) = (
                dir.path().join(format!("f{mode:o}")),
                dir.path().join(format!("c{mode:o}")),
            );
            std::fs::write(&from, "data").unwrap();
            Local.set_mode(&from, mode).unwrap();
            assert_eq!(
                Local.stat(&from).unwrap().unwrap().mode,
                mode,
                "planting {mode:o}"
            );
            Local.copy(&from, &to).unwrap();
            let st = Local.stat(&to).unwrap().unwrap();
            assert_eq!(
                (st.mode, st.kind),
                (mode & 0o777, FileKind::File),
                "{mode:o}"
            );
            assert_eq!(std::fs::read_to_string(&to).unwrap(), "data");
        }
    }

    /// Unprivileged, a copy of a file someone else owns cannot be given to
    /// them: the `EPERM` from `fchown` is ignored, as Ansible's
    /// `preserved_copy` does, and the runner keeps its own copy. As root the
    /// owner is given (T2 in `it_file_ops`); this one is skipped as root.
    #[test]
    fn a_copy_that_cannot_be_given_its_owner_is_kept() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let src = Path::new("/etc/passwd");
        assert_eq!(
            Local.stat(src).unwrap().unwrap().uid,
            0,
            "a root-owned source"
        );
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("passwd");
        Local.copy(src, &to).unwrap();
        let st = Local.stat(&to).unwrap().unwrap();
        assert_eq!(st.uid, rustix::process::geteuid().as_raw());
        assert_eq!(Local.read(&to).unwrap(), Local.read(src).unwrap());
    }

    /// A FIFO source is refused at once, before it is opened: opening one
    /// for reading blocks until a writer appears, which would hang the step.
    /// Bounded, so a regression fails instead of wedging the suite.
    #[cfg(target_os = "linux")]
    #[test]
    fn copy_refuses_a_fifo_without_blocking() {
        let dir = tempfile::tempdir().unwrap();
        let (fifo, to) = (dir.path().join("fifo"), dir.path().join("copy"));
        let made = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo");
        let (tx, rx) = std::sync::mpsc::channel();
        let (f, t) = (fifo.clone(), to.clone());
        std::thread::spawn(move || tx.send(Local.copy(&f, &t)));
        let err = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("copy blocked on a FIFO")
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert!(Local.stat(&to).unwrap().is_none());
    }

    /// A copy that fails part-way removes what it created. `/proc/self/mem`
    /// is a regular file that opens and then fails to read at offset 0,
    /// which nothing is mapped at.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_copy_that_fails_part_way_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let to = dir.path().join("half");
        let mem = Path::new("/proc/self/mem");
        let meta = std::fs::metadata(mem).unwrap();
        assert!(
            meta.is_file(),
            "the failure has to come after the copy is created"
        );
        std::fs::File::open(mem).expect("opening /proc/self/mem, or the test proves nothing");
        let err = Local.copy(mem, &to).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert!(
            Local.stat(&to).unwrap().is_none(),
            "a half-written copy was left"
        );
    }

    /// A rewrite keeps the old file's whole mode, setuid and setgid
    /// included, and its owner. Runs unprivileged on the real filesystem:
    /// Linux clears setuid on any successful `chown`, even an unprivileged
    /// one to the ids the file already has, so copying the mode before the
    /// `chown` fails here as it did in a container as root (issue #51).
    /// Linux only: macOS documents that clearing for non-root callers too,
    /// but nobody has measured it here.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_rewrite_keeps_setuid_and_setgid() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o4755, 0o2755, 0o6755, 0o2745, 0o1755, 0o640] {
            let f = dir.path().join(format!("f{mode:o}"));
            std::fs::write(&f, "v1").unwrap();
            Local.set_mode(&f, mode).unwrap();
            let before = Local.stat(&f).unwrap().unwrap();
            // Planted as asked, or the assertion below proves nothing (a
            // kernel drops setgid for an owner outside the file's group).
            assert_eq!(before.mode, mode, "planting {mode:o}");

            Local.write(&f, b"v2").unwrap();
            let after = Local.stat(&f).unwrap().unwrap();
            assert_eq!(std::fs::read(&f).unwrap(), b"v2");
            assert_eq!(
                (after.mode, after.uid, after.gid),
                (mode, before.uid, before.gid),
                "{mode:o}"
            );
        }
        // Nothing left behind: the temporary files were all renamed.
        assert_eq!(Local.read_dir(dir.path()).unwrap().len(), 6);
    }

    /// Writing at a symlink's path replaces the link with a regular file
    /// carrying the target's mode, and leaves the target alone. The `Fake`
    /// mirrors this (`a_write_at_a_symlink_takes_the_targets_mode`).
    #[test]
    fn a_write_at_a_symlink_takes_the_targets_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::fs::write(&target, "t").unwrap();
        Local.set_mode(&target, 0o640).unwrap();
        Local.symlink(&target, &link).unwrap();

        Local.write(&link, b"new").unwrap();
        let st = Local.stat(&link).unwrap().unwrap();
        assert_eq!((st.kind, st.mode), (FileKind::File, 0o640));
        assert_eq!(std::fs::read(&target).unwrap(), b"t");
    }

    // ---- Staged ----

    /// The `.rustible-*` temporary files in `dir`.
    fn staged_in(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".rustible-")
            })
            .collect()
    }

    /// The mode of the one temporary file in `dir`.
    fn staged_mode(dir: &Path) -> u32 {
        let tmp = staged_in(dir);
        assert_eq!(tmp.len(), 1, "{tmp:?}");
        std::fs::metadata(&tmp[0]).unwrap().permissions().mode() & 0o7777
    }

    /// A rewrite's content is staged at 0600 for the whole stream, between
    /// any two chunks, and so is a new file given attributes; the target
    /// keeps its old content until the commit.
    #[test]
    fn the_staged_file_is_0600_between_two_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        std::fs::write(&old, "before").unwrap();
        Local.set_mode(&old, 0o644).unwrap();
        let new = dir.path().join("new");
        let wanted = WriteAttrs {
            mode: Some(0o644),
            owner: None,
        };
        for (p, attrs) in [(&old, None), (&new, Some(wanted))] {
            let mut s = Staged::begin(p, attrs).unwrap();
            assert_eq!(staged_mode(dir.path()), 0o600, "{}", p.display());
            s.write(b"one chunk, ").unwrap();
            assert_eq!(staged_mode(dir.path()), 0o600, "{}", p.display());
            s.write(b"then another").unwrap();
            assert_eq!(staged_mode(dir.path()), 0o600, "{}", p.display());
            s.commit().unwrap();
            assert!(staged_in(dir.path()).is_empty());
            assert_eq!(std::fs::read(p).unwrap(), b"one chunk, then another");
            assert_eq!(Local.stat(p).unwrap().unwrap().mode, 0o644);
        }
    }

    /// The mode asked for is on the staged file before the rename, and the
    /// target does not exist until then. Setuid included, on Linux, where an
    /// unprivileged setuid `chmod` of one's own file is measured to work.
    #[test]
    fn requested_attributes_are_in_place_before_the_rename() {
        let dir = tempfile::tempdir().unwrap();
        let modes: &[u32] = if cfg!(target_os = "linux") {
            &[0o640, 0o600, 0o4750]
        } else {
            &[0o640, 0o600]
        };
        let me = rustix::process::geteuid().as_raw();
        let group = rustix::process::getegid().as_raw();
        for &mode in modes {
            for owner in [None, Some((me, group))] {
                let p = dir.path().join(format!("f{mode:o}-{}", owner.is_some()));
                let mut s = Staged::begin(
                    &p,
                    Some(WriteAttrs {
                        mode: Some(mode),
                        owner,
                    }),
                )
                .unwrap();
                s.write(b"secret").unwrap();
                s.prepare().unwrap();
                assert_eq!(staged_mode(dir.path()), mode, "{mode:o} {owner:?}");
                assert!(!p.exists(), "renamed before the attributes were set");
                s.persist().unwrap();
                assert_eq!(Local.stat(&p).unwrap().unwrap().mode, mode);
                assert_eq!(std::fs::read(&p).unwrap(), b"secret");
            }
        }
    }

    /// A new file without attributes keeps the mode any new file gets here,
    /// 0666 minus the umask; with only an owner asked for, it gets that mode
    /// too, once the data is in.
    #[test]
    fn a_new_file_gets_the_umask_mode_unless_one_is_asked_for() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::write(&plain, "x").unwrap();
        let umasked = Local.stat(&plain).unwrap().unwrap().mode;
        let me = (
            rustix::process::geteuid().as_raw(),
            rustix::process::getegid().as_raw(),
        );
        for (name, attrs) in [
            ("none", None),
            (
                "owner",
                Some(WriteAttrs {
                    mode: None,
                    owner: Some(me),
                }),
            ),
        ] {
            let p = dir.path().join(name);
            let mut s = Staged::begin(&p, attrs).unwrap();
            s.write(b"x").unwrap();
            s.commit().unwrap();
            assert_eq!(Local.stat(&p).unwrap().unwrap().mode, umasked, "{name}");
        }
    }

    /// A rewrite given only a mode keeps the old file's owner, and only the
    /// mode changes.
    #[test]
    fn a_rewrite_given_a_mode_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "v1").unwrap();
        Local.set_mode(&p, 0o644).unwrap();
        let before = Local.stat(&p).unwrap().unwrap();
        let mut s = Staged::begin(
            &p,
            Some(WriteAttrs {
                mode: Some(0o600),
                owner: None,
            }),
        )
        .unwrap();
        s.write(b"v2").unwrap();
        s.commit().unwrap();
        let after = Local.stat(&p).unwrap().unwrap();
        assert_eq!(
            (after.mode, after.uid, after.gid),
            (0o600, before.uid, before.gid)
        );
    }

    /// An owner asked for that the kernel refuses fails the write: the target
    /// keeps its content and mode, and no temporary file is left. Needs a
    /// runner that is not root, which may give a file to anyone.
    #[test]
    fn a_requested_owner_that_is_refused_leaves_the_target_untouched() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, "before").unwrap();
        Local.set_mode(&p, 0o640).unwrap();
        let mut s = Staged::begin(
            &p,
            Some(WriteAttrs {
                mode: Some(0o600),
                owner: Some((0, 0)),
            }),
        )
        .unwrap();
        s.write(b"after").unwrap();
        let err = s.commit().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        assert!(
            err.to_string()
                .starts_with("could not give the new file owner 0:0 ("),
            "{err}"
        );
        assert_eq!(std::fs::read(&p).unwrap(), b"before");
        assert_eq!(Local.stat(&p).unwrap().unwrap().mode, 0o640);
        assert!(staged_in(dir.path()).is_empty());
    }

    /// A write dropped before its commit leaves the target as it was and
    /// nothing beside it, new file or rewrite.
    #[test]
    fn an_uncommitted_write_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        std::fs::write(&old, "before").unwrap();
        for p in [old.clone(), dir.path().join("new")] {
            let mut s = Staged::begin(&p, None).unwrap();
            s.write(b"half").unwrap();
            drop(s);
            assert!(staged_in(dir.path()).is_empty());
        }
        assert_eq!(std::fs::read(&old).unwrap(), b"before");
        assert!(!dir.path().join("new").exists());
    }

    fn spec(program: &str, args: &[&str], stdin: Option<Vec<u8>>) -> CmdSpec {
        CmdSpec {
            program: program.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: Default::default(),
            cwd: None,
            stdin,
            prefix: vec![],
        }
    }

    #[test]
    fn stdin_reaches_the_child_and_a_large_input_does_not_deadlock() {
        // Bounded on purpose. The bug this pins is a deadlock, so an
        // unbounded assertion would hang instead of failing, and in CI it
        // would hold the job until the workflow timeout rather than telling
        // anyone what broke. The work runs on a thread and the test fails if
        // it has not finished in time.
        let done = within(Duration::from_secs(30), || {
            let out = Local
                .spawn(&spec("cat", &[], Some(b"hello stdin".to_vec())))
                .unwrap();
            assert_eq!(out.status, 0);
            assert_eq!(out.stdout, b"hello stdin");

            // Larger than any pipe buffer (64 KiB on Linux), echoed back in
            // full: the child writes while we are still feeding it.
            let big = vec![b'x'; 4 * 1024 * 1024];
            let out = Local.spawn(&spec("cat", &[], Some(big.clone()))).unwrap();
            assert_eq!(out.stdout.len(), big.len());

            // Without stdin the child sees EOF at once, not our terminal.
            let out = Local.spawn(&spec("cat", &[], None)).unwrap();
            assert_eq!(out.status, 0);
            assert!(out.stdout.is_empty());

            // A child that never reads its input still exits cleanly.
            let out = Local
                .spawn(&spec("true", &[], Some(vec![b'y'; 1024 * 1024])))
                .unwrap();
            assert_eq!(out.status, 0);
        });
        assert!(
            done,
            "spawn with stdin did not finish in 30s: the feeder is blocking \
             again, so a child that writes before reading deadlocks"
        );
    }

    /// Run `f` on a thread and report whether it finished within `limit`.
    /// A test that hangs tells nobody anything; a test that fails does.
    fn within(limit: Duration, f: impl FnOnce() + Send + 'static) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            f();
            let _ = tx.send(());
        });
        rx.recv_timeout(limit).is_ok()
    }
}
