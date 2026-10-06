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
//! ## Streams
//!
//! Requests and answers go in lockstep: one request, one answer, under the
//! connection lock, and no request is sent before the previous one is
//! answered. So there is no queue anywhere and nothing to correlate.
//!
//! Anything that can be large crosses in chunks of at most
//! [`CHUNK_SIZE`](crate::protocol::CHUNK_SIZE) (1 MiB), one request per
//! chunk, so every frame stays near 1.4 MiB whatever the size of the file:
//! a file's contents, read or written, a directory's listing, a command's
//! stdin and its output (`[ISSUE-85]`). A stream the helper keeps open
//! between two requests is a *handle* in its table: an id the helper chose,
//! naming a write staged beside its target, an open file, an open directory,
//! a command's stdin or a finished command's output. The first request of a
//! stream carries or returns the first chunk, so anything of one chunk or
//! less is still exactly one round trip, and opens no handle at all. The
//! connection lock is held per request, not per stream, so between two
//! chunks any other primitive on the same identity may run: an archive read
//! through [`Backend::open_read`] while each member is written through
//! [`Backend::write_from`] alternates, chunk by chunk, on one helper.
//!
//! A write is staged by the same [`Staged`] writer `Local` uses and renamed
//! over its target only after its last chunk, so a failure part way leaves
//! the target as it was. The helper removes a stream whose request failed,
//! and its temporary file with it; this side releases a stream it gives up
//! on (an error from the source being written, a reader dropped before its
//! end). When the parent goes away mid-stream, the helper sees EOF, its
//! table drops, and each staged write removes its temporary file. A helper
//! killed mid-stream can leave one `.rustible-*` file per write stream it
//! had open, each beside its target, as a local write killed part way
//! does; the targets are untouched either way.
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
//! No message this module produces quotes file contents, a command's stdin
//! or its output. The label of a request gives the primitive, its paths,
//! and for a chunk the byte count: the contents may be a secret
//! (`ctx.local_secret`), and these messages are rendered, logged and
//! shipped to the orchestrator. Every chunk is held in a buffer wiped on
//! drop, on both sides.
//!
//! Paths travel as the bytes of the OS string, base64, so a name that is not
//! UTF-8 works through a helper as it does on `Local`.
//!
//! Mutations are refused while the calling step is in its `check` phase, on
//! both sides: the main process guards before it builds the request, and
//! [`serve_helper`] refuses again after it arrives. The helper's copy is a
//! second latch against an op that reaches around the first one, not a
//! boundary against a hostile parent: it believes the request's `checking`
//! flag, and a parent that lies gets its mutation. That parent chose the
//! helper's binary and its user, so it had the authority already.
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

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::{Backend, CmdSpec, Local, Output, Staged, Stat, WriteAttrs, coded, errno_of};
use crate::launch::{self, Answer, Launch, Mode, Next};
use crate::protocol::{
    CHUNK_SIZE, FrameTooLarge, MAX_FRAME, encode_frame, encode_frame_sized, read_frame, write_body,
};
use crate::secret::{Secret, extend_wiping};

/// The most streams a helper keeps open at once (`[ISSUE-85]`). When that
/// many are open, a request that could open one more is refused before it
/// does anything (opens a file, runs a command), so a parent that forgets to
/// close its streams cannot run the helper out of descriptors or memory;
/// only a write of one chunk, which opens nothing, still goes through.
/// Closing a stream frees its slot. Far above what any op holds: an archive
/// extraction holds two.
const MAX_HANDLES: usize = 64;

/// How large the helper lets a frame and a chunk be. Always [`Limits::REAL`]
/// outside tests, which lower both so a test of a large file or a frame
/// limit does not build megabytes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Limits {
    /// The largest frame either side writes: [`MAX_FRAME`].
    pub(crate) frame: usize,
    /// The most bytes in one chunk of a stream: [`CHUNK_SIZE`].
    pub(crate) chunk: usize,
}

impl Limits {
    /// The limits every real helper runs with.
    pub(crate) const REAL: Limits = Limits {
        frame: MAX_FRAME,
        chunk: CHUNK_SIZE,
    };
}

