//! Events the binary emits toward the orchestrator. This is the `Up` side of
//! the protocol, minus transport concerns. For the spike they are written as
//! JSON lines or rendered directly.

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

/// The sink a local run writes to: JSON lines or the pretty renderer.
pub fn stdout_sink(json: bool, host: &str, verbosity: u8) -> SharedSink {
    if json {
        Arc::new(JsonLines(Mutex::new(std::io::stdout())))
    } else {
        Arc::new(Pretty::new(std::io::stdout(), host, verbosity))
    }
}

/// Renders the Ansible-style step list straight to a writer. This is what the
/// orchestrator will do from the event stream; for the spike the binary does it.
pub struct Pretty<W: Write + Send> {
    w: Mutex<W>,
    host: String,
    verbosity: u8,
}

impl<W: Write + Send> Pretty<W> {
    pub fn new(w: W, host: impl Into<String>, verbosity: u8) -> Self {
        Pretty {
            w: Mutex::new(w),
            host: host.into(),
            verbosity,
        }
    }
}

impl<W: Write + Send> EventSink for Pretty<W> {
    fn emit(&self, event: Event) {
        let mut w = self.w.lock().unwrap();
        let host = &self.host;
        let indent = |d: u8| "  ".repeat(d as usize);
        let _ = match event {
            Event::Facts(f) => writeln!(
                w,
                "[{host}]  facts: {:?} {} {:?} {:?} cpus={} mem={}MB user={}",
                f.distro, f.distro_version, f.arch, f.package_manager, f.cpus, f.memory_mb, f.user
            ),
            Event::SectionStarted { depth, name } => {
                writeln!(w, "[{host}]  {}{name}", indent(depth))
            }
            Event::SectionFinished { .. } => Ok(()),
            Event::StepStarted { .. } => Ok(()),
            Event::StepFinished {
                depth,
                name,
                identity,
                status,
                diff,
                note,
                ..
            } => {
                let label = format!("{}{name} ", indent(depth));
                let status_s = match status {
                    Status::Ok => "ok",
                    Status::Changed => "changed",
                    Status::WouldChange => "would change",
                    Status::Skipped => "skipped",
                    Status::Failed => "FAILED",
                };
                let mut tail = String::new();
                if let Some(d) = &diff {
                    tail.push_str(&format!("   {}", d.short()));
                }
                if let Some(n) = note {
                    tail.push_str(&format!("   {n}"));
                }
                if identity != "self" {
                    tail.push_str(&format!("   as {identity}"));
                }
                let r = writeln!(w, "[{host}]  {label:.<44} {status_s:<13}{tail}");
                if self.verbosity >= 1
                    && let Some(d) = diff
                    && matches!(status, Status::Changed | Status::WouldChange)
                {
                    for line in d.render().lines() {
                        let _ = writeln!(w, "{}      | {line}", indent(depth));
                    }
                }
                r
            }
            Event::StepSkipped {
                depth,
                name,
                reason,
                ..
            } => {
                let label = format!("{}{name} ", indent(depth));
                writeln!(w, "[{host}]  {label:.<44} {:<13}   {reason}", "skipped")
            }
            Event::Log { level, msg } => match level {
                Level::Debug if self.verbosity < 1 => Ok(()),
                Level::Debug => writeln!(w, "[{host}]    debug: {msg}"),
                Level::Info => writeln!(w, "[{host}]    {msg}"),
                Level::Warn => writeln!(w, "[{host}]    WARNING: {msg}"),
            },
            Event::CmdRan {
                identity,
                argv,
                status,
                elapsed_ms,
            } => {
                if self.verbosity >= 2 {
                    writeln!(
                        w,
                        "[{host}]    $ {} (as {identity}, exit {status}, {elapsed_ms}ms)",
                        argv.join(" ")
                    )
                } else {
                    Ok(())
                }
            }
            Event::Failed { step, error, cmd } => {
                let r = match step {
                    Some(s) => writeln!(w, "[{host}]  FAILED at `{s}`: {error}"),
                    None => writeln!(w, "[{host}]  FAILED: {error}"),
                };
                if self.verbosity >= 1
                    && let Some(c) = cmd
                {
                    let _ = writeln!(w, "[{host}]    $ {} (exit {})", c.argv.join(" "), c.status);
                    for line in c.stderr.lines() {
                        let _ = writeln!(w, "[{host}]      {line}");
                    }
                }
                r
            }
            Event::Finished(s) => writeln!(
                w,
                "\n{host:<8} ok={} changed={} would_change={} skipped={} failed={} warnings={}",
                s.ok, s.changed, s.would_change, s.skipped, s.failed, s.warnings
            ),
        };
    }
}
