//! The wire protocol between orchestrator and playbook binary. Frames are a
//! u32 big-endian length followed by a JSON payload. JSON for now because it
//! is debuggable; the codec is the only place that would change.
//!
//! Two enums cross the wire: [`Down`] from the orchestrator and [`Up`] from
//! the binary, both in serde's default externally tagged form, so the variant
//! name is the JSON key. Everything they carry is part of the format too,
//! which means [`HostInfo`], [`Event`] and [`Secret`] here, and `CmdSpec`,
//! `Output` and `Stat` in the helper protocol, which reuses these frames.
//! Adding a field with `#[serde(default)]` leaves an older peer's frames
//! readable; any other change to the shapes is incompatible and bumps
//! [`PROTOCOL_VERSION`], which the two ends compare in [`Up::Hello`] before
//! the first step runs.
//!
//! Byte fields (file chunks, command stdin) travel as base64 strings: a JSON
//! array of numbers would cost 3.5x the payload, base64 costs 1.33x, and the
//! frame stays printable. Every frame body is zeroized after decoding so a
//! secret chunk or an escalation password does not linger in freed memory.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::ctx::HostInfo;
use crate::event::Event;
use crate::secret::Secret;

/// Bumped on every incompatible frame change. 2: `Start.playbook`, `Failed.cmd`.
/// 3: file streaming frames (`FileRequest`, `FileChunk`, `FileDenied`,
/// `FetchChunk`), `Start.escalate_password`, base64 byte fields.
/// 4: `Facts.package_managers` (a set) replaces `package_manager`, `Pm::Other`
/// is gone, and `Os`, `Distro`, `Pm`, `Init` gain the macOS variants.
/// 5: `Ctx::block` — `BlockStarted`/`BlockFinished` replace
/// `SectionStarted`/`SectionFinished`, steps carry `blocks` instead of `depth`.
/// 6: the playbook's `ssh_user` — `--describe` entries carry `ssh_user`, and
/// `Start.host` carries `login_override`.
/// 7: the host's verdict is what the playbook returns — `Summary.recovered`
/// counts the failed steps the playbook caught, `Summary.failed` only those
/// that failed the host, and `Failed` carries the step's `id` and `blocks`.
pub const PROTOCOL_VERSION: u32 = 7;

/// Bytes per streamed chunk (vision doc 5.6).
pub const CHUNK_SIZE: usize = 1024 * 1024;

/// Orchestrator -> binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Down {
    /// Always the first frame, and the only one the binary reads
    /// synchronously: `--remote` refuses to start on anything else. A second
    /// `Start` in the same connection is ignored rather than restarting the
    /// run.
    Start {
        /// Unique per host run, minted by the orchestrator. It names the
        /// run's temp directory on the target, so two runs against one host
        /// do not tread on each other.
        run_id: String,
        /// Registry name of the playbook to run (`cadu/mc`). A shipped binary
        /// holds one playbook, but an IDE-style build holds them all. Empty
        /// (older orchestrators) means "the only playbook in this binary".
        #[serde(default)]
        playbook: String,
        /// The inventory's view of this host: its name, its groups, and the
        /// escalation and connection parameters resolved for it. The binary
        /// never reads an inventory of its own, so this is the whole of what
        /// it knows about where it is running.
        host: HostInfo,
        /// Merged inventory vars for this host. The macro will deserialize
        /// this into the playbook's typed struct.
        vars: serde_json::Value,
        /// `--check`: the binary runs every op's `check` and no `apply`.
        /// The orchestrator decides this once for the whole run, and the
        /// binary has no way to turn it off.
        check_mode: bool,
        /// How many `-v` flags the operator passed, so the binary can know
        /// what will be rendered. The binary currently emits every event
        /// regardless and lets the orchestrator do the filtering, so this is
        /// carried but not acted on.
        verbosity: u8,
        /// The escalation password when the host's `sudo` needs one; the
        /// helper spawn feeds it with `sudo -S`. In memory only, zeroized on
        /// drop (vision doc 11.3).
        #[serde(default)]
        escalate_password: Option<Secret>,
    },
    /// Ctrl-c on the orchestrator: stop between steps (vision doc 5.5).
    Cancel,
    /// Answers a `FileRequest`. `last` marks the final chunk; an empty file is
    /// one empty last chunk.
    FileChunk {
        /// Echoes the `req` of the [`Up::FileRequest`] this answers. The
        /// binary can have several requests open at once, so this is the
        /// only thing that says which stream a chunk belongs to.
        req: u32,
        /// Byte offset of this chunk within the file. Chunks for one `req`
        /// arrive in order, so the receiver appends; the offset is what
        /// lets it check that assumption.
        offset: u64,
        /// Up to [`CHUNK_SIZE`] bytes of the file, base64 on the wire.
        #[serde(with = "b64")]
        bytes: Vec<u8>,
        /// Set on the final chunk. Exactly one chunk per `req` carries it,
        /// unless the request ended in a [`Down::FileDenied`] instead;
        /// after either the id is finished and must not be reused.
        last: bool,
    },
    /// The orchestrator refused a `FileRequest` (outside the workspace, missing).
    ///
    /// Also sent to abandon a transfer already in flight, for example when
    /// the run is cancelled mid-file: the binary's read fails at once rather
    /// than waiting for chunks that will never come. Ends the request just
    /// as a `last` chunk does.
    FileDenied {
        /// Echoes the `req` of the [`Up::FileRequest`] being refused.
        req: u32,
        /// Why, in the orchestrator's words, ready to be shown to the
        /// operator inside the step's error.
        reason: String,
    },
}

