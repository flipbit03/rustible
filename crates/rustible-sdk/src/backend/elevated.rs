//! The `Elevated` backend (vision doc 11.3): every primitive is a request to
//! a helper process, which is this same binary started as another user in
//! `--helper` mode (`sudo -n -u <user> <exe> --helper`). The helper is a
//! `Local` backend wrapped in a request loop (`serve_helper`); frames are the
//! same length-prefixed JSON as the main channel. One helper per identity,
//! spawned on first use, kept for the run, closed on drop.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Backend, CmdSpec, Local, Output, Stat};
use crate::protocol::{read_frame, write_frame};
use crate::secret::Secret;

/// One `Backend` primitive on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HelperOp {
    Read {
        path: PathBuf,
    },
    Write {
        path: PathBuf,
        #[serde(with = "crate::protocol::b64")]
        bytes: Vec<u8>,
    },
    Stat {
        path: PathBuf,
    },
    StatFollow {
        path: PathBuf,
    },
    MkdirAll {
        path: PathBuf,
    },
    Remove {
        path: PathBuf,
    },
    RemoveAll {
        path: PathBuf,
    },
    Rename {
        from: PathBuf,
        to: PathBuf,
    },
    SetMode {
        path: PathBuf,
        mode: u32,
    },
    SetOwner {
        path: PathBuf,
        uid: u32,
        gid: u32,
    },
    Copy {
        from: PathBuf,
        to: PathBuf,
    },
    Symlink {
        target: PathBuf,
        link: PathBuf,
    },
    ReadLink {
        path: PathBuf,
    },
    ReadDir {
        path: PathBuf,
    },
    Spawn(CmdSpec),
}

impl HelperOp {
    /// Mutations are refused by the helper while the main process is in a
    /// step's `check` phase: the guard holds on both sides (vision doc 11.3).
    pub fn mutates(&self) -> bool {
        !matches!(
            self,
            HelperOp::Read { .. }
                | HelperOp::Stat { .. }
                | HelperOp::StatFollow { .. }
                | HelperOp::ReadLink { .. }
                | HelperOp::ReadDir { .. }
                | HelperOp::Spawn(_)
        )
    }
}

/// Main process -> helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperRequest {
    /// True while the requesting step is in `check`; mutations are refused.
    pub checking: bool,
    pub op: HelperOp,
}

/// Helper -> main process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HelperResponse {
    Unit,
    Bytes(#[serde(with = "crate::protocol::b64")] Vec<u8>),
    Stat(Option<Stat>),
    Path(PathBuf),
    Paths(Vec<PathBuf>),
    Output(Output),
    Err {
        /// The OS errno when there was one, so `NotFound` and friends survive.
        code: Option<i32>,
        message: String,
    },
}

impl HelperResponse {
    fn from_io(e: io::Error) -> Self {
        HelperResponse::Err {
            code: e.raw_os_error(),
            message: e.to_string(),
        }
    }

    fn into_io(self) -> io::Result<HelperResponse> {
        match self {
            HelperResponse::Err { code, message } => Err(match code {
                Some(c) => io::Error::new(io::Error::from_raw_os_error(c).kind(), message),
                None => io::Error::other(message),
            }),
            other => Ok(other),
        }
    }
}

