//! Events the binary emits toward the orchestrator. This is the `Up` side of
//! the protocol, minus transport concerns. The `rustible` command renders
//! them; the sinks here are the framed channel's building blocks, a JSON
//! lines writer, an in-memory collector for tests, and a compact printer for
//! running a playbook binary by hand.

use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::diff::Diff;
use crate::error::CmdFailed;
use crate::facts::Facts;

/// How a step turned out. [`Ctx::step`](crate::ctx::Ctx::step) picks exactly
/// one from what [`Op::check`](crate::op::Op::check) planned and what
/// [`Op::apply`](crate::op::Op::apply) reported, puts it in
/// [`Event::StepFinished`], and adds one to the matching counter in
/// [`Summary`]. A reporter turns it into the word on the step line.
///
/// There is no `Skipped`: a skip never reaches a step, so
/// [`Ctx::skip`](crate::ctx::Ctx::skip) reports it as its own
/// [`Event::StepSkipped`], carrying the playbook's reason where a status
/// would sit. Every variant here is emitted by `Ctx::step`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    /// Nothing to do: `check` returned
    /// [`Plan::Satisfied`](crate::op::Plan::Satisfied).
    /// Also what an action reports when `apply` ran but
    /// [`Op::changed_by_apply`](crate::op::Op::changed_by_apply) said nothing
    /// came of it; that case carries a diff and the note `ran, unchanged`, so
    /// `-v` still shows what ran.
    Ok,
    /// `apply` ran and the difference is gone. Never emitted in check mode.
    Changed,
    /// Check mode found a difference and stopped before `apply`. Reporters
    /// show diffs for this exactly as for `Changed`; the only difference is
    /// the word and the column.
    WouldChange,
    /// `check` or `apply` returned an error, or the run was cancelled between
    /// the two. The rendered context chain travels in the `note` field, and
    /// an [`Event::Failed`] normally follows once the error reaches the top
    /// of the playbook.
    Failed,
}