/// Binary -> orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)] // wire type; size is irrelevant
pub enum Up {
    /// The binary's first frame, sent before anything runs. The
    /// orchestrator fails the host on a mismatch in either field rather than
    /// starting a run it cannot interpret.
    Hello {
        /// The binary's [`PROTOCOL_VERSION`]. A value other than the
        /// orchestrator's own means the workspace was built against a
        /// different rustible and has to be rebuilt.
        protocol: u32,
        /// Which playbook the binary resolved out of the `Start` frame. The
        /// orchestrator checks it against the name it asked for, so a binary
        /// holding several playbooks cannot quietly run the wrong one.
        playbook: String,
    },
    /// A step boundary, a log line, a command that ran. This is the bulk of
    /// the traffic and the only frame the orchestrator turns into report
    /// output.
    Event(Event),
    /// "Send me `path`", relative to the workspace root.
    FileRequest {
        /// A fresh id from the binary's counter, which starts at 1 and only
        /// goes up. Every [`Down::FileChunk`] and [`Down::FileDenied`] for
        /// this file quotes it back.
        req: u32,
        /// Workspace-relative path. The orchestrator resolves it against
        /// the workspace root and refuses anything that escapes, so the
        /// binary cannot read arbitrary files off the control machine.
        path: String,
    },
    /// A piece of a file the binary is sending back (`ctx.fetch`), to be
    /// written at `dest` relative to the workspace root.
    FetchChunk {
        /// Identifies the fetch, from the same counter as
        /// [`Up::FileRequest`]. Nothing comes back for it: the orchestrator
        /// writes as it receives and only reports at `last`.
        req: u32,
        /// Workspace-relative destination, repeated on every chunk so the
        /// orchestrator never has to remember which id was writing where.
        /// It is resolved and refused under the same rules as a
        /// `FileRequest` path.
        dest: String,
        /// Byte offset to write this chunk at. The orchestrator seeks, so a
        /// chunk that arrives out of order still lands correctly.
        offset: u64,
        /// Up to [`CHUNK_SIZE`] bytes, base64 on the wire.
        #[serde(with = "b64")]
        bytes: Vec<u8>,
        /// Set on the final chunk of this fetch. That is when the
        /// orchestrator counts the file as written and reports it.
        last: bool,
    },
}

/// A 1 MiB chunk of base64 plus JSON framing fits with room to spare.
pub const MAX_FRAME: usize = 64 * 1024 * 1024;

