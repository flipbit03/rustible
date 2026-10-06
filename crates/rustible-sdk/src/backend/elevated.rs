//! The `Elevated` backend (vision doc 11.3): every primitive is a request to
//! a helper process, which is this same binary started as another user in
//! `--helper` mode. The helper is a `Local` backend wrapped in a request
//! loop (`serve_helper`); frames are the same length-prefixed JSON as the
//! main channel. One helper per identity, spawned on first use, kept for the
//! run, closed on drop.
//!
//! Root runs the binary where it is (`sudo -n -u root <exe> --helper`).
//! Any other account usually cannot read it, so [`Spawner`] streams the
//! binary to that account's own cache, or a private per-run temp directory,
//! and starts the helper from there ([`launch`](crate::launch), #62).
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

use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Backend, CmdSpec, Local, Output, Stat};
use crate::launch::{self, Answer, Launch, Mode, Next};
use crate::protocol::{
    FrameTooLarge, MAX_FRAME, MAX_FRAME_PAYLOAD, encode_frame, read_frame, write_body, write_frame,
};
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
    /// cannot be read through a helper at all: [`serve_helper`] answers it
    /// with a refusal naming the file, its size and the account.
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
        /// A regular file (symlinks followed), readable by the helper's
        /// user. Anything else is refused before it is opened.
        from: PathBuf,
        /// Created new: an existing path, a symlink included, is refused
        /// with `AlreadyExists` and left as it was. Not atomic, but a
        /// failure part way removes what it created, so no half-written
        /// copy is left behind.
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
    /// [`Backend::read_dir`]. The whole listing comes back in one frame, so
    /// a directory whose listing does not fit is refused by
    /// [`serve_helper`], naming the directory and how many entries it has.
    ReadDir {
        /// The directory. The answer holds full paths of the direct
        /// children, sorted, not bare names and not the tree.
        path: PathBuf,
    },
    /// [`Backend::spawn`]. The command runs as the helper's user with no
    /// further `sudo`, so [`CmdSpec::prefix`] is empty on this path;
    /// `sys.cmd` only fills it in for a `Fake` system, which has no helper
    /// to be the user for it. [`CmdSpec::stdin`] rides in the request and
    /// counts against the frame limit, and the output comes back whole in
    /// one frame: [`serve_helper`] refuses output that does not fit,
    /// naming the command, and the helper stays usable.
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
    /// they belong to: for a command's stdin, the program. `None` for ops
    /// whose request carries no payload.
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
    /// A whole file, from [`HelperOp::Read`]. Base64 on the wire. A file too
    /// large for a frame never comes back as this: [`serve_helper`] sends
    /// an [`Err`](Self::Err) naming the file, its size and the account
    /// instead.
    Bytes(#[serde(with = "crate::protocol::b64")] Vec<u8>),
    /// From [`HelperOp::Stat`] or [`HelperOp::StatFollow`]. `None` means the
    /// path is not there, which is an answer and not a failure.
    Stat(Option<Stat>),
    /// A link target, from [`HelperOp::ReadLink`].
    Path(PathBuf),
    /// The direct children of a directory, from [`HelperOp::ReadDir`]. A
    /// listing too large for a frame is an [`Err`](Self::Err) instead.
    Paths(Vec<PathBuf>),
    /// A finished command, from [`HelperOp::Spawn`]. A non-zero exit arrives
    /// here and not in [`Err`](Self::Err): the command ran, and what it did
    /// is the op's business. Output too large for a frame is an
    /// [`Err`](Self::Err) instead.
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
/// No frame this writes is larger than [`MAX_FRAME`], the most the far end
/// reads. An answer that would be (a file, a command's output, a directory
/// listing) is replaced by a [`HelperResponse::Err`] naming the request, its
/// size and the account the helper runs as, so an oversized answer is a
/// refusal of that one request and the helper goes on serving. Sending it
/// would leave a frame on the pipe the far end cannot read past, which is
/// a dead helper for the rest of the run.
///
/// `tx` is the frame stream and nothing else may write to it. Under
/// `--helper` that is the process's stdout, which is why the helper's own
/// diagnostics go to stderr.
pub fn serve_helper<R: Read, W: Write>(rx: &mut R, tx: &mut W) -> io::Result<()> {
    serve(rx, tx, MAX_FRAME, account_name)
}

/// [`serve_helper`], with the frame limit and the account its refusals
/// name as parameters, so a test can refuse an answer without building a
/// frame of [`MAX_FRAME`] bytes, and name the account its `Elevated` was
/// given.
fn serve<R: Read, W: Write>(
    rx: &mut R,
    tx: &mut W,
    max_frame: usize,
    account: impl Fn() -> String,
) -> io::Result<()> {
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
            answer(&local, &req.op)
        };
        let body = match encode_frame(&resp, max_frame)? {
            Some(body) => body,
            None => {
                let refusal = HelperResponse::Err {
                    code: None,
                    message: too_large(&req.op, &resp, max_frame, &account()),
                };
                drop(resp);
                encode_frame(&refusal, max_frame)?.ok_or_else(|| {
                    io::Error::other(format!(
                        "a refusal does not fit in a {max_frame}-byte frame"
                    ))
                })?
            }
        };
        write_body(tx, &body)?;
    }
    Ok(())
}

/// Perform one primitive on `local` and wrap what it returned.
fn answer(local: &Local, op: &HelperOp) -> HelperResponse {
    match op {
        HelperOp::Read { path } => local
            .read(path)
            .map(HelperResponse::Bytes)
            .unwrap_or_else(HelperResponse::from_io),
        HelperOp::Write { path, bytes } => unit(local.write(path, bytes)),
        HelperOp::Stat { path } => local
            .stat(path)
            .map(HelperResponse::Stat)
            .unwrap_or_else(HelperResponse::from_io),
        HelperOp::StatFollow { path } => local
            .stat_follow(path)
            .map(HelperResponse::Stat)
            .unwrap_or_else(HelperResponse::from_io),
        HelperOp::MkdirAll { path } => unit(local.mkdir_all(path)),
        HelperOp::Remove { path } => unit(local.remove(path)),
        HelperOp::RemoveAll { path } => unit(local.remove_all(path)),
        HelperOp::Rename { from, to } => unit(local.rename(from, to)),
        HelperOp::SetMode { path, mode } => unit(local.set_mode(path, *mode)),
        HelperOp::SetOwner { path, uid, gid } => unit(local.set_owner(path, *uid, *gid)),
        HelperOp::Copy { from, to } => unit(local.copy(from, to)),
        HelperOp::Symlink { target, link } => unit(local.symlink(target, link)),
        HelperOp::ReadLink { path } => local
            .read_link(path)
            .map(HelperResponse::Path)
            .unwrap_or_else(HelperResponse::from_io),
        HelperOp::ReadDir { path } => local
            .read_dir(path)
            .map(HelperResponse::Paths)
            .unwrap_or_else(HelperResponse::from_io),
        HelperOp::Spawn(spec) => local
            .spawn(spec)
            .map(HelperResponse::Output)
            .unwrap_or_else(HelperResponse::from_io),
    }
}

