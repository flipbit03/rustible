//! The `Elevated` backend (vision doc 11.3): every primitive is a request to
//! a helper process, which is this same binary started as another user in
//! `--helper` mode (`sudo -n -u <user> <exe> --helper`). The helper is a
//! `Local` backend wrapped in a request loop (`serve_helper`); frames are the
//! same length-prefixed JSON as the main channel. One helper per identity,
//! spawned on first use, kept for the run, closed on drop.
//!
//! ## What crosses, and what does not
//!
//! The escalation password is never in an argument vector: `/proc` makes
//! argv readable by every user on the host, so it goes to `sudo -S` on the
//! helper's stdin, as the first line, ahead of any frame. On a NOPASSWD host
//! it is not sent at all, because [`Spawner`] probes with
//! `sudo -n -u <user> true` first and only feeds the password when that
//! probe fails. It lives in a [`Secret`] the whole time, which zeroizes on
//! drop and prints its length rather than its bytes.
//!
//! No message this module produces quotes file contents.
//! [`HelperOp::label`] gives the primitive, its paths, and for a write the
//! byte count: the contents may be a secret (`ctx.local_secret`), and these
//! messages are rendered, logged and shipped to the orchestrator.
//!
//! Mutations are refused while the calling step is in its `check` phase, on
//! both sides: the main process guards before it builds the request, and
//! [`serve_helper`] refuses again after it arrives. The helper's copy is a
//! second latch against an op that reaches around the first one, not a
//! boundary against a hostile parent: it believes
//! [`HelperRequest::checking`], and a parent that lies gets its mutation.
//! That parent chose the helper's binary and its user, so it had the
//! authority already.
//!
//! Nothing here narrows what the helper may do. The far side is a full
//! [`Local`] backend running as the target user, so a request is bounded by
//! that user's permissions and by nothing else: there is no path allowlist
//! and no read-only mode, and `as_root` means root.
//!
//! A helper that dies is not replaced. The first failure is latched, because
//! retrying costs one `sudo` authentication per primitive: a syslog line and
//! mail to root each time, and `pam_faillock` locking the account out after
//! a handful.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Backend, CmdSpec, Local, Output, Stat};
use crate::protocol::{FrameTooLarge, MAX_FRAME_PAYLOAD, read_frame, write_frame};
use crate::secret::Secret;