/// The largest payload that survives a single frame: bytes travel as
/// base64, so four bytes on the wire carry three of payload, and the JSON
/// envelope needs a little room besides. Anything that puts a whole file
/// in one frame (`HelperResponse::Bytes`, and so every escalated read)
/// must refuse above this rather than build a frame the far end rejects.
pub const MAX_FRAME_PAYLOAD: usize = MAX_FRAME / 4 * 3 - 64 * 1024;

/// Serialize `msg` and write it as one length-prefixed frame, flushing
/// before returning so the far end is not left waiting on a buffer.
///
/// Errors if the value will not serialize, if the body does not fit in the
/// `u32` length ("frame too large"), or on the write itself. The encoded
/// body is held in `Zeroizing` memory, so a frame carrying a secret or a
/// file chunk is wiped rather than left in the freed allocation.
///
/// Callers have to serialize their own access to `w`: two frames written
/// concurrently would interleave. [`FrameSink`] is the wrapper that does
/// this for the binary's stdout.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = Zeroizing::new(serde_json::to_vec(msg).map_err(io::Error::other)?);
    let len = u32::try_from(body.len()).map_err(|_| io::Error::other("frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// A frame's length prefix asked for more than [`MAX_FRAME`] bytes.
///
/// Carried as the inner error of the `io::Error` [`read_frame`] returns, so
/// a caller that has a better refusal to offer can recognise this case with
/// `err.get_ref().and_then(|e| e.downcast_ref::<FrameTooLarge>())` instead
/// of matching on the message text. The
/// [`Elevated`](crate::backend::Elevated) backend does exactly that, to say
/// which file was too large to read as another user.
#[derive(Debug, thiserror::Error)]
#[error("frame of {len} bytes exceeds limit")]
pub struct FrameTooLarge {
    /// What the length prefix asked for, in bytes. Nothing was allocated:
    /// the prefix is checked before the body is read.
    pub len: usize,
}

/// Read one length-prefixed frame and deserialize it.
///
/// `Ok(None)` on clean EOF before a frame starts, which is how each end
/// learns the other has gone away. EOF part way through a frame is an error,
/// as is a length above [`MAX_FRAME`] (refused before allocating, so a
/// corrupt or hostile prefix cannot ask for gigabytes, and reported as a
/// [`FrameTooLarge`] inside the `io::Error`) and a body that does not
/// deserialize into `T`. The body is held in `Zeroizing` memory and
/// wiped once decoded.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other(FrameTooLarge { len }));
    }
    let mut body = Zeroizing::new(vec![0u8; len]);
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

/// Serde helper: a byte field as a base64 string. `#[serde(with = "b64")]`.
pub mod b64 {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Standard base64 with padding, emitted as a JSON string.
    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

    /// Decodes the string back to bytes, borrowing it when the format
    /// allows. Anything that is not valid standard base64 is a
    /// deserialization error, so a mangled frame fails here rather than
    /// producing truncated file contents.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        STANDARD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)
    }
}

/// Serde helper for `Option<Vec<u8>>`. `#[serde(with = "b64_opt")]`.
pub mod b64_opt {
    use serde::{Deserialize, Deserializer, Serializer};

    /// `None` becomes JSON `null`, `Some` a base64 string. Pair it with
    /// `#[serde(default)]` so an older peer that omits the field decodes as
    /// `None` instead of failing.
    pub fn serialize<S: Serializer>(bytes: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => super::b64::serialize(b, s),
            None => s.serialize_none(),
        }
    }

    /// `null` gives `None`; a string is decoded by
    /// [`b64::deserialize`](super::b64::deserialize) and its errors apply.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<u8>>, D::Error> {
        #[derive(Deserialize)]
        struct Wrap(#[serde(with = "super::b64")] Vec<u8>);
        Ok(Option::<Wrap>::deserialize(d)?.map(|w| w.0))
    }
}