/// Paths on the helper's wire: the bytes of the OS string, base64. A JSON
/// string cannot hold a name that is not UTF-8, and `Local` handles those, so
/// the helper must too.
mod wire_path {
    use std::ffi::OsString;
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use serde::{Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(p: &Path, s: S) -> Result<S::Ok, S::Error> {
        crate::protocol::b64::serialize(p.as_os_str().as_bytes(), s)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
        Ok(OsString::from_vec(crate::protocol::b64::deserialize(d)?).into())
    }
}

/// [`wire_path`] for an optional path.
mod wire_path_opt {
    use std::path::PathBuf;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(p: &Option<PathBuf>, s: S) -> Result<S::Ok, S::Error> {
        match p {
            Some(p) => super::wire_path::serialize(p, s),
            None => s.serialize_none(),
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<PathBuf>, D::Error> {
        #[derive(Deserialize)]
        struct Wrap(#[serde(with = "super::wire_path")] PathBuf);
        Ok(Option::<Wrap>::deserialize(d)?.map(|w| w.0))
    }
}

/// Optional bytes held in a zeroizing buffer, base64 on the wire.
mod b64_zeroizing_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use zeroize::Zeroizing;

    pub(super) fn serialize<S: Serializer>(
        bytes: &Option<Zeroizing<Vec<u8>>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        crate::protocol::b64_opt::serialize(bytes, s)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, D::Error> {
        #[derive(Deserialize)]
        struct Wrap(#[serde(with = "crate::protocol::b64_zeroizing")] Zeroizing<Vec<u8>>);
        Ok(Option::<Wrap>::deserialize(d)?.map(|w| w.0))
    }
}

/// One directory entry's name on the wire, as [`wire_path`] carries it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Name(#[serde(with = "wire_path")] PathBuf);

/// A [`CmdSpec`] on the helper's wire: its working directory as
/// [`wire_path`] bytes, and its stdin, when it is small enough to ride in
/// the request, in a zeroizing buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WireCmd {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    #[serde(with = "wire_path_opt")]
    cwd: Option<PathBuf>,
    #[serde(with = "b64_zeroizing_opt")]
    stdin: Option<Zeroizing<Vec<u8>>>,
    prefix: Vec<String>,
}

impl WireCmd {
    /// `spec` for the wire, with its stdin only when `inline`: a larger one
    /// has been staged in the helper already.
    fn of(spec: &CmdSpec, inline: bool) -> WireCmd {
        WireCmd {
            program: spec.program.clone(),
            args: spec.args.clone(),
            env: spec.env.clone(),
            cwd: spec.cwd.clone(),
            stdin: if inline {
                spec.stdin.as_ref().map(|s| Zeroizing::new(s.clone()))
            } else {
                None
            },
            prefix: spec.prefix.clone(),
        }
    }

    /// The command to run, its stdin being `staged` when there is one. The
    /// bytes move into the spec without a copy; whoever runs it wipes them.
    fn into_spec(self, staged: Option<Zeroizing<Vec<u8>>>) -> CmdSpec {
        CmdSpec {
            program: self.program,
            args: self.args,
            env: self.env,
            cwd: self.cwd,
            stdin: staged
                .or(self.stdin)
                .map(|mut bytes| std::mem::take(&mut *bytes)),
            prefix: self.prefix,
        }
    }

    fn argv(&self) -> String {
        self.prefix
            .iter()
            .chain([&self.program])
            .chain(&self.args)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// One `Backend` primitive, or one step of a stream, on the wire. The
/// helper's private format: both ends are always the same build
/// (`[ISSUE-62]`), so nothing outside this crate names it (`[ISSUE-85]`).
///
/// There is deliberately no request the trait cannot reach: a helper offers
/// the same surface as a local run, not a wider one. Paths travel exactly
/// as the op wrote them, resolved on the helper's side.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum HelperOp {
    /// Open a file for reading and return its first chunk:
    /// [`HelperResponse::Data`], with a handle only when there is more.
    ReadBegin {
        #[serde(with = "wire_path")]
        path: PathBuf,
    },
    /// The chunk at `offset` of an open file, or of a finished command's
    /// output: [`HelperResponse::Data`]. Positional, so the helper keeps no
    /// cursor the two sides could disagree on. The stream ends, and its
    /// handle with it, at the chunk that reaches the end.
    ReadChunk { handle: u32, offset: u64 },
    /// Stage a write of `path` with its first chunk. With `last` it is the
    /// whole content, committed at once ([`HelperResponse::Unit`]);
    /// otherwise the answer is the stream's [`HelperResponse::Handle`].
    WriteBegin {
        #[serde(with = "wire_path")]
        path: PathBuf,
        attrs: Option<WriteAttrs>,
        #[serde(with = "crate::protocol::b64_zeroizing")]
        bytes: Zeroizing<Vec<u8>>,
        last: bool,
    },
    /// The next chunk of a staged write. `offset` must be what was written
    /// so far, or the stream is abandoned with `EINVAL`. With `last`, the
    /// write is committed: synced, given its mode and owner, renamed.
    WriteChunk {
        handle: u32,
        offset: u64,
        #[serde(with = "crate::protocol::b64_zeroizing")]
        bytes: Zeroizing<Vec<u8>>,
        last: bool,
    },
    /// Abandon a staged write: its temporary file goes, the target stays as
    /// it was. A handle that is not open is fine, so it is always safe.
    WriteAbort { handle: u32 },
    /// Release any stream. A handle that is not open is fine.
    Close { handle: u32 },
    /// [`Backend::stat`]: `lstat`, so a symlink reports itself.
    Stat {
        #[serde(with = "wire_path")]
        path: PathBuf,
    },
    /// [`Backend::stat_follow`]: follows the link and reports what it lands
    /// on. A dangling link answers `None`, exactly as a missing path does.
    StatFollow {
        #[serde(with = "wire_path")]
        path: PathBuf,
    },
    /// [`Backend::mkdir_all`]. Every missing parent is created too, and all
    /// of them belong to the helper's user with the helper's umask.
    MkdirAll {
        #[serde(with = "wire_path")]
        path: PathBuf,
    },
    /// [`Backend::remove`]: one entry, and a populated directory is an
    /// error.
    Remove {
        #[serde(with = "wire_path")]
        path: PathBuf,
    },
    /// [`Backend::remove_all`]. The only recursive delete a helper will do,
    /// and so the one request where a wrong path costs a tree.
    RemoveAll {
        #[serde(with = "wire_path")]
        path: PathBuf,
    },
    /// [`Backend::rename`]. Both ends are the helper's, and it is one
    /// syscall, so it cannot cross filesystems.
    Rename {
        #[serde(with = "wire_path")]
        from: PathBuf,
        #[serde(with = "wire_path")]
        to: PathBuf,
    },
    /// [`Backend::set_mode`], following a symlink.
    SetMode {
        #[serde(with = "wire_path")]
        path: PathBuf,
        mode: u32,
    },
    /// [`Backend::set_owner`], following a symlink. Handing a file to a
    /// third user needs the helper to be root.
    SetOwner {
        #[serde(with = "wire_path")]
        path: PathBuf,
        uid: u32,
        gid: u32,
    },
    /// [`Backend::copy`]. Both ends are on the target host and the bytes
    /// never touch the wire. The copy is created new: an existing path, a
    /// symlink included, is refused with `AlreadyExists` and left as it was.
    /// Not atomic, but a failure part way removes what it created.
    Copy {
        #[serde(with = "wire_path")]
        from: PathBuf,
        #[serde(with = "wire_path")]
        to: PathBuf,
    },
    /// [`Backend::symlink`]. Fails if `link` exists.
    Symlink {
        #[serde(with = "wire_path")]
        target: PathBuf,
        #[serde(with = "wire_path")]
        link: PathBuf,
    },
    /// [`Backend::read_link`]. Failing rather than answering `None` is how a
    /// caller learns `path` is not a link.
    ReadLink {
        #[serde(with = "wire_path")]
        path: PathBuf,
    },
    /// Open a directory and return the first batch of its entries' names:
    /// [`HelperResponse::Names`], with a handle only when there is more.
    ReadDirBegin {
        #[serde(with = "wire_path")]
        path: PathBuf,
    },
    /// The next batch of an open directory's names. The stream ends, and
    /// its handle with it, at the batch that reaches the end.
    ReadDirChunk { handle: u32 },
    /// Stage the first chunk of a command's stdin that is too large to ride
    /// in its [`HelperOp::Spawn`]: [`HelperResponse::Handle`].
    StdinBegin {
        #[serde(with = "crate::protocol::b64_zeroizing")]
        bytes: Zeroizing<Vec<u8>>,
    },
    /// The next chunk of a staged stdin; `offset` must be what was staged
    /// so far.
    StdinChunk {
        handle: u32,
        offset: u64,
        #[serde(with = "crate::protocol::b64_zeroizing")]
        bytes: Zeroizing<Vec<u8>>,
    },
    /// [`Backend::spawn`]. The command runs as the helper's user with no
    /// further `sudo`, so its prefix is empty on this path; `sys.cmd` only
    /// fills it in for a `Fake` system. Its stdin rides in `cmd`, or was
    /// staged as the stream `stdin`, which this consumes. Output of one
    /// chunk or less comes back as [`HelperResponse::Output`]; more is kept
    /// behind a handle ([`HelperResponse::OutputHandle`]) and read with
    /// [`HelperOp::ReadChunk`].
    Spawn { cmd: WireCmd, stdin: Option<u32> },
}

impl HelperOp {
    /// The request and the paths it touches, for messages. Never the bytes:
    /// a chunk may be a secret (`ctx.local_secret`), so it is a count.
    pub(crate) fn label(&self) -> String {
        let one = |verb: &str, p: &Path| format!("{verb} {}", p.display());
        let two =
            |verb: &str, a: &Path, b: &Path| format!("{verb} {} -> {}", a.display(), b.display());
        match self {
            HelperOp::ReadBegin { path } => one("read", path),
            HelperOp::ReadChunk { handle, offset } => {
                format!("read stream {handle} at {offset}")
            }
            HelperOp::WriteBegin {
                path, bytes, last, ..
            } => {
                let what = if *last { "" } else { "first " };
                format!("write {} ({what}{} bytes)", path.display(), bytes.len())
            }
            HelperOp::WriteChunk {
                handle,
                offset,
                bytes,
                ..
            } => format!("write stream {handle} ({} bytes at {offset})", bytes.len()),
            HelperOp::WriteAbort { handle } => format!("abort write stream {handle}"),
            HelperOp::Close { handle } => format!("close stream {handle}"),
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
            HelperOp::ReadDirBegin { path } => one("read_dir", path),
            HelperOp::ReadDirChunk { handle } => format!("read_dir stream {handle}"),
            HelperOp::StdinBegin { bytes } => {
                format!("stage stdin (first {} bytes)", bytes.len())
            }
            HelperOp::StdinChunk {
                handle,
                offset,
                bytes,
            } => format!(
                "stage stdin stream {handle} ({} bytes at {offset})",
                bytes.len()
            ),
            HelperOp::Spawn { cmd, .. } => format!("spawn {}", cmd.argv()),
        }
    }

    /// About what a request carrying this encodes to, for
    /// [`encode_frame_sized`]: the base64 of its bytes and paths, and room
    /// for the envelope. Nothing for a request without bulk, which is small.
    fn size_hint(&self) -> usize {
        let b64 = |n: usize| base64_len(n as u64) as usize;
        let path = |p: &Path| b64(p.as_os_str().len());
        match self {
            HelperOp::WriteBegin { path: p, bytes, .. } => b64(bytes.len()) + path(p) + 256,
            HelperOp::WriteChunk { bytes, .. }
            | HelperOp::StdinBegin { bytes }
            | HelperOp::StdinChunk { bytes, .. } => b64(bytes.len()) + 256,
            HelperOp::Spawn { cmd, .. } => {
                let text: usize = cmd
                    .prefix
                    .iter()
                    .chain([&cmd.program])
                    .chain(&cmd.args)
                    .chain(cmd.env.iter().flat_map(|(k, v)| [k, v]))
                    .map(|a| a.len() + 4)
                    .sum();
                b64(cmd.stdin.as_ref().map_or(0, |s| s.len()))
                    + cmd.cwd.as_deref().map_or(0, path)
                    + text * 2
                    + 256
            }
            _ => 0,
        }
    }

    /// Mutations are refused by the helper while the main process is in a
    /// step's `check` phase: the guard holds on both sides (vision doc 11.3).
    /// Releasing a stream is never one, so it is always allowed, and neither
    /// is staging a command's stdin, since commands are not guarded.
    pub(crate) fn mutates(&self) -> bool {
        !matches!(
            self,
            HelperOp::ReadBegin { .. }
                | HelperOp::ReadChunk { .. }
                | HelperOp::WriteAbort { .. }
                | HelperOp::Close { .. }
                | HelperOp::Stat { .. }
                | HelperOp::StatFollow { .. }
                | HelperOp::ReadLink { .. }
                | HelperOp::ReadDirBegin { .. }
                | HelperOp::ReadDirChunk { .. }
                | HelperOp::StdinBegin { .. }
                | HelperOp::StdinChunk { .. }
                | HelperOp::Spawn { .. }
        )
    }
}

/// Main process -> helper.
///
/// One request, one response, in lockstep down one pipe: [`Elevated`] holds
/// the connection lock across both halves, so a helper never has two of
/// these in flight and never has to correlate them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HelperRequest {
    /// True while the requesting step is in `check`; mutations are refused.
    pub(crate) checking: bool,
    /// The primitive to perform, or the step of a stream.
    pub(crate) op: HelperOp,
}

/// Helper -> main process.
///
/// The request decides the shape: [`Elevated`] knows which variant each
/// request should come back as and turns anything else into
/// `unexpected helper response`, so a helper built from different source is
/// a failed step rather than a misread value. [`Err`](Self::Err) can answer
/// any request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum HelperResponse {
    /// Done, nothing to return.
    Unit,
    /// From [`HelperOp::Stat`] or [`HelperOp::StatFollow`]. `None` means the
    /// path is not there, which is an answer and not a failure.
    Stat(Option<Stat>),
    /// A link target, from [`HelperOp::ReadLink`].
    Path(#[serde(with = "wire_path")] PathBuf),
    /// A stream the helper now holds open.
    Handle(u32),
    /// A chunk of a file or of a command's output. `handle` is the stream
    /// when there is more, and `None` once this chunk reached the end, at
    /// which point the helper has released it.
    Data {
        handle: Option<u32>,
        #[serde(with = "crate::protocol::b64_zeroizing")]
        bytes: Zeroizing<Vec<u8>>,
    },
    /// A batch of a directory's names, `handle` as for [`Data`](Self::Data).
    Names {
        handle: Option<u32>,
        names: Vec<Name>,
    },
    /// A finished command whose output fits one chunk. A non-zero exit
    /// arrives here and not in [`Err`](Self::Err): the command ran, and what
    /// it did is the op's business.
    Output(Output),
    /// A finished command whose output is larger, kept behind `handle` as
    /// its stdout then its stderr, `stdout` and `stderr` bytes long.
    OutputHandle {
        handle: u32,
        status: i32,
        signal: Option<i32>,
        stdout: u64,
        stderr: u64,
    },
    /// The request failed, or the helper refused it. Carries no payload, so
    /// nothing the op was writing can come back inside an error message.
    Err {
        /// The OS errno when there was one, so `NotFound` and friends
        /// survive. [`serve_helper`]'s own refusals of an answer set `EFBIG`
        /// (too large for a frame) or `EILSEQ` (cannot be encoded), which
        /// tells [`Elevated`] to name the request and the account in front.
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
            code: errno_of(&e),
            message: e.to_string(),
        }
    }

    fn into_io(self) -> io::Result<HelperResponse> {
        match self {
            HelperResponse::Err { code, message } => Err(match code {
                Some(c) => coded(c, message),
                None => io::Error::other(message),
            }),
            other => Ok(other),
        }
    }

    /// The variant's name, for a message about an answer of the wrong
    /// shape: never its contents, which may be a file's.
    fn kind(&self) -> &'static str {
        match self {
            HelperResponse::Unit => "Unit",
            HelperResponse::Stat(_) => "Stat",
            HelperResponse::Path(_) => "Path",
            HelperResponse::Handle(_) => "Handle",
            HelperResponse::Data { .. } => "Data",
            HelperResponse::Names { .. } => "Names",
            HelperResponse::Output(_) => "Output",
            HelperResponse::OutputHandle { .. } => "OutputHandle",
            HelperResponse::Err { .. } => "Err",
        }
    }

    /// About what this answer encodes to, for [`encode_frame_sized`]: a
    /// chunk's base64, a batch of names, a command's output, and room for
    /// the envelope. Nothing for an answer without bulk.
    fn size_hint(&self) -> usize {
        match self {
            HelperResponse::Data { bytes, .. } => base64_len(bytes.len() as u64) as usize + 128,
            HelperResponse::Names { names, .. } => {
                names
                    .iter()
                    .map(|n| base64_len(n.0.as_os_str().len() as u64) as usize + 3)
                    .sum::<usize>()
                    + 128
            }
            HelperResponse::Output(o) => encoded_output_len(o) as usize,
            _ => 0,
        }
    }

    /// Wipe the bytes a command printed once they are encoded: the one
    /// answer whose payload is not already in a zeroizing buffer, because
    /// [`Output`] is the public type every backend returns.
    fn wipe(&mut self) {
        if let HelperResponse::Output(o) = self {
            o.stdout.zeroize();
            o.stderr.zeroize();
        }
    }
}

/// Serve requests from `rx` on a `Local` backend until EOF. This is what
/// `--helper` runs; tests run it on a pipe in a thread.
///
/// A failed primitive is an error answer, not a return: the loop ends only
/// when the far end closes the pipe, and the `io::Result` reports a broken
/// channel rather than anything a playbook asked for. A request that
/// mutates while its step is checking is refused before it reaches the
/// filesystem, and the refusal quotes the request's label, never a payload.
///
/// The streams it holds open live in a table here, at most 64 of them. When
/// the far end goes away, the loop ends and the table with it: every staged
/// write removes its temporary file, and nothing is renamed.
///
/// No frame this writes is larger than [`MAX_FRAME`], the most the far end
/// reads. Nothing it answers comes near that, since every bulk answer is a
/// chunk of 1 MiB, but the check stays: an answer that would pass it, or
/// that cannot be encoded at all, is replaced by an error saying why, so it
/// is a refusal of that one request and the helper goes on serving. Sending
/// it, or exiting, would leave a pipe the far end cannot read past, which is
/// a dead helper for the rest of the run. These refusals quote nothing from
/// the request, so they fit a frame of any size that can carry the bare
/// error, and they carry a marker (`EFBIG` for an answer too large, `EILSEQ`
/// for one that cannot be encoded) on which [`Elevated`] puts the request
/// and the account in front of the message.
///
/// `tx` is the frame stream and nothing else may write to it. Under
/// `--helper` that is the process's stdout, which is why the helper's own
/// diagnostics go to stderr.
pub fn serve_helper<R: Read, W: Write>(rx: &mut R, tx: &mut W) -> io::Result<()> {
    serve(rx, tx, Limits::REAL)
}

/// The marker on a refusal of an answer too large for a frame.
fn too_large_code() -> i32 {
    rustix::io::Errno::FBIG.raw_os_error()
}

/// The marker on a refusal of an answer that cannot be encoded.
fn unencodable_code() -> i32 {
    rustix::io::Errno::ILSEQ.raw_os_error()
}

/// [`serve_helper`], with the limits as a parameter, so a test can use
/// small chunks and frames.
fn serve<R: Read, W: Write>(rx: &mut R, tx: &mut W, limits: Limits) -> io::Result<()> {
    let local = Local;
    let mut table = Table::default();
    while let Some(req) = read_frame::<_, HelperRequest>(rx)? {
        let mut resp = if req.checking && req.op.mutates() {
            // A refused chunk ends its write, as any failed one does; a
            // stream of another kind under that handle is not its to end.
            if let HelperOp::WriteChunk { handle, .. } = &req.op
                && matches!(table.streams.get(handle), Some(Stream::Write(_)))
            {
                table.streams.remove(handle);
            }
            HelperResponse::Err {
                code: None,
                message: format!(
                    "mutation during check refused by helper: {}",
                    req.op.label()
                ),
            }
        } else {
            table
                .answer(&local, req.op, limits)
                .unwrap_or_else(HelperResponse::from_io)
        };
        let encoded = encode_frame_sized(&resp, limits.frame, resp.size_hint());
        resp.wipe();
        let body = match encoded {
            Ok(Some(body)) => body,
            Ok(None) => refuse(
                too_large_code(),
                format!(
                    "the answer is more than one helper frame holds ({} bytes)",
                    limits.frame
                ),
                limits.frame,
            )?,
            Err(e) => refuse(
                unencodable_code(),
                format!("the helper's answer cannot be encoded: {e}"),
                limits.frame,
            )?,
        };
        write_body(tx, &body)?;
    }
    Ok(())
}

/// One stream the helper holds open between requests.
enum Stream {
    /// A write staged beside its target.
    Write(Staged),
    /// A file open for reading, and where its own cursor is: a read at the
    /// cursor is a plain `read`, which a FIFO needs, and any other is a
    /// positional `pread`.
    Read { file: std::fs::File, cursor: u64 },
    /// A directory being listed.
    Dir(std::iter::Peekable<std::fs::ReadDir>),
    /// A command's stdin, staged until its `Spawn`.
    Stdin(Zeroizing<Vec<u8>>),
    /// A finished command's output, read as its stdout then its stderr.
    Output {
        stdout: Zeroizing<Vec<u8>>,
        stderr: Zeroizing<Vec<u8>>,
    },
}

impl Stream {
    /// Up to `chunk` bytes at `offset`, and whether they reach the end.
    fn chunk_at(&mut self, offset: u64, chunk: usize) -> io::Result<(Zeroizing<Vec<u8>>, bool)> {
        match self {
            Stream::Read { file, cursor } => {
                let mut buf = Zeroizing::new(vec![0u8; chunk]);
                let mut n = 0;
                while n < chunk {
                    let at = offset + n as u64;
                    let got = if at == *cursor {
                        let got = retry(|| file.read(&mut buf[n..]))?;
                        *cursor += got as u64;
                        got
                    } else {
                        retry(|| file.read_at(&mut buf[n..], at))?
                    };
                    if got == 0 {
                        break;
                    }
                    n += got;
                }
                buf.truncate(n);
                // A full chunk may or may not be the last: one byte past it
                // says, so a file of exactly one chunk is still one round
                // trip. Something that cannot be read at an offset (a FIFO)
                // is taken to have more, and its next chunk comes back empty.
                let eof = n < chunk || matches!(file.read_at(&mut [0u8], offset + n as u64), Ok(0));
                Ok((buf, eof))
            }
            Stream::Output { stdout, stderr } => {
                let total = (stdout.len() + stderr.len()) as u64;
                let start = offset.min(total) as usize;
                let end = (start + chunk).min(total as usize);
                let mut buf = Zeroizing::new(Vec::with_capacity(end - start));
                let split = stdout.len();
                if start < split {
                    buf.extend_from_slice(&stdout[start..end.min(split)]);
                }
                if end > split {
                    buf.extend_from_slice(&stderr[start.max(split) - split..end - split]);
                }
                Ok((buf, end == total as usize))
            }
            _ => unreachable!("taken as a readable stream"),
        }
    }
}

/// `f`, retried while it is interrupted by a signal.
fn retry(mut f: impl FnMut() -> io::Result<usize>) -> io::Result<usize> {
    loop {
        match f() {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            other => return other,
        }
    }
}

/// The streams one helper holds open, by the handle it gave each.
#[derive(Default)]
struct Table {
    streams: BTreeMap<u32, Stream>,
    /// The last handle given out; the next is the next free one after it.
    last: u32,
}

impl Table {
    /// Refuse to open another stream when [`MAX_HANDLES`] are open.
    fn room(&self) -> io::Result<()> {
        if self.streams.len() < MAX_HANDLES {
            return Ok(());
        }
        Err(coded(
            rustix::io::Errno::MFILE.raw_os_error(),
            format!(
                "the helper already holds {MAX_HANDLES} open streams, the most it keeps at \
                 once; one has to be finished or closed before another is opened"
            ),
        ))
    }

    /// Hold `stream` open and return its handle.
    fn open(&mut self, stream: Stream) -> io::Result<u32> {
        self.room()?;
        loop {
            self.last = self.last.wrapping_add(1);
            if self.last != 0 && !self.streams.contains_key(&self.last) {
                self.streams.insert(self.last, stream);
                return Ok(self.last);
            }
        }
    }

    /// Take the open stream `handle` out of the table, when it is of the
    /// kind `fits` accepts; `EBADF` otherwise, and the table as it was. The
    /// caller puts it back when the stream goes on, so a stream whose
    /// request failed is gone.
    fn take(&mut self, handle: u32, what: &str, fits: fn(&Stream) -> bool) -> io::Result<Stream> {
        match self.streams.get(&handle) {
            Some(s) if fits(s) => Ok(self.streams.remove(&handle).expect("present")),
            _ => Err(coded(
                rustix::io::Errno::BADF.raw_os_error(),
                format!(
                    "the helper has no open {what} stream {handle}: it ended, failed, or was \
                     never opened"
                ),
            )),
        }
    }