/// One `Backend` primitive on the wire.
///
/// One variant per method of [`Backend`], carrying that method's arguments
/// and nothing more. There is deliberately no variant the trait does not
/// have: a helper offers the same surface as a local run, not a wider one.
/// Paths travel exactly as the op wrote them, resolved on the helper's side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HelperOp {
    /// [`Backend::read`], answered with [`HelperResponse::Bytes`]. The file
    /// crosses whole in one frame, so one larger than `MAX_FRAME_PAYLOAD`
    /// cannot be read through a helper at all.
    Read {
        /// Read by the helper's user, so a file the main process cannot open
        /// is still fine.
        path: PathBuf,
    },
    /// [`Backend::write`]. The only request that carries file contents, and
    /// the reason [`HelperOp::label`] prints sizes instead of payloads.
    Write {
        /// Written with `Local`'s temp file and rename, so the helper's user
        /// needs write permission on the parent directory, not just the file.
        path: PathBuf,
        /// The complete new contents. Base64 on the wire, and checked
        /// against `MAX_FRAME_PAYLOAD` before the frame is built, so an
        /// oversized write is refused by a message that names the file
        /// rather than by the framing code, which does not know it.
        #[serde(with = "crate::protocol::b64")]
        bytes: Vec<u8>,
    },
    /// [`Backend::stat`]: `lstat`, so a symlink reports itself.
    Stat {
        /// A path that is not there is not an error; the answer is
        /// [`HelperResponse::Stat`] carrying `None`.
        path: PathBuf,
    },
    /// [`Backend::stat_follow`]: follows the link and reports what it lands
    /// on.
    StatFollow {
        /// A dangling link answers `None`, exactly as a missing path does.
        path: PathBuf,
    },
    /// [`Backend::mkdir_all`].
    MkdirAll {
        /// The deepest directory. Every missing parent is created too, and
        /// all of them belong to the helper's user with the helper's umask.
        path: PathBuf,
    },
    /// [`Backend::remove`]: one entry, and a populated directory is an
    /// error.
    Remove {
        /// A path that is already gone succeeds, so removal is idempotent.
        path: PathBuf,
    },
    /// [`Backend::remove_all`]. The only recursive delete a helper will do,
    /// and so the one request where a wrong path costs a tree.
    RemoveAll {
        /// The root of what goes, followed by everything under it.
        path: PathBuf,
    },
    /// [`Backend::rename`]. Both ends are the helper's, and it is one
    /// syscall, so it cannot cross filesystems.
    Rename {
        /// Must exist.
        from: PathBuf,
        /// Replaced atomically if it exists.
        to: PathBuf,
    },
    /// [`Backend::set_mode`].
    SetMode {
        /// Followed if it is a symlink: the target's mode changes, not the
        /// link's.
        path: PathBuf,
        /// Permission bits as `chmod` takes them (`0o644`), not a whole
        /// `st_mode` with the file type in it.
        mode: u32,
    },
    /// [`Backend::set_owner`]. Handing a file to a third user needs the
    /// helper to be root; a helper running as an ordinary user can only fail
    /// this, which is why ops that chown ask for `as_root` and not just
    /// `as_user`.
    SetOwner {
        /// Followed if it is a symlink: `chown(2)`, not `lchown(2)`.
        path: PathBuf,
        /// Numeric. Names are resolved before the request is built, never by
        /// the helper.
        uid: u32,
        /// Numeric, likewise.
        gid: u32,
    },
    /// [`Backend::copy`]. Both ends are on the target host and the bytes
    /// never touch the wire, so copying a file as another user works at
    /// sizes at which reading it would be refused.
    Copy {
        /// Must exist and be readable by the helper's user.
        from: PathBuf,
        /// Created or truncated. Not atomic, so a reader can catch it half
        /// written.
        to: PathBuf,
    },
    /// [`Backend::symlink`]. Fails if `link` exists: there is no
    /// replace-in-place primitive, so an op that wants one removes first.
    Symlink {
        /// What the link will point at. Never resolved and never checked, so
        /// a link to something that does not exist yet is legal and is
        /// usually the point.
        target: PathBuf,
        /// The link to create.
        link: PathBuf,
    },
    /// [`Backend::read_link`].
    ReadLink {
        /// The link itself; nothing is followed. Failing rather than
        /// answering `None` is how a caller learns `path` is not a link.
        path: PathBuf,
    },
    /// [`Backend::read_dir`]. The whole listing comes back in one frame.
    ReadDir {
        /// The directory. The answer holds full paths of the direct
        /// children, sorted, not bare names and not the tree.
        path: PathBuf,
    },
    /// [`Backend::spawn`]. The command runs as the helper's user with no
    /// further `sudo`, so [`CmdSpec::prefix`] is empty on this path;
    /// `sys.cmd` only fills it in for a `Fake` system, which has no helper
    /// to be the user for it. [`CmdSpec::stdin`] rides in the request and
    /// counts against the frame limit.
    Spawn(CmdSpec),
}

