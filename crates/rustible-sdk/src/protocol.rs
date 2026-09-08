//! The wire protocol between orchestrator and playbook binary. Frames are a
//! u32 big-endian length followed by a JSON payload. JSON for now because it
//! is debuggable; the codec is the only place that would change.
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
pub const PROTOCOL_VERSION: u32 = 3;

/// Bytes per streamed chunk (vision doc 5.6).
pub const CHUNK_SIZE: usize = 1024 * 1024;

/// Orchestrator -> binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Down {
    Start {
        run_id: String,
        /// Registry name of the playbook to run (`cadu/mc`). A shipped binary
        /// holds one playbook, but an IDE-style build holds them all. Empty
        /// (older orchestrators) means "the only playbook in this binary".
        #[serde(default)]
        playbook: String,
        host: HostInfo,
        /// Merged inventory vars for this host. The macro will deserialize
        /// this into the playbook's typed struct.
        vars: serde_json::Value,
        check_mode: bool,
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
        req: u32,
        offset: u64,
        #[serde(with = "b64")]
        bytes: Vec<u8>,
        last: bool,
    },
    /// The orchestrator refused a `FileRequest` (outside the workspace, missing).
    FileDenied { req: u32, reason: String },
}

/// Binary -> orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)] // wire type; size is irrelevant
pub enum Up {
    Hello {
        protocol: u32,
        playbook: String,
    },
    Event(Event),
    /// "Send me `path`", relative to the workspace root.
    FileRequest {
        req: u32,
        path: String,
    },
    /// A piece of a file the binary is sending back (`ctx.fetch`), to be
    /// written at `dest` relative to the workspace root.
    FetchChunk {
        req: u32,
        dest: String,
        offset: u64,
        #[serde(with = "b64")]
        bytes: Vec<u8>,
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

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = Zeroizing::new(serde_json::to_vec(msg).map_err(io::Error::other)?);
    let len = u32::try_from(body.len()).map_err(|_| io::Error::other("frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// `Ok(None)` on clean EOF before a frame starts.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<T>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other(format!(
            "frame of {len} bytes exceeds limit"
        )));
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

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(bytes))
    }

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

    pub fn serialize<S: Serializer>(bytes: &Option<Vec<u8>>, s: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(b) => super::b64::serialize(b, s),
            None => s.serialize_none(),
        }
    }

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
}
