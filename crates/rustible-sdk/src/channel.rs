//! The binary's link to whoever drives it (vision doc 5.5, 11): in `--remote`
//! mode the orchestrator over stdin/stdout, in a local run the process itself
//! serving files from the working directory, in tests nothing at all.
//!
//! Down frames that are not `Start` arrive on a reader thread (`Feeder`):
//! `Cancel` flips a flag that `Ctx::step` checks between phases; file frames
//! are queued by request id so requests may overlap.

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::error::{Error, Result};
use crate::protocol::{Down, Up, UpLink};
use crate::stream::{Chunk, WorkspaceFiles, chunks};

/// How many file frames may sit in the inbox before the reader thread
/// stops taking them off the pipe. Chunks are 1 MiB, so this is the whole
/// buffer between the orchestrator and the playbook's writer. Without a
/// bound the reader always outruns the consumer writing to disk and
/// `pending` grows to the size of the file, which is exactly what chunking
/// exists to avoid: a 50 MB stream was held in memory whole on the target.
///
/// One global bound is safe because at most one file request is ever in
/// flight: `Ctx` is `!Send` and `local_file`, `local_secret` and `fetch`
/// each block the playbook thread until they finish. Genuinely concurrent
/// requests would need per-request flow control, since one reader thread
/// blocked on a full queue stops delivering every id, not just the full
/// one.
const MAX_PENDING_FRAMES: usize = 8;

/// Frames queued for `local_file`/`local_secret`, by request id.
#[derive(Default)]
struct Inbox {
    frames: Mutex<InboxState>,
    /// Signals both directions: a frame arrived, or one was taken. Both
    /// waiters re-check their own predicate, so one condvar is enough.
    changed: Condvar,
}

#[derive(Default)]
struct InboxState {
    pending: VecDeque<Down>,
    /// The reader thread saw EOF: nothing more will ever arrive.
    closed: bool,
    /// Requests whose consumer gave up part way (a disk write failed, the
    /// orchestrator denied the transfer mid-stream). Their frames are
    /// dropped on arrival: queueing them would fill the inbox with frames
    /// nobody will ever collect, and a reader thread blocked on a full
    /// inbox stops delivering `Cancel` as well.
    abandoned: BTreeSet<u32>,
}

impl Inbox {
    fn push(&self, frame: Down) {
        let mut state = self.frames.lock().unwrap();
        loop {
            if let Some(req) = frame_req(&frame)
                && state.abandoned.contains(&req)
            {
                if ends_a_request(&frame) {
                    state.abandoned.remove(&req);
                }
                drop(state);
                self.changed.notify_all();
                return;
            }
            if state.pending.len() < MAX_PENDING_FRAMES || state.closed {
                break;
            }
            state = self.changed.wait(state).unwrap();
        }
        state.pending.push_back(frame);
        drop(state);
        self.changed.notify_all();
    }

    /// The consumer of `req` has gone. Drop what is queued for it and keep
    /// dropping what arrives until the stream ends.
    fn abandon(&self, req: u32) {
        let mut state = self.frames.lock().unwrap();
        state.pending.retain(|f| frame_req(f) != Some(req));
        state.abandoned.insert(req);
        drop(state);
        self.changed.notify_all();
    }

    fn close(&self) {
        self.frames.lock().unwrap().closed = true;
        self.changed.notify_all();
    }

    /// Block until a frame for `req` arrives; frames for other requests stay
    /// queued. Errors once the channel is closed.
    fn recv(&self, req: u32) -> Result<Down> {
        let mut state = self.frames.lock().unwrap();
        loop {
            if let Some(i) = state.pending.iter().position(|f| frame_req(f) == Some(req)) {
                let frame = state.pending.remove(i).expect("indexed");
                drop(state);
                // A slot opened: wake the reader thread if it was waiting.
                self.changed.notify_all();
                return Ok(frame);
            }
            if state.closed {
                return Err(Error::msg(
                    "the orchestrator closed the channel before the file arrived",
                ));
            }
            state = self.changed.wait(state).unwrap();
        }
    }
}

/// The last frame of a request: nothing more arrives for that id after it.
fn ends_a_request(f: &Down) -> bool {
    matches!(
        f,
        Down::FileDenied { .. } | Down::FileChunk { last: true, .. }
    )
}

fn frame_req(f: &Down) -> Option<u32> {
    match f {
        Down::FileChunk { req, .. } | Down::FileDenied { req, .. } => Some(*req),
        _ => None,
    }
}

enum Source {
    /// `--remote`: requests go up as frames, answers come through the inbox.
    Remote { up: Arc<dyn UpLink>, inbox: Inbox },
    /// A local run: served in-process from a directory, same rules.
    Local(WorkspaceFiles),
    /// Nothing behind the channel (tests).
    None,
}