/// The binary's end of the channel: everything that goes up (events and
/// streaming frames) is serialized through one writer, so frames never
/// interleave (vision doc 5.5: nothing else may write to stdout in `--remote`).
pub trait UpLink: Send + Sync {
    /// Write one frame, whole, before any other caller's. Implementations
    /// take a lock rather than buffering, so a `send` that returns has put
    /// the frame on the wire.
    ///
    /// The error is the write's own: a broken pipe here means the
    /// orchestrator is gone. Event emission ignores it, because there is
    /// nobody left to tell; the file-streaming callers turn it into a step
    /// failure.
    fn send(&self, up: &Up) -> io::Result<()>;
}

/// An `EventSink` and `UpLink` that writes frames to a writer.
pub struct FrameSink<W: Write + Send>(pub std::sync::Mutex<W>);

impl<W: Write + Send> crate::event::EventSink for FrameSink<W> {
    fn emit(&self, event: Event) {
        let _ = self.send(&Up::Event(event));
    }
}

impl<W: Write + Send> UpLink for FrameSink<W> {
    fn send(&self, up: &Up) -> io::Result<()> {
        let mut w = self.0.lock().unwrap();
        write_frame(&mut *w, up)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_with_binary_chunks() {
        let bytes: Vec<u8> = (0..=255u8).cycle().take(3000).collect();
        let down = Down::FileChunk {
            req: 7,
            offset: 1024,
            bytes: bytes.clone(),
            last: true,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &down).unwrap();
        // The body is JSON with a base64 string, not a number array.
        let text = String::from_utf8_lossy(&buf[4..]).into_owned();
        assert!(text.contains("\"bytes\":\""), "{text}");
        let back: Down = read_frame(&mut buf.as_slice()).unwrap().unwrap();
        match back {
            Down::FileChunk {
                req,
                offset,
                bytes: b,
                last,
            } => {
                assert_eq!((req, offset, last), (7, 1024, true));
                assert_eq!(b, bytes);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            read_frame::<_, Down>(&mut &buf[buf.len()..])
                .unwrap()
                .is_none()
        );

        // The block events and the step events that carry the block path
        // come back through a frame exactly as they went in.
        let path = vec!["outer".to_string(), "inner".to_string()];
        let events = [
            Event::BlockStarted {
                blocks: path.clone(),
            },
            Event::StepStarted {
                id: 1,
                blocks: path.clone(),
                name: "s".into(),
                identity: "self".into(),
            },
            Event::StepFinished {
                id: 1,
                blocks: path.clone(),
                name: "s".into(),
                identity: "self".into(),
                status: crate::event::Status::WouldChange,
                diff: None,
                note: None,
                elapsed_ms: 2,
            },
            Event::StepSkipped {
                id: 2,
                blocks: vec![],
                name: "k".into(),
                reason: "r".into(),
            },
            Event::BlockFinished {
                blocks: path.clone(),
            },
        ];
        let mut buf = Vec::new();
        for e in &events {
            write_frame(&mut buf, &Up::Event(e.clone())).unwrap();
        }
        let mut rd = buf.as_slice();
        for e in &events {
            let Up::Event(back) = read_frame::<_, Up>(&mut rd).unwrap().unwrap() else {
                panic!("not an event frame")
            };
            assert_eq!(
                serde_json::to_value(&back).unwrap(),
                serde_json::to_value(e).unwrap()
            );
        }
    }

    #[test]
    fn start_frame_password_is_optional_and_secret() {
        let json = r#"{"Start":{"run_id":"r","host":{"name":"h","groups":[]},"vars":{},"check_mode":false,"verbosity":0}}"#;
        let d: Down = serde_json::from_str(json).unwrap();
        let Down::Start {
            escalate_password, ..
        } = d
        else {
            panic!()
        };
        assert!(escalate_password.is_none());

        let json = r#"{"Start":{"run_id":"r","host":{"name":"h","groups":[]},"vars":{},"check_mode":false,"verbosity":0,"escalate_password":"hunter2"}}"#;
        let d: Down = serde_json::from_str(json).unwrap();
        let Down::Start {
            escalate_password, ..
        } = &d
        else {
            panic!()
        };
        assert_eq!(escalate_password.as_ref().unwrap().as_bytes(), b"hunter2");
        assert!(!format!("{d:?}").contains("hunter2"));
    }

    /// A `Diff` rides in every `StepFinished`, so its JSON is part of the
    /// format. Making the type opaque (a `#[serde(transparent)]` struct over
    /// a private enum) must not move a byte: each shape's encoding here is
    /// exactly what the public enum produced before it, captured from that
    /// enum, so an orchestrator and a target built either side of the change
    /// still agree and `PROTOCOL_VERSION` did not move.
    #[test]
    fn diff_json_is_byte_identical_to_the_public_enum_encoding() {
        use crate::diff::{AttrChange, Diff};

        let mode = || Diff::attrs("/etc/x", vec![AttrChange::new("mode", "0644", "0600")]);
        let cases = [
            (
                Diff::text("/etc/hosts", "a\n", "b\n"),
                r#"{"Text":{"path":"/etc/hosts","before":"a\n","after":"b\n"}}"#,
            ),
            (
                mode(),
                r#"{"Attrs":{"subject":"/etc/x","changes":[{"name":"mode","from":"0644","to":"0600"}]}}"#,
            ),
            (
                Diff::summary("restarted nginx"),
                r#"{"Summary":"restarted nginx"}"#,
            ),
            (
                Diff::many([mode(), Diff::text("/k", "", "key\n")]).unwrap(),
                r#"{"Many":[{"Attrs":{"subject":"/etc/x","changes":[{"name":"mode","from":"0644","to":"0600"}]}},{"Text":{"path":"/k","before":"","after":"key\n"}}]}"#,
            ),
        ];
        for (diff, json) in cases {
            assert_eq!(serde_json::to_string(&diff).unwrap(), json);
            // And an older peer's frame reads back into the same diff.
            let back: Diff = serde_json::from_str(json).unwrap();
            assert_eq!(back.render(), diff.render());
            assert_eq!(serde_json::to_string(&back).unwrap(), json);
        }
    }

    /// Version 5 is `Ctx::block`: the block events replace the section
    /// events, and every step event carries the block path instead of a
    /// depth. The number itself is pinned by the newest version's test.
    #[test]
    fn protocol_5_carries_block_paths() {
        let started = Event::BlockStarted {
            blocks: vec!["a".into(), "b".into()],
        };
        assert_eq!(
            serde_json::to_string(&started).unwrap(),
            r#"{"BlockStarted":{"blocks":["a","b"]}}"#
        );
        let skipped = Event::StepSkipped {
            id: 3,
            blocks: vec!["a".into()],
            name: "n".into(),
            reason: "r".into(),
        };
        assert_eq!(
            serde_json::to_string(&skipped).unwrap(),
            r#"{"StepSkipped":{"id":3,"blocks":["a"],"name":"n","reason":"r"}}"#
        );
        // And the same JSON reads back into the same shapes.
        let Event::BlockFinished { blocks } =
            serde_json::from_str(r#"{"BlockFinished":{"blocks":["a","b"]}}"#).unwrap()
        else {
            panic!("not BlockFinished")
        };
        assert_eq!(blocks, ["a", "b"]);
        let Event::BlockStarted { blocks } =
            serde_json::from_str(r#"{"BlockStarted":{"blocks":["a"]}}"#).unwrap()
        else {
            panic!("not BlockStarted")
        };
        assert_eq!(blocks, ["a"]);
        let Event::StepStarted { blocks, .. } = serde_json::from_str(
            r#"{"StepStarted":{"id":1,"blocks":["a"],"name":"n","identity":"self"}}"#,
        )
        .unwrap() else {
            panic!("not StepStarted")
        };
        assert_eq!(blocks, ["a"]);
        let Event::StepSkipped { blocks, .. } = serde_json::from_str(
            r#"{"StepSkipped":{"id":3,"blocks":["a"],"name":"n","reason":"r"}}"#,
        )
        .unwrap() else {
            panic!("not StepSkipped")
        };
        assert_eq!(blocks, ["a"]);
        // A version-4 frame, with `depth` and no `blocks`, does not read.
        let old = r#"{"StepSkipped":{"id":3,"depth":1,"name":"n","reason":"r"}}"#;
        assert!(serde_json::from_str::<Event>(old).is_err());
    }

    /// Version 6 is the playbook's `ssh_user`: `Start.host` says, when the
    /// attribute chose the login, what the inventory would have used, so the
    /// binary's escalation failures can say so. An orchestrator that sets
    /// nothing sends the same bytes as before, and a `Start` without the
    /// field reads as "the inventory chose". The number itself is pinned by
    /// the newest version's test.
    #[test]
    fn protocol_6_carries_the_login_override() {
        use crate::ctx::{InventoryLogin, LoginOverride};

        let mut host = HostInfo::local();
        let plain = serde_json::to_value(&host).unwrap();
        assert!(plain.get("login_override").is_none(), "{plain}");

        host.login_override = Some(Box::new(LoginOverride {
            ssh_user: "minecraft".into(),
            inventory: Some(InventoryLogin {
                ssh_user: "cadu".into(),
                source: "defaults".into(),
            }),
        }));
        let json = serde_json::to_value(&host).unwrap();
        assert_eq!(
            json["login_override"],
            serde_json::json!({
                "ssh_user": "minecraft",
                "inventory": { "ssh_user": "cadu", "source": "defaults" },
            })
        );
        let back: HostInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back.login_override, host.login_override);

        let json = r#"{"Start":{"run_id":"r","host":{"name":"h","groups":[]},"vars":{},"check_mode":false,"verbosity":0}}"#;
        let Down::Start { host, .. } = serde_json::from_str(json).unwrap() else {
            panic!("not Start")
        };
        assert!(host.login_override.is_none());
    }

    /// Version 7 is the host's verdict as the playbook returns it (#44):
    /// `Summary` gains `recovered` beside a `failed` that no longer counts
    /// every failed step, and `Failed` names the failed step's id and block
    /// path.
    /// The meaning of `failed` moved with it, so a version-6 peer would
    /// disagree about a host without failing to parse anything: the number
    /// is what stops a mixed pair. Pinned here so the shape and the number
    /// move together.
    #[test]
    fn protocol_7_carries_recovered_and_the_failed_steps_blocks() {
        assert_eq!(PROTOCOL_VERSION, 7);
        let summary = crate::event::Summary {
            ok: 4,
            changed: 1,
            failed: 0,
            recovered: 2,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&Event::Finished(summary)).unwrap(),
            r#"{"Finished":{"ok":4,"changed":1,"would_change":0,"skipped":0,"failed":0,"recovered":2,"warnings":0}}"#
        );
        let failed = Event::Failed {
            step: Some("boom".into()),
            id: Some(7),
            blocks: vec!["outer".into(), "inner".into()],
            error: "step `boom`: nope".into(),
            cmd: None,
        };
        let json = r#"{"Failed":{"step":"boom","id":7,"blocks":["outer","inner"],"error":"step `boom`: nope","cmd":null}}"#;
        assert_eq!(serde_json::to_string(&failed).unwrap(), json);
        let Event::Failed { id, blocks, .. } = serde_json::from_str(json).unwrap() else {
            panic!("not Failed")
        };
        assert_eq!(
            id,
            Some(7),
            "the id is what pairs the frame with its step line"
        );
        assert_eq!(blocks, ["outer", "inner"]);
        // A version-6 summary, with no `recovered`, does not read, and nor
        // does a version-6 `Failed`, with no `id` or `blocks`.
        let old = r#"{"Finished":{"ok":1,"changed":0,"would_change":0,"skipped":0,"failed":0,"warnings":0}}"#;
        assert!(serde_json::from_str::<Event>(old).is_err());
        let old = r#"{"Failed":{"step":"boom","error":"step `boom`: nope","cmd":null}}"#;
        assert!(serde_json::from_str::<Event>(old).is_err());
    }
}