    /// Perform one request on `local`.
    fn answer(
        &mut self,
        local: &Local,
        op: HelperOp,
        limits: Limits,
    ) -> io::Result<HelperResponse> {
        Ok(match op {
            HelperOp::ReadBegin { path } => {
                // Before the file is opened: opening a FIFO, or reading a
                // device, is not undone by a refusal afterwards.
                self.room()?;
                let file = std::fs::File::open(&path)?;
                let mut stream = Stream::Read { file, cursor: 0 };
                let (bytes, eof) = stream.chunk_at(0, limits.chunk)?;
                let handle = if eof { None } else { Some(self.open(stream)?) };
                HelperResponse::Data { handle, bytes }
            }
            HelperOp::ReadChunk { handle, offset } => {
                let mut stream = self.take(handle, "read", |s| {
                    matches!(s, Stream::Read { .. } | Stream::Output { .. })
                })?;
                let (bytes, eof) = stream.chunk_at(offset, limits.chunk)?;
                let handle = if eof {
                    None
                } else {
                    self.streams.insert(handle, stream);
                    Some(handle)
                };
                HelperResponse::Data { handle, bytes }
            }
            HelperOp::WriteBegin {
                path,
                attrs,
                bytes,
                last,
            } => {
                if !last {
                    self.room()?;
                }
                let mut staged = Staged::begin(&path, attrs)?;
                staged.write(&bytes)?;
                if last {
                    staged.commit()?;
                    HelperResponse::Unit
                } else {
                    HelperResponse::Handle(self.open(Stream::Write(staged))?)
                }
            }
            HelperOp::WriteChunk {
                handle,
                offset,
                bytes,
                last,
            } => {
                let Stream::Write(mut staged) =
                    self.take(handle, "write", |s| matches!(s, Stream::Write(_)))?
                else {
                    unreachable!("taken as a write")
                };
                if offset != staged.written() {
                    return Err(coded(
                        rustix::io::Errno::INVAL.raw_os_error(),
                        format!(
                            "write stream {handle}: a chunk at offset {offset}, but {} bytes \
                             were written so far; the write is abandoned and nothing was \
                             replaced",
                            staged.written()
                        ),
                    ));
                }
                staged.write(&bytes)?;
                if last {
                    staged.commit()?;
                } else {
                    self.streams.insert(handle, Stream::Write(staged));
                }
                HelperResponse::Unit
            }
            // Abandoning a write that is not open is fine, so it is always
            // safe to send; one under a handle that is not a write is
            // refused and left alone.
            HelperOp::WriteAbort { handle } => match self.streams.get(&handle) {
                None | Some(Stream::Write(_)) => {
                    self.streams.remove(&handle);
                    HelperResponse::Unit
                }
                Some(_) => {
                    return Err(coded(
                        rustix::io::Errno::BADF.raw_os_error(),
                        format!("stream {handle} is not a write, so it is not abandoned as one"),
                    ));
                }
            },
            HelperOp::Close { handle } => {
                self.streams.remove(&handle);
                HelperResponse::Unit
            }
            HelperOp::Stat { path } => HelperResponse::Stat(local.stat(&path)?),
            HelperOp::StatFollow { path } => HelperResponse::Stat(local.stat_follow(&path)?),
            HelperOp::MkdirAll { path } => unit(local.mkdir_all(&path))?,
            HelperOp::Remove { path } => unit(local.remove(&path))?,
            HelperOp::RemoveAll { path } => unit(local.remove_all(&path))?,
            HelperOp::Rename { from, to } => unit(local.rename(&from, &to))?,
            HelperOp::SetMode { path, mode } => unit(local.set_mode(&path, mode))?,
            HelperOp::SetOwner { path, uid, gid } => unit(local.set_owner(&path, uid, gid))?,
            HelperOp::Copy { from, to } => unit(local.copy(&from, &to))?,
            HelperOp::Symlink { target, link } => unit(local.symlink(&target, &link))?,
            HelperOp::ReadLink { path } => HelperResponse::Path(local.read_link(&path)?),
            HelperOp::ReadDirBegin { path } => {
                self.room()?;
                let mut dir = std::fs::read_dir(&path)?.peekable();
                let names = batch(&mut dir, limits.chunk)?;
                let handle = match dir.peek() {
                    None => None,
                    Some(_) => Some(self.open(Stream::Dir(dir))?),
                };
                HelperResponse::Names { handle, names }
            }
            HelperOp::ReadDirChunk { handle } => {
                let Stream::Dir(mut dir) =
                    self.take(handle, "read_dir", |s| matches!(s, Stream::Dir(_)))?
                else {
                    unreachable!("taken as a listing")
                };
                let names = batch(&mut dir, limits.chunk)?;
                let handle = match dir.peek() {
                    None => None,
                    Some(_) => {
                        self.streams.insert(handle, Stream::Dir(dir));
                        Some(handle)
                    }
                };
                HelperResponse::Names { handle, names }
            }
            HelperOp::StdinBegin { bytes } => {
                HelperResponse::Handle(self.open(Stream::Stdin(bytes))?)
            }
            HelperOp::StdinChunk {
                handle,
                offset,
                bytes,
            } => {
                let Stream::Stdin(mut staged) =
                    self.take(handle, "stdin", |s| matches!(s, Stream::Stdin(_)))?
                else {
                    unreachable!("taken as stdin")
                };
                if offset != staged.len() as u64 {
                    return Err(coded(
                        rustix::io::Errno::INVAL.raw_os_error(),
                        format!(
                            "stdin stream {handle}: a chunk at offset {offset}, but {} bytes \
                             were staged so far; the stdin is abandoned",
                            staged.len()
                        ),
                    ));
                }
                crate::secret::extend_wiping(&mut staged, &bytes);
                self.streams.insert(handle, Stream::Stdin(staged));
                HelperResponse::Unit
            }
            HelperOp::Spawn { cmd, stdin } => {
                // Output too large for one answer needs a slot once the
                // command has run, and refusing it then would throw away
                // what the command did; so a full table refuses it first.
                // The stdin it consumes frees one, so that counts.
                if stdin.is_none_or(|h| !self.streams.contains_key(&h)) {
                    self.room()?;
                }
                let staged = match stdin {
                    Some(handle) => {
                        match self.take(handle, "stdin", |s| matches!(s, Stream::Stdin(_)))? {
                            Stream::Stdin(bytes) => Some(bytes),
                            _ => unreachable!("taken as stdin"),
                        }
                    }
                    None => None,
                };
                let mut spec = cmd.into_spec(staged);
                let ran = local.spawn(&spec);
                spec.stdin.zeroize();
                let mut out = ran?;
                if out.stdout.len() + out.stderr.len() <= limits.chunk
                    && encoded_output_len(&out) <= limits.frame as u64
                {
                    return Ok(HelperResponse::Output(out));
                }
                let (stdout, stderr) = (out.stdout.len() as u64, out.stderr.len() as u64);
                let handle = self.open(Stream::Output {
                    stdout: Zeroizing::new(std::mem::take(&mut out.stdout)),
                    stderr: Zeroizing::new(std::mem::take(&mut out.stderr)),
                })?;
                HelperResponse::OutputHandle {
                    handle,
                    status: out.status,
                    signal: out.signal,
                    stdout,
                    stderr,
                }
            }
        })
    }
}

/// The next names of `dir`, as many as fit in `budget` bytes of encoded
/// answer, and at least one when there is one.
fn batch(dir: &mut std::iter::Peekable<std::fs::ReadDir>, budget: usize) -> io::Result<Vec<Name>> {
    let mut names = Vec::new();
    let mut size = 0;
    while let Some(next) = dir.peek() {
        // Its base64, its quotes and a comma.
        let cost = match next {
            Ok(entry) => base64_len(entry.file_name().len() as u64) as usize + 3,
            Err(_) => 0,
        };
        if !names.is_empty() && size + cost > budget {
            break;
        }
        let entry = dir.next().expect("peeked")?;
        names.push(Name(entry.file_name().into()));
        size += cost;
    }
    Ok(names)
}

/// The length of `size` bytes in base64: four characters for every three
/// bytes or part of three.
fn base64_len(size: u64) -> u64 {
    size.div_ceil(3) * 4
}

/// The encoded length of a [`HelperResponse::Output`] carrying `o`, exactly,
/// without encoding the output: the envelope with both streams empty, which
/// is a few dozen bytes to encode, plus their base64. Base64 needs no JSON
/// escaping, so the two add up.
fn encoded_output_len(o: &Output) -> u64 {
    let empty = HelperResponse::Output(Output {
        status: o.status,
        signal: o.signal,
        stdout: Vec::new(),
        stderr: Vec::new(),
    });
    let envelope = serde_json::to_vec(&empty).map_or(0, |v| v.len()) as u64;
    envelope + base64_len(o.stdout.len() as u64) + base64_len(o.stderr.len() as u64)
}

/// The frame carrying a refusal marked `code`: `message`, or, at a limit
/// too small for it, the bare marker. Fails only at a limit too small for
/// the bare error, which ends the helper.
fn refuse(code: i32, message: String, max_frame: usize) -> io::Result<Zeroizing<Vec<u8>>> {
    for message in [message, String::new()] {
        let refusal = HelperResponse::Err {
            code: Some(code),
            message,
        };
        if let Some(body) = encode_frame(&refusal, max_frame)? {
            return Ok(body);
        }
    }
    Err(io::Error::other(format!(
        "not even a bare refusal fits in a {max_frame}-byte frame"
    )))
}

/// `s`, or its first `max` bytes (backed off to a character boundary) and
/// an ellipsis. Messages quote a request's label through this, so an argv
/// of a megabyte does not become a message of one.
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

fn unit(r: io::Result<()>) -> io::Result<HelperResponse> {
    r.map(|()| HelperResponse::Unit)
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
                    {
                        let mut t = tail.lock().unwrap();
                        if t.len() >= 5 {
                            t.remove(0);
                        }
                        t.push(line.clone());
                    }
                    if echo.load(Ordering::SeqCst) {
                        // One string, so one write, where `eprintln!` with
                        // the pieces writes each: a process that exits
                        // between them leaves a bare `[` behind (#90). The
                        // macro, not `io::stderr()`, so tests capture it.
                        let msg = format!("[{label}] {line}\n");
                        eprint!("{msg}");
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
                    settle(&mut self.reader);
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
        settle(&mut self.reader);
        (status, self.tail.lock().unwrap().join("\n"))
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
            stderr_reader: self.reader.take(),
            note: self.note,
        }
    }
}

/// How long a report of a dead child waits for its stderr reader.
const SETTLE: Duration = Duration::from_secs(2);

/// Wait for a child's stderr reader to reach EOF, briefly: the report
/// quotes its tail, and a line still in the pipe is usually the one that
/// says why. A child that has exited closed its end, so EOF comes at once;
/// a report built from the tail as it stood when the exit was seen lost
/// that line now and then (#90). Bounded, because something the child left
/// running may hold the pipe open; then the reader is let go, so nothing
/// waits for it a second time.
fn settle(reader: &mut Option<std::thread::JoinHandle<()>>) {
    let Some(r) = reader.take() else { return };
    let t0 = Instant::now();
    while !r.is_finished() {
        if t0.elapsed() > SETTLE {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = r.join();
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
    /// The thread filling `stderr_tail`, which a report waits for.
    stderr_reader: Option<std::thread::JoinHandle<()>>,
    /// [`Spawner::note`], carried to where the failure is described.
    note: Option<String>,
}

impl Connection {
    /// Send one request, already encoded as a frame body, and read its
    /// answer.
    fn call(&mut self, body: &[u8]) -> io::Result<HelperResponse> {
        let answer = write_body(&mut self.tx, body)
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
        settle(&mut self.stderr_reader);
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

    /// Close stdin so the helper's loop ends, wait briefly, then kill. Its
    /// last lines of stderr are echoed before this returns, so a run that
    /// ends next does not cut them off.
    ///
    /// Its stdout is closed too before the wait for stderr: a helper killed
    /// under `sudo` may leave a process blocked writing to it, which keeps
    /// stderr open until it gets `EPIPE`.
    fn shutdown(mut self) {
        self.tx = Box::new(io::sink());
        if let Some(mut child) = self.child.take() {
            let t0 = Instant::now();
            let exited = loop {
                if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                    break true;
                }
                if t0.elapsed() >= Duration::from_secs(2) {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(20));
            };
            if !exited {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        self.rx = Box::new(io::empty());
        settle(&mut self.stderr_reader);
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
/// It narrows nothing, and it limits no size. The far side is a full
/// [`Local`] running as that user, so a request is bounded by that user's
/// permissions and by nothing this type adds, and a file, a listing or a
/// command's input and output of any size crosses in chunks of 1 MiB (see
/// the module doc). What it does add is refusals: a mutation while the step
/// is checking, a request larger than one frame or that cannot be encoded
/// (a command line of tens of megabytes), and every primitive after the
/// first failure of the helper itself. Only the last is a latch; the others
/// refuse one request.
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
    /// The largest request frame sent: [`MAX_FRAME`], lowered by tests.
    max_frame: usize,
    /// The most bytes this side puts in one chunk of a write or of a
    /// command's stdin: [`CHUNK_SIZE`], lowered by tests.
    chunk: usize,
}

/// A mutex's guard whether or not another thread panicked holding it. The
/// state behind these locks is a connection and a latch, both still
/// meaningful after a panic elsewhere, and cleanup that runs while
/// unwinding must not panic again.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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
            max_frame: MAX_FRAME,
            chunk: CHUNK_SIZE,
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
                stderr_reader: None,
                note: None,
            })),
            failed: Mutex::new(None),
            max_frame: MAX_FRAME,
            chunk: CHUNK_SIZE,
        }
    }

    /// An `Elevated` over [`serve_helper`] running on a thread of this
    /// process, joined to it by two pipes: a real helper loop and a real
    /// [`Local`], as this process's own user, without `sudo`. `wrap_tx` and
    /// `wrap_rx` may put something between this side and the pipes, which
    /// is how a test watches the frames. The thread ends when this is
    /// dropped and the helper sees EOF.
    pub(crate) fn in_process_with(
        user: &str,
        phase: Arc<AtomicU8>,
        limits: Limits,
        wrap_tx: impl FnOnce(io::PipeWriter) -> Box<dyn Write + Send>,
        wrap_rx: impl FnOnce(io::PipeReader) -> Box<dyn Read + Send>,
    ) -> io::Result<Elevated> {
        let (req_r, req_w) = io::pipe()?;
        let (resp_r, resp_w) = io::pipe()?;
        std::thread::spawn(move || {
            let (mut rx, mut tx) = (req_r, resp_w);
            let _ = serve(&mut rx, &mut tx, limits);
        });
        let mut e = Elevated::connected(user, wrap_tx(req_w), wrap_rx(resp_r), phase);
        e.max_frame = limits.frame;
        e.chunk = limits.chunk;
        Ok(e)
    }

