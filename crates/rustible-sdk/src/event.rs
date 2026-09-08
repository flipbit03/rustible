//! Events the binary emits toward the orchestrator. This is the `Up` side of
//! the protocol, minus transport concerns. The `rustible` command renders
//! them; the sinks here are the framed channel's building blocks, a JSON
//! lines writer, an in-memory collector for tests, and a compact printer for
//! running a playbook binary by hand.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::diff::Diff;
use crate::error::CmdFailed;
use crate::facts::Facts;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Ok,
    Changed,
    WouldChange,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Facts(Facts),
    SectionStarted {
        depth: u8,
        name: String,
    },
    SectionFinished {
        depth: u8,
        name: String,
    },
    StepStarted {
        id: u32,
        depth: u8,
        name: String,
        identity: String,
    },
    StepFinished {
        id: u32,
        depth: u8,
        name: String,
        identity: String,
        status: Status,
        diff: Option<Diff>,
        note: Option<String>,
        elapsed_ms: u64,
    },
    StepSkipped {
        id: u32,
        depth: u8,
        name: String,
        reason: String,
    },
    Log {
        level: Level,
        msg: String,
    },
    CmdRan {
        identity: String,
        argv: Vec<String>,
        status: i32,
        elapsed_ms: u64,
    },
    Failed {
        step: Option<String>,
        /// The rendered context chain, outermost first.
        error: String,
        /// Present when a command failure is in the chain (rendered at -v).
        /// Additive field: absent from older binaries' frames.
        #[serde(default)]
        cmd: Option<CmdFailed>,
    },
    Finished(Summary),
}

impl Event {
    /// Build a `Failed` event from an error, extracting the command if any.
    pub fn failed(step: Option<String>, e: &crate::Error) -> Event {
        Event::Failed {
            step,
            error: e.chain(),
            cmd: e.cmd_failed().cloned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    Debug,
    Info,
    Warn,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    pub ok: u32,
    pub changed: u32,
    pub would_change: u32,
    pub skipped: u32,
    pub failed: u32,
    pub warnings: u32,
}

/// Where events go. The runtime owns one; `System` and `Ctx` hold clones.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: Event);
}

pub type SharedSink = Arc<dyn EventSink>;

/// JSON lines to any writer. What the real channel will look like, minus framing.
pub struct JsonLines<W: Write + Send>(pub Mutex<W>);

impl<W: Write + Send> EventSink for JsonLines<W> {
    fn emit(&self, event: Event) {
        let mut w = self.0.lock().unwrap();
        let _ = serde_json::to_writer(&mut *w, &event);
        let _ = w.write_all(b"\n");
        let _ = w.flush();
    }
}

/// Collects events in memory. For tests.
#[derive(Default)]
pub struct Collect(pub Mutex<Vec<Event>>);

impl EventSink for Collect {
    fn emit(&self, event: Event) {
        self.0.lock().unwrap().push(event);
    }
}

impl Collect {
    pub fn events(&self) -> Vec<Event> {
        self.0.lock().unwrap().clone()
    }
}

/// One line per event, unbuffered, for a playbook binary run by hand
/// (`<binary> <name>`). The `rustible` command renders the real view from
/// the frame stream; this only has to be readable.
pub struct Compact<W: Write + Send> {
    w: Mutex<W>,
    verbosity: u8,
}

impl<W: Write + Send> Compact<W> {
    pub fn new(w: W, verbosity: u8) -> Self {
        Compact {
            w: Mutex::new(w),
            verbosity,
        }
    }
}

impl<W: Write + Send> EventSink for Compact<W> {
    fn emit(&self, event: Event) {
        let mut w = self.w.lock().unwrap();
        let _ = match event {
            Event::Facts(f) => writeln!(
                w,
                "facts: {:?} {} {:?} {:?} cpus={} mem={}MB user={}",
                f.distro, f.distro_version, f.arch, f.package_manager, f.cpus, f.memory_mb, f.user
            ),
            Event::SectionStarted { name, .. } => writeln!(w, "section: {name}"),
            Event::SectionFinished { .. } | Event::StepStarted { .. } => Ok(()),
            Event::StepFinished {
                name,
                identity,
                status,
                diff,
                note,
                ..
            } => {
                let status_s = match status {
                    Status::Ok => "ok",
                    Status::Changed => "changed",
                    Status::WouldChange => "would change",
                    Status::Skipped => "skipped",
                    Status::Failed => "FAILED",
                };
                let mut tail = String::new();
                if let Some(d) = &diff {
                    tail.push_str(&format!("  {}", d.short()));
                }
                if let Some(n) = note {
                    tail.push_str(&format!("  {n}"));
                }
                if identity != "self" {
                    tail.push_str(&format!("  as {identity}"));
                }
                let r = writeln!(w, "{status_s}: {name}{tail}");
                if self.verbosity >= 1
                    && let Some(d) = diff
                    && matches!(status, Status::Changed | Status::WouldChange)
                {
                    for line in d.render().lines() {
                        let _ = writeln!(w, "    | {line}");
                    }
                }
                r
            }
            Event::StepSkipped { name, reason, .. } => writeln!(w, "skipped: {name}  {reason}"),
            Event::Log { level, msg } => match level {
                Level::Debug if self.verbosity < 1 => Ok(()),
                Level::Debug => writeln!(w, "debug: {msg}"),
                Level::Info => writeln!(w, "{msg}"),
                Level::Warn => writeln!(w, "WARNING: {msg}"),
            },
            Event::CmdRan {
                identity,
                argv,
                status,
                elapsed_ms,
            } if self.verbosity >= 2 => writeln!(
                w,
                "$ {} (as {identity}, exit {status}, {elapsed_ms}ms)",
                argv.join(" ")
            ),
            Event::CmdRan { .. } => Ok(()),
            Event::Failed { step, error, cmd } => {
                let r = match step {
                    Some(s) => writeln!(w, "FAILED at `{s}`: {error}"),
                    None => writeln!(w, "FAILED: {error}"),
                };
                if self.verbosity >= 1
                    && let Some(c) = cmd
                {
                    let _ = writeln!(w, "  $ {} (exit {})", c.argv.join(" "), c.status);
                    for line in c.stderr.lines() {
                        let _ = writeln!(w, "    {line}");
                    }
                }
                r
            }
            Event::Finished(s) => writeln!(
                w,
                "ok={} changed={} would_change={} skipped={} failed={} warnings={}",
                s.ok, s.changed, s.would_change, s.skipped, s.failed, s.warnings
            ),
        };
    }
}
