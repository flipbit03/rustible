//! The wire protocol between orchestrator and playbook binary. Frames are a
//! u32 big-endian length followed by a JSON payload. JSON for now because it
//! is debuggable; the codec is the only place that would change.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ctx::HostInfo;
use crate::event::Event;

pub const PROTOCOL_VERSION: u32 = 1;

/// Orchestrator -> binary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Down {
    Start {
        run_id: String,
        /// Registry name of the playbook to run (`cadu/mc`). A shipped binary
        /// holds one playbook, but an IDE-style build holds them all.
        playbook: String,
        host: HostInfo,
        /// Merged inventory vars for this host. The macro will deserialize
        /// this into the playbook's typed struct.
        vars: serde_json::Value,
        check_mode: bool,
        verbosity: u8,
    },
    Cancel,
}

/// Binary -> orchestrator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)] // wire type; size is irrelevant
pub enum Up {
    Hello { protocol: u32, playbook: String },
    Event(Event),
}

const MAX_FRAME: usize = 64 * 1024 * 1024;

pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
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
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(io::Error::other)
}

/// An `EventSink` that writes `Up::Event` frames to a writer.
pub struct FrameSink<W: Write + Send>(pub std::sync::Mutex<W>);

impl<W: Write + Send> crate::event::EventSink for FrameSink<W> {
    fn emit(&self, event: Event) {
        let mut w = self.0.lock().unwrap();
        let _ = write_frame(&mut *w, &Up::Event(event));
    }
}