    /// The user the helper runs as, as messages name it. Never the calling
    /// process's own user:
    /// [`System::as_user`](crate::system::System::as_user) hands back the
    /// plain local backend for that one instead of building this.
    pub fn user(&self) -> &str {
        &self.user
    }

    fn call(&self, op: HelperOp) -> io::Result<HelperResponse> {
        self.call_with(op, true)
    }

    /// Release a stream this side is giving up on, best effort: an error is
    /// ignored, no helper is spawned for it, and a poisoned lock is not a
    /// panic, so it is safe from a `Drop` that runs while unwinding.
    fn release(&self, op: HelperOp) {
        let _ = self.call_with(op, false);
    }

    /// Send `op` and return the answer. Spawns the helper when there is none
    /// and `spawn` allows.
    fn call_with(&self, op: HelperOp, spawn: bool) -> io::Result<HelperResponse> {
        let checking = self.phase.load(Ordering::SeqCst) == crate::system::Phase::Checking as u8;
        if let Some(first) = lock(&self.failed).as_ref() {
            return Err(io::Error::other(HelperGone {
                what: format!(
                    "the helper running as `{}` failed earlier and is not retried: {}",
                    self.user, first.what
                ),
                note: first.note.clone(),
            }));
        }
        // A request's label is quoted in front of every refusal below; cut,
        // so an argv of megabytes does not become a message of megabytes.
        let label = cut(&op.label(), 512);
        // Encoded before anything is written, so a request that cannot be
        // sent is refused with the stream intact: no shutdown, no latch.
        let req = HelperRequest { checking, op };
        let body = match encode_frame_sized(&req, self.max_frame, req.op.size_hint()) {
            Ok(Some(body)) => body,
            Ok(None) => {
                return Err(io::Error::other(format!(
                    "{label} as `{}`: the request is more than one helper frame holds \
                     ({} bytes) once encoded; a request this large cannot be sent through \
                     `as_user`/`as_root`, so pass large input to a command in a file on the \
                     target or on its stdin rather than in its arguments",
                    self.user, self.max_frame
                )));
            }
            Err(e) => {
                return Err(io::Error::other(format!(
                    "{label} as `{}`: the request cannot be encoded: {e}",
                    self.user
                )));
            }
        };
        let mut guard = lock(&self.conn);
        if guard.is_none() {
            let spawner = self
                .spawner
                .as_ref()
                .filter(|_| spawn)
                .ok_or_else(|| io::Error::other("helper connection is closed"))?;
            *guard = Some(self.latch(spawner.spawn(&self.user))?);
        }
        let conn = guard.as_mut().expect("connected");
        let resp = conn.call(&body).map_err(|e| {
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
                     than the {MAX_FRAME} a frame may hold. A helper refuses an answer \
                     that large instead of sending it, so the stream between the two is \
                     out of step: most likely something else wrote to the helper's stdout, \
                     such as a banner from sudo or PAM, or the stream was corrupted. Check \
                     what the host's sudoers and PAM configuration print for `{}`",
                    self.user, big.len, self.user
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
        drop(guard);
        match self.latch(resp)? {
            // The helper's own refusals of an answer quote nothing from the
            // request (see `serve_helper`); this side names it, and the
            // account, which only it knows by the name the playbook used.
            HelperResponse::Err {
                code: Some(code),
                message,
            } if code == too_large_code() || code == unencodable_code() => {
                // Empty when the frame limit left no room for the helper's
                // wording; say which refusal it was.
                let reason = match message.as_str() {
                    "" if code == too_large_code() => {
                        "refused: the answer is larger than one helper frame"
                    }
                    "" => "refused: the answer cannot be encoded",
                    m => m,
                };
                Err(coded(code, format!("{label} as `{}`: {reason}", self.user)))
            }
            other => other.into_io(),
        }
    }

    /// Remember the first failure so later calls report it instead of
    /// spawning another helper.
    fn latch<T>(&self, r: io::Result<T>) -> io::Result<T> {
        if let Err(e) = &r {
            let mut f = lock(&self.failed);
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
            other => Err(unexpected(&other)),
        }
    }

    /// Open `p` for reading through the helper: its first chunk comes back
    /// with the open, so a file of one chunk or less is one round trip and
    /// leaves nothing open.
    fn reader(&self, p: &Path) -> io::Result<HelperReader<'_>> {
        match self.call(HelperOp::ReadBegin { path: p.into() })? {
            HelperResponse::Data { handle, bytes } => Ok(HelperReader {
                offset: bytes.len() as u64,
                buf: bytes,
                pos: 0,
                held: Held::new(self, handle, Release::Close),
            }),
            other => Err(unexpected(&other)),
        }
    }

    /// Write what `src` yields to `p`, chunk by chunk: each chunk is filled
    /// whole before it is sent, and one byte past it is read first, so the
    /// last chunk is known to be the last and a file of one chunk or less is
    /// one request. The lock is taken per request and never while `src` is
    /// read, so `src` may be a reader on this same helper. An error from
    /// `src` abandons the write and is returned as it was.
    fn write_stream(
        &self,
        p: &Path,
        src: &mut dyn Read,
        attrs: Option<WriteAttrs>,
    ) -> io::Result<u64> {
        let chunk = self.chunk;
        let mut buf = Zeroizing::new(Vec::with_capacity(chunk + 1));
        fill(src, &mut buf, chunk + 1)?;
        let n = buf.len().min(chunk);
        let last = buf.len() <= chunk;
        let begin = HelperOp::WriteBegin {
            path: p.into(),
            attrs,
            bytes: Zeroizing::new(buf[..n].to_vec()),
            last,
        };
        if last {
            self.expect_unit(begin)?;
            return Ok(n as u64);
        }
        let handle = match self.call(begin)? {
            HelperResponse::Handle(h) => h,
            other => return Err(unexpected(&other)),
        };
        let held = Held::new(self, Some(handle), Release::Abort);
        let mut offset = n as u64;
        buf.drain(..n);
        loop {
            fill(src, &mut buf, chunk + 1)?;
            let n = buf.len().min(chunk);
            let last = buf.len() <= chunk;
            self.expect_unit(HelperOp::WriteChunk {
                handle,
                offset,
                bytes: Zeroizing::new(buf[..n].to_vec()),
                last,
            })?;
            offset += n as u64;
            if last {
                held.done();
                return Ok(offset);
            }
            buf.drain(..n);
        }
    }

    /// Stage `input` in the helper as a command's stdin, chunk by chunk,
    /// and return the stream holding it.
    fn stage_stdin(&self, input: &[u8]) -> io::Result<Held<'_>> {
        let mut parts = input.chunks(self.chunk);
        let first = parts.next().unwrap_or_default();
        let handle = match self.call(HelperOp::StdinBegin {
            bytes: Zeroizing::new(first.to_vec()),
        })? {
            HelperResponse::Handle(h) => h,
            other => return Err(unexpected(&other)),
        };
        let held = Held::new(self, Some(handle), Release::Close);
        let mut offset = first.len() as u64;
        for part in parts {
            self.expect_unit(HelperOp::StdinChunk {
                handle,
                offset,
                bytes: Zeroizing::new(part.to_vec()),
            })?;
            offset += part.len() as u64;
        }
        Ok(held)
    }

    /// A finished command's output kept behind `handle`, read back whole.
    fn output(
        &self,
        handle: u32,
        status: i32,
        signal: Option<i32>,
        stdout: u64,
        stderr: u64,
    ) -> io::Result<Output> {
        let size = |n: u64| {
            usize::try_from(n)
                .map_err(|_| io::Error::other("the command's output does not fit in memory"))
        };
        let (want_out, want_err) = (size(stdout)?, size(stderr)?);
        // Each stream in a buffer of its own size, so neither grows nor
        // carries the other's bytes in its spare room.
        let (mut out, mut err) = (Vec::with_capacity(want_out), Vec::with_capacity(want_err));
        let mut held = Held::new(self, Some(handle), Release::Close);
        while let Some(h) = held.handle {
            let got = out.len() + err.len();
            match self.call(HelperOp::ReadChunk {
                handle: h,
                offset: got as u64,
            })? {
                HelperResponse::Data { handle, bytes } => {
                    if bytes.is_empty() && handle.is_some() {
                        return Err(io::Error::other(
                            "the helper sent an empty chunk of a command's output",
                        ));
                    }
                    if got + bytes.len() > want_out + want_err {
                        return Err(io::Error::other(format!(
                            "the helper sent more of a command's output than the {} bytes it \
                             announced",
                            want_out + want_err
                        )));
                    }
                    let to_out = bytes.len().min(want_out - out.len());
                    out.extend_from_slice(&bytes[..to_out]);
                    err.extend_from_slice(&bytes[to_out..]);
                    held.handle = handle;
                }
                other => return Err(unexpected(&other)),
            }
        }
        if out.len() + err.len() != want_out + want_err {
            return Err(io::Error::other(format!(
                "the helper sent {} bytes of a command's output, not the {} it announced",
                out.len() + err.len(),
                want_out + want_err
            )));
        }
        Ok(Output {
            status,
            signal,
            stdout: out,
            stderr: err,
        })
    }
}

/// Read from `src` until `buf` holds `want` bytes or `src` ends. `buf`'s
/// capacity is at least `want`, so it does not grow, and an interrupted
/// read is retried.
fn fill(src: &mut dyn Read, buf: &mut Vec<u8>, want: usize) -> io::Result<()> {
    let mut filled = buf.len();
    buf.resize(want, 0);
    while filled < want {
        match src.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                buf.truncate(filled);
                return Err(e);
            }
        }
    }
    buf.truncate(filled);
    Ok(())
}

/// How [`Held`] lets go of a stream.
#[derive(Clone, Copy)]
enum Release {
    /// [`HelperOp::WriteAbort`]: a staged write, which leaves nothing.
    Abort,
    /// [`HelperOp::Close`]: anything else.
    Close,
}

/// A stream the helper holds open for this side, released when this side
/// lets go of it before its end (an error, a panic, a reader dropped half
/// way), and forgotten once the helper has ended it.
struct Held<'a> {
    e: &'a Elevated,
    handle: Option<u32>,
    release: Release,
}

impl<'a> Held<'a> {
    fn new(e: &'a Elevated, handle: Option<u32>, release: Release) -> Self {
        Held { e, handle, release }
    }

    /// The stream ended as it should: nothing to release.
    fn done(mut self) {
        self.handle = None;
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.e.release(match self.release {
                Release::Abort => HelperOp::WriteAbort { handle },
                Release::Close => HelperOp::Close { handle },
            });
        }
    }
}

/// [`Backend::open_read`] through a helper: one request per chunk, the lock
/// taken per request and released between them, and each chunk's remainder
/// kept here, so a reader that takes 512 bytes at a time (a tar header) does
/// not cost a round trip each.
struct HelperReader<'a> {
    held: Held<'a>,
    buf: Zeroizing<Vec<u8>>,
    pos: usize,
    /// Bytes received so far, which is the next chunk's offset.
    offset: u64,
}

impl HelperReader<'_> {
    /// When the chunk held here is used up, fetch the next one; false at the
    /// end of the file.
    fn refill(&mut self) -> io::Result<bool> {
        while self.pos == self.buf.len() {
            let Some(handle) = self.held.handle else {
                return Ok(false);
            };
            match self.held.e.call(HelperOp::ReadChunk {
                handle,
                offset: self.offset,
            })? {
                HelperResponse::Data { handle, bytes } => {
                    self.offset += bytes.len() as u64;
                    self.buf = bytes;
                    self.pos = 0;
                    self.held.handle = handle;
                }
                other => return Err(unexpected(&other)),
            }
        }
        Ok(true)
    }
}

impl Read for HelperReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if !self.refill()? {
            return Ok(0);
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn unexpected(r: &HelperResponse) -> io::Error {
    io::Error::other(format!("unexpected helper response {}", r.kind()))
}

impl Drop for Elevated {
    fn drop(&mut self) {
        if let Some(c) = lock(&self.conn).take() {
            c.shutdown();
        }
    }
}

impl Backend for Elevated {
    /// Grown through `extend_wiping`, so each buffer it outgrows is wiped:
    /// `read_to_end`'s growth left a copy of the file so far in each one.
    /// A file of one chunk is the size of its first chunk exactly.
    fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
        let mut r = self.reader(p)?;
        let mut out = Vec::with_capacity(r.buf.len());
        loop {
            extend_wiping(&mut out, &r.buf[r.pos..]);
            r.pos = r.buf.len();
            if !r.refill()? {
                return Ok(out);
            }
        }
    }

    fn write(&self, p: &Path, mut bytes: &[u8]) -> io::Result<()> {
        self.write_stream(p, &mut bytes, None).map(drop)
    }

    fn write_from(
        &self,
        p: &Path,
        src: &mut dyn Read,
        attrs: Option<WriteAttrs>,
    ) -> io::Result<u64> {
        self.write_stream(p, src, attrs)
    }