/// See the module docs.
pub struct Channel {
    cancelled: AtomicBool,
    cancel_reason: Mutex<Option<String>>,
    next_req: AtomicU32,
    source: Source,
}

/// The reader thread's handle: everything after `Start` goes through here.
pub struct Feeder(Arc<Channel>);

impl Feeder {
    /// Hand one down frame to the channel. [`Down::Cancel`] flips the cancel
    /// flag; a second [`Down::Start`] is dropped, since the runtime consumed
    /// the run's `Start` before this thread existed; file frames go to the
    /// inbox, keyed by request id. Blocks while the inbox is full, which is
    /// how backpressure reaches the pipe instead of the whole file reaching
    /// memory.
    pub fn feed(&self, frame: Down) {
        match frame {
            Down::Cancel => self.0.cancel("cancelled by the orchestrator"),
            Down::Start { .. } => {} // one Start per run; a second is ignored
            other => {
                if let Source::Remote { inbox, .. } = &self.0.source {
                    inbox.push(other);
                }
            }
        }
    }

    /// EOF on stdin. The run is cancelled: with the orchestrator gone nobody
    /// receives the report, and stopping between steps beats a blind run.
    pub fn close(&self) {
        if let Source::Remote { inbox, .. } = &self.0.source {
            inbox.close();
        }
        self.0.cancel("the orchestrator closed the channel");
    }
}

impl Channel {
    fn with_source(source: Source) -> Arc<Channel> {
        Arc::new(Channel {
            cancelled: AtomicBool::new(false),
            cancel_reason: Mutex::new(None),
            next_req: AtomicU32::new(1),
            source,
        })
    }

    /// Driven by an orchestrator through `up`; feed the down frames.
    pub fn remote(up: Arc<dyn UpLink>) -> (Arc<Channel>, Feeder) {
        let ch = Self::with_source(Source::Remote {
            up,
            inbox: Inbox::default(),
        });
        (ch.clone(), Feeder(ch))
    }

    /// Files served from `root` in-process (a local run). `Feeder` still
    /// works for cancellation.
    pub fn local(files: WorkspaceFiles) -> (Arc<Channel>, Feeder) {
        let ch = Self::with_source(Source::Local(files));
        (ch.clone(), Feeder(ch))
    }

    /// No orchestrator and no files; `local_file` fails. For tests.
    pub fn detached() -> Arc<Channel> {
        Self::with_source(Source::None)
    }

