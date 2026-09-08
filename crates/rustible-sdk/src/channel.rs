//! The binary's link to whoever drives it (vision doc 5.5, 11): in `--remote`
//! mode the orchestrator over stdin/stdout, in a local run the process itself
//! serving files from the working directory, in tests nothing at all.
//!
//! Down frames that are not `Start` arrive on a reader thread (`Feeder`):
//! `Cancel` flips a flag that `Ctx::step` checks between phases; file frames
//! are queued by request id so requests may overlap.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::error::{Error, Result};
use crate::protocol::{Down, Up, UpLink};
use crate::stream::{Chunk, WorkspaceFiles, chunks};

/// Frames queued for `local_file`/`local_secret`, by request id.
#[derive(Default)]
struct Inbox {
    frames: Mutex<InboxState>,
    changed: Condvar,
}

#[derive(Default)]
struct InboxState {
    pending: VecDeque<Down>,
    /// The reader thread saw EOF: nothing more will ever arrive.
    closed: bool,
}

impl Inbox {
    fn push(&self, frame: Down) {
        self.frames.lock().unwrap().pending.push_back(frame);
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
                return Ok(state.pending.remove(i).expect("indexed"));
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

    pub fn cancel(&self, reason: impl Into<String>) {
        let mut r = self.cancel_reason.lock().unwrap();
        if r.is_none() {
            *r = Some(reason.into());
        }
        self.cancelled.store(true, Ordering::SeqCst);
    }

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
                loop {
                    match inbox.recv(req)? {
                        Down::FileChunk {
                            offset,
                            bytes,
                            last,
                            ..
                        } => {
                            sink(Chunk {
                                offset,
                                bytes,
                                last,
                            })?;
                            if last {
                                return Ok(());
                            }
                        }
                        Down::FileDenied { reason, .. } => {
                            return Err(Error::msg(format!("`{path}` denied: {reason}")));
                        }
                        _ => unreachable!("inbox only holds file frames"),
                    }
                }
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