    fn open_read(&self, p: &Path) -> io::Result<Box<dyn Read + Send + '_>> {
        Ok(Box::new(self.reader(p)?))
    }

    fn stat(&self, p: &Path) -> io::Result<Option<Stat>> {
        match self.call(HelperOp::Stat { path: p.into() })? {
            HelperResponse::Stat(s) => Ok(s),
            other => Err(unexpected(&other)),
        }
    }

    fn stat_follow(&self, p: &Path) -> io::Result<Option<Stat>> {
        match self.call(HelperOp::StatFollow { path: p.into() })? {
            HelperResponse::Stat(s) => Ok(s),
            other => Err(unexpected(&other)),
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
            other => Err(unexpected(&other)),
        }
    }

    /// Batch by batch, each about a chunk of encoded names, then sorted as
    /// `Local` sorts them: a listing of one batch is one round trip.
    fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        let mut held = Held::new(self, None, Release::Close);
        let mut next = HelperOp::ReadDirBegin { path: p.into() };
        loop {
            match self.call(next)? {
                HelperResponse::Names { handle, names } => {
                    out.extend(names.into_iter().map(|n| p.join(n.0)));
                    held.handle = handle;
                }
                other => return Err(unexpected(&other)),
            }
            match held.handle {
                Some(handle) => next = HelperOp::ReadDirChunk { handle },
                None => break,
            }
        }
        out.sort();
        Ok(out)
    }

    /// A stdin larger than one chunk is staged in the helper first, chunk by
    /// chunk; output larger than one comes back the same way. Either is one
    /// round trip when it is small.
    fn spawn(&self, spec: &CmdSpec) -> io::Result<Output> {
        let staged = match &spec.stdin {
            Some(input) if input.len() > self.chunk => Some(self.stage_stdin(input)?),
            _ => None,
        };
        let resp = self.call(HelperOp::Spawn {
            cmd: WireCmd::of(spec, staged.is_none()),
            stdin: staged.as_ref().and_then(|s| s.handle),
        });
        // An answer means the helper took the staged stdin, whether or not
        // the command started. An error may be the helper's answer, or this
        // side refusing to send the request (too large to encode), which left
        // the stdin staged; it is closed then, and closing one the helper
        // already took is harmless.
        if resp.is_ok()
            && let Some(staged) = staged
        {
            staged.done();
        }
        match resp? {
            HelperResponse::Output(o) => Ok(o),
            HelperResponse::OutputHandle {
                handle,
                status,
                signal,
                stdout,
                stderr,
            } => self.output(handle, status, signal, stdout, stderr),
            other => Err(unexpected(&other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::backend::FileKind;
    use crate::protocol::write_frame;
    use crate::system::Phase;

    fn applying() -> Arc<AtomicU8> {
        Arc::new(AtomicU8::new(Phase::Applying as u8))
    }

    /// An in-process helper with the real limits.
    fn in_process(phase: Arc<AtomicU8>) -> Elevated {
        in_process_within(phase, Limits::REAL)
    }

    /// An in-process helper, both sides at `limits`.
    fn in_process_within(phase: Arc<AtomicU8>, limits: Limits) -> Elevated {
        Elevated::in_process_with("tester", phase, limits, |w| Box::new(w), |r| Box::new(r))
            .unwrap()
    }

    /// Small chunks, so a test of a file of several chunks writes kilobytes;
    /// and a frame they fit in with room to spare.
    const SMALL: Limits = Limits {
        frame: 16 * 1024,
        chunk: 4096,
    };

    /// The frames that went one way through a pipe, as their lengths, read
    /// off the bytes as they pass whatever size the writes and reads are.
    #[derive(Default)]
    struct FrameLog {
        prefix: Vec<u8>,
        remaining: usize,
        sizes: Vec<usize>,
    }

    impl FrameLog {
        fn feed(&mut self, mut bytes: &[u8]) {
            while !bytes.is_empty() {
                if self.remaining == 0 {
                    let take = (4 - self.prefix.len()).min(bytes.len());
                    self.prefix.extend_from_slice(&bytes[..take]);
                    bytes = &bytes[take..];
                    if self.prefix.len() == 4 {
                        let len = u32::from_be_bytes(self.prefix[..].try_into().unwrap());
                        self.sizes.push(len as usize);
                        self.remaining = len as usize;
                        self.prefix.clear();
                    }
                } else {
                    let take = self.remaining.min(bytes.len());
                    self.remaining -= take;
                    bytes = &bytes[take..];
                }
            }
        }
    }

    type Log = Arc<Mutex<FrameLog>>;

    /// A pipe end that records the frames through it.
    struct Recorded<T> {
        inner: T,
        log: Log,
    }

    impl<W: Write> Write for Recorded<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let n = self.inner.write(buf)?;
            self.log.lock().unwrap().feed(&buf[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    impl<R: Read> Read for Recorded<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.log.lock().unwrap().feed(&buf[..n]);
            Ok(n)
        }
    }

    /// An in-process helper at `limits` whose requests and answers are
    /// recorded.
    fn recorded(limits: Limits) -> (Elevated, Log, Log) {
        let (requests, answers) = (Log::default(), Log::default());
        let (rq, an) = (requests.clone(), answers.clone());
        let e = Elevated::in_process_with(
            "tester",
            applying(),
            limits,
            move |w| Box::new(Recorded { inner: w, log: rq }),
            move |r| Box::new(Recorded { inner: r, log: an }),
        )
        .unwrap();
        (e, requests, answers)
    }

    fn frames(log: &Log) -> usize {
        log.lock().unwrap().sizes.len()
    }

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

    /// `n` bytes that are not a repeated pattern, so a chunk out of place or
    /// a byte lost shows.
    fn data(n: usize) -> Vec<u8> {
        let mut x = 0x9e37_79b9_u32;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// The `.rustible-*` temporary files in `dir`.
    fn staged_in(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().as_bytes().starts_with(b".rustible-"))
            .collect()
    }

    /// The errno an error from the helper carries.
    fn errno(e: &io::Error) -> Option<i32> {
        errno_of(e)
    }

    fn ebadf() -> Option<i32> {
        Some(rustix::io::Errno::BADF.raw_os_error())
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

    /// The helper holds no stream: all [`MAX_HANDLES`] can be opened, and
    /// not one more. Each is a read of a file of two chunks, closed again
    /// when the readers drop.
    fn holds_no_stream(e: &Elevated, chunk: usize) {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("two-chunks");
        std::fs::write(&f, data(chunk + 1)).unwrap();
        let readers: Vec<_> = (0..MAX_HANDLES).map(|_| e.reader(&f).unwrap()).collect();
        assert!(readers.iter().all(|r| r.held.handle.is_some()));
        let Err(full) = e.reader(&f) else {
            panic!("a stream past the cap was opened")
        };
        assert!(
            full.to_string().contains("already holds 64 open streams"),
            "{full}"
        );
    }

    #[test]
    fn primitives_round_trip_through_the_helper_loop() {
        let e = in_process(Arc::new(AtomicU8::new(Phase::Idle as u8)));
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
        let not_a_dir = e.read_dir(&dir.path().join("nope")).unwrap_err();
        assert_eq!(not_a_dir.kind(), io::ErrorKind::NotFound, "{not_a_dir}");

        let mut cmd = sh("cat; echo err >&2; exit 3");
        cmd.stdin = Some(b"in\x00put".to_vec());
        let out = e.spawn(&cmd).unwrap();
        assert_eq!(
            (out.status, &out.stdout[..], out.stderr_str().trim()),
            (3, &b"in\x00put"[..], "err")
        );
    }

    /// Files of 0, `chunk - 1`, `chunk`, `chunk + 1` and several chunks
    /// round-trip byte for byte, and one of a chunk or less is exactly one
    /// request and one answer each way: the first request of a stream
    /// carries or returns the first chunk, and one byte past it says
    /// whether it was the last.
    #[test]
    fn files_of_every_size_round_trip_and_a_small_one_is_one_round_trip() {
        let chunk = SMALL.chunk;
        let (e, requests, answers) = recorded(SMALL);
        let dir = tempfile::tempdir().unwrap();
        for n in [0, 1, chunk - 1, chunk, chunk + 1, 3 * chunk + 5] {
            let f = dir.path().join(format!("f{n}"));
            let body = data(n);
            let before = frames(&requests);
            e.write(&f, &body).unwrap();
            let writes = frames(&requests) - before;
            assert_eq!(std::fs::read(&f).unwrap(), body, "{n} bytes written");

            let before = frames(&requests);
            assert_eq!(e.read(&f).unwrap(), body, "{n} bytes read");
            let reads = frames(&requests) - before;
            let want = n.div_ceil(chunk).max(1);
            assert_eq!((writes, reads), (want, want), "{n} bytes");
        }
        assert_eq!(frames(&requests), frames(&answers));
        assert!(staged_in(dir.path()).is_empty());
        holds_no_stream(&e, chunk);
    }

    /// Over 48 MiB, more than one frame could ever carry whole, at the real
    /// chunk size and through `System`: `write_from`, `open_read`,
    /// `write_atomic` and `read` all carry it byte for byte, and no frame
    /// either way is larger than a chunk's base64 and a little envelope,
    /// measured on the pipes themselves.
    #[test]
    fn a_file_over_48_mib_crosses_the_helper_in_bounded_frames() {
        use crate::system::System;

        let (requests, answers) = (Log::default(), Log::default());
        let (rq, an) = (requests.clone(), answers.clone());
        let sys = System::over_helper(Arc::new(crate::event::Collect::default()), |phase| {
            Elevated::in_process_with(
                "tester",
                phase,
                Limits::REAL,
                move |w| Box::new(Recorded { inner: w, log: rq }),
                move |r| Box::new(Recorded { inner: r, log: an }),
            )
        })
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        let body = data(50 << 20);
        assert!(body.len() > crate::protocol::MAX_FRAME_PAYLOAD);

        assert_eq!(
            sys.write_from(&a, body.as_slice(), None).unwrap(),
            body.len() as u64
        );
        let mut back = Vec::new();
        sys.open_read(&a).unwrap().read_to_end(&mut back).unwrap();
        assert!(back == body, "open_read differs");
        sys.write_atomic(&b, &body).unwrap();
        assert!(sys.read(&b).unwrap() == body, "read differs");

        let bound = CHUNK_SIZE / 3 * 4 + 64 * 1024;
        for (way, log) in [("request", &requests), ("answer", &answers)] {
            let sizes = &log.lock().unwrap().sizes;
            let largest = sizes.iter().max().copied().unwrap_or(0);
            assert!(largest <= bound, "a {largest}-byte {way} frame");
            // Fifty chunks a transfer, four transfers.
            assert_eq!(sizes.len(), 200, "{way}s");
        }
    }

    /// An escalated read leaves no copy of the file in freed memory, on
    /// either side: not the chunks, not their base64, not the result as it
    /// grows.
    #[test]
    fn an_escalated_read_leaves_no_copy_behind() {
        let probe = crate::freed::exclusive();
        let e = in_process_within(applying(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        let body = crate::freed::marked(5 * SMALL.chunk + 7);
        std::fs::write(&f, &body).unwrap();
        let left = probe.unwiped_frees(|| {
            let mut got = e.read(&f).unwrap();
            assert!(got == body);
            got.zeroize();
            // Room to spare, so `read_to_end` does not grow it.
            let mut got = Zeroizing::new(Vec::with_capacity(2 * body.len()));
            e.open_read(&f).unwrap().read_to_end(&mut got).unwrap();
            assert!(*got == body);
            // One more round trip, inside the window: the helper answers it
            // only after it has dropped what it held for the last one.
            e.stat(&f).unwrap();
        });
        assert_eq!(left, 0, "buffers freed with the file in them");
    }

    /// A reader that takes a few hundred bytes at a time, as a tar walk
    /// does, costs one request per chunk, not one per read: each chunk's
    /// remainder is kept on this side.
    #[test]
    fn the_reader_keeps_each_chunks_remainder() {
        let chunk = SMALL.chunk;
        let (e, requests, _) = recorded(SMALL);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        let body = data(5 * chunk + 100);
        std::fs::write(&f, &body).unwrap();
        let mut r = e.reader(&f).unwrap();
        let mut got = Vec::new();
        let mut piece = [0u8; 512];
        loop {
            let n = r.read(&mut piece).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&piece[..n]);
        }
        assert_eq!(got, body);
        assert_eq!(frames(&requests), 6);
        assert_eq!(r.held.handle, None, "ended with its last chunk");
    }

    /// A reader dropped half way closes its stream, so the helper's table is
    /// empty again; a write that fails half way aborts its own.
    #[test]
    fn a_stream_given_up_half_way_is_released() {
        let chunk = SMALL.chunk;
        let e = in_process_within(applying(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, data(3 * chunk)).unwrap();
        for _ in 0..2 * MAX_HANDLES {
            let mut r = e.reader(&f).unwrap();
            r.read_exact(&mut [0u8; 10]).unwrap();
        }
        for _ in 0..2 * MAX_HANDLES {
            let mut failing = io::Cursor::new(data(2 * chunk + 2)).chain(Failing);
            assert!(
                e.write_stream(&dir.path().join("w"), &mut failing, None)
                    .is_err()
            );
        }
        // A listing given up on: the guard `read_dir` holds, dropped with
        // the stream still open.
        let listed = dir.path().join("listed");
        std::fs::create_dir(&listed).unwrap();
        for i in 0..200 {
            std::fs::write(listed.join(format!("{i:0>96}")), b"").unwrap();
        }
        for _ in 0..2 * MAX_HANDLES {
            let HelperResponse::Names { handle, .. } = e
                .call(HelperOp::ReadDirBegin {
                    path: listed.clone(),
                })
                .unwrap()
            else {
                panic!("not names")
            };
            assert!(handle.is_some(), "the listing fit one batch");
            drop(Held::new(&e, handle, Release::Close));
        }
        holds_no_stream(&e, chunk);
        assert!(!dir.path().join("w").exists());
        assert!(staged_in(dir.path()).is_empty());
    }

    /// A source that fails when it is read.
    struct Failing;

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("the source broke"))
        }
    }

    /// A source that yields `body` and then fails at its end, as a reader
    /// that checks a digest at EOF does.
    struct FailsAtEof(io::Cursor<Vec<u8>>);

    impl Read for FailsAtEof {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.0.read(buf)? {
                0 => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "checksum mismatch at the end",
                )),
                n => Ok(n),
            }
        }
    }

    /// A source that fails part way, or only at its end, writes nothing: the
    /// target keeps its content, nothing is left beside it, the error is the
    /// source's own, and the helper goes on serving. Small and large, new
    /// file and rewrite.
    #[test]
    fn a_source_that_fails_writes_nothing() {
        let chunk = SMALL.chunk;
        let e = in_process_within(applying(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        std::fs::write(&old, "before").unwrap();
        let new = dir.path().join("new");
        for n in [10, chunk + 1, 3 * chunk] {
            for p in [&old, &new] {
                let mut part_way = io::Cursor::new(data(n)).chain(Failing);
                let err = e.write_stream(p, &mut part_way, None).unwrap_err();
                assert_eq!(err.to_string(), "the source broke");

                let mut at_eof = FailsAtEof(io::Cursor::new(data(n)));
                let err = e.write_stream(p, &mut at_eof, None).unwrap_err();
                assert_eq!(err.kind(), io::ErrorKind::InvalidData);
                assert_eq!(err.to_string(), "checksum mismatch at the end");
            }
        }
        assert_eq!(std::fs::read(&old).unwrap(), b"before");
        assert!(!new.exists());
        assert!(staged_in(dir.path()).is_empty());
        still_serves(&e);
    }

    /// Through the helper, a rewrite's content and a new file given
    /// attributes are staged at 0600 between two chunks; the target is the
    /// old one until the last chunk, then the new one with its mode.
    #[test]
    fn a_staged_write_is_0600_between_two_chunks_through_the_helper() {
        let chunk = SMALL.chunk;
        let e = in_process_within(applying(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        std::fs::write(&old, "before").unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o644)).unwrap();
        let new = dir.path().join("new");
        let attrs = WriteAttrs {
            mode: Some(0o640),
            owner: None,
        };

        /// Yields `body`, and once it is past the first chunk and the one
        /// byte read ahead of it (so the helper has staged the first
        /// chunk), records the staged file's mode at every read.
        struct Watching<'a> {
            body: io::Cursor<Vec<u8>>,
            dir: &'a Path,
            target: &'a Path,
            after: u64,
            seen: Vec<(u32, Vec<u8>)>,
        }
        impl Read for Watching<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if self.body.position() >= self.after {
                    let tmp = staged_in(self.dir);
                    assert_eq!(tmp.len(), 1, "{tmp:?}");
                    let mode = std::fs::metadata(&tmp[0]).unwrap().permissions().mode() & 0o7777;
                    let target = std::fs::read(self.target).unwrap_or_default();
                    self.seen.push((mode, target));
                }
                self.body.read(buf)
            }
        }

        for (p, attrs, mode) in [(&old, None, 0o644), (&new, Some(attrs), 0o640)] {
            let body = data(3 * chunk);
            let before = std::fs::read(p).unwrap_or_default();
            let mut src = Watching {
                body: io::Cursor::new(body.clone()),
                dir: dir.path(),
                target: p,
                after: chunk as u64 + 1,
                seen: Vec::new(),
            };
            e.write_stream(p, &mut src, attrs).unwrap();
            assert!(src.seen.len() >= 2, "{}", src.seen.len());
            for (seen, target) in &src.seen {
                assert_eq!(*seen, 0o600, "{}", p.display());
                assert_eq!(*target, before, "replaced before the last chunk");
            }
            assert_eq!(std::fs::read(p).unwrap(), body);
            assert_eq!(
                std::fs::metadata(p).unwrap().permissions().mode() & 0o7777,
                mode
            );
        }
        assert!(staged_in(dir.path()).is_empty());
    }

    /// A read and a write on one helper interleave chunk by chunk: the
    /// write's source is a reader on the same helper, so each chunk read is
    /// a request between two chunks written. Holding the connection for a
    /// whole stream would deadlock here, so it is bounded.
    #[test]
    fn a_read_and_a_write_interleave_on_one_helper() {
        let (tx, rx) = mpsc::channel();
        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        let body = data(10 * SMALL.chunk + 7);
        std::fs::write(&src, &body).unwrap();
        let (s, d) = (src.clone(), dst.clone());
        std::thread::spawn(move || {
            let (e, requests, _) = recorded(SMALL);
            let written = e
                .write_stream(&d, &mut e.reader(&s).unwrap(), None)
                .unwrap();
            let _ = tx.send((written, frames(&requests)));
        });
        let (written, requests) = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("a read and a write on one helper deadlocked");
        assert_eq!(written, body.len() as u64);
        assert_eq!(std::fs::read(&dst).unwrap(), body);
        // Eleven chunks each way.
        assert_eq!(requests, 22);
    }

    /// A listing of several batches comes back whole, in `Local`'s order,
    /// and the names that are not UTF-8 among them; one of a single batch is
    /// one request.
    #[test]
    fn a_listing_of_several_batches_comes_back_whole_and_sorted() {
        let (e, requests, _) = recorded(SMALL);
        let dir = tempfile::tempdir().unwrap();
        for i in 0..300 {
            std::fs::write(dir.path().join(format!("{i:0>96}")), b"").unwrap();
        }
        plant_a_name_that_is_not_utf8(dir.path());
        let got = e.read_dir(dir.path()).unwrap();
        assert_eq!(got, Local.read_dir(dir.path()).unwrap());
        assert!(got.len() >= 300);
        assert!(frames(&requests) > 4, "{} requests", frames(&requests));

        let small = dir.path().join("small");
        std::fs::create_dir(&small).unwrap();
        std::fs::write(small.join("a"), b"").unwrap();
        let before = frames(&requests);
        assert_eq!(e.read_dir(&small).unwrap(), [small.join("a")]);
        assert_eq!(frames(&requests) - before, 1);
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(e.read_dir(&empty).unwrap().is_empty());
        holds_no_stream(&e, SMALL.chunk);
    }

    /// The bytes of a name that is not UTF-8.
    const BAD: &[u8] = b"bad\xff";

    /// Create an empty file named `bad\xff`, not UTF-8, in `dir`, and say
    /// whether that worked. A filesystem that only stores UTF-8 names
    /// refuses it with `EILSEQ` (APFS on a mac does): a test of such names
    /// skips there and says so on stderr.
    fn plant_a_name_that_is_not_utf8(dir: &Path) -> bool {
        let name = dir.join(std::ffi::OsStr::from_bytes(BAD));
        match std::fs::write(&name, b"") {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(unencodable_code()) => {
                eprintln!(
                    "note: skipping a name that is not UTF-8: this filesystem refuses one ({e})"
                );
                false
            }
            Err(e) => panic!("creating {}: {e}", name.display()),
        }
    }

    /// A name that is not UTF-8 works through a helper as it does on
    /// `Local`: listed, stat'ed, read, written and renamed, since paths
    /// cross as bytes.
    #[test]
    fn a_name_that_is_not_utf8_works_through_the_helper() {
        let e = in_process(applying());
        let dir = tempfile::tempdir().unwrap();
        if !plant_a_name_that_is_not_utf8(dir.path()) {
            return;
        }
        let bad = dir.path().join(std::ffi::OsStr::from_bytes(BAD));
        assert_eq!(e.read_dir(dir.path()).unwrap(), std::slice::from_ref(&bad));
        assert_eq!(e.stat(&bad).unwrap().unwrap().kind, FileKind::File);
        e.write(&bad, b"through the helper").unwrap();
        assert_eq!(e.read(&bad).unwrap(), b"through the helper");
        let link = dir.path().join(std::ffi::OsStr::from_bytes(b"link\xfe"));
        e.symlink(&bad, &link).unwrap();
        assert_eq!(e.read_link(&link).unwrap(), bad);
        let sub = dir.path().join(std::ffi::OsStr::from_bytes(b"sub\xfe"));
        e.mkdir_all(&sub).unwrap();
        let mut cmd = sh("pwd");
        cmd.cwd = Some(sub.clone());
        let out = e.spawn(&cmd).unwrap();
        assert_eq!(out.stdout, [sub.as_os_str().as_bytes(), b"\n"].concat());
    }

    #[test]
    fn helper_refuses_mutations_while_checking() {
        let phase = Arc::new(AtomicU8::new(Phase::Checking as u8));
        let e = in_process_within(phase.clone(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("x");
        // One chunk and several: refused at the first, naming the write and
        // its size, never the bytes: a write can carry a secret, and this
        // message is rendered and logged.
        for (secret, label) in [
            (b"hunter2-the-secret".to_vec(), "(18 bytes)"),
            (b"hunter2-the-secret".repeat(1000), "(first 4096 bytes)"),
        ] {
            let err = e.write(&f, &secret).unwrap_err().to_string();
            assert!(err.contains("mutation during check"), "{err}");
            assert!(
                err.contains(&format!("write {} {label}", f.display())),
                "{err}"
            );
            assert!(!err.contains("hunter2"), "{err}");
            assert!(!err.contains("104"), "byte values leaked: {err}");
            assert!(!f.exists());
        }
        // And through `write_from`, with attributes.
        let secret = b"hunter2-the-secret".repeat(1000);
        let attrs = WriteAttrs {
            mode: Some(0o600),
            owner: None,
        };
        let err = Backend::write_from(&e, &f, &mut secret.as_slice(), Some(attrs))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            format!(
                "mutation during check refused by helper: write {} (first 4096 bytes)",
                f.display()
            )
        );
        // Reads, listings, commands and releasing are fine while checking.
        assert_eq!(e.stat(&f).unwrap(), None);
        assert!(e.read_dir(dir.path()).unwrap().is_empty());
        assert!(e.spawn(&sh("true")).unwrap().success());
        e.expect_unit(HelperOp::Close { handle: 9 }).unwrap();
        e.expect_unit(HelperOp::WriteAbort { handle: 9 }).unwrap();

        // A chunk of a write begun before the check started is refused too,
        // and the refusal ends the stream: the temporary file goes.
        phase.store(Phase::Applying as u8, Ordering::SeqCst);
        let HelperResponse::Handle(handle) = e
            .call(HelperOp::WriteBegin {
                path: f.clone(),
                attrs: None,
                bytes: Zeroizing::new(b"hunter2".to_vec()),
                last: false,
            })
            .unwrap()
        else {
            panic!("no handle")
        };
        assert_eq!(staged_in(dir.path()).len(), 1);
        phase.store(Phase::Checking as u8, Ordering::SeqCst);
        let chunk = |last| HelperOp::WriteChunk {
            handle,
            offset: 7,
            bytes: Zeroizing::new(b"-the-secret".to_vec()),
            last,
        };
        let err = e.call(chunk(true)).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "mutation during check refused by helper: write stream {handle} (11 bytes at 7)"
            )
        );
        assert!(staged_in(dir.path()).is_empty());
        phase.store(Phase::Applying as u8, Ordering::SeqCst);
        assert_eq!(errno(&e.call(chunk(true)).unwrap_err()), ebadf());
        assert!(!f.exists());
        e.write(&f, b"x").unwrap();
        assert!(f.exists());
    }

    /// While a step checks, every request a read of several chunks makes is
    /// allowed, not only the first: a file of several chunks
    /// (`ReadChunk`), a listing of several batches (`ReadDirChunk`), a
    /// command whose stdin is staged (`StdinBegin`, `StdinChunk`) and whose
    /// output comes back in chunks.
    #[test]
    fn reads_of_several_chunks_are_allowed_while_checking() {
        let e = in_process_within(Arc::new(AtomicU8::new(Phase::Checking as u8)), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        let body = data(3 * SMALL.chunk + 1);
        std::fs::write(&f, &body).unwrap();
        assert_eq!(e.read(&f).unwrap(), body);
        let mut streamed = Vec::new();
        e.open_read(&f).unwrap().read_to_end(&mut streamed).unwrap();
        assert_eq!(streamed, body);

        let listed = dir.path().join("listed");
        std::fs::create_dir(&listed).unwrap();
        for i in 0..200 {
            std::fs::write(listed.join(format!("{i:0>96}")), b"").unwrap();
        }
        assert_eq!(e.read_dir(&listed).unwrap().len(), 200);

        let mut cat = sh("cat");
        cat.stdin = Some(body.clone());
        assert_eq!(e.spawn(&cat).unwrap().stdout, body);
    }

    /// A stream the helper does not hold is `EBADF` for every request that
    /// needs one, and closing or aborting it is fine: release is
    /// idempotent. A stream of another kind is not taken for one.
    #[test]
    fn an_unknown_handle_is_ebadf_and_releasing_one_is_fine() {
        let e = in_process(applying());
        let z = || Zeroizing::new(b"x".to_vec());
        for op in [
            HelperOp::ReadChunk {
                handle: 42,
                offset: 0,
            },
            HelperOp::WriteChunk {
                handle: 42,
                offset: 0,
                bytes: z(),
                last: true,
            },
            HelperOp::ReadDirChunk { handle: 42 },
            HelperOp::StdinChunk {
                handle: 42,
                offset: 0,
                bytes: z(),
            },
            HelperOp::Spawn {
                cmd: WireCmd::of(&sh("true"), true),
                stdin: Some(42),
            },
        ] {
            let label = op.label();
            let err = e.call(op).unwrap_err();
            assert_eq!(errno(&err), ebadf(), "{label}: {err}");
            assert!(err.to_string().contains("stream 42"), "{err}");
        }
        e.expect_unit(HelperOp::WriteAbort { handle: 42 }).unwrap();
        e.expect_unit(HelperOp::Close { handle: 42 }).unwrap();

        // A staged stdin is not a write, and stays as it was.
        let HelperResponse::Handle(stdin) = e.call(HelperOp::StdinBegin { bytes: z() }).unwrap()
        else {
            panic!("no handle")
        };
        let err = e
            .call(HelperOp::WriteChunk {
                handle: stdin,
                offset: 0,
                bytes: z(),
                last: true,
            })
            .unwrap_err();
        assert_eq!(errno(&err), ebadf());
        let mut cat = sh("cat");
        cat.stdin = None;
        let HelperResponse::Output(o) = e
            .call(HelperOp::Spawn {
                cmd: WireCmd::of(&cat, true),
                stdin: Some(stdin),
            })
            .unwrap()
        else {
            panic!("no output")
        };
        assert_eq!(o.stdout, b"x");
        still_serves(&e);
    }

    /// A chunk at the wrong offset is refused as invalid input, and the
    /// stream is gone with its temporary file: the target never changes.
    #[test]
    fn a_chunk_at_the_wrong_offset_abandons_the_write() {
        let e = in_process(applying());
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, "before").unwrap();
        let HelperResponse::Handle(handle) = e
            .call(HelperOp::WriteBegin {
                path: f.clone(),
                attrs: None,
                bytes: Zeroizing::new(b"12345".to_vec()),
                last: false,
            })
            .unwrap()
        else {
            panic!("no handle")
        };
        let chunk = |offset| HelperOp::WriteChunk {
            handle,
            offset,
            bytes: Zeroizing::new(b"678".to_vec()),
            last: true,
        };
        let err = e.call(chunk(4)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
        assert_eq!(
            err.to_string(),
            format!(
                "write stream {handle}: a chunk at offset 4, but 5 bytes were written so \
                 far; the write is abandoned and nothing was replaced"
            )
        );
        assert!(staged_in(dir.path()).is_empty());
        assert_eq!(errno(&e.call(chunk(5)).unwrap_err()), ebadf());
        assert_eq!(std::fs::read(&f).unwrap(), b"before");
    }

    /// At most 64 streams are open at once: the 65th open is refused naming
    /// the cap, of whatever kind, and closing one frees a slot. A whole
    /// write, which opens nothing, still works when the table is full.
    #[test]
    fn the_65th_stream_is_refused_and_closing_one_frees_a_slot() {
        let e = in_process_within(applying(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        std::fs::write(&big, data(2 * SMALL.chunk)).unwrap();
        let mut readers: Vec<_> = (0..MAX_HANDLES).map(|_| e.reader(&big).unwrap()).collect();
        let full = |r: io::Result<HelperResponse>| {
            let err = r.unwrap_err();
            assert_eq!(
                errno(&err),
                Some(rustix::io::Errno::MFILE.raw_os_error()),
                "{err}"
            );
            assert_eq!(
                err.to_string(),
                "the helper already holds 64 open streams, the most it keeps at once; one \
                 has to be finished or closed before another is opened"
            );
        };
        full(e.call(HelperOp::ReadBegin { path: big.clone() }));
        full(e.call(HelperOp::WriteBegin {
            path: dir.path().join("w"),
            attrs: None,
            bytes: Zeroizing::new(b"x".to_vec()),
            last: false,
        }));
        full(e.call(HelperOp::StdinBegin {
            bytes: Zeroizing::new(b"x".to_vec()),
        }));
        assert!(staged_in(dir.path()).is_empty());
        // A whole write opens nothing and goes through; a read is refused
        // before the file is opened, whatever its size.
        e.write(&dir.path().join("small"), b"small").unwrap();
        full(e.call(HelperOp::ReadBegin {
            path: dir.path().join("small"),
        }));

        readers.pop();
        let mut again = e.reader(&big).unwrap();
        let mut all = Vec::new();
        again.read_to_end(&mut all).unwrap();
        assert_eq!(all.len(), 2 * SMALL.chunk);
        drop(readers);
        holds_no_stream(&e, SMALL.chunk);
    }

    /// With the table full, a request that could open a stream is refused
    /// before it does anything: a command is not run (its output might need
    /// a slot, and refusing after it ran would lose it), and a file is not
    /// read (a FIFO here would block the test if it were opened and read).
    #[test]
    fn a_full_table_refuses_before_any_side_effect() {
        let e = in_process_within(applying(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        std::fs::write(&big, data(2 * SMALL.chunk)).unwrap();
        let fifo = dir.path().join("fifo");
        let made = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(made.success(), "mkfifo");
        let readers: Vec<_> = (0..MAX_HANDLES).map(|_| e.reader(&big).unwrap()).collect();
        let emfile = Some(rustix::io::Errno::MFILE.raw_os_error());

        let ran = dir.path().join("ran");
        let err = e
            .spawn(&sh(&format!("touch {}", ran.display())))
            .unwrap_err();
        assert_eq!(errno(&err), emfile, "{err}");
        assert!(!ran.exists(), "the command ran before it was refused");

        let (tx, rx) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let r = e.call(HelperOp::ReadBegin { path: fifo.clone() });
                let _ = tx.send(r.map(|_| ()));
            });
            let r = rx.recv_timeout(Duration::from_secs(10));
            if r.is_err() {
                // Unblock the helper so the scope can end, then fail.
                let _ = std::fs::write(&fifo, b"");
                panic!("the FIFO was opened before the full table was seen");
            }
            assert_eq!(errno(&r.unwrap().unwrap_err()), emfile);
        });
        let err = e.call(HelperOp::ReadDirBegin {
            path: dir.path().to_path_buf(),
        });
        assert_eq!(errno(&err.unwrap_err()), emfile);
        drop(readers);
        assert!(e.spawn(&sh("true")).unwrap().success());
    }

    /// Releasing or refusing a stream of another kind leaves it alone: a
    /// `WriteAbort` of a read stream is `EBADF`, and a write chunk refused
    /// while checking does not end a read stream that happens to carry its
    /// handle. Either way the read goes on.
    #[test]
    fn a_stream_of_another_kind_is_not_ended() {
        let phase = applying();
        let e = in_process_within(phase.clone(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        let body = data(3 * SMALL.chunk);
        std::fs::write(&f, &body).unwrap();
        let mut r = e.reader(&f).unwrap();
        let handle = r.held.handle.unwrap();

        let err = e.call(HelperOp::WriteAbort { handle }).unwrap_err();
        assert_eq!(errno(&err), ebadf(), "{err}");
        phase.store(Phase::Checking as u8, Ordering::SeqCst);
        let refused = e.call(HelperOp::WriteChunk {
            handle,
            offset: 0,
            bytes: Zeroizing::new(b"x".to_vec()),
            last: true,
        });
        assert!(refused.is_err());
        phase.store(Phase::Applying as u8, Ordering::SeqCst);

        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert!(got == body);
        // A handle nobody holds is still fine to abort.
        e.expect_unit(HelperOp::WriteAbort { handle: 999 }).unwrap();
    }

    /// The parent going away part way through a write (its pipe closed
    /// after a chunk, no commit) leaves no temporary file and the target as
    /// it was: the helper sees EOF, and its table drops.
    #[test]
    fn a_parent_gone_mid_write_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, "before").unwrap();
        // The whole exchange on a thread, bounded: a helper that does not
        // answer, or does not see EOF, would otherwise hang the suite.
        let (finished_tx, finished) = mpsc::channel();
        let (at, file) = (dir.path().to_path_buf(), f.clone());
        std::thread::spawn(move || {
            let (req_r, mut req_w) = io::pipe().unwrap();
            let (mut resp_r, resp_w) = io::pipe().unwrap();
            let (done_tx, done) = mpsc::channel();
            std::thread::spawn(move || {
                let (mut rx, mut tx) = (req_r, resp_w);
                let _ = done_tx.send(serve(&mut rx, &mut tx, Limits::REAL));
            });
            let send = |w: &mut io::PipeWriter, op| {
                write_frame(
                    w,
                    &HelperRequest {
                        checking: false,
                        op,
                    },
                )
                .unwrap()
            };
            send(
                &mut req_w,
                HelperOp::WriteBegin {
                    path: file,
                    attrs: None,
                    bytes: Zeroizing::new(b"new ".to_vec()),
                    last: false,
                },
            );
            let Some(HelperResponse::Handle(handle)) = read_frame(&mut resp_r).unwrap() else {
                panic!("no handle")
            };
            send(
                &mut req_w,
                HelperOp::WriteChunk {
                    handle,
                    offset: 4,
                    bytes: Zeroizing::new(b"content".to_vec()),
                    last: false,
                },
            );
            assert!(matches!(
                read_frame(&mut resp_r).unwrap(),
                Some(HelperResponse::Unit)
            ));
            assert_eq!(staged_in(&at).len(), 1);
            drop(req_w);
            let served = done
                .recv_timeout(Duration::from_secs(30))
                .expect("the helper did not end when its parent went away");
            let _ = finished_tx.send(served);
        });
        finished
            .recv_timeout(Duration::from_secs(60))
            .expect("the exchange with the helper hung or failed")
            .unwrap();
        assert!(staged_in(dir.path()).is_empty());
        assert_eq!(std::fs::read(&f).unwrap(), b"before");
    }

    /// A command's stdin larger than one chunk is staged in the helper and
    /// reaches the child byte for byte; output larger than one chunk on
    /// stdout and stderr comes back whole; a small command is one round
    /// trip.
    #[test]
    fn a_commands_large_stdin_and_output_cross_in_chunks() {
        let chunk = SMALL.chunk;
        let (e, requests, _) = recorded(SMALL);
        let input = data(5 * chunk + 3);
        let mut cat = sh("cat; cat /dev/null >&2");
        cat.stdin = Some(input.clone());
        let out = e.spawn(&cat).unwrap();
        assert_eq!((out.status, out.stdout.len()), (0, input.len()));
        assert!(out.stdout == input, "stdout differs");

        let out = e
            .spawn(&sh(
                "head -c 9000 /dev/zero; head -c 10000 /dev/zero | tr '\\0' e >&2; exit 4",
            ))
            .unwrap();
        assert_eq!(out.status, 4);
        assert_eq!(out.stdout, vec![0u8; 9000]);
        assert_eq!(out.stderr, vec![b'e'; 10000]);

        let mut small = sh("cat");
        small.stdin = Some(b"small".to_vec());
        let before = frames(&requests);
        assert_eq!(e.spawn(&small).unwrap().stdout, b"small");
        assert_eq!(frames(&requests) - before, 1);
        holds_no_stream(&e, chunk);
    }

    /// Output is answered whole when its frame fits the limit exactly, and
    /// kept behind a handle one byte under it (#97's leftover nit: a `>=`
    /// in place of the `>` survived the suite).
    #[test]
    fn output_that_fits_the_frame_exactly_is_answered_whole() {
        let spec = sh("head -c 100 /dev/zero; printf xy >&2; exit 2");
        let o = Output {
            status: 2,
            signal: None,
            stdout: vec![0; 100],
            stderr: b"xy".to_vec(),
        };
        let exact = encoded_output_len(&o) as usize;
        for (frame, whole) in [(exact, true), (exact - 1, false)] {
            let mut rx = Vec::new();
            write_frame(
                &mut rx,
                &HelperRequest {
                    checking: false,
                    op: HelperOp::Spawn {
                        cmd: WireCmd::of(&spec, true),
                        stdin: None,
                    },
                },
            )
            .unwrap();
            let mut tx = Vec::new();
            serve(&mut &rx[..], &mut tx, Limits { frame, chunk: 4096 }).unwrap();
            let resp: HelperResponse = read_frame(&mut &tx[..]).unwrap().unwrap();
            match resp {
                HelperResponse::Output(got) if whole => {
                    assert_eq!(
                        (got.stdout, got.stderr),
                        (o.stdout.clone(), o.stderr.clone())
                    )
                }
                HelperResponse::OutputHandle {
                    status: 2,
                    stdout: 100,
                    stderr: 2,
                    ..
                } if !whole => {}
                other => panic!("frame {frame}: {other:?}"),
            }
        }
    }

    /// Every frame `serve` writes is within its limit, whatever the
    /// answer: chunks of a large file, batches of a large listing, a
    /// command's large output, a check-mode refusal quoting an absurd path,
    /// a command whose argv alone is larger than a frame. The requests go
    /// in as one stream and the answers are read back frame by frame from
    /// what `serve` wrote, so a frame over the limit is caught here, not by
    /// a reader that gives up on it.
    #[test]
    fn no_frame_the_helper_writes_is_over_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big");
        std::fs::write(&big, data(20000)).unwrap();
        let listed = dir.path().join("listed");
        std::fs::create_dir(&listed).unwrap();
        for i in 0..200 {
            std::fs::write(listed.join(format!("{i:0>96}")), b"").unwrap();
        }
        let absurd = dir.path().join("a".repeat(SMALL.frame));
        let mut long_argv = sh("head -c 20000 /dev/zero");
        long_argv.args.push("y".repeat(SMALL.frame));

        let requests = [
            (false, HelperOp::ReadBegin { path: big.clone() }),
            (false, HelperOp::ReadDirBegin { path: listed }),
            (
                false,
                HelperOp::Spawn {
                    cmd: WireCmd::of(&sh("head -c 20000 /dev/zero"), true),
                    stdin: None,
                },
            ),
            (true, HelperOp::Remove { path: absurd }),
            (
                false,
                HelperOp::Spawn {
                    cmd: WireCmd::of(&long_argv, true),
                    stdin: None,
                },
            ),
        ];
        let mut rx = Vec::new();
        for (checking, op) in requests {
            write_frame(&mut rx, &HelperRequest { checking, op }).unwrap();
        }
        let mut tx = Vec::new();
        serve(&mut &rx[..], &mut tx, SMALL).unwrap();

        let mut answers = Vec::new();
        let mut stream = &tx[..];
        while !stream.is_empty() {
            let len = u32::from_be_bytes(stream[..4].try_into().unwrap()) as usize;
            assert!(
                len <= SMALL.frame,
                "a {len}-byte frame, answer {}",
                answers.len()
            );
            answers.push(serde_json::from_slice::<HelperResponse>(&stream[4..4 + len]).unwrap());
            stream = &stream[4 + len..];
        }
        assert_eq!(answers.len(), 5, "{answers:?}");
        assert!(
            matches!(&answers[0], HelperResponse::Data { handle: Some(_), bytes } if bytes.len() == SMALL.chunk)
        );
        assert!(matches!(
            &answers[1],
            HelperResponse::Names {
                handle: Some(_),
                ..
            }
        ));
        assert!(matches!(
            &answers[2],
            HelperResponse::OutputHandle { stdout: 20000, .. }
        ));
        // A check-mode refusal over the limit only because of the path it
        // quotes is refused in its turn, quoting nothing.
        let HelperResponse::Err {
            code: Some(code),
            message,
        } = &answers[3]
        else {
            panic!("{:?}", answers[3])
        };
        assert_eq!(*code, too_large_code());
        assert_eq!(
            message,
            "the answer is more than one helper frame holds (16384 bytes)"
        );
        assert!(matches!(
            &answers[4],
            HelperResponse::OutputHandle { stdout: 20000, .. }
        ));
    }

    /// The refusal of an answer fits a frame of a couple of kilobytes
    /// whatever the request said, since it quotes nothing from the request:
    /// here a chunk larger than the frame, for a path as long as a name may
    /// be.
    #[test]
    fn the_refusal_fits_a_small_frame_whatever_the_request_said() {
        let limit = 2500;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("n".repeat(250));
        std::fs::write(&f, data(5000)).unwrap();
        let mut rx = Vec::new();
        write_frame(
            &mut rx,
            &HelperRequest {
                checking: false,
                op: HelperOp::ReadBegin { path: f },
            },
        )
        .unwrap();
        let mut tx = Vec::new();
        serve(
            &mut &rx[..],
            &mut tx,
            Limits {
                frame: limit,
                chunk: 4096,
            },
        )
        .unwrap();
        let len = u32::from_be_bytes(tx[..4].try_into().unwrap()) as usize;
        assert!(len <= limit, "a {len}-byte frame");
        let resp: HelperResponse = serde_json::from_slice(&tx[4..]).unwrap();
        let HelperResponse::Err { message, .. } = resp else {
            panic!("{resp:?}")
        };
        assert_eq!(
            message,
            "the answer is more than one helper frame holds (2500 bytes)"
        );
    }

    /// A command through the helper with a large stdin, against a child
    /// that fills its stdout pipe before reading a byte of its input. The
    /// helper executes through `Local`, so this pins the escalated path to
    /// `Local::spawn`'s threaded feeder: with a blocking inline write the
    /// two would wait on each other forever. Bounded by a channel timeout so
    /// a regression fails the test instead of hanging the suite.
    #[test]
    fn a_command_through_the_helper_does_not_deadlock_on_large_stdin() {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let e = in_process(applying());
            // 3 MiB in, staged in chunks, and the child writes 1 MiB out
            // before it reads.
            let mut spec = sh("yes hello | head -c 1048576; wc -c");
            spec.stdin = Some(vec![b'x'; 3 * 1024 * 1024]);
            let out = e.spawn(&spec);
            let _ = done_tx.send(out);
        });
        let out = done_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("the helper deadlocked writing stdin to a chatty child")
            .expect("spawn through the helper");
        assert_eq!(out.status, 0);
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.ends_with("3145728\n"), "{}", &text[text.len() - 40..]);
    }

    /// A command's variables cross to the helper and reach the child. The
    /// helper is started by `sudo`, whose reset environment is what its
    /// children inherit, so a variable an op needs there has to ride in the
    /// `CmdSpec`: `systemd`'s `.user(true)` sets `XDG_RUNTIME_DIR` this way
    /// for an `as_user` target (#55).
    #[test]
    fn a_commands_env_reaches_the_child_through_the_helper() {
        let e = in_process(applying());
        let mut spec = sh("printf %s \"$XDG_RUNTIME_DIR\"");
        spec.env = BTreeMap::from([("XDG_RUNTIME_DIR".into(), "/run/user/1002".into())]);
        let out = e.spawn(&spec).unwrap();
        assert_eq!((out.status, out.stdout_str()), (0, "/run/user/1002".into()));
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
            applying(),
        );
        let err = e.read(Path::new("/etc/shadow")).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "read /etc/shadow: the helper running as `tester` sent a frame of {} bytes, \
                 more than the {MAX_FRAME} a frame may hold. A helper refuses an answer that \
                 large instead of sending it, so the stream between the two is out of step: \
                 most likely something else wrote to the helper's stdout, such as a banner \
                 from sudo or PAM, or the stream was corrupted. Check what the host's sudoers \
                 and PAM configuration print for `tester`",
                MAX_FRAME + 1
            )
        );
        let again = e.stat(Path::new("/etc/hostname")).unwrap_err().to_string();
        assert!(
            again.contains("failed earlier and is not retried"),
            "{again}"
        );
        assert!(
            again.contains("stream between the two is out of step"),
            "{again}"
        );
    }

    /// A request larger than a frame is refused before a byte is written:
    /// the stream is intact, so the helper is neither shut down nor latched.
    /// The message names the command, cut short, and the account.
    #[test]
    fn an_oversized_request_is_refused_without_latching() {
        let mut e = in_process(applying());
        e.max_frame = 128 * 1024;
        let mut spec = sh("cat > /dev/null");
        spec.args.push("y".repeat(100 * 1024));
        spec.stdin = Some(vec![0u8; 40 * 1024]);
        let err = e.spawn(&spec).unwrap_err().to_string();
        assert!(err.starts_with("spawn sh -c cat > /dev/null yyy"), "{err}");
        assert!(
            err.contains(
                "y… as `tester`: the request is more than one helper frame holds \
                 (131072 bytes) once encoded"
            ),
            "{err}"
        );
        assert!(err.len() < 1024, "{} bytes of message", err.len());
        still_serves(&e);
    }

    /// The size of a command's output in a frame is computed, not encoded,
    /// and computed exactly: a frame of exactly that size carries it, one
    /// byte less does not. Output of every length modulo three, with and
    /// without a signal, a negative status, and both streams.
    #[test]
    fn a_commands_output_is_measured_exactly_without_encoding_it() {
        for (status, signal, out, err) in [
            (0, None, 0, 0),
            (0, None, 1, 0),
            (3, None, 2, 5),
            (-1, Some(9), 3, 4),
            (127, None, 20000, 1),
        ] {
            let o = Output {
                status,
                signal,
                stdout: vec![b'o'; out],
                stderr: vec![0xff; err],
            };
            let exact = encoded_output_len(&o) as usize;
            let resp = HelperResponse::Output(o);
            let body = encode_frame(&resp, exact).unwrap();
            assert_eq!(body.map(|b| b.len()), Some(exact), "{resp:?}");
            assert!(
                encode_frame(&resp, exact - 1).unwrap().is_none(),
                "{resp:?}"
            );
        }
    }

    /// At a limit too small for the helper's wording, the refusal is the
    /// bare marker, and the main side still says what was refused and why.
    #[test]
    fn a_bare_refusal_still_says_what_was_refused() {
        // Room for `{"Err":{"code":27,"message":""}}` and not much more, on
        // the helper's side only.
        let mut e = in_process_within(
            applying(),
            Limits {
                frame: 50,
                chunk: CHUNK_SIZE,
            },
        );
        e.max_frame = MAX_FRAME;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, vec![b'x'; 1000]).unwrap();
        let err = e.read(&f).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "read {} as `tester`: refused: the answer is larger than one helper frame",
                f.display()
            )
        );
    }

    /// A rewrite through the helper keeps the old file's whole mode,
    /// setuid and setgid included, and its owner: the helper's writes are
    /// `Local`'s `Staged`, so `Local`'s rules hold (issue #51). Linux only,
    /// as `Local`'s own test is.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_rewrite_through_the_helper_keeps_setuid_and_setgid() {
        let e = in_process_within(applying(), SMALL);
        let dir = tempfile::tempdir().unwrap();
        for mode in [0o4755, 0o2755, 0o6755, 0o2745, 0o1755, 0o640] {
            let f = dir.path().join(format!("f{mode:o}"));
            std::fs::write(&f, "v1").unwrap();
            Local.set_mode(&f, mode).unwrap();
            let before = Local.stat(&f).unwrap().unwrap();
            assert_eq!(before.mode, mode, "planting {mode:o}");
            let body = data(3 * SMALL.chunk);
            e.write(&f, &body).unwrap();
            let after = Local.stat(&f).unwrap().unwrap();
            assert_eq!(std::fs::read(&f).unwrap(), body);
            assert_eq!(
                (after.mode, after.uid, after.gid),
                (mode, before.uid, before.gid),
                "{mode:o}"
            );
        }
        assert!(staged_in(dir.path()).is_empty());
    }

    /// Releasing a stream never starts a helper, so a reader dropped after
    /// the helper is gone, or before one ever ran, costs no `sudo`; and it
    /// does not panic on a lock poisoned by a panic elsewhere, so it is safe
    /// from a `Drop` that runs while unwinding.
    #[test]
    fn releasing_a_stream_never_spawns_a_helper_or_panics() {
        // `none` refuses at spawn without running anything, and a spawn
        // that failed would be latched.
        let spawner = Spawner {
            method: "none".into(),
            exe: PathBuf::from("/nonexistent/rustible-bin"),
            password: None,
            note: None,
        };
        let e = Elevated::new("root", spawner, applying());
        e.release(HelperOp::Close { handle: 1 });
        drop(Held::new(&e, Some(2), Release::Abort));
        assert!(lock(&e.failed).is_none(), "a helper was started");

        let e = in_process(applying());
        std::thread::scope(|s| {
            let _ = s
                .spawn(|| {
                    let _held = e.conn.lock().unwrap();
                    panic!("poisoning the connection lock on purpose");
                })
                .join();
        });
        assert!(e.conn.is_poisoned());
        e.release(HelperOp::Close { handle: 3 });
        drop(Held::new(&e, Some(4), Release::Close));
        still_serves(&e);
    }

    /// Not a test: the measurement decision 23 on #85 asked for, one 1 GiB
    /// write and one 1 GiB read through an in-process helper, against the
    /// same through `Local`. Run by hand, in release:
    /// `cargo test --release -p rustible-sdk --lib -- --ignored --nocapture measure_one_gib`.
    #[test]
    #[ignore = "a measurement, run by hand in release"]
    fn measure_one_gib_through_the_helper() {
        /// `left` pseudo-random bytes, made as they are read.
        struct Gen {
            left: u64,
            x: u64,
        }
        impl Read for Gen {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let n = buf.len().min(self.left as usize);
                for b in &mut buf[..n] {
                    self.x ^= self.x << 13;
                    self.x ^= self.x >> 7;
                    self.x ^= self.x << 17;
                    *b = self.x as u8;
                }
                self.left -= n as u64;
                Ok(n)
            }
        }
        const GIB: u64 = 1 << 30;
        let dir = tempfile::tempdir().unwrap();
        let e = in_process(applying());
        for (name, backend) in [("Local", &Local as &dyn Backend), ("helper", &e)] {
            let f = dir.path().join(name);
            let t0 = Instant::now();
            let n = backend
                .write_from(&f, &mut Gen { left: GIB, x: 7 }, None)
                .unwrap();
            let wrote = t0.elapsed();
            assert_eq!(n, GIB);
            let t0 = Instant::now();
            let read = io::copy(&mut backend.open_read(&f).unwrap(), &mut io::sink()).unwrap();
            let took = t0.elapsed();
            assert_eq!(read, GIB);
            let rate = |d: Duration| 1024.0 / d.as_secs_f64();
            println!(
                "{name}: write 1 GiB {wrote:.2?} ({:.0} MiB/s), read 1 GiB {took:.2?} ({:.0} MiB/s)",
                rate(wrote),
                rate(took)
            );
            std::fs::remove_file(&f).unwrap();
        }
    }

    /// A chunk's frame is built in one buffer of about its size, either way:
    /// without the estimate it doubled its way there, the last time at the
    /// closing quote, to twice the frame (and copied and wiped each time).
    #[test]
    fn a_chunk_frame_is_built_without_growing() {
        let bytes = Zeroizing::new(data(CHUNK_SIZE));
        let answer = HelperResponse::Data {
            handle: Some(3),
            bytes: bytes.clone(),
        };
        let request = HelperRequest {
            checking: false,
            op: HelperOp::WriteChunk {
                handle: 3,
                offset: 0,
                bytes,
                last: false,
            },
        };
        for (what, body) in [
            (
                "answer",
                encode_frame_sized(&answer, MAX_FRAME, answer.size_hint()),
            ),
            (
                "request",
                encode_frame_sized(&request, MAX_FRAME, request.op.size_hint()),
            ),
        ] {
            let body = body.unwrap().unwrap();
            assert!(
                body.capacity() < body.len() + 1024,
                "{what}: {} bytes in a buffer of {}",
                body.len(),
                body.capacity()
            );
        }
    }

    /// An answer of the wrong shape is reported by its variant's name, never
    /// its contents, which may be a file's.
    #[test]
    fn an_unexpected_answer_quotes_nothing() {
        let err = unexpected(&HelperResponse::Data {
            handle: None,
            bytes: Zeroizing::new(b"hunter2".to_vec()),
        });
        assert_eq!(err.to_string(), "unexpected helper response Data");
    }

    /// An `Elevated` whose "helper" printed to stderr and exited without
    /// answering, which is what a refused `sudo -n` looks like from here.
    /// The connection comes from [`Spawner::connection`], the same call
    /// `Spawner::spawn` makes once `sudo` has started, so only the `sudo`
    /// itself is bypassed.
    fn dead_helper(note: Option<String>) -> Elevated {
        dead_helper_running("echo boom >&2; exit 7", note)
    }

    /// [`dead_helper`] with `script` as the "helper". The request goes out
    /// straight away, without waiting for the stderr reader, because that
    /// race is what a real refused `sudo` runs (#90).
    fn dead_helper_running(script: &str, note: Option<String>) -> Elevated {
        let spawner = Spawner {
            method: "sudo".into(),
            exe: PathBuf::from("/nonexistent/rustible-bin"),
            password: None,
            note,
        };
        let child = Command::new("sh")
            .args(["-c", script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let conn = spawner.connection("root", child);
        Elevated {
            user: "root".into(),
            spawner: Some(spawner),
            phase: Arc::new(AtomicU8::new(0)),
            conn: Mutex::new(Some(conn)),
            failed: Mutex::new(None),
            max_frame: MAX_FRAME,
            chunk: CHUNK_SIZE,
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

    /// A helper that says why on stderr and exits at once has that line in
    /// the report, every time: the report waits for its stderr reader to
    /// reach EOF rather than reading the tail as it stands when the exit is
    /// seen, which left it empty now and then on a busy machine (#90). Run
    /// many times, because one run loses the race only occasionally, and on
    /// a thread with a bound, so a report that waits forever fails instead
    /// of hanging.
    #[test]
    fn a_helper_that_exits_at_once_reports_its_stderr_every_time() {
        const RUNS: usize = 3000;
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let mut lost = Vec::new();
            for _ in 0..RUNS {
                let e =
                    dead_helper_running("echo 'sudo: a password is required' >&2; exit 1", None);
                let err = e.read(Path::new("/root")).unwrap_err().to_string();
                if !err.contains("helper exited 1: sudo: a password is required") {
                    lost.push(err);
                }
            }
            let _ = done.send(lost);
        });
        let lost = finished
            .recv_timeout(Duration::from_secs(120))
            .expect("the reports did not all come back within 120s");
        assert!(
            lost.is_empty(),
            "{} of {RUNS} reports lost the stderr line, e.g. {:?}",
            lost.len(),
            lost[0]
        );
    }

    /// A helper that exits while something it started keeps its stderr open
    /// is still reported, without waiting for that to let go: the wait for
    /// the stderr reader is bounded, and the report has what came before.
    #[test]
    fn a_helper_whose_stderr_outlives_it_is_reported_within_the_bound() {
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let e = dead_helper_running(
                "echo boom >&2; sleep 30 </dev/null >/dev/null & exit 7",
                None,
            );
            let t0 = Instant::now();
            let err = e.read(Path::new("/etc/hostname")).unwrap_err().to_string();
            let _ = done.send((err, t0.elapsed()));
        });
        let (err, took) = finished
            .recv_timeout(Duration::from_secs(20))
            .expect("a helper whose stderr stayed open was not reported within 20s");
        assert!(err.starts_with("helper exited 7: boom"), "{err}");
        assert!(took < Duration::from_secs(10), "took {took:?}");
    }

    /// Shutting a helper down waits for its stderr reader without adding
    /// to the wait: one that exits at EOF on stdin is gone in a moment, and
    /// one that has to be killed is not held up a further [`SETTLE`] by
    /// what it left writing to its stdout, which keeps its stderr open
    /// until that stdout is closed.
    #[test]
    fn a_shutdown_waits_for_stderr_only_as_long_as_the_helper() {
        let spawner = Spawner {
            method: "sudo".into(),
            exe: PathBuf::from("/nonexistent/rustible-bin"),
            password: None,
            note: None,
        };
        let shutdown = move |script: &str| {
            let child = Command::new("sh")
                .args(["-c", script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let conn = spawner.connection("root", child);
            let t0 = Instant::now();
            conn.shutdown();
            t0.elapsed()
        };
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            let healthy = shutdown("cat > /dev/null");
            let killed = shutdown("yes < /dev/null & wait");
            let _ = done.send((healthy, killed));
        });
        let (healthy, killed) = finished
            .recv_timeout(Duration::from_secs(20))
            .expect("the shutdowns did not finish within 20s");
        assert!(healthy < Duration::from_secs(1), "took {healthy:?}");
        // Two seconds for the helper to go before it is killed, and none
        // waiting for stderr after.
        assert!(killed < Duration::from_secs(3), "took {killed:?}");
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