    /// Ask the run to stop. Nothing is interrupted: the flag is sticky and
    /// `Ctx::step` reads it between phases, so no op is torn off mid-apply.
    /// The first `reason` wins and later calls only re-set the flag, because
    /// the orchestrator's `Cancel` is immediately followed by the EOF that
    /// would otherwise overwrite it with a less useful message.
    pub fn cancel(&self, reason: impl Into<String>) {
        let mut r = self.cancel_reason.lock().unwrap();
        if r.is_none() {
            *r = Some(reason.into());
        }
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Whether [`Channel::cancel`] has been called. Once true it never goes
    /// back. Use [`Channel::check_cancelled`] where an error is wanted; this
    /// is for a long-running op that wants to bail out of its own loop.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// `Err("cancelled: <reason>")` once cancelled, so a step can `?` it.
    pub fn check_cancelled(&self) -> Result<()> {
        if self.is_cancelled() {
            let reason = self
                .cancel_reason
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default();
            return Err(Error::msg(format!("cancelled: {reason}")));
        }
        Ok(())
    }

    /// Stream a workspace file, chunk by chunk, into `sink`. Denials and
    /// missing files come back as errors with the orchestrator's reason.
    pub fn stream_file(&self, path: &str, sink: &mut dyn FnMut(Chunk) -> Result<()>) -> Result<()> {
        match &self.source {
            Source::Remote { up, inbox } => {
                let req = self.next_req.fetch_add(1, Ordering::SeqCst);
                up.send(&Up::FileRequest {
                    req,
                    path: path.to_string(),
                })
                .map_err(|e| Error::msg(format!("requesting `{path}`: {e}")))?;
                let mut ended = false;
                let out = loop {
                    let frame = match inbox.recv(req) {
                        Ok(f) => f,
                        Err(e) => break Err(e),
                    };
                    match frame {
                        Down::FileChunk {
                            offset,
                            bytes,
                            last,
                            ..
                        } => {
                            if let Err(e) = sink(Chunk {
                                offset,
                                bytes,
                                last,
                            }) {
                                break Err(e);
                            }
                            if last {
                                ended = true;
                                break Ok(());
                            }
                        }
                        Down::FileDenied { reason, .. } => {
                            ended = true;
                            break Err(Error::msg(format!("`{path}` denied: {reason}")));
                        }
                        _ => unreachable!("inbox only holds file frames"),
                    }
                };
                if !ended {
                    // The sink failed or the channel closed part way. The
                    // orchestrator is still sending chunks for this id, so
                    // say so rather than let them pile up behind a bounded
                    // inbox and stall the reader thread.
                    inbox.abandon(req);
                }
                out
            }
            Source::Local(files) => {
                let f = files
                    .open(path)
                    .map_err(|reason| Error::msg(format!("`{path}` denied: {reason}")))?;
                for c in chunks(f) {
                    let c = c.map_err(|e| Error::msg(format!("reading `{path}`: {e}")))?;
                    sink(c)?;
                }
                Ok(())
            }
            Source::None => Err(Error::msg(format!(
                "`{path}`: no orchestrator to stream files from"
            ))),
        }
    }

    /// Send one chunk of a fetched file toward `dest` on the orchestrator.
    pub fn send_fetch(&self, req: u32, dest: &str, chunk: &Chunk) -> Result<()> {
        match &self.source {
            Source::Remote { up, .. } => up
                .send(&Up::FetchChunk {
                    req,
                    dest: dest.to_string(),
                    offset: chunk.offset,
                    bytes: chunk.bytes.clone(),
                    last: chunk.last,
                })
                .map_err(|e| Error::msg(format!("sending `{dest}`: {e}"))),
            Source::Local(files) => files
                .write_chunk(dest, chunk.offset, &chunk.bytes)
                .map(|_| ())
                .map_err(|reason| Error::msg(format!("`{dest}` denied: {reason}"))),
            Source::None => Err(Error::msg(format!(
                "`{dest}`: no orchestrator to fetch files to"
            ))),
        }
    }

    /// Take the next request id, counting from 1. Ids come from the same
    /// counter [`Channel::stream_file`] draws from, so a fetch can never
    /// collide with an in-flight file request. `ctx.fetch` takes one id and
    /// reuses it for every chunk of the file; `ctx.local_file` takes one to
    /// name a fresh subdirectory of the run's temp directory, so two
    /// downloads of files with the same basename do not overwrite each other.
    pub fn next_req(&self) -> u32 {
        self.next_req.fetch_add(1, Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct Capture(Mutex<Vec<Up>>);
    impl UpLink for Capture {
        fn send(&self, up: &Up) -> io::Result<()> {
            self.0.lock().unwrap().push(up.clone());
            Ok(())
        }
    }

    #[test]
    fn cancel_flag_is_sticky_and_keeps_first_reason() {
        let (ch, feeder) = Channel::remote(Arc::new(Capture(Mutex::new(vec![]))));
        assert!(!ch.is_cancelled());
        assert!(ch.check_cancelled().is_ok());
        feeder.feed(Down::Cancel);
        assert!(ch.is_cancelled());
        let err = ch.check_cancelled().unwrap_err().to_string();
        assert_eq!(err, "cancelled: cancelled by the orchestrator");
        feeder.close();
        assert_eq!(
            ch.check_cancelled().unwrap_err().to_string(),
            "cancelled: cancelled by the orchestrator"
        );
    }

    #[test]
    fn eof_on_the_channel_cancels() {
        let (ch, feeder) = Channel::remote(Arc::new(Capture(Mutex::new(vec![]))));
        feeder.close();
        assert!(
            ch.check_cancelled()
                .unwrap_err()
                .to_string()
                .contains("closed the channel")
        );
    }

    /// The inbox is bounded, so a feeder that outruns the consumer waits
    /// instead of buffering the whole file. Before this, a 50 MB stream was
    /// held in memory on the target and the chunking bought nothing.
    #[test]
    fn the_feeder_waits_rather_than_buffering_a_whole_file() {
        let (ch, feeder) = Channel::remote(Arc::new(Capture(Mutex::new(vec![]))));
        let fed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (fed_w, total) = (fed.clone(), MAX_PENDING_FRAMES * 4);
        std::thread::spawn(move || {
            for i in 0..total {
                feeder.feed(Down::FileChunk {
                    req: 1,
                    offset: i as u64,
                    bytes: vec![b'x'],
                    last: i + 1 == total,
                });
                fed_w.fetch_add(1, Ordering::SeqCst);
            }
        });
        // Give the feeder every chance to run ahead; it cannot get past the
        // bound plus the one frame a blocked push is holding.
        for _ in 0..50 {
            if fed.load(Ordering::SeqCst) > MAX_PENDING_FRAMES {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            fed.load(Ordering::SeqCst) <= MAX_PENDING_FRAMES + 1,
            "the feeder buffered {} frames with a bound of {MAX_PENDING_FRAMES}",
            fed.load(Ordering::SeqCst)
        );
        let mut got = 0usize;
        ch.stream_file("files/big", &mut |c| {
            got += c.bytes.len();
            Ok(())
        })
        .unwrap();
        assert_eq!(got, total);
    }

    /// A consumer that gives up part way must not wedge the reader thread:
    /// with a bounded inbox, frames nobody collects would fill it and the
    /// thread that delivers `Cancel` would block behind them forever.
    #[test]
    fn abandoning_a_transfer_does_not_wedge_the_reader() {
        let (ch, feeder) = Channel::remote(Arc::new(Capture(Mutex::new(vec![]))));
        let feeder = Arc::new(feeder);
        // The sink fails on the first chunk, so request 1 is abandoned with
        // the orchestrator still sending.
        feeder.feed(Down::FileChunk {
            req: 1,
            offset: 0,
            bytes: vec![b'x'],
            last: false,
        });
        let err = ch
            .stream_file("files/big", &mut |_| Err(Error::msg("disk full")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("disk full"), "{err}");

        // Far more leftover chunks than the bound. Each must be dropped on
        // arrival rather than queued, so this returns rather than blocking.
        let f = feeder.clone();
        let done = std::thread::spawn(move || {
            for i in 1..(MAX_PENDING_FRAMES * 5) {
                f.feed(Down::FileChunk {
                    req: 1,
                    offset: i as u64,
                    bytes: vec![b'x'],
                    last: false,
                });
            }
        });
        for _ in 0..200 {
            if done.is_finished() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            done.is_finished(),
            "the reader thread blocked on frames of an abandoned request"
        );
        done.join().unwrap();

        // And the channel still works: `Cancel` gets through, and a fresh
        // request is served normally.
        feeder.feed(Down::Cancel);
        assert!(ch.is_cancelled());
    }

    #[test]
    fn file_frames_are_matched_by_request_id_and_may_overlap() {
        let up = Arc::new(Capture(Mutex::new(vec![])));
        let (ch, feeder) = Channel::remote(up.clone());
        // Frames for request 2 arrive before request 1 is even made.
        feeder.feed(Down::FileChunk {
            req: 2,
            offset: 0,
            bytes: b"two".to_vec(),
            last: true,
        });
        feeder.feed(Down::FileChunk {
            req: 1,
            offset: 0,
            bytes: b"on".to_vec(),
            last: false,
        });
        feeder.feed(Down::FileChunk {
            req: 1,
            offset: 2,
            bytes: b"e".to_vec(),
            last: true,
        });
        let mut got = Vec::new();
        ch.stream_file("files/one", &mut |c| {
            got.extend(c.bytes);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, b"one");
        let mut got = Vec::new();
        ch.stream_file("files/two", &mut |c| {
            got.extend(c.bytes);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, b"two");
        let sent = up.0.lock().unwrap();
        assert!(matches!(&sent[0], Up::FileRequest { req: 1, path } if path == "files/one"));
        assert!(matches!(&sent[1], Up::FileRequest { req: 2, path } if path == "files/two"));
    }

    #[test]
    fn denial_and_eof_surface_as_errors() {
        let (ch, feeder) = Channel::remote(Arc::new(Capture(Mutex::new(vec![]))));
        feeder.feed(Down::FileDenied {
            req: 1,
            reason: "outside the workspace".into(),
        });
        let err = ch
            .stream_file("../x", &mut |_| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("denied") && err.contains("outside"), "{err}");
        feeder.close();
        let err = ch
            .stream_file("files/y", &mut |_| Ok(()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed the channel"), "{err}");
    }

    #[test]
    fn local_source_serves_and_writes_under_the_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"local").unwrap();
        let (ch, _feeder) = Channel::local(WorkspaceFiles::new(dir.path()).unwrap());
        let mut got = Vec::new();
        ch.stream_file("f.txt", &mut |c| {
            got.extend(c.bytes);
            Ok(())
        })
        .unwrap();
        assert_eq!(got, b"local");
        assert!(ch.stream_file("/etc/passwd", &mut |_| Ok(())).is_err());
        ch.send_fetch(
            1,
            "out/h",
            &Chunk {
                offset: 0,
                bytes: b"host".to_vec(),
                last: true,
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(dir.path().join("out/h")).unwrap(), b"host");
        assert!(
            Channel::detached()
                .stream_file("f.txt", &mut |_| Ok(()))
                .is_err()
        );
    }
}