/// What [`serve`] sends instead of `resp`, an answer to `op` larger than a
/// `max_frame`-byte frame: the request, the answer's size in the terms the
/// request asked for, the account, and what to do instead. Never a byte of
/// the answer itself. The request's label is cut to a few hundred bytes,
/// so an absurdly long argv or path cannot make the refusal too large in
/// its turn.
fn too_large(op: &HelperOp, resp: &HelperResponse, max_frame: usize, account: &str) -> String {
    let label = cut(&op.label(), 512);
    let frame = format!("more than one helper frame holds ({max_frame} bytes)");
    match resp {
        HelperResponse::Bytes(b) => format!(
            "{label} as `{account}`: the file is {} bytes, which base64-encoded is {frame}; \
             reading a file this large has to be done without `as_user`/`as_root`",
            b.len()
        ),
        HelperResponse::Output(o) => format!(
            "{label} as `{account}` wrote {} bytes to stdout and {} to stderr, which \
             base64-encoded is {frame}; redirect its output to a file in the command \
             (`sh -c '… > /path'`) and read that file",
            o.stdout.len(),
            o.stderr.len()
        ),
        HelperResponse::Paths(p) => format!(
            "{label} as `{account}`: the directory has {} entries, a listing {frame}; \
             listing a directory this large has to be done without `as_user`/`as_root`",
            p.len()
        ),
        _ => format!("{label} as `{account}`: the answer is {frame}"),
    }
}

/// `s`, or its first `max` bytes (backed off to a character boundary) and
/// an ellipsis.
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// The account this process runs as, as a helper's refusals name it: the
/// name `/etc/passwd` gives the effective uid, else `$USER`, which `sudo`
/// and `doas` set to the account they switch to (a mac's `/etc/passwd`
/// lists only its system accounts), else the uid written out. Only asked
/// for when there is a refusal to word.
fn account_name() -> String {
    let uid = rustix::process::geteuid().as_raw();
    std::fs::read_to_string("/etc/passwd")
        .ok()
        .and_then(|passwd| {
            passwd.lines().find_map(|l| {
                let mut it = l.split(':');
                let name = it.next()?;
                let _pw = it.next()?;
                let id: u32 = it.next()?.parse().ok()?;
                (id == uid).then(|| name.to_string())
            })
        })
        .or_else(|| std::env::var("USER").ok().filter(|u| !u.is_empty()))
        .unwrap_or_else(|| uid.to_string())
}

fn unit(r: io::Result<()>) -> HelperResponse {
    r.map(|()| HelperResponse::Unit)
        .unwrap_or_else(HelperResponse::from_io)
}

/// The command line that starts a helper as `user` through `method`
/// (`sudo` or `doas`, the inventory's `escalate` parameter) by exec'ing
/// `exe` directly, which is how a root helper starts. With a password, sudo
/// reads it from stdin (`-S`); doas cannot, and `none` means the host
/// forbids escalation.
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
    let mut argv = launch::escalation(method, user, with_password, false)?;
    argv.extend([exe.to_string_lossy().into_owned(), "--helper".to_string()]);
    Ok(argv)
}

/// How to start a helper: which binary, through which method, with which
/// password. Shared by every identity of a run.
///
/// Root reads the binary where it is, so a root helper execs `exe`. Any
/// other account usually cannot (vision 11.3), so the binary streams itself
/// to that account first, through the spawns [`launch`] describes, and the
/// helper runs from the account's own copy.
#[derive(Clone)]
pub struct Spawner {
    /// `sudo`, `doas` or `none`, from the inventory's `escalate` parameter.
    /// Anything else is not a fallback to `sudo`; it fails at spawn with the
    /// name quoted.
    pub method: String,
    /// The binary to re-exec in `--helper` mode, normally
    /// `std::env::current_exe()`. The helper is this same playbook binary,
    /// so escalation installs nothing on the target. Its file name is what a
    /// streamed copy is cached under.
    pub exe: PathBuf,
    /// The escalation password, when the host needs one. `None` means
    /// passwordless escalation only, and a host that then asks for one gets
    /// a failed step, not a prompt: there is no terminal to prompt on. When
    /// present it reaches `sudo -S` on the helper's stdin and never argv,
    /// and only after `sudo -n` has been seen to fail.
    pub password: Option<Secret>,
    /// Appended to every report of a helper that died, which is how a
    /// refused `sudo` reaches the step. Set when the playbook's `ssh_user`
    /// chose the account escalating, so the failure says where that account
    /// came from ([`LoginOverride::note`](crate::ctx::LoginOverride::note)).
    pub note: Option<String>,
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
        if user == "root" {
            self.spawn_root(user)
        } else {
            self.spawn_streamed(user)
        }
    }

    /// Root execs `exe` where it is, as it always has. With a password the
    /// command goes through [`launch::EXEC`] so it answers a byte once sudo
    /// has accepted the password; without one there is nothing to wait for,
    /// since `sudo -n` never prompts, and the argv is plain [`helper_argv`].
    fn spawn_root(&self, user: &str) -> io::Result<Connection> {
        let with_password = self.needs_password(user);
        if !with_password {
            let argv = helper_argv(&self.method, user, &self.exe, false)?;
            return Ok(self.start(user, &argv, false, Kind::Plain)?.connection());
        }
        let mut argv = launch::escalation(&self.method, user, true, false)?;
        argv.extend(
            ["/bin/sh", "-c", launch::EXEC, "rustible"]
                .map(String::from)
                .into_iter()
                .chain([self.exe.to_string_lossy().into_owned(), "--helper".into()]),
        );
        let mut started = self.start(user, &argv, true, Kind::Exec)?;
        match started.first_byte(launch::FIRST_BYTE_DEADLINE)? {
            Some(b'R') => Ok(started.connection()),
            other => Err(started.gone(other)),
        }
    }

    /// Try the account's cached copy, install one if there is none, and
    /// fall back to a private temp directory: [`launch::Launch`] decides,
    /// this runs the spawns. Every spawn writes one byte before anything is
    /// written to it, so a password line is the only thing on its stdin
    /// until sudo has accepted it.
    fn spawn_streamed(&self, user: &str) -> io::Result<Connection> {
        let mut exe = std::fs::File::open(&self.exe).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("opening {} to copy it to `{user}`: {e}", self.exe.display()),
            )
        })?;
        let size = exe.metadata()?.len();
        let name = self
            .exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut plan = Launch::new(name, size, Mode::Helper, user, format!("as_user({user})"));
        loop {
            let spawn = plan.spawn();
            let with_password = self.needs_password(user);
            let mut argv = launch::escalation(&self.method, user, with_password, true)?;
            argv.extend(plan.argv(spawn));
            let mut started = self.start(user, &argv, with_password, Kind::Shell)?;
            let byte = started.first_byte(launch::FIRST_BYTE_DEADLINE)?;
            let next = match spawn {
                launch::Spawn::Try(_) => {
                    let Some(answer) = byte.and_then(Answer::from_byte) else {
                        return Err(started.gone(byte));
                    };
                    let next = plan.after_try(answer);
                    if next == Next::Ready {
                        return Ok(started.connection());
                    }
                    started.finish();
                    next
                }
                launch::Spawn::Install(_) => {
                    if byte != Some(launch::INSTALL_READY) {
                        return Err(started.gone(byte));
                    }
                    exe.seek(io::SeekFrom::Start(0))?;
                    started.feed(&mut exe)?;
                    let (status, stderr) = started.finish();
                    plan.after_install(status, &stderr)
                }
            };
            match next {
                Next::Spawn(_) => continue,
                Next::Refuse(what) => {
                    return Err(io::Error::other(HelperGone {
                        what,
                        note: self.note.clone(),
                    }));
                }
                Next::Ready => unreachable!("only a try answers Ready"),
            }
        }
    }

    /// Start `argv` with piped stdio, as `kind` says, and write the password
    /// line first when there is one.
    fn start(
        &self,
        user: &str,
        argv: &[String],
        with_password: bool,
        kind: Kind,
    ) -> io::Result<Started> {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if kind != Kind::Plain {
            cmd.env("LC_ALL", "C");
        }
        if kind == Kind::Shell {
            cmd.current_dir("/");
        }
        let child = cmd.spawn().map_err(|e| {
            io::Error::new(e.kind(), format!("spawning `{}`: {e}", printable(argv)))
        })?;
        let mut started = Started::new(user, child, kind == Kind::Plain, self.note.clone());
        if with_password && let Some(pw) = &self.password {
            // A sudo that already exited shows as EOF on the first byte,
            // with its stderr; the write failing says less than that.
            let tx = started.tx.as_mut().expect("piped");
            let _ = tx
                .write_all(pw.as_bytes())
                .and_then(|()| tx.write_all(b"\n"))
                .and_then(|()| tx.flush());
        }
        Ok(started)
    }

    /// A [`Connection`] over a started helper's pipes, with its stderr
    /// echoed and its tail kept for the failure report, and this spawner's
    /// `note` attached.
    #[cfg(test)]
    fn connection(&self, user: &str, child: Child) -> Connection {
        Started::new(user, child, true, self.note.clone()).connection()
    }
}

