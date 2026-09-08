use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::{Backend, CmdSpec, FileKind, Output, Stat};

/// The production backend: real filesystem, real processes.
pub struct Local;

impl Backend for Local {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(p)
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        let dir = p.parent().unwrap_or(Path::new("."));
        let existing = std::fs::metadata(p).ok();
        let mut tmp = tempfile::Builder::new()
            .prefix(".rustible-")
            .tempfile_in(dir)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        if let Some(meta) = existing {
            std::fs::set_permissions(tmp.path(), meta.permissions())?;
            // Best effort: only root can chown; ignore EPERM so unprivileged
            // rewrites of own files still work.
            let _ = std::os::unix::fs::chown(tmp.path(), Some(meta.uid()), Some(meta.gid()));
        }
        tmp.persist(p).map_err(|e| e.error)?;
        Ok(())
    }

    fn stat(&self, p: &Path) -> io::Result<Option<Stat>> {
        match std::fs::symlink_metadata(p) {
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

    fn mkdir_all(&self, p: &Path) -> io::Result<()> {
        std::fs::create_dir_all(p)
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        match std::fs::symlink_metadata(p) {
            Ok(m) if m.is_dir() => std::fs::remove_dir_all(p),
            Ok(_) => std::fs::remove_file(p),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()> {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
    }

    fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> io::Result<()> {
        std::os::unix::fs::chown(p, Some(uid), Some(gid))
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        std::fs::copy(from, to).map(|_| ())
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
        if let Some(input) = &spec.stdin {
            let mut stdin = child.stdin.take().expect("piped stdin");
            stdin.write_all(input)?;
            drop(stdin);
        }
        let out = child.wait_with_output()?;
        Ok(Output {
            status: out.status.code().unwrap_or(-1),
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
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
}