/// Serve requests from `rx` on a `Local` backend until EOF. This is what
/// `--helper` runs; tests run it on a pipe in a thread.
pub fn serve_helper<R: Read, W: Write>(rx: &mut R, tx: &mut W) -> io::Result<()> {
    let local = Local;
    while let Some(req) = read_frame::<_, HelperRequest>(rx)? {
        let resp = if req.checking && req.op.mutates() {
            HelperResponse::Err {
                code: None,
                message: format!("mutation during check refused by helper: {:?}", req.op),
            }
        } else {
            match req.op {
                HelperOp::Read { path } => local
                    .read(&path)
                    .map(HelperResponse::Bytes)
                    .unwrap_or_else(HelperResponse::from_io),
                HelperOp::Write { path, bytes } => unit(local.write(&path, &bytes)),
                HelperOp::Stat { path } => local
                    .stat(&path)
                    .map(HelperResponse::Stat)
                    .unwrap_or_else(HelperResponse::from_io),
                HelperOp::StatFollow { path } => local
                    .stat_follow(&path)
                    .map(HelperResponse::Stat)
                    .unwrap_or_else(HelperResponse::from_io),
                HelperOp::MkdirAll { path } => unit(local.mkdir_all(&path)),
                HelperOp::Remove { path } => unit(local.remove(&path)),
                HelperOp::RemoveAll { path } => unit(local.remove_all(&path)),
                HelperOp::Rename { from, to } => unit(local.rename(&from, &to)),
                HelperOp::SetMode { path, mode } => unit(local.set_mode(&path, mode)),
                HelperOp::SetOwner { path, uid, gid } => unit(local.set_owner(&path, uid, gid)),
                HelperOp::Copy { from, to } => unit(local.copy(&from, &to)),
                HelperOp::Symlink { target, link } => unit(local.symlink(&target, &link)),
                HelperOp::ReadLink { path } => local
                    .read_link(&path)
                    .map(HelperResponse::Path)
                    .unwrap_or_else(HelperResponse::from_io),
                HelperOp::ReadDir { path } => local
                    .read_dir(&path)
                    .map(HelperResponse::Paths)
                    .unwrap_or_else(HelperResponse::from_io),
                HelperOp::Spawn(spec) => local
                    .spawn(&spec)
                    .map(HelperResponse::Output)
                    .unwrap_or_else(HelperResponse::from_io),
            }
        };
        write_frame(tx, &resp)?;
    }
    Ok(())
}

fn unit(r: io::Result<()>) -> HelperResponse {
    r.map(|()| HelperResponse::Unit)
        .unwrap_or_else(HelperResponse::from_io)
}

/// The command line that starts a helper as `user` through `method`
/// (`sudo` or `doas`, the inventory's `escalate` parameter). With a
/// password, sudo reads it from stdin (`-S`); doas cannot, and `none` means
/// the host forbids escalation.
pub fn helper_argv(
    method: &str,
    user: &str,
    exe: &Path,
    with_password: bool,
) -> io::Result<Vec<String>> {
    let exe = exe.to_string_lossy().into_owned();
    let tail = [exe, "--helper".to_string()];
    let argv: Vec<String> = match (method, with_password) {
        ("sudo", false) => ["sudo", "-n", "-u", user].map(String::from).to_vec(),
        ("sudo", true) => ["sudo", "-S", "-p", "", "-u", user]
            .map(String::from)
            .to_vec(),
        ("doas", false) => ["doas", "-n", "-u", user].map(String::from).to_vec(),
        ("doas", true) => {
            return Err(io::Error::other(
                "doas cannot take a password from a pipe; configure doas for passwordless use",
            ));
        }
        ("none", _) => {
            return Err(io::Error::other(format!(
                "cannot run as `{user}`: this host's escalation method is `none`"
            )));
        }
        (other, _) => {
            return Err(io::Error::other(format!(
                "unknown escalation method `{other}` (expected sudo, doas, or none)"
            )));
        }
    };
    Ok(argv.into_iter().chain(tail).collect())
}

/// How to start a helper: which binary, through which method, with which
/// password. Shared by every identity of a run.
#[derive(Clone)]
pub struct Spawner {
    pub method: String,
    pub exe: PathBuf,
    pub password: Option<Secret>,
}

impl Spawner {
    /// `sudo -n` succeeds without a password on NOPASSWD hosts; only when it
    /// does not is the password fed through `-S`. Probing first keeps the
    /// password line off the request pipe when sudo would not consume it.
    fn needs_password(&self, user: &str) -> bool {
        if self.password.is_none() || self.method != "sudo" {
            return false;
        }
        let probe = Command::new("sudo")
            .args(["-n", "-u", user, "true"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        !probe.map(|s| s.success()).unwrap_or(false)
    }

    fn spawn(&self, user: &str) -> io::Result<Connection> {
        let with_password = self.needs_password(user);
        let argv = helper_argv(&self.method, user, &self.exe, with_password)?;
        let mut child = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| io::Error::new(e.kind(), format!("spawning `{}`: {e}", argv.join(" "))))?;
        let mut tx: Box<dyn Write + Send> = Box::new(child.stdin.take().expect("piped"));
        let rx: Box<dyn Read + Send> = Box::new(child.stdout.take().expect("piped"));
        let stderr = child.stderr.take().expect("piped");
        let tail: Arc<Mutex<Vec<String>>> = Arc::default();
        let tail_w = tail.clone();
        let label = format!("helper as {user}");
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[{label}] {line}");
                let mut t = tail_w.lock().unwrap();
                if t.len() >= 5 {
                    t.remove(0);
                }
                t.push(line);
            }
        });
        if with_password && let Some(pw) = &self.password {
            tx.write_all(pw.as_bytes())?;
            tx.write_all(b"\n")?;
            tx.flush()?;
        }
        Ok(Connection {
            tx,
            rx,
            child: Some(child),
            stderr_tail: tail,
        })
    }
}