/// How [`Spawner::start`] starts a child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A root helper without a password, exactly as before streaming
    /// existed: the caller's working directory and environment, stderr
    /// echoed from the start, no ready byte to wait for.
    Plain,
    /// A root helper behind [`launch::EXEC`], because a password is fed:
    /// `LC_ALL=C`, so sudo's rejection line is in the words
    /// [`launch::password_rejected`] knows. The working directory stays the
    /// caller's, which root can always read.
    Exec,
    /// A [`launch`] script as another account: `LC_ALL=C`, and run from
    /// `/`, because the caller's directory may be unreadable to the account
    /// and macOS's `/bin/sh` complains about that on stderr before running
    /// anything.
    Shell,
}

/// An argv for a message: a script argument is long and says nothing the
/// reader needs, so it is shown as `<script>`.
fn printable(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if [launch::TRY, launch::INSTALL, launch::EXEC].contains(&a.as_str()) {
                "<script>"
            } else {
                a.as_str()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What the readers of a [`Started`] child report.
enum Event {
    /// A line of the child's stderr.
    Line(String),
    /// The first byte of its stdout (`None` at EOF), and the pipe back.
    Byte(io::Result<Option<u8>>, ChildStdout),
}

/// A child of the escalation tool, before and while it is a helper: its
/// pipes, and a thread reading its stderr that keeps the last lines for the
/// failure report, forwards each line to [`Started::first_byte`] while that
/// waits, and echoes them once the child is a helper.
struct Started {
    user: String,
    child: Child,
    tx: Option<ChildStdin>,
    rx: Option<ChildStdout>,
    tail: Arc<Mutex<Vec<String>>>,
    echo: Arc<AtomicBool>,
    events: mpsc::Receiver<Event>,
    events_tx: mpsc::Sender<Event>,
    reader: Option<std::thread::JoinHandle<()>>,
    note: Option<String>,
}

impl Started {
    fn new(user: &str, mut child: Child, echo: bool, note: Option<String>) -> Started {
        let tx = child.stdin.take();
        let rx = child.stdout.take();
        let stderr = child.stderr.take().expect("piped");
        let tail: Arc<Mutex<Vec<String>>> = Arc::default();
        let echo = Arc::new(AtomicBool::new(echo));
        let (events_tx, events) = mpsc::channel();
        let reader = {
            let (tail, echo, events_tx) = (tail.clone(), echo.clone(), events_tx.clone());
            let label = format!("helper as {user}");
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    if echo.load(Ordering::SeqCst) {
                        eprintln!("[{label}] {line}");
                    }
                    {
                        let mut t = tail.lock().unwrap();
                        if t.len() >= 5 {
                            t.remove(0);
                        }
                        t.push(line.clone());
                    }
                    // Nobody listens once the handshake is over.
                    let _ = events_tx.send(Event::Line(line));
                }
            })
        };
        Started {
            user: user.to_string(),
            child,
            tx,
            rx,
            tail,
            echo,
            events,
            events_tx,
            reader: Some(reader),
            note,
        }
    }

    /// The child's first byte on stdout, `None` at EOF. Refused, with the
    /// child killed, when its stderr shows sudo rejecting the password
    /// before then, or when nothing arrives within `deadline`: `sudo -S`
    /// that did not accept a password reads another line from the same
    /// pipe, and nothing would ever write one.
    fn first_byte(&mut self, deadline: Duration) -> io::Result<Option<u8>> {
        let mut rx = self.rx.take().expect("stdout is read once");
        let events_tx = self.events_tx.clone();
        std::thread::spawn(move || {
            let mut b = [0u8];
            let r = loop {
                match rx.read(&mut b) {
                    Ok(0) => break Ok(None),
                    Ok(_) => break Ok(Some(b[0])),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => break Err(e),
                }
            };
            let _ = events_tx.send(Event::Byte(r, rx));
        });
        let until = Instant::now() + deadline;
        loop {
            let wait = until.saturating_duration_since(Instant::now());
            match self.events.recv_timeout(wait) {
                Ok(Event::Byte(r, rx)) => {
                    self.rx = Some(rx);
                    return r;
                }
                Ok(Event::Line(line)) if launch::password_rejected(&line) => {
                    self.kill();
                    return Err(self.refusal(launch::password_rejected_message(&self.user, &line)));
                }
                Ok(Event::Line(_)) => {}
                Err(_) => {
                    self.kill();
                    self.settle();
                    let tail = self.tail.lock().unwrap().join("\n");
                    return Err(self.refusal(launch::deadline_message(&self.user, deadline, &tail)));
                }
            }
        }
    }

    /// Stream `src` to the child's stdin and close it. A write that fails
    /// is not reported: the child stopped reading, and its exit status and
    /// stderr say why. A read that fails is, with the child killed.
    fn feed(&mut self, src: &mut impl Read) -> io::Result<()> {
        let mut tx = self.tx.take().expect("stdin is fed once");
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match src.read(&mut buf) {
                Ok(0) => return Ok(()),
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.kill();
                    return Err(e);
                }
            };
            if tx.write_all(&buf[..n]).is_err() {
                return Ok(());
            }
        }
    }

    /// Close stdin, reap the child, and let the stderr reader finish: its
    /// exit status (-1 for a signal) and everything it said, for
    /// [`Launch::after_install`].
    fn finish(&mut self) -> (i32, String) {
        self.tx = None;
        let status = self.child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        self.settle();
        (status, self.tail.lock().unwrap().join("\n"))
    }

    /// Wait for the stderr reader to reach EOF, briefly: the report quotes
    /// its tail, and a line still in the pipe is usually the one that says
    /// why. Bounded, because something the child left running may hold the
    /// pipe open.
    fn settle(&mut self) {
        let t0 = Instant::now();
        while let Some(r) = &self.reader {
            if r.is_finished() {
                let _ = self.reader.take().map(|r| r.join());
                return;
            }
            if t0.elapsed() > Duration::from_secs(2) {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Close stdin, SIGKILL, reap. Stdin goes first: a sudo that runs with
    /// a real uid of 0 while it reads a password cannot be signalled by the
    /// caller, and EOF on the password read is what ends it then.
    fn kill(&mut self) {
        self.stop(true);
    }

    fn stop(&mut self, signal: bool) {
        self.tx = None;
        if signal {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }

    fn refusal(&self, what: String) -> io::Error {
        io::Error::other(HelperGone {
            what,
            note: self.note.clone(),
        })
    }

    /// The child exited, or answered something no script writes: its exit
    /// status and stderr, as for a helper that died.
    fn gone(mut self, byte: Option<u8>) -> io::Error {
        if byte.is_some() {
            self.kill();
        }
        let (status, tail) = self.finish();
        let mut what = format!("helper exited {status}");
        let tail = tail.replace('\n', " / ");
        if !tail.is_empty() {
            what = format!("{what}: {tail}");
        }
        if let Some(b) = byte {
            what = format!("{what} (it answered {:?} before any frame)", b as char);
        }
        self.refusal(what)
    }

    /// The child is a helper now: its pipes become the frame channel, and
    /// its stderr is echoed from here on.
    fn connection(mut self) -> Connection {
        self.echo.store(true, Ordering::SeqCst);
        Connection {
            tx: Box::new(self.tx.take().expect("piped")),
            rx: Box::new(self.rx.take().expect("piped")),
            child: Some(self.child),
            stderr_tail: self.tail,
            note: self.note,
        }
    }
}

/// Why a helper is gone, and the spawner's note. It travels as the inner
/// error of an `io::Error` so [`System`](crate::system::System) can tell it
/// from an ordinary I/O failure: the report already says everything, and
/// wrapping it in `IoAt`, which prints its source and also chains it, would
/// print it, note included, twice.
#[derive(Debug, Clone)]
pub(crate) struct HelperGone {
    /// How it went: the exit status and the tail of its stderr.
    pub(crate) what: String,
    /// [`Spawner::note`], printed once after `what`.
    pub(crate) note: Option<String>,
}

impl std::fmt::Display for HelperGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.note {
            Some(note) => write!(f, "{}; {note}", self.what),
            None => f.write_str(&self.what),
        }
    }
}

impl std::error::Error for HelperGone {}

impl HelperGone {
    /// The report inside `e`, when `e` carries one.
    pub(crate) fn inside(e: &io::Error) -> Option<&HelperGone> {
        e.get_ref()
            .and_then(|inner| inner.downcast_ref::<HelperGone>())
    }
}

struct Connection {
    tx: Box<dyn Write + Send>,
    rx: Box<dyn Read + Send>,
    child: Option<Child>,
    stderr_tail: Arc<Mutex<Vec<String>>>,
    /// [`Spawner::note`], carried to where the failure is described.
    note: Option<String>,
}

impl Connection {
    fn call(&mut self, req: &HelperRequest) -> io::Result<HelperResponse> {
        let answer = write_frame(&mut self.tx, req)
            .and_then(|()| read_frame::<_, HelperResponse>(&mut self.rx));
        match answer {
            Ok(Some(resp)) => Ok(resp),
            // EOF or a broken pipe: the helper is gone (sudo refused, it
            // crashed, or it was killed); say how it went.
            Ok(None) => Err(io::Error::other(self.gone(None))),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof
                ) =>
            {
                Err(io::Error::other(self.gone(Some(&e))))
            }
            Err(e) => Err(e),
        }
    }

    /// The helper's death as a [`HelperGone`]: exit status, the tail of its
    /// stderr, `cause` when a broken pipe or EOF is how it showed, and the
    /// spawner's note.
    fn gone(&mut self, cause: Option<&io::Error>) -> HelperGone {
        let status = match &mut self.child {
            Some(c) => match c.wait() {
                Ok(s) => format!("exited {}", s.code().unwrap_or(-1)),
                Err(e) => format!("wait failed: {e}"),
            },
            None => "closed the connection".to_string(),
        };
        let tail = self.stderr_tail.lock().unwrap().join(" / ");
        let mut what = if tail.is_empty() {
            format!("helper {status}")
        } else {
            format!("helper {status}: {tail}")
        };
        if let Some(cause) = cause {
            what = format!("{what} ({cause})");
        }
        HelperGone {
            what,
            note: self.note.clone(),
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
/// this type adds. What it does add is four refusals: a mutation while the
/// step is checking, a request or an answer larger than one frame (the
/// helper refuses the answer, and goes on serving), and every primitive
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
    failed: Mutex<Option<HelperGone>>,
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
                note: None,
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
        if let Some(first) = self.failed.lock().unwrap().as_ref() {
            return Err(io::Error::other(HelperGone {
                what: format!(
                    "the helper running as `{}` failed earlier and is not retried: {}",
                    self.user, first.what
                ),
                note: first.note.clone(),
            }));
        }
        if let Some((path, len)) = op.payload()
            && len > MAX_FRAME_PAYLOAD
        {
            return Err(io::Error::other(match &op {
                HelperOp::Spawn(_) => format!(
                    "{} as `{}`: its stdin is {len} bytes, more than one helper frame can \
                     carry ({MAX_FRAME_PAYLOAD} bytes); have the command read input this \
                     large from a file on the target instead",
                    op.label(),
                    self.user
                ),
                _ => format!(
                    "{}: {len} bytes is more than one helper frame can carry \
                     ({MAX_FRAME_PAYLOAD} bytes); running as `{}` sends the whole file in \
                     one request, so a file this large has to be handled without \
                     `as_user`/`as_root`",
                    path.display(),
                    self.user
                ),
            }));
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
            // report back into it.
            match e
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<FrameTooLarge>())
            {
                // The helper refuses an answer this large rather than send
                // it (`serve`), so this one is not an answer: the stream is
                // out of step, and the latch below is the right response.
                Some(big) => io::Error::other(format!(
                    "{label}: the helper running as `{}` sent a frame of {} bytes, more \
                     than the {MAX_FRAME} a frame may hold; a helper refuses an answer \
                     that large instead of sending it, so the stream between the two is \
                     corrupt",
                    self.user, big.len
                )),
                None => e,
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
                *f = Some(
                    HelperGone::inside(e)
                        .cloned()
                        .unwrap_or_else(|| HelperGone {
                            what: e.to_string(),
                            note: None,
                        }),
                );
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
        in_process_within(phase, MAX_FRAME)
    }

    /// [`in_process`], with a helper that refuses answers over `max_frame`
    /// bytes, so a test can be refused without building 64 MiB frames. Its
    /// refusals name `tester`, the account the `Elevated` is given.
    fn in_process_within(phase: Arc<AtomicU8>, max_frame: usize) -> Elevated {
        let (req_r, req_w) = io::pipe().unwrap();
        let (resp_r, resp_w) = io::pipe().unwrap();
        std::thread::spawn(move || {
            let (mut rx, mut tx) = (req_r, resp_w);
            serve(&mut rx, &mut tx, max_frame, || "tester".into()).unwrap();
        });
        Elevated::connected("tester", Box::new(req_w), Box::new(resp_r), phase)
    }

    /// The frame limit the oversized-answer tests give their helper: small
    /// enough that a test's file, output or listing passes it cheaply, and
    /// large enough for any refusal.
    const SMALL_FRAME: usize = 16 * 1024;

    /// A shell command, as `sys.cmd` would build it.
    fn sh(script: &str) -> CmdSpec {
        CmdSpec {
            program: "sh".into(),
            args: vec!["-c".into(), script.into()],
            env: BTreeMap::new(),
            cwd: None,
            stdin: None,
            prefix: vec![],
        }
    }

    /// `stat`, `read` and `write` still work on `e`, in a fresh directory:
    /// the helper answered the refusal before and is serving after it.
    fn still_serves(e: &Elevated) {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("after");
        assert_eq!(e.stat(&f).unwrap(), None);
        e.write(&f, b"still here").unwrap();
        assert_eq!(e.read(&f).unwrap(), b"still here");
        assert_eq!(e.stat(&f).unwrap().unwrap().size, 10);
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
        // `copy` never writes through an existing path or a link, and the
        // refusal comes back as `AlreadyExists`, which `System::backup`
        // reads to try the next name (issue #75).
        let y_link = dir.path().join("y-link");
        e.symlink(&dir.path().join("y"), &y_link).unwrap();
        e.write(&f, b"other").unwrap();
        for to in [dir.path().join("y"), y_link] {
            let taken = e.copy(&f, &to).unwrap_err();
            assert_eq!(taken.kind(), io::ErrorKind::AlreadyExists, "{taken}");
        }
        assert_eq!(e.read(&dir.path().join("y")).unwrap(), payload, "untouched");
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

    /// A command's variables cross to the helper and reach the child. The
    /// helper is started by `sudo`, whose reset environment is what its
    /// children inherit, so a variable an op needs there has to ride in the
    /// `CmdSpec`: `systemd`'s `.user(true)` sets `XDG_RUNTIME_DIR` this way
    /// for an `as_user` target (#55).
    #[test]
    fn a_commands_env_reaches_the_child_through_the_helper() {
        let phase = Arc::new(AtomicU8::new(Phase::Applying as u8));
        let e = in_process(phase);
        let out = e
            .spawn(&CmdSpec {
                program: "sh".into(),
                args: vec!["-c".into(), "printf %s \"$XDG_RUNTIME_DIR\"".into()],
                env: BTreeMap::from([("XDG_RUNTIME_DIR".into(), "/run/user/1002".into())]),
                cwd: None,
                stdin: None,
                prefix: vec![],
            })
            .unwrap();
        assert_eq!((out.status, out.stdout_str()), (0, "/run/user/1002".into()));
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

    /// Output larger than a frame is refused by the helper, naming the
    /// command, both sizes and the account, with what to do instead; and the
    /// same `Elevated` goes on serving. Before #84 the helper sent the frame
    /// anyway, the main side could not read past it, and every later
    /// primitive on that identity failed with a message about a file.
    #[test]
    fn oversized_command_output_is_refused_and_the_helper_survives() {
        let e = in_process_within(Arc::new(AtomicU8::new(Phase::Applying as u8)), SMALL_FRAME);
        let err = e
            .spawn(&sh("head -c 20000 /dev/zero; echo oops >&2"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "spawn sh -c head -c 20000 /dev/zero; echo oops >&2 as `tester` wrote 20000 \
             bytes to stdout and 5 to stderr, which base64-encoded is more than one \
             helper frame holds (16384 bytes); redirect its output to a file in the \
             command (`sh -c '… > /path'`) and read that file"
        );
        still_serves(&e);
        // And a command whose output fits still comes back whole.
        let out = e.spawn(&sh("head -c 1000 /dev/zero")).unwrap();
        assert_eq!(out.stdout, vec![0u8; 1000]);
    }

    /// A file larger than a frame is refused by the helper, naming the file,
    /// its size and the account; the helper stays usable. This is the read
    /// that used to kill a helper for the rest of the run.
    #[test]
    fn an_oversized_read_is_refused_and_the_helper_survives() {
        let e = in_process_within(Arc::new(AtomicU8::new(Phase::Applying as u8)), SMALL_FRAME);
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        std::fs::write(&big, vec![b'x'; 20000]).unwrap();
        let err = e.read(&big).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other, "{err}");
        assert_eq!(
            err.to_string(),
            format!(
                "read {} as `tester`: the file is 20000 bytes, which base64-encoded is more \
                 than one helper frame holds (16384 bytes); reading a file this large has \
                 to be done without `as_user`/`as_root`",
                big.display()
            )
        );
        still_serves(&e);
        // The same file, smaller, reads.
        std::fs::write(&big, b"small").unwrap();
        assert_eq!(e.read(&big).unwrap(), b"small");
    }

    /// A listing larger than a frame is refused like a file is, naming the
    /// directory and how many entries it has.
    #[test]
    fn an_oversized_listing_is_refused_and_the_helper_survives() {
        let e = in_process_within(Arc::new(AtomicU8::new(Phase::Applying as u8)), SMALL_FRAME);
        let dir = tempfile::tempdir().unwrap();
        for i in 0..200 {
            std::fs::write(dir.path().join(format!("{i:0>96}")), b"").unwrap();
        }
        let err = e.read_dir(dir.path()).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "read_dir {} as `tester`: the directory has 200 entries, a listing more \
                 than one helper frame holds (16384 bytes); listing a directory this large \
                 has to be done without `as_user`/`as_root`",
                dir.path().display()
            )
        );
        still_serves(&e);
    }

    /// Every frame `serve` writes is within its limit, whatever the answer:
    /// a file, a command's output and a listing over it (`Bytes`, `Output`,
    /// `Paths`), a refusal quoting an absurd path, and a refusal for a command
    /// whose argv alone is larger than a frame. The requests go in as one
    /// stream and the answers are read back frame by frame from what `serve`
    /// wrote, so a frame over the limit is caught here, not by a reader that
    /// gives up on it.
    #[test]
    fn no_frame_the_helper_writes_is_over_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        std::fs::write(&big, vec![b'x'; 20000]).unwrap();
        let small = dir.path().join("small");
        std::fs::write(&small, b"small").unwrap();
        let listed = dir.path().join("listed");
        std::fs::create_dir(&listed).unwrap();
        for i in 0..200 {
            std::fs::write(listed.join(format!("{i:0>96}")), b"").unwrap();
        }
        let absurd = dir.path().join("a".repeat(SMALL_FRAME));
        let mut long_argv = sh("head -c 20000 /dev/zero");
        long_argv.args.push("y".repeat(SMALL_FRAME));

        let requests = [
            (false, HelperOp::Read { path: big.clone() }),
            (false, HelperOp::Read { path: small }),
            (false, HelperOp::ReadDir { path: listed }),
            (false, HelperOp::Spawn(sh("head -c 20000 /dev/zero"))),
            (false, HelperOp::Spawn(sh("printf ok"))),
            (true, HelperOp::Remove { path: absurd }),
            (false, HelperOp::Spawn(long_argv)),
        ];
        let mut rx = Vec::new();
        for (checking, op) in requests {
            write_frame(&mut rx, &HelperRequest { checking, op }).unwrap();
        }
        let mut tx = Vec::new();
        serve(&mut &rx[..], &mut tx, SMALL_FRAME, || "tester".into()).unwrap();

        let mut answers = Vec::new();
        let mut stream = &tx[..];
        while !stream.is_empty() {
            let len = u32::from_be_bytes(stream[..4].try_into().unwrap()) as usize;
            assert!(
                len <= SMALL_FRAME,
                "a {len}-byte frame, answer {}",
                answers.len()
            );
            let resp: HelperResponse = serde_json::from_slice(&stream[4..4 + len]).unwrap();
            answers.push(resp);
            stream = &stream[4 + len..];
        }
        let err = |r: &HelperResponse| match r {
            HelperResponse::Err {
                code: None,
                message,
            } => message.clone(),
            other => panic!("expected a refusal, got {other:?}"),
        };
        assert_eq!(answers.len(), 7, "{answers:?}");
        let read = err(&answers[0]);
        assert!(
            read.starts_with(&format!(
                "read {} as `tester`: the file is 20000 bytes",
                big.display()
            )),
            "{read}"
        );
        assert!(matches!(&answers[1], HelperResponse::Bytes(b) if b == b"small"));
        assert!(err(&answers[2]).contains("the directory has 200 entries"));
        assert!(err(&answers[3]).contains("wrote 20000 bytes to stdout"));
        assert!(matches!(&answers[4], HelperResponse::Output(o) if o.stdout == b"ok"));
        // A refusal over the limit only because of the path it quotes is
        // refused in its turn, quoting the path cut short.
        let quoted = err(&answers[5]);
        assert!(quoted.starts_with("remove "), "{quoted}");
        assert!(
            quoted.contains(
                "… as `tester`: the answer is more than one helper frame holds (16384 bytes)"
            ),
            "{quoted}"
        );
        let spawn = err(&answers[6]);
        assert!(
            spawn.starts_with("spawn sh -c head -c 20000 /dev/zero yyy"),
            "{spawn}"
        );
        assert!(
            spawn.contains("… as `tester` wrote 20000 bytes to stdout"),
            "{spawn}"
        );
    }

    /// The refusal for a command's stdin larger than a frame names the
    /// command and the account, not a file, and nothing reaches the helper.
    #[test]
    fn an_oversized_stdin_is_refused_naming_the_command() {
        let e = in_process(Arc::new(AtomicU8::new(Phase::Applying as u8)));
        let mut spec = sh("cat > /dev/null");
        spec.stdin = Some(vec![0u8; MAX_FRAME_PAYLOAD + 1]);
        let err = e.spawn(&spec).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "spawn sh -c cat > /dev/null as `tester`: its stdin is {} bytes, more than \
                 one helper frame can carry ({MAX_FRAME_PAYLOAD} bytes); have the command \
                 read input this large from a file on the target instead",
                MAX_FRAME_PAYLOAD + 1
            )
        );
        still_serves(&e);
    }

    /// A frame over the limit from the helper's side cannot be an answer any
    /// more, since the helper refuses those itself: it is a stream out of
    /// step, reported as one, and the helper is not used again. The
    /// detection is on the typed [`FrameTooLarge`] signal, so the wording of
    /// the framing error is free to change.
    #[test]
    fn a_frame_over_the_limit_is_a_corrupt_stream_and_is_latched() {
        // A "helper" that answers every request with a length prefix one
        // byte over the frame ceiling. Nothing else has to be there: the
        // read fails on the prefix, before a body is allocated.
        let prefix = u32::try_from(MAX_FRAME + 1).unwrap();
        let e = Elevated::connected(
            "tester",
            Box::new(io::sink()),
            Box::new(io::Cursor::new(prefix.to_be_bytes().to_vec())),
            Arc::new(AtomicU8::new(Phase::Applying as u8)),
        );
        let err = e.read(Path::new("/etc/shadow")).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "read /etc/shadow: the helper running as `tester` sent a frame of {} bytes, \
                 more than the {MAX_FRAME} a frame may hold; a helper refuses an answer that \
                 large instead of sending it, so the stream between the two is corrupt",
                MAX_FRAME + 1
            )
        );
        let again = e.stat(Path::new("/etc/hostname")).unwrap_err().to_string();
        assert!(
            again.contains("failed earlier and is not retried"),
            "{again}"
        );
        assert!(
            again.contains("stream between the two is corrupt"),
            "{again}"
        );
    }

    /// The account a real helper's refusals name is the one `id -un` gives
    /// for this process.
    #[test]
    fn the_account_named_is_the_effective_users() {
        let Ok(out) = Command::new("id").arg("-un").output() else {
            return;
        };
        if !out.status.success() {
            return;
        }
        assert_eq!(account_name(), String::from_utf8_lossy(&out.stdout).trim());
    }

    /// An `Elevated` whose "helper" printed to stderr and exited without
    /// answering, which is what a refused `sudo -n` looks like from here.
    /// The connection comes from [`Spawner::connection`], the same call
    /// `Spawner::spawn` makes once `sudo` has started, so only the `sudo`
    /// itself is bypassed.
    fn dead_helper(note: Option<String>) -> Elevated {
        let spawner = Spawner {
            method: "sudo".into(),
            exe: PathBuf::from("/nonexistent/rustible-bin"),
            password: None,
            note,
        };
        let child = Command::new("sh")
            .args(["-c", "echo boom >&2; exit 7"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let conn = spawner.connection("root", child);
        // The stderr reader is a thread of its own; let it finish the one
        // line before the report reads the tail.
        let t0 = Instant::now();
        while conn.stderr_tail.lock().unwrap().is_empty() {
            assert!(
                t0.elapsed() < Duration::from_secs(10),
                "no stderr from the helper"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        Elevated {
            user: "root".into(),
            spawner: Some(spawner),
            phase: Arc::new(AtomicU8::new(0)),
            conn: Mutex::new(Some(conn)),
            failed: Mutex::new(None),
        }
    }

    #[test]
    fn dead_helper_reports_exit_and_stderr() {
        let e = dead_helper(None);
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

    /// A playbook whose `ssh_user` attribute chose the login has the
    /// escalating account's origin on every report of a dead helper,
    /// including the latched one later primitives return.
    #[test]
    fn a_dead_helper_carries_the_spawners_note() {
        let note = "the login user `minecraft` comes from the playbook's `ssh_user` attribute";
        let e = dead_helper(Some(note.into()));
        let err = e.read(Path::new("/etc/hostname")).unwrap_err().to_string();
        // A broken pipe may or may not come between, depending on whether
        // the helper had exited before the request was written.
        assert!(err.starts_with("helper exited 7: boom"), "{err}");
        assert!(err.ends_with(&format!("; {note}")), "{err}");
        let again = e.stat(Path::new("/etc/hostname")).unwrap_err().to_string();
        assert!(again.contains(note), "{again}");
    }

    /// Through `System`, as a step sees it: the report, note included, is
    /// printed once, for a file primitive and for a command alike. As the
    /// source of an `IoAt` it printed twice.
    #[test]
    fn a_dead_helpers_report_reaches_a_step_once() {
        use crate::event::Collect;
        use crate::system::System;

        let note = "the login user `minecraft` comes from the playbook's `ssh_user` attribute";
        let sink = Arc::new(Collect::default());
        let facts = System::fake(Arc::new(crate::backend::Fake::new()), sink.clone())
            .facts()
            .clone();
        let sys = System::new(Arc::new(dead_helper(Some(note.into()))), facts, false, sink);

        let read = sys.read("/etc/hostname").unwrap_err().chain();
        assert_eq!(read.matches(note).count(), 1, "{read}");
        assert_eq!(read.matches("exited 7").count(), 1, "{read}");
        assert!(
            read.starts_with("/etc/hostname: helper exited 7: boom"),
            "{read}"
        );

        let cmd = sys.cmd("true").run().unwrap_err().chain();
        assert_eq!(cmd.matches(note).count(), 1, "{cmd}");
        assert!(cmd.contains("failed earlier and is not retried"), "{cmd}");
    }

    /// A step whose escalation died, as a refused `sudo` shows inside the
    /// binary: one recorded failure, claimed by the run when its error
    /// escapes, and one `Failed` frame naming the step, with the report and
    /// its note printed once. The verdict is then `classify`'s like any other
    /// step's (#44): failed when the error leaves the playbook, recovered
    /// when it is caught.
    #[test]
    fn a_dead_helper_fails_its_step_like_any_other_failure() {
        use crate::ctx::Ctx;
        use crate::event::{Collect, Event};
        use crate::op::{Op, Plan};
        use crate::system::System;

        struct ReadsHostname;
        impl Op for ReadsHostname {
            type Output = ();
            type Intent = std::convert::Infallible;
            fn check(&self, sys: &System) -> crate::Result<Plan<Self>> {
                sys.read("/etc/hostname")?;
                Ok(Plan::Satisfied(()))
            }
            fn apply(&self, _: &System, intent: Self::Intent) -> crate::Result<()> {
                match intent {}
            }
        }

        let note = "the login user `minecraft` comes from the playbook's `ssh_user` attribute";
        let sink = Arc::new(Collect::default());
        let facts = System::fake(Arc::new(crate::backend::Fake::new()), sink.clone())
            .facts()
            .clone();
        let sys = System::new(Arc::new(dead_helper(Some(note.into()))), facts, false, sink);
        let mut ctx = Ctx::new(sys, crate::ctx::HostInfo::local());

        let e = ctx.step("read the hostname", ReadsHostname).unwrap_err();
        let failures = ctx.failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(!failures[0].cancelled);
        let escaped = e.step_failed().and_then(|s| ctx.claims(s));
        assert_eq!(escaped, Some(failures[0].id));

        let Event::Failed {
            step, id, error, ..
        } = Event::failed(&e, escaped)
        else {
            panic!("not Failed")
        };
        assert_eq!(step.as_deref(), Some("read the hostname"));
        assert_eq!(id, escaped);
        assert_eq!(error.matches(note).count(), 1, "{error}");
        assert_eq!(error.matches("exited 7").count(), 1, "{error}");
        assert!(
            error.starts_with("step `read the hostname`: /etc/hostname: helper exited 7"),
            "{error}"
        );
    }

    // ---- streaming, behind a stand-in `sudo` ----
    //
    // `sudo` is resolved through `PATH`, and changing `PATH` in this process
    // would race every other test that spawns a program, so the outer test
    // re-runs this binary with a stand-in first on `PATH` and the inner one
    // does the work. The stand-in drops sudo's options, checks a password
    // the way `sudo -S` does when told to, and runs the rest as the test's
    // own user, with the `HOME` and `TMPDIR` the outer test chose.

    const STAND_IN_SUDO: &str = r#"#!/bin/sh
printf 'LC_ALL=%s cwd=%s | %s\n' "$LC_ALL" "$(pwd)" "$*" >> "$RUSTIBLE_TEST_SUDO_LOG"
case "$RUSTIBLE_TEST_NO_READY $*" in
  1*"printf I"*) ( sleep 1; kill $$ ) & exec cat > "$RUSTIBLE_TEST_DIR/early" ;;
esac
pw=
while [ $# -gt 0 ]; do
  case $1 in -n|-H) shift ;; -S) pw=1; shift ;; -p|-u) shift 2 ;; *) break ;; esac
done
if [ -n "$RUSTIBLE_TEST_PASSWORD" ]; then
  [ -n "$pw" ] || { echo "sudo: a password is required" >&2; exit 1; }
  IFS= read -r line
  if [ "$line" != "$RUSTIBLE_TEST_PASSWORD" ]; then
    echo "Sorry, try again." >&2
    IFS= read -r line
    echo "sudo: 2 incorrect password attempts" >&2
    exit 1
  fi
fi
exec "$@"
"#;

    /// The playbook binary's stand-in: it says where it ran from and how.
    const STAND_IN_EXE: &str = "#!/bin/sh\necho \"ran $0 $*\"\n";

    const HASHED: &str = "pb-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Run the inner test for `case` behind the stand-in, in a fresh
    /// directory holding `home/`, `tmp/` and the binary to stream.
    fn behind_stand_in_sudo(case: &str) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("sudo"), STAND_IN_SUDO).unwrap();
        std::fs::set_permissions(bin.join("sudo"), std::fs::Permissions::from_mode(0o755)).unwrap();
        for d in ["home", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "backend::elevated::tests::streamed_spawn_behind_a_stand_in_sudo",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("PATH", path)
            .env("HOME", dir.path().join("home"))
            .env("TMPDIR", dir.path().join("tmp"))
            .env("RUSTIBLE_TEST_SUDO_LOG", dir.path().join("sudo.log"))
            .env("RUSTIBLE_TEST_STREAM", case)
            .env("RUSTIBLE_TEST_DIR", dir.path())
            .env_remove("LC_ALL")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{case}:\n{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "{case}: did not run\n{stdout}");
    }

    #[test]
    fn a_cold_then_warm_helper_runs_from_the_accounts_own_copy() {
        behind_stand_in_sudo("cold-warm");
    }

    #[test]
    fn an_account_without_a_home_gets_a_private_temp_copy() {
        behind_stand_in_sudo("no-home");
    }

    #[test]
    fn no_home_and_no_temp_directory_is_refused_naming_both() {
        behind_stand_in_sudo("nowhere");
    }

    #[test]
    fn a_password_is_fed_before_each_streamed_spawn() {
        behind_stand_in_sudo("password");
    }

    #[test]
    fn a_rejected_password_is_refused_instead_of_hanging() {
        behind_stand_in_sudo("wrong-password");
    }

    #[test]
    fn a_root_helper_with_a_password_waits_for_its_ready_byte() {
        behind_stand_in_sudo("root-password");
    }

    #[test]
    fn a_root_helper_without_a_password_starts_as_it_always_did() {
        behind_stand_in_sudo("root-plain");
    }

    #[test]
    fn no_byte_of_the_binary_is_written_before_the_install_answers() {
        behind_stand_in_sudo("no-ready");
    }

    /// The inner half of the tests above.
    #[test]
    #[ignore = "run by the tests that call behind_stand_in_sudo"]
    fn streamed_spawn_behind_a_stand_in_sudo() {
        use std::os::unix::fs::PermissionsExt;
        let Ok(case) = std::env::var("RUSTIBLE_TEST_STREAM") else {
            return;
        };
        let dir = PathBuf::from(std::env::var("RUSTIBLE_TEST_DIR").unwrap());
        let (home, tmp) = (dir.join("home"), dir.join("tmp"));
        let exe = dir.join(HASHED);
        std::fs::write(&exe, STAND_IN_EXE).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let spawner = |password: Option<&str>| Spawner {
            method: "sudo".into(),
            exe: exe.clone(),
            password: password.map(|p| Secret::from(p.to_string())),
            note: None,
        };
        let log = || std::fs::read_to_string(dir.join("sudo.log")).unwrap_or_default();
        // Each call as `LC_ALL=<value> cwd=<dir>` and the argv.
        let calls = || -> Vec<(String, String)> {
            log()
                .lines()
                .map(|l| {
                    let (env, argv) = l.split_once(" | ").unwrap();
                    (env.to_string(), argv.to_string())
                })
                .collect()
        };
        // What the helper's stand-in printed: where it ran from, and how.
        let ran = |conn: Connection| {
            let mut out = String::new();
            let mut rx = conn.rx;
            rx.read_to_string(&mut out).unwrap();
            out
        };
        let cached = home.join(".cache/rustible/bin").join(HASHED);
        // The spawns, by their script: `try`, `install`, `exec`.
        let spawns = || {
            calls()
                .into_iter()
                .map(|(_, argv)| argv)
                .filter(|l| !l.ends_with(" true"))
                .map(|l| {
                    if l.contains(launch::INSTALL) {
                        "install"
                    } else if l.contains(launch::EXEC) {
                        "exec"
                    } else {
                        "try"
                    }
                })
                .collect::<Vec<_>>()
        };
        match case.as_str() {
            "cold-warm" => {
                let conn = spawner(None).spawn("svc").unwrap();
                assert_eq!(ran(conn), format!("ran {} --helper\n", cached.display()));
                assert_eq!(spawns(), ["try", "install", "try"]);
                // Every streamed spawn runs from `/` under `LC_ALL=C`.
                for (env, argv) in calls() {
                    assert_eq!(env, "LC_ALL=C cwd=/", "{argv}");
                    assert!(argv.starts_with("-n -H -u svc /bin/sh -c "), "{argv}");
                }
                assert_eq!(std::fs::read(&cached).unwrap(), STAND_IN_EXE.as_bytes());
                std::fs::remove_file(dir.join("sudo.log")).unwrap();
                let conn = spawner(None).spawn("svc").unwrap();
                assert_eq!(ran(conn), format!("ran {} --helper\n", cached.display()));
                assert_eq!(spawns(), ["try"]);
            }
            "no-home" => {
                std::fs::remove_dir(&home).unwrap();
                let conn = spawner(None).spawn("svc").unwrap();
                let out = ran(conn);
                let copy = out
                    .strip_prefix("ran ")
                    .and_then(|o| o.strip_suffix(" --helper --ephemeral\n"))
                    .unwrap_or_else(|| panic!("{out}"));
                let copy = Path::new(copy);
                assert!(copy.starts_with(&tmp), "{out}");
                assert!(
                    copy.parent()
                        .unwrap()
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("rustible-")
                );
                let mode = std::fs::metadata(copy.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o700);
                assert_eq!(spawns(), ["try", "install", "install", "try"]);
            }
            "nowhere" => {
                std::fs::remove_dir(&home).unwrap();
                std::fs::remove_dir(&tmp).unwrap();
                let Err(err) = spawner(None).spawn("svc") else {
                    panic!("spawned")
                };
                let err = err.to_string();
                assert!(
                    err.starts_with(&format!(
                        "as_user(svc): no usable home ({} does not exist) and cannot create {}/rustible-",
                        home.display(),
                        tmp.display()
                    )),
                    "{err}"
                );
                assert!(
                    err.ends_with(", so svc cannot run its copy of the playbook binary"),
                    "{err}"
                );
            }
            "password" => {
                // SAFETY: the inner test runs alone in its process.
                unsafe { std::env::set_var("RUSTIBLE_TEST_PASSWORD", "right") };
                let conn = spawner(Some("right")).spawn("svc").unwrap();
                assert_eq!(ran(conn), format!("ran {} --helper\n", cached.display()));
                // Every spawn probed with `-n`, was refused, and got `-S`.
                assert_eq!(spawns(), ["try", "install", "try"]);
                assert_eq!(log().lines().filter(|l| l.ends_with(" true")).count(), 3);
                assert_eq!(std::fs::read(&cached).unwrap(), STAND_IN_EXE.as_bytes());
            }
            "wrong-password" => {
                // SAFETY: the inner test runs alone in its process.
                unsafe { std::env::set_var("RUSTIBLE_TEST_PASSWORD", "right") };
                let t0 = Instant::now();
                let Err(err) = spawner(Some("wrong")).spawn("svc") else {
                    panic!("spawned")
                };
                assert_eq!(
                    err.to_string(),
                    "escalation as `svc` failed: the password given with \
                     --escalate-password-env was rejected (Sorry, try again.)"
                );
                assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());
                assert!(!cached.exists());
            }
            "root-password" => {
                // SAFETY: the inner test runs alone in its process.
                unsafe { std::env::set_var("RUSTIBLE_TEST_PASSWORD", "right") };
                let conn = spawner(Some("right")).spawn("root").unwrap();
                // Root is not streamed: it runs the binary where it is.
                assert_eq!(ran(conn), format!("ran {} --helper\n", exe.display()));
                assert_eq!(spawns(), ["exec"]);
                // `LC_ALL=C` for sudo's rejection line, and the caller's own
                // working directory, which root can always read.
                let (env, _) = calls()
                    .into_iter()
                    .find(|(_, a)| a.contains(launch::EXEC))
                    .unwrap();
                assert!(env.starts_with("LC_ALL=C cwd="), "{env}");
                assert_ne!(env, "LC_ALL=C cwd=/", "{env}");
                let Err(err) = spawner(Some("wrong")).spawn("root") else {
                    panic!("spawned")
                };
                assert!(err.to_string().contains("was rejected"), "{err}");
            }
            "root-plain" => {
                let conn = spawner(None).spawn("root").unwrap();
                assert_eq!(ran(conn), format!("ran {} --helper\n", exe.display()));
                // The argv, environment and working directory of before.
                let cwd = std::env::current_dir().unwrap();
                assert_eq!(
                    calls(),
                    [(
                        format!("LC_ALL= cwd={}", cwd.display()),
                        format!("-n -u root {} --helper", exe.display())
                    )]
                );
            }
            "no-ready" => {
                // SAFETY: the inner test runs alone in its process.
                unsafe { std::env::set_var("RUSTIBLE_TEST_NO_READY", "1") };
                // The stand-in's install never answers `I`: it reads what it
                // is sent for a second, then dies.
                let Err(err) = spawner(None).spawn("svc") else {
                    panic!("spawned")
                };
                assert!(err.to_string().starts_with("helper exited "), "{err}");
                let early = std::fs::read(dir.join("early")).unwrap();
                assert!(early.is_empty(), "{} bytes sent before `I`", early.len());
            }
            other => panic!("unknown case {other}"),
        }
    }

    /// Stopping a child closes its stdin before it is reaped, so one that
    /// the signal does not reach still ends: a sudo reading a password as
    /// root cannot be signalled by its caller, but sees EOF.
    #[test]
    fn a_child_the_signal_misses_is_reaped_through_eof() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let child = Command::new("sh")
                .args(["-c", "cat >/dev/null"])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let mut started = Started::new("svc", child, false, None);
            // No signal: only the closed stdin can end it.
            started.stop(false);
            let _ = done_tx.send(());
        });
        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the child was waited for with its stdin still open");
    }

    /// A child that never writes a byte is killed at the deadline, and the
    /// refusal quotes what it said last.
    #[test]
    fn a_silent_spawn_is_killed_at_the_deadline() {
        let child = Command::new("sh")
            .args(["-c", "echo 'Password for svc:' >&2; exec sleep 30"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut started = Started::new("svc", child, false, None);
        let t0 = Instant::now();
        let err = started
            .first_byte(Duration::from_millis(300))
            .unwrap_err()
            .to_string();
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", t0.elapsed());
        assert_eq!(
            err,
            "escalation as `svc` did not answer within 300ms; it may be waiting for a \
             password it did not accept, or at a prompt rustible does not recognise: \
             Password for svc:"
        );
        assert!(started.child.try_wait().unwrap().is_some(), "not killed");
    }
}