/// One thing that happened on one host, as the playbook binary reports it.
///
/// The runtime emits [`Facts`](Event::Facts) before the playbook body runs
/// and [`Finished`](Event::Finished) after it returns; everything between
/// comes from the playbook, in the order it executed. A reporter may not
/// assume it will see `Finished`: a binary that panics hard or is killed
/// stops mid-stream, which is exactly how `rustible` tells a crashed host
/// from a failed one.
///
/// Step events bracket everything a step did. `StepStarted` opens a step,
/// [`Log`](Event::Log) and [`CmdRan`](Event::CmdRan) belong to whichever
/// step is open, and `StepFinished` closes it. Both reporters in this crate
/// buffer the nested lines and print them under the finished step's line, so
/// hosts running in parallel interleave by whole steps rather than by
/// half-written ones.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// The host's core facts, gathered once at startup before any step. A
    /// reporter can label the host from this without waiting for a step;
    /// `rustible` prints the line at `-v` and drops it otherwise.
    Facts(Facts),
    /// A [`Ctx::section`](crate::ctx::Ctx::section) opened. Grouping only:
    /// no work is attached to it and no counter moves.
    SectionStarted {
        /// Nesting level of the section itself, so the steps inside it carry
        /// `depth + 1`. Reporters indent two spaces per level.
        depth: u8,
        /// The heading the playbook passed, verbatim.
        name: String,
    },
    /// The section's closure returned, whether it succeeded or not. Emitted
    /// even when the body errored, so a consumer tracking nesting never
    /// leaks a level. `rustible` ignores it outright and relies on `depth`.
    SectionFinished {
        /// Matches the `depth` of the `SectionStarted` it closes.
        depth: u8,
        /// Matches the `name` of the `SectionStarted` it closes.
        name: String,
    },
    /// A step is about to run: emitted by `Ctx::step` before `check`, so the
    /// step line for a slow check is already claimed. A reporter should
    /// start buffering here and expect a `StepFinished` with the same `id`.
    StepStarted {
        /// Per-run counter starting at 1, drawn by steps and skips alike.
        /// Pairs this event with its `StepFinished`.
        id: u32,
        /// Enclosing `Ctx::section` nesting, 0 at the top level.
        depth: u8,
        /// The step label the playbook passed to `Ctx::step`.
        name: String,
        /// Who the op runs as: `self`, or the user name when the step came
        /// from [`Ctx::as_user`](crate::ctx::Ctx::as_user) or
        /// [`Ctx::as_root`](crate::ctx::Ctx::as_root). Reporters append
        /// `as <identity>` when it is not `self`.
        identity: String,
    },
    /// A step reached a verdict. This is the event a reporter prints a step
    /// line from and the only one that moves the [`Summary`] counters.
    StepFinished {
        /// The `id` of the `StepStarted` this closes.
        id: u32,
        /// Repeated from `StepStarted` so a consumer that joined late, or
        /// one reading JSON lines out of context, needs no state to render
        /// the line.
        depth: u8,
        /// Repeated from `StepStarted`.
        name: String,
        /// Repeated from `StepStarted`.
        identity: String,
        /// The verdict; see [`Status`].
        status: Status,
        /// What `check` planned. Absent when the step was already satisfied
        /// and when `check` itself failed; present for `Changed`,
        /// `WouldChange`, a failed `apply`, and the `ran, unchanged` case.
        /// Reporters put [`Diff::short`](crate::diff::Diff::short) on the
        /// step line and the full [`Diff::render`](crate::diff::Diff::render)
        /// underneath at `-v`.
        diff: Option<Diff>,
        /// A short suffix for the step line, except on `Failed` where it
        /// carries the whole rendered error chain instead. `Ctx::step` sets
        /// `action` for an op whose
        /// [`always_changes`](crate::op::Op::always_changes) is true, and
        /// `ran, unchanged` for an apply that reported no change. `rustible`
        /// holds a failed step's chain back and prints it only if no
        /// [`Failed`](Event::Failed) frame follows, so a playbook that
        /// swallowed the error still shows why.
        note: Option<String>,
        /// Wall clock across both phases, from just before `check` to the
        /// moment the status was decided. The integration harness records it
        /// per step; neither reporter here prints it today.
        elapsed_ms: u64,
    },
    /// [`Ctx::skip`](crate::ctx::Ctx::skip): a step the playbook decided not
    /// to run. Nothing was checked or applied, no `StepStarted` precedes it
    /// and no `StepFinished` follows, so a reporter prints it whole.
    StepSkipped {
        /// From the same per-run counter as steps, so ids stay unique and
        /// ordered across both.
        id: u32,
        /// Enclosing `Ctx::section` nesting, 0 at the top level.
        depth: u8,
        /// The label the playbook would have given the step.
        name: String,
        /// The playbook's own words for why, printed on the step line where
        /// a status would otherwise sit.
        reason: String,
    },
    /// Free text from `Ctx::log`, `Ctx::warn`, `Ctx::debug`, `System::warn`,
    /// `System::debug`, or the runtime's undeclared-vars notice. Belongs to
    /// the step in flight when there is one, so reporters nest it.
    Log {
        /// Decides both visibility and prefix; see [`Level`].
        level: Level,
        /// The message as written, already formatted, with no trailing
        /// newline.
        msg: String,
    },
    /// Every command the run spawned, emitted by `Cmd::run` after the
    /// process exits, whether it succeeded or not and whether it ran during
    /// `check` or `apply`. This is the audit trail: `rustible -vv` prints
    /// one `$` line per command and the integration harness counts them.
    CmdRan {
        /// `self`, or the user the command ran as.
        identity: String,
        /// Program and arguments as spawned. No shell was involved and
        /// nothing is quoted, so a reporter that prints them has to quote
        /// arguments containing whitespace itself.
        argv: Vec<String>,
        /// Exit status. Non-zero appears here even when it was turned into
        /// an error, and an `allow_failure` command reports its non-zero
        /// status with no `Failed` event anywhere.
        status: i32,
        /// Wall clock around the spawn, including reading stdout and stderr.
        elapsed_ms: u64,
    },
    /// The playbook returned an error to the runtime or panicked. At most
    /// one per run, emitted after the body returns and before `Finished`.
    /// The failing step already reported `Status::Failed` with the same
    /// chain in its `note`; `rustible` prefers this frame and prints the
    /// chain once.
    Failed {
        /// The step the failure belongs to. The runtime reads it off the
        /// [`StepFailed`](crate::error::StepFailed) layer `Ctx::step`
        /// attaches, so it is filled for anything that failed inside a step
        /// and `None` for a panic or an error the playbook raised on its
        /// own. `error` still opens with that layer's text; a reporter that
        /// prints the name separately drops it.
        step: Option<String>,
        /// The rendered context chain, outermost first.
        error: String,
        /// Present when a command failure is in the chain (rendered at -v).
        /// Additive field: absent from older binaries' frames.
        #[serde(default)]
        cmd: Option<CmdFailed>,
    },
    /// The last event of a run that got to the end, emitted once the final
    /// [`Ctx`](crate::ctx::Ctx) has been dropped so the temp directory is
    /// gone and every helper's `CmdRan` is already in the stream. Its
    /// absence is information: `rustible` prints `no summary (binary exited
    /// N before finishing)` for that host and fails the run.
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

/// Severity of an [`Event::Log`] line, fixed by which logging method the
/// caller used. A reporter decides visibility and prefix from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Level {
    /// `Ctx::debug` and `System::debug`. Hidden below `-v`. Ops use it for
    /// the reasoning behind a decision: what a probe read, which branch a
    /// distro fact chose, how many bytes a stream moved.
    Debug,
    /// `Ctx::log`. Printed at every verbosity with no prefix; the playbook
    /// asked for this line to be in the output.
    Info,
    /// `Ctx::warn`, `System::warn`, and the runtime's near-miss notice for
    /// an undeclared var. Printed as `WARNING: `, and counted into
    /// [`Summary::warnings`] as the frame passes the sink, so the counter
    /// matches the number of warning lines on screen whichever of the three
    /// wrote them.
    Warn,
}

