use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Backend, CmdSpec, FileKind, Output, Stat};

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

/// Give the replacement file at `tmp` the owner and mode of the file it
/// replaces (`old`), setuid and setgid included.
///
/// Three steps, in this order, each for a reason:
///
/// 1. The mode **without** setuid and setgid, while this process still owns
///    the file: `chmod` of a file someone else owns takes `CAP_FOWNER`, which
///    a root without it (a container that drops it) lacks, so a `chmod`
///    after the `chown` alone would fail there. Leaving the two bits out
///    means the replacement is never setuid to the writer, even for a moment,
///    when it is about to belong to someone else.
/// 2. The owner, best effort: only root can give a file away, and an
///    unprivileged rewrite of another user's file goes on as before, the
///    file now the writer's.
/// 3. Setuid and setgid, when the old mode has them. On Linux every
///    successful `chown` of a non-directory clears setuid, and setgid when
///    group execute is set, even to the ids the file already has
///    (`[FAKE-CHOWN]` in `docs/plan/DECISIONS.md` has the measurements), so
///    they are set after it. Copying the whole mode before the `chown`
///    rewrote a 4755 file as 0755 and reported success (issue #51). This
///    `chmod` fails only for a root without `CAP_FOWNER` whose `chown`
///    succeeded, and then the write fails saying so, rather than leaving
///    the file without its bits.
///
/// Then the mode the replacement ended up with is read back. A `chmod`
/// that asks for setgid does not fail when the caller is neither in the
/// file's group nor holds `CAP_FSETID`: the kernel drops the bit and
/// reports success (a root without `CAP_FSETID` rewriting a `2755` file
/// another group owns got `0755`). That too fails the write, before the
/// rename, so the old file stays as it was.
///
/// Step 1 sets the old group bits while the file still has the writer's
/// group, so for that moment members of the writer's group could open the
/// new content as the old file's group could. That is accepted rather than
/// masked: masking them means a `chmod` after the `chown` for nearly every
/// rewrite, which is exactly what a root without `CAP_FOWNER` cannot do.
/// For root the writer's group is gid 0, already privileged; unprivileged,
/// it is the writer's own group, and the content is the writer's.
fn keep_owner_and_mode(tmp: &Path, old: &std::fs::Metadata) -> io::Result<()> {
    let mode = old.permissions().mode() & 0o7777;
    let special = mode & 0o6000;
    std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode & !0o6000))?;
    let _ = std::os::unix::fs::chown(tmp, Some(old.uid()), Some(old.gid()));
    if special != 0 {
        std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(mode)).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "gave the replacement file owner {}:{} but could not set its mode back \
                     to {mode:04o} ({e}); setting setuid or setgid on a file this process \
                     does not own takes CAP_FOWNER",
                    old.uid(),
                    old.gid()
                ),
            )
        })?;
    }
    let got = std::fs::metadata(tmp)?;
    let got_mode = got.permissions().mode() & 0o7777;
    if got_mode != mode {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "set mode {mode:04o} on the replacement file but the kernel left {got_mode:04o} \
                 (group {}); setting setgid on a file whose group this process is not in \
                 takes CAP_FSETID. The file was not replaced",
                got.gid()
            ),
        ));
    }
    Ok(())
}

impl Backend for Local {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(p)
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        let dir = p.parent().unwrap_or(Path::new("."));
        // Followed: writing at a symlink's path takes the target's mode and
        // owner, and the rename then replaces the link itself.
        let existing = std::fs::metadata(p).ok();
        // A new file gets the mode any newly created file would (0666 minus
        // the umask); tempfile's own default is 0600, which is not what an
        // op that creates a config file expects. A rewrite starts at 0600
        // instead, so the new content is never readable by more than the
        // writer until the old file's own mode is copied onto it below.
        let create = if existing.is_some() { 0o600 } else { 0o666 };
        let mut tmp = tempfile::Builder::new()
            .prefix(".rustible-")
            .permissions(std::fs::Permissions::from_mode(create))
            .tempfile_in(dir)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        if let Some(meta) = existing {
            keep_owner_and_mode(tmp.path(), &meta)?;
        }
        tmp.persist(p).map_err(|e| e.error)?;
        Ok(())
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
    // stands in for `O_NOFOLLOW`, which `std` has no constant for. The copy
    // is created `0600`, so its content is never readable by anyone else
    // while it is being written, and gets its mode, without `0o7000`, by
    // `fchmod` once the data is in. `EEXIST` keeps its errno, so the
    // `Elevated` helper hands `AlreadyExists` back intact.
    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        let mut src = std::fs::File::open(from)?;
        let meta = src.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: not a regular file, so not copied", from.display()),
            ));
        }
        let mut dst = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(to)?;
        let mode = meta.permissions().mode() & 0o777;
        let filled = io::copy(&mut src, &mut dst)
            .and_then(|_| dst.set_permissions(std::fs::Permissions::from_mode(mode)));
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
        let feeder = spec.stdin.clone().map(|input| {
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