struct Connection {
    tx: Box<dyn Write + Send>,
    rx: Box<dyn Read + Send>,
    child: Option<Child>,
    stderr_tail: Arc<Mutex<Vec<String>>>,
}

impl Connection {
    fn call(&mut self, req: &HelperRequest) -> io::Result<HelperResponse> {
        let answer = write_frame(&mut self.tx, req)
            .and_then(|()| read_frame::<_, HelperResponse>(&mut self.rx));
        match answer {
            Ok(Some(resp)) => Ok(resp),
            // EOF or a broken pipe: the helper is gone (sudo refused, it
            // crashed, or it was killed); say how it went.
            Ok(None) => Err(io::Error::other(self.exit_description())),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
                ) =>
            {
                Err(io::Error::other(format!(
                    "{} ({e})",
                    self.exit_description()
                )))
            }
            Err(e) => Err(e),
        }
    }

    fn exit_description(&mut self) -> String {
        let status = match &mut self.child {
            Some(c) => match c.wait() {
                Ok(s) => format!("exited {}", s.code().unwrap_or(-1)),
                Err(e) => format!("wait failed: {e}"),
            },
            None => "closed the connection".to_string(),
        };
        let tail = self.stderr_tail.lock().unwrap().join(" / ");
        if tail.is_empty() {
            format!("helper {status}")
        } else {
            format!("helper {status}: {tail}")
        }
    }

    /// Close stdin so the helper's loop ends, wait briefly, then kill.
    fn shutdown(mut self) {
        self.tx = Box::new(io::sink());
        if let Some(mut child) = self.child.take() {
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(2) {
                if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A `Backend` that runs as another user by proxying to a helper process.
pub struct Elevated {
    user: String,
    spawner: Option<Spawner>,
    /// The owning `System`'s phase cell; `Checking` travels with each request.
    phase: Arc<AtomicU8>,
    conn: Mutex<Option<Connection>>,
}

impl Elevated {
    /// Spawns the helper on first use.
    pub fn new(user: impl Into<String>, spawner: Spawner, phase: Arc<AtomicU8>) -> Self {
        Elevated {
            user: user.into(),
            spawner: Some(spawner),
            phase,
            conn: Mutex::new(None),
        }
    }

    /// Talk to an already running helper over any pair of streams (tests).
    pub fn connected(
        user: impl Into<String>,
        tx: Box<dyn Write + Send>,
        rx: Box<dyn Read + Send>,
        phase: Arc<AtomicU8>,
    ) -> Self {
        Elevated {
            user: user.into(),
            spawner: None,
            phase,
            conn: Mutex::new(Some(Connection {
                tx,
                rx,
                child: None,
                stderr_tail: Arc::default(),
            })),
        }
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    fn call(&self, op: HelperOp) -> io::Result<HelperResponse> {
        let checking = self.phase.load(Ordering::SeqCst) == crate::system::Phase::Checking as u8;
        let mut guard = self.conn.lock().unwrap();
        if guard.is_none() {
            let spawner = self
                .spawner
                .as_ref()
                .ok_or_else(|| io::Error::other("helper connection is closed"))?;
            *guard = Some(spawner.spawn(&self.user)?);
        }
        let conn = guard.as_mut().expect("connected");
        let req = HelperRequest { checking, op };
        let resp = conn.call(&req);
        if resp.is_err() {
            // A dead helper is not reused; the next call reports the failure again.
            if let Some(c) = guard.take() {
                c.shutdown();
            }
        }
        resp.and_then(|r| r.into_io())
    }

    fn expect_unit(&self, op: HelperOp) -> io::Result<()> {
        match self.call(op)? {
            HelperResponse::Unit => Ok(()),
            other => Err(unexpected(other)),
        }
    }
}

fn unexpected(r: HelperResponse) -> io::Error {
    io::Error::other(format!("unexpected helper response {r:?}"))
}

impl Drop for Elevated {
    fn drop(&mut self) {
        if let Ok(mut g) = self.conn.lock()
            && let Some(c) = g.take()
        {
            c.shutdown();
        }
    }
}

impl Backend for Elevated {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        match self.call(HelperOp::Read { path: p.into() })? {
            HelperResponse::Bytes(b) => Ok(b),
            other => Err(unexpected(other)),
        }
    }

    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
        self.expect_unit(HelperOp::Write {
            path: p.into(),
            bytes: bytes.to_vec(),
        })
    }

    fn stat(&self, p: &Path) -> io::Result<Option<Stat>> {
        match self.call(HelperOp::Stat { path: p.into() })? {
            HelperResponse::Stat(s) => Ok(s),
            other => Err(unexpected(other)),
        }
    }

    fn stat_follow(&self, p: &Path) -> io::Result<Option<Stat>> {
        match self.call(HelperOp::StatFollow { path: p.into() })? {
            HelperResponse::Stat(s) => Ok(s),
            other => Err(unexpected(other)),
        }
    }

    fn mkdir_all(&self, p: &Path) -> io::Result<()> {
        self.expect_unit(HelperOp::MkdirAll { path: p.into() })
    }

    fn remove(&self, p: &Path) -> io::Result<()> {
        self.expect_unit(HelperOp::Remove { path: p.into() })
    }

    fn remove_all(&self, p: &Path) -> io::Result<()> {
        self.expect_unit(HelperOp::RemoveAll { path: p.into() })
    }

    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.expect_unit(HelperOp::Rename {
            from: from.into(),
            to: to.into(),
        })
    }

    fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()> {
        self.expect_unit(HelperOp::SetMode {
            path: p.into(),
            mode,
        })
    }

    fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> io::Result<()> {
        self.expect_unit(HelperOp::SetOwner {
            path: p.into(),
            uid,
            gid,
        })
    }

    fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.expect_unit(HelperOp::Copy {
            from: from.into(),
            to: to.into(),
        })
    }

    fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
        self.expect_unit(HelperOp::Symlink {
            target: target.into(),
            link: link.into(),
        })
    }

    fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
        match self.call(HelperOp::ReadLink { path: p.into() })? {
            HelperResponse::Path(p) => Ok(p),
            other => Err(unexpected(other)),
        }
    }

    fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
        match self.call(HelperOp::ReadDir { path: p.into() })? {
            HelperResponse::Paths(v) => Ok(v),
            other => Err(unexpected(other)),
        }
    }

    fn spawn(&self, spec: &CmdSpec) -> io::Result<Output> {
        match self.call(HelperOp::Spawn(spec.clone()))? {
            HelperResponse::Output(o) => Ok(o),
            other => Err(unexpected(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::FileKind;
    use crate::system::Phase;
    use std::collections::BTreeMap;

    /// An in-process helper: `serve_helper` on a thread, joined by two pipes.
    fn in_process(phase: Arc<AtomicU8>) -> Elevated {
        let (req_r, req_w) = io::pipe().unwrap();
        let (resp_r, resp_w) = io::pipe().unwrap();
        std::thread::spawn(move || {
            let (mut rx, mut tx) = (req_r, resp_w);
            serve_helper(&mut rx, &mut tx).unwrap();
        });
        Elevated::connected("tester", Box::new(req_w), Box::new(resp_r), phase)
    }

    #[test]
    fn primitives_round_trip_through_the_helper_loop() {
        let phase = Arc::new(AtomicU8::new(Phase::Idle as u8));
        let e = in_process(phase.clone());
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("sub/x.txt");

        assert_eq!(e.stat(&f).unwrap(), None);
        e.mkdir_all(f.parent().unwrap()).unwrap();
        let payload: Vec<u8> = (0..=255u8).collect();
        e.write(&f, &payload).unwrap();
        assert_eq!(e.read(&f).unwrap(), payload);
        e.set_mode(&f, 0o600).unwrap();
        let st = e.stat(&f).unwrap().unwrap();
        assert_eq!((st.mode, st.size), (0o600, 256));
        e.copy(&f, &dir.path().join("y")).unwrap();
        assert_eq!(e.read(&dir.path().join("y")).unwrap(), payload);
        e.remove(&f).unwrap();
        assert_eq!(e.stat(&f).unwrap(), None);

        // The M6 primitives: symlinks, listing, rename, recursive removal.
        let sub = dir.path().join("sub");
        let link = dir.path().join("link");
        e.symlink(&sub, &link).unwrap();
        assert_eq!(e.read_link(&link).unwrap(), sub);
        assert_eq!(e.stat(&link).unwrap().unwrap().kind, FileKind::Symlink);
        assert_eq!(e.stat_follow(&link).unwrap().unwrap().kind, FileKind::Dir);
        assert!(e.read_link(&sub).is_err(), "a directory is not a link");
        e.write(&sub.join("a"), b"a").unwrap();
        e.write(&sub.join("b"), b"b").unwrap();
        assert_eq!(
            e.read_dir(&sub).unwrap(),
            vec![sub.join("a"), sub.join("b")]
        );
        let moved = dir.path().join("moved");
        e.rename(&sub, &moved).unwrap();
        assert_eq!(e.stat(&sub).unwrap(), None);
        assert_eq!(e.read(&moved.join("b")).unwrap(), b"b");
        let full = e.remove(&moved).unwrap_err();
        assert_eq!(full.kind(), io::ErrorKind::DirectoryNotEmpty, "{full}");
        e.remove_all(&moved).unwrap();
        assert_eq!(e.stat(&moved).unwrap(), None);
        e.remove(&link).unwrap();

        let missing = e.read(&dir.path().join("nope")).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound, "{missing}");

        let out = e
            .spawn(&CmdSpec {
                program: "sh".into(),
                args: vec!["-c".into(), "cat; echo err >&2; exit 3".into()],
                env: BTreeMap::new(),
                cwd: None,
                stdin: Some(b"in\x00put".to_vec()),
                prefix: vec![],
            })
            .unwrap();
        assert_eq!(
            (out.status, &out.stdout[..], out.stderr_str().trim()),
            (3, &b"in\x00put"[..], "err")
        );
    }

    #[test]
    fn helper_refuses_mutations_while_checking() {
        let phase = Arc::new(AtomicU8::new(Phase::Checking as u8));
        let e = in_process(phase.clone());
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        let err = e.write(&f, b"x").unwrap_err();
        assert!(err.to_string().contains("mutation during check"), "{err}");
        assert!(!f.exists());
        // Reads and spawns are fine while checking.
        assert_eq!(e.stat(&f).unwrap(), None);
        phase.store(Phase::Applying as u8, Ordering::SeqCst);
        e.write(&f, b"x").unwrap();
        assert!(f.exists());
    }

    #[test]
    fn helper_argv_shapes() {
        let exe = Path::new("/tmp/bin");
        assert_eq!(
            helper_argv("sudo", "root", exe, false).unwrap(),
            ["sudo", "-n", "-u", "root", "/tmp/bin", "--helper"]
        );
        assert_eq!(
            helper_argv("sudo", "postgres", exe, true).unwrap(),
            [
                "sudo", "-S", "-p", "", "-u", "postgres", "/tmp/bin", "--helper"
            ]
        );
        assert_eq!(
            helper_argv("doas", "root", exe, false).unwrap(),
            ["doas", "-n", "-u", "root", "/tmp/bin", "--helper"]
        );
        assert!(helper_argv("doas", "root", exe, true).is_err());
        assert!(
            helper_argv("none", "root", exe, false)
                .unwrap_err()
                .to_string()
                .contains("none")
        );
        assert!(helper_argv("pkexec", "root", exe, false).is_err());
    }

    #[test]
    fn dead_helper_reports_exit_and_stderr() {
        // A "helper" that prints to stderr and exits without answering.
        let spawner = Spawner {
            method: "sudo".into(),
            exe: PathBuf::from("/nonexistent/rustible-bin"),
            password: None,
        };
        // Bypass `sudo` by connecting to a shell that quits immediately.
        let mut child = Command::new("sh")
            .args(["-c", "echo boom >&2; exit 7"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let tail: Arc<Mutex<Vec<String>>> = Arc::default();
        let stderr = child.stderr.take().unwrap();
        let tail_w = tail.clone();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                tail_w.lock().unwrap().push(line);
            }
        });
        let conn = Connection {
            tx: Box::new(child.stdin.take().unwrap()),
            rx: Box::new(child.stdout.take().unwrap()),
            child: Some(child),
            stderr_tail: tail,
        };
        let e = Elevated {
            user: "root".into(),
            spawner: Some(spawner),
            phase: Arc::new(AtomicU8::new(0)),
            conn: Mutex::new(Some(conn)),
        };
        reader.join().unwrap();
        let err = e.read(Path::new("/etc/hostname")).unwrap_err().to_string();
        assert!(err.contains("exited 7") && err.contains("boom"), "{err}");
    }
}