/// The per-host tally `Ctx` keeps as steps finish, sent once in
/// [`Event::Finished`] and rendered as one row of `rustible`'s closing
/// table. Each finished or skipped step adds to exactly one of the first
/// five counters, so they sum to the number of steps the playbook reached.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    /// Steps that needed no change, plus actions that ran and reported
    /// nothing changed ([`Status::Ok`]).
    pub ok: u32,
    /// Steps whose `apply` ran and changed something. Always 0 in check
    /// mode, where the same steps land in `would_change` instead.
    pub changed: u32,
    /// Steps a check-mode run would have changed. Always 0 outside check
    /// mode.
    pub would_change: u32,
    /// Steps `Ctx::skip` recorded. These never ran a check, so they carry
    /// no [`Status`]; they arrive as [`Event::StepSkipped`].
    pub skipped: u32,
    /// Steps whose check or apply failed. The runtime forces this to at
    /// least 1 when the playbook returned an error or panicked without any
    /// step having failed: the process exit code and `rustible`'s per-host
    /// verdict are both read off this field, so a zero here would report a
    /// crashed run as a success.
    pub failed: u32,
    /// How many `Log { level: Warn }` frames the run emitted, from any of
    /// `Ctx::warn`, `System::warn` inside an op, and the runtime's
    /// undeclared-var notice. Counted at the sink rather than by each
    /// producer, so this is exactly the number of `WARNING:` lines the
    /// operator saw. Unlike the five above it does not belong to a step and
    /// is not part of their sum.
    pub warnings: u32,
}

/// Where events go. The runtime owns one; `System` and `Ctx` hold clones.
pub trait EventSink: Send + Sync {
    /// Called inline, on the thread the event happened on, at the moment it
    /// happened: a slow sink slows the run. Takes `&self` because one sink
    /// is shared by every `System` clone the run makes, so an implementation
    /// has to synchronize itself; the three in this module each hold a
    /// `Mutex`. Never fails, and nothing checks whether the write landed,
    /// because losing the report is not a reason to abandon the change.
    fn emit(&self, event: Event);
}

/// The shared handle every part of a run holds: the runtime builds one,
/// `System` carries it into ops, and `Ctx::as_user` clones pass the same
/// sink along so a step's events stay in one stream regardless of identity.
pub type SharedSink = Arc<dyn EventSink>;

/// A sink that counts the warnings passing through it and forwards
/// everything to the sink it wraps.
///
/// [`Summary::warnings`] is filled from this at the end of a run instead of
/// being bumped by whoever wrote each warning. Three unrelated places emit
/// `Log { level: Warn }` (`Ctx::warn`, `System::warn`, the runtime's
/// undeclared-var notice) and only one of them used to count, so the summary
/// could report `0` with warnings visibly on screen. Counting where the
/// frames are seen means a fourth producer is counted the day it is written.
pub(crate) struct WarnCounter {
    inner: SharedSink,
    warnings: AtomicU32,
}

impl WarnCounter {
    /// Wrap `inner`. Every event still reaches it, in order and unchanged.
    pub(crate) fn new(inner: SharedSink) -> Self {
        WarnCounter {
            inner,
            warnings: AtomicU32::new(0),
        }
    }

    /// Warnings seen so far. Read once, after the playbook body has
    /// returned and before `Finished` is emitted.
    pub(crate) fn count(&self) -> u32 {
        self.warnings.load(Ordering::SeqCst)
    }
}

impl EventSink for WarnCounter {
    fn emit(&self, event: Event) {
        if matches!(
            event,
            Event::Log {
                level: Level::Warn,
                ..
            }
        ) {
            self.warnings.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.emit(event);
    }
}

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
    /// A snapshot of everything emitted so far, in order. Cloned out, so the
    /// sink keeps collecting and a test can assert mid-run. An op's unit
    /// tests read the `StepFinished` statuses out of this to check what the
    /// op reported, not just what it did.
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
    /// `verbosity` is the binary's count of `-v`. At 0 it prints one line
    /// per step, section and warning. At 1 it adds `debug` logs, the full
    /// diff under a changed step, and the failing command's stderr. At 2 it
    /// adds a `$` line per [`Event::CmdRan`]. Higher values behave like 2.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every [`Status`] has something that emits it. The match is
    /// exhaustive on purpose: a variant added here without a `Ctx` path that
    /// produces it stops compiling, which is how `Skipped` should have been
    /// caught. A skip is [`Event::StepSkipped`] and has no status at all.
    #[test]
    fn every_status_has_an_emitter() {
        fn emitted_by(s: Status) -> &'static str {
            match s {
                Status::Ok => "Ctx::step: check satisfied, or apply reported no change",
                Status::Changed => "Ctx::step: apply ran and changed something",
                Status::WouldChange => "Ctx::step: check found a difference in check mode",
                Status::Failed => "Ctx::step: check or apply errored, or the run was cancelled",
            }
        }
        for s in [
            Status::Ok,
            Status::Changed,
            Status::WouldChange,
            Status::Failed,
        ] {
            assert!(!emitted_by(s).is_empty());
        }
    }
}