impl HelperOp {
    /// The variant and the paths it touches, for messages. Never the bytes:
    /// a `Write` carries file contents, which may be a secret
    /// (`ctx.local_secret`) or 50 MB of them.
    pub fn label(&self) -> String {
        let one = |verb: &str, p: &Path| format!("{verb} {}", p.display());
        let two =
            |verb: &str, a: &Path, b: &Path| format!("{verb} {} -> {}", a.display(), b.display());
        match self {
            HelperOp::Read { path } => one("read", path),
            HelperOp::Write { path, bytes } => {
                format!("write {} ({} bytes)", path.display(), bytes.len())
            }
            HelperOp::Stat { path } => one("stat", path),
            HelperOp::StatFollow { path } => one("stat_follow", path),
            HelperOp::MkdirAll { path } => one("mkdir_all", path),
            HelperOp::Remove { path } => one("remove", path),
            HelperOp::RemoveAll { path } => one("remove_all", path),
            HelperOp::Rename { from, to } => two("rename", from, to),
            HelperOp::SetMode { path, mode } => {
                format!("set_mode {} {mode:o}", path.display())
            }
            HelperOp::SetOwner { path, uid, gid } => {
                format!("set_owner {} {uid}:{gid}", path.display())
            }
            HelperOp::Copy { from, to } => two("copy", from, to),
            HelperOp::Symlink { target, link } => two("symlink", link, target),
            HelperOp::ReadLink { path } => one("read_link", path),
            HelperOp::ReadDir { path } => one("read_dir", path),
            HelperOp::Spawn(spec) => format!("spawn {}", spec.argv().join(" ")),
        }
    }

    /// The file bytes this op would put in a single frame, with the path
    /// they belong to. `None` for ops whose request carries no payload.
    ///
    /// Every primitive is one request and one response, so a file crosses
    /// the helper boundary whole. Bytes travel as base64, so the ceiling is
    /// `MAX_FRAME_PAYLOAD`, well under the size of artifacts people copy.
    /// Streaming the primitives would lift it; until then the limit is
    /// reported up front rather than discovered inside the framing.
    pub fn payload(&self) -> Option<(&Path, usize)> {
        match self {
            HelperOp::Write { path, bytes } => Some((path, bytes.len())),
            HelperOp::Spawn(spec) => spec
                .stdin
                .as_ref()
                .map(|b| (Path::new(&spec.program), b.len())),
            _ => None,
        }
    }

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
///
/// One request, one response, in lockstep down one pipe: [`Elevated`] holds
/// the connection lock across both halves, so a helper never has two of
/// these in flight and never has to correlate them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelperRequest {
    /// True while the requesting step is in `check`; mutations are refused.
    pub checking: bool,
    /// The primitive to perform. Nothing else is negotiated: there is no
    /// session, no state kept between requests, and no way to ask a helper
    /// for something [`Backend`] does not have.
    pub op: HelperOp,
}

/// Helper -> main process.
///
/// The request decides the shape: [`Elevated`] knows which variant each
/// primitive should come back as and turns anything else into
/// `unexpected helper response`, so a helper built from different source is
/// a failed step rather than a misread value. [`Err`](Self::Err) can answer
/// any request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum HelperResponse {
    /// The primitive succeeded and had nothing to return: the writes, the
    /// removals, the rename, the mode and owner changes, the copy, the
    /// symlink.
    Unit,
    /// A whole file, from [`HelperOp::Read`]. Base64 on the wire; a file too
    /// large for a frame fails inside the framing, and [`Elevated`] rewrites
    /// that failure to name the file and the identity.
    Bytes(#[serde(with = "crate::protocol::b64")] Vec<u8>),
    /// From [`HelperOp::Stat`] or [`HelperOp::StatFollow`]. `None` means the
    /// path is not there, which is an answer and not a failure.
    Stat(Option<Stat>),
    /// A link target, from [`HelperOp::ReadLink`].
    Path(PathBuf),
    /// The direct children of a directory, from [`HelperOp::ReadDir`].
    Paths(Vec<PathBuf>),
    /// A finished command, from [`HelperOp::Spawn`]. A non-zero exit arrives
    /// here and not in [`Err`](Self::Err): the command ran, and what it did
    /// is the op's business.
    Output(Output),
    /// The primitive failed, or the helper refused it. Carries no payload,
    /// so nothing the op was writing can come back inside an error message.
    Err {
        /// The OS errno when there was one, so `NotFound` and friends survive.
        code: Option<i32>,
        /// The `Display` of the helper's `io::Error`, or the refusal text
        /// [`serve_helper`] wrote. Rebuilt into an `io::Error` on the main
        /// side, so this string is what the playbook eventually prints.
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
///
/// A failed primitive is a [`HelperResponse::Err`] frame, not a return: the
/// loop ends only when the far end closes the pipe, and the `io::Result`
/// reports a broken channel rather than anything a playbook asked for. A
/// request whose [`checking`](HelperRequest::checking) flag is set and whose
/// op [`mutates`](HelperOp::mutates) is refused before it reaches the
/// filesystem, and the refusal quotes [`HelperOp::label`], never a payload.
///
/// `tx` is the frame stream and nothing else may write to it. Under
/// `--helper` that is the process's stdout, which is why the helper's own
/// diagnostics go to stderr.
pub fn serve_helper<R: Read, W: Write>(rx: &mut R, tx: &mut W) -> io::Result<()> {
    let local = Local;
    while let Some(req) = read_frame::<_, HelperRequest>(rx)? {
        let resp = if req.checking && req.op.mutates() {
            HelperResponse::Err {
                code: None,
                message: format!(
                    "mutation during check refused by helper: {}",
                    req.op.label()
                ),
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
///
/// The vector is safe to print, and error messages do print it: it holds the
/// method, the user, the binary and `--helper`, and never the password,
/// which in argv would be readable by every user on the host through
/// `/proc/<pid>/cmdline`.
///
/// Fails for `doas` with a password, since it has no way to read one from a
/// pipe; for `none`, which is how the inventory says this host does not
/// escalate; and for any other method name, quoted back.
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
    /// `sudo`, `doas` or `none`, from the inventory's `escalate` parameter.
    /// Anything else is not a fallback to `sudo`; it fails at spawn with the
    /// name quoted.
    pub method: String,
    /// The binary to re-exec in `--helper` mode, normally
    /// `std::env::current_exe()`. The helper is this same playbook binary,
    /// so escalation installs nothing on the target.
    pub exe: PathBuf,
    /// The escalation password, when the host needs one. `None` means
    /// passwordless escalation only, and a host that then asks for one gets
    /// a failed step, not a prompt: there is no terminal to prompt on. When
    /// present it reaches `sudo -S` on the helper's stdin and never argv,
    /// and only after `sudo -n` has been seen to fail.
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
///
/// One per identity per run, made by
/// [`System::as_user`](crate::system::System::as_user) and shared by every
/// clone that asks for the same user. The helper is spawned on the first
/// primitive rather than at construction, so naming a user the playbook
/// never reaches costs nothing and `sudo` is never asked about an identity
/// nobody used.
///
/// It narrows nothing. The far side is a full [`Local`] running as that
/// user, so a request is bounded by that user's permissions and by nothing
/// this type adds. What it does add is three refusals: a mutation while the
/// step is checking, a payload larger than one frame, and every primitive
/// after the first failure.
pub struct Elevated {
    user: String,
    spawner: Option<Spawner>,
    /// The owning `System`'s phase cell; `Checking` travels with each request.
    phase: Arc<AtomicU8>,
    conn: Mutex<Option<Connection>>,
    /// The first failure, latched. Once the helper has died, every later
    /// primitive reports this instead of spawning another one: a wrong
    /// password would otherwise mean one `sudo` authentication attempt per
    /// file operation, which is a syslog line and mail to root each time
    /// and `pam_faillock` locking the account out after a handful.
    failed: Mutex<Option<String>>,
}

impl Elevated {
    /// Spawns the helper on first use, so this itself runs no `sudo` and
    /// cannot fail.
    ///
    /// `phase` is the owning [`System`](crate::system::System)'s phase cell,
    /// shared rather than copied: the flag on each request reflects where
    /// the run is at the moment the request is made, so one long-lived
    /// helper is guarded correctly across many steps.
    pub fn new(user: impl Into<String>, spawner: Spawner, phase: Arc<AtomicU8>) -> Self {
        Elevated {
            user: user.into(),
            spawner: Some(spawner),
            phase,
            conn: Mutex::new(None),
            failed: Mutex::new(None),
        }
    }

    /// Talk to an already running helper over any pair of streams (tests).
    ///
    /// There is no [`Spawner`], so a connection that dies is not replaced:
    /// the next primitive reports `helper connection is closed` instead of
    /// starting a process the caller never asked for.
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
            failed: Mutex::new(None),
        }
    }

    /// The user the helper runs as, as messages name it. Never the calling
    /// process's own user:
    /// [`System::as_user`](crate::system::System::as_user) hands back the
    /// plain local backend for that one instead of building this.
    pub fn user(&self) -> &str {
        &self.user
    }

    fn call(&self, op: HelperOp) -> io::Result<HelperResponse> {
        let checking = self.phase.load(Ordering::SeqCst) == crate::system::Phase::Checking as u8;
        if let Some(why) = self.failed.lock().unwrap().as_ref() {
            return Err(io::Error::other(format!(
                "the helper running as `{}` failed earlier and is not retried: {why}",
                self.user
            )));
        }
        if let Some((path, len)) = op.payload()
            && len > MAX_FRAME_PAYLOAD
        {
            return Err(io::Error::other(format!(
                "{}: {len} bytes is more than one helper frame can carry ({} bytes); \
                 running as `{}` sends the whole file in one request, so a file this \
                 large has to be handled without `as_user`/`as_root`",
                path.display(),
                MAX_FRAME_PAYLOAD,
                self.user
            )));
        }
        let mut guard = self.conn.lock().unwrap();
        if guard.is_none() {
            let spawner = self
                .spawner
                .as_ref()
                .ok_or_else(|| io::Error::other("helper connection is closed"))?;
            *guard = Some(self.latch(spawner.spawn(&self.user))?);
        }
        let conn = guard.as_mut().expect("connected");
        let label = op.label();
        let req = HelperRequest { checking, op };
        let resp = conn.call(&req).map_err(|e| {
            // The typed signal, not the message: `protocol.rs` is free to
            // reword the framing error without silently degrading this
            // refusal back into it.
            if e.get_ref()
                .and_then(|inner| inner.downcast_ref::<FrameTooLarge>())
                .is_some()
            {
                io::Error::other(format!(
                    "{label}: the helper's answer is larger than one frame can carry \
                     ({MAX_FRAME_PAYLOAD} bytes of payload); reading a file this large \
                     as `{}` is not supported, do it without `as_user`/`as_root`",
                    self.user
                ))
            } else {
                e
            }
        });
        if resp.is_err() {
            // A dead helper is neither reused nor replaced: the connection
            // goes, and `failed` stops the next call from spawning a
            // successor that would fail exactly the same way.
            if let Some(c) = guard.take() {
                c.shutdown();
            }
        }
        self.latch(resp)?.into_io()
    }

    /// Remember the first failure so later calls report it instead of
    /// spawning another helper.
    fn latch<T>(&self, r: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &r {
            let mut f = self.failed.lock().unwrap();
            if f.is_none() {
                *f = Some(e.to_string());
            }
        }
        r
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
        let err = e.write(&f, b"hunter2-the-secret").unwrap_err();
        assert!(err.to_string().contains("mutation during check"), "{err}");
        // The message names the write and its size, never the bytes: a write
        // can carry a secret, and this message is rendered and logged.
        assert!(err.to_string().contains("18 bytes"), "{err}");
        assert!(!err.to_string().contains("hunter2"), "{err}");
        assert!(
            !err.to_string().contains("104"),
            "byte values leaked: {err}"
        );
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

    /// A command run through the helper with a large stdin, against a child
    /// that fills its stdout pipe before reading a byte of its input. The
    /// helper executes through `Local`, so this pins the escalated path to
    /// `Local::spawn`'s threaded feeder: with a blocking inline write the
    /// two would wait on each other forever. Bounded by a channel timeout so
    /// a regression fails the test instead of hanging the suite.
    #[test]
    fn a_command_through_the_helper_does_not_deadlock_on_large_stdin() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let phase = Arc::new(AtomicU8::new(Phase::Applying as u8));
            let e = in_process(phase);
            // 1 MiB in, and the child writes 1 MiB out before it reads.
            let input = vec![b'x'; 1024 * 1024];
            let spec = CmdSpec {
                program: "sh".into(),
                args: vec!["-c".into(), "yes hello | head -c 1048576; wc -c".into()],
                env: Default::default(),
                cwd: None,
                stdin: Some(input),
                prefix: vec![],
            };
            let out = e.spawn(&spec);
            let _ = done_tx.send(out);
        });
        let out = done_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the helper deadlocked writing stdin to a chatty child")
            .expect("spawn through the helper");
        assert_eq!(out.status, 0);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.ends_with("1048576\n"), "{}", &text[text.len() - 40..]);
    }

    #[test]
    fn an_oversized_write_is_refused_before_it_reaches_the_wire() {
        let phase = Arc::new(AtomicU8::new(Phase::Applying as u8));
        let e = in_process(phase);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("big");
        // One byte over what a frame can carry. The refusal has to name the
        // file, both numbers and the identity, because the failure it
        // replaces was "frame of N bytes exceeds limit" from inside the
        // framing, which named none of them.
        let too_big = vec![0u8; MAX_FRAME_PAYLOAD + 1];
        let err = e.write(&f, &too_big).unwrap_err().to_string();
        assert!(err.contains(&f.display().to_string()), "{err}");
        assert!(err.contains(&(MAX_FRAME_PAYLOAD + 1).to_string()), "{err}");
        assert!(err.contains(&MAX_FRAME_PAYLOAD.to_string()), "{err}");
        assert!(err.contains("tester"), "{err}");
        assert!(!f.exists(), "the write reached the helper anyway");

        // The helper is still usable: this is a refusal, not a failure.
        e.write(&f, b"small").unwrap();
        assert_eq!(e.read(&f).unwrap(), b"small");
    }

    /// A response too large for a frame gets the friendly refusal, which
    /// names the file, the limit and the identity, rather than the framing
    /// error. The detection is on the typed
    /// [`FrameTooLarge`](crate::protocol::FrameTooLarge) signal, so the
    /// wording of the framing error is free to change.
    #[test]
    fn an_oversized_response_gets_the_friendly_refusal() {
        // A "helper" that answers every request with a length prefix one
        // byte over the frame ceiling. Nothing else has to be there: the
        // read fails on the prefix, before a body is allocated.
        let prefix = u32::try_from(crate::protocol::MAX_FRAME + 1).unwrap();
        let e = Elevated::connected(
            "tester",
            Box::new(io::sink()),
            Box::new(io::Cursor::new(prefix.to_be_bytes().to_vec())),
            Arc::new(AtomicU8::new(Phase::Applying as u8)),
        );
        let err = e.read(Path::new("/etc/shadow")).unwrap_err().to_string();
        assert!(err.contains("/etc/shadow"), "{err}");
        assert!(err.contains(&MAX_FRAME_PAYLOAD.to_string()), "{err}");
        assert!(err.contains("tester"), "{err}");
        assert!(err.contains("as_user"), "{err}");
        assert!(!err.contains("exceeds limit"), "raw framing error: {err}");
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
            failed: Mutex::new(None),
        };
        reader.join().unwrap();
        let err = e.read(Path::new("/etc/hostname")).unwrap_err().to_string();
        assert!(err.contains("exited 7") && err.contains("boom"), "{err}");

        // The failure is latched: the next primitive reports it rather than
        // spawning a successor. A respawn would try `/nonexistent/rustible-bin`
        // and say so, which is how this tells the two apart.
        let again = e.stat(Path::new("/etc/hostname")).unwrap_err().to_string();
        assert!(
            again.contains("failed earlier and is not retried"),
            "{again}"
        );
        assert!(again.contains("exited 7"), "{again}");
        assert!(!again.contains("/nonexistent/rustible-bin"), "{again}");
    }
}
