//! The per-host, per-step view of a run (vision doc 5.2 step 9 and 14).
//!
//! Every host's frames arrive on their own task; the renderer serializes
//! them into one stream where a step is printed as a unit: its line, then
//! everything that happened inside it (`CmdRan` at `-vv`, logs). Hosts
//! therefore interleave by whole steps, never by half a step. What happens
//! outside a step (facts, sections, top-level logs) prints as it comes.
//! A summary table closes the run.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;

use rustible_sdk::event::{Event, Level, Status, Summary};

/// Column the status starts in on a step line.
const NAME_WIDTH: usize = 44;

#[derive(Default)]
struct HostState {
    /// Lines buffered under the step in flight, already indented.
    open: Option<Vec<String>>,
    summary: Option<Summary>,
    /// Something outside the playbook went wrong: connect, upload, protocol.
    error: Option<String>,
    exit: Option<i32>,
    /// A step reported `Failed` with its chain in `note`; the binary's
    /// `Failed` frame normally follows and prints it once. If it does not
    /// (the playbook swallowed the error), the chain prints from here.
    pending_fail: Option<(String, String)>,
}

pub struct Renderer<W: Write> {
    w: W,
    verbosity: u8,
    width: usize,
    hosts: BTreeMap<String, HostState>,
    order: Vec<String>,
}

impl<W: Write> Renderer<W> {
    /// `hosts` in the order the summary table lists them.
    pub fn new(w: W, hosts: &[String], verbosity: u8) -> Self {
        Renderer {
            w,
            verbosity,
            width: hosts.iter().map(String::len).max().unwrap_or(0),
            hosts: hosts
                .iter()
                .map(|h| (h.clone(), HostState::default()))
                .collect(),
            order: hosts.to_vec(),
        }
    }

    fn state(&mut self, host: &str) -> &mut HostState {
        if !self.hosts.contains_key(host) {
            self.order.push(host.to_string());
            self.width = self.width.max(host.len());
        }
        self.hosts.entry(host.to_string()).or_default()
    }

    fn line(&mut self, host: &str, text: &str) {
        let _ = writeln!(self.w, "[{host:<w$}]  {text}", w = self.width);
    }

    /// A line that belongs to the step in flight when there is one, else
    /// prints now.
    fn nested(&mut self, host: &str, text: String) {
        match &mut self.state(host).open {
            Some(buf) => buf.push(text),
            None => self.line(host, &text),
        }
    }

    fn flush(&mut self, host: &str) {
        if let Some(buf) = self.state(host).open.take() {
            for l in buf {
                self.line(host, &l);
            }
        }
        if let Some((step, chain)) = self.state(host).pending_fail.take() {
            self.line(host, &format!("FAILED at `{step}`: {chain}"));
        }
    }

    /// An orchestrator-side note (connected, uploaded); shown at `-v`.
    pub fn note(&mut self, host: &str, msg: &str) {
        if self.verbosity >= 1 {
            self.nested(host, format!("  {msg}"));
        }
    }

    /// The host is out of the run for a reason outside the playbook.
    pub fn failed(&mut self, host: &str, msg: &str) {
        self.flush(host);
        self.line(host, &format!("FAILED: {msg}"));
        self.state(host).error = Some(msg.to_string());
    }

    /// Raw stderr of the binary (panics, sudo complaints), once it exited.
    pub fn stderr(&mut self, host: &str, text: &str) {
        self.flush(host);
        for l in text.lines() {
            self.line(host, &format!("  stderr: {l}"));
        }
    }

    pub fn exited(&mut self, host: &str, code: i32) {
        self.flush(host);
        self.state(host).exit = Some(code);
    }

    pub fn event(&mut self, host: &str, ev: &Event) {
        let indent = |d: u8| "  ".repeat(d as usize);
        match ev {
            Event::Facts(f) => {
                if self.verbosity >= 1 {
                    let text = format!(
                        "facts: {:?} {} {:?} {:?} cpus={} mem={}MB user={}",
                        f.distro,
                        f.distro_version,
                        f.arch,
                        f.package_manager,
                        f.cpus,
                        f.memory_mb,
                        f.user
                    );
                    self.line(host, &text);
                }
            }
            Event::SectionStarted { depth, name } => {
                self.flush(host);
                self.line(host, &format!("{}{name}", indent(*depth)));
            }
            Event::SectionFinished { .. } => {}
            Event::StepStarted { .. } => {
                self.flush(host);
                self.state(host).open = Some(vec![]);
            }
            Event::StepFinished {
                depth,
                name,
                identity,
                status,
                diff,
                note,
                ..
            } => {
                let mut tail = String::new();
                if let Some(d) = diff {
                    let _ = write!(tail, "   {}", d.short());
                }
                let mut pending = None;
                match note {
                    Some(chain) if *status == Status::Failed => {
                        pending = Some((name.clone(), chain.clone()));
                    }
                    Some(n) => {
                        let _ = write!(tail, "   {n}");
                    }
                    None => {}
                }
                if identity != "self" {
                    let _ = write!(tail, "   as {identity}");
                }
                let text = step_line(*depth, name, status_word(*status), &tail);
                let buffered = self.state(host).open.take().unwrap_or_default();
                self.line(host, &text);
                if self.verbosity >= 1
                    && let Some(d) = diff
                    && matches!(status, Status::Changed | Status::WouldChange)
                {
                    for l in d.render().lines() {
                        self.line(host, &format!("{}    | {l}", indent(*depth)));
                    }
                }
                for l in buffered {
                    self.line(host, &l);
                }
                self.state(host).pending_fail = pending;
            }
            Event::StepSkipped {
                depth,
                name,
                reason,
                ..
            } => {
                self.flush(host);
                let text = step_line(*depth, name, "skipped", &format!("   {reason}"));
                self.line(host, &text);
            }
            Event::Log { level, msg } => match level {
                Level::Debug if self.verbosity < 1 => {}
                Level::Debug => self.nested(host, format!("  debug: {msg}")),
                Level::Info => self.nested(host, format!("  {msg}")),
                Level::Warn => self.nested(host, format!("  WARNING: {msg}")),
            },
            Event::CmdRan {
                identity,
                argv,
                status,
                elapsed_ms,
            } => {
                if self.verbosity >= 2 {
                    let text = format!(
                        "  $ {} (as {identity}, exit {status}, {elapsed_ms}ms)",
                        argv_text(argv)
                    );
                    self.nested(host, text);
                }
            }
            Event::Failed { step, error, cmd } => {
                let (step, error) = split_step(step.as_deref(), error);
                // The frame carries the chain; the step line's copy is not needed.
                if let Some(p) = &self.state(host).pending_fail
                    && step == Some(p.0.as_str())
                {
                    self.state(host).pending_fail = None;
                }
                self.flush(host);
                let text = match step {
                    Some(s) => format!("FAILED at `{s}`: {error}"),
                    None => format!("FAILED: {error}"),
                };
                self.line(host, &text);
                if self.verbosity >= 1
                    && let Some(c) = cmd
                {
                    self.line(
                        host,
                        &format!("  $ {} (exit {})", argv_text(&c.argv), c.status),
                    );
                    for l in c.stderr.lines() {
                        self.line(host, &format!("    {l}"));
                    }
                }
            }
            Event::Finished(s) => {
                self.flush(host);
                self.state(host).summary = Some(s.clone());
            }
        }
    }

    /// The summary table. Returns whether any host failed: a failed step,
    /// a non-zero exit, no summary at all, or an orchestrator-side error.
    pub fn finish(&mut self) -> bool {
        let mut any_failed = false;
        let mut out = String::new();
        let w = self.width.max(4);
        let _ = writeln!(
            out,
            "\n{:<w$}  {:>3}  {:>7}  {:>12}  {:>7}  {:>6}  {:>8}",
            "host", "ok", "changed", "would change", "skipped", "failed", "warnings"
        );
        for host in &self.order {
            let st = &self.hosts[host];
            let failed = st.error.is_some()
                || st.exit.is_some_and(|c| c != 0)
                || st.summary.as_ref().is_none_or(|s| s.failed > 0);
            any_failed |= failed;
            match (&st.summary, &st.error) {
                (_, Some(e)) => {
                    let _ = writeln!(out, "{host:<w$}  failed: {e}");
                }
                (Some(s), None) => {
                    let _ = writeln!(
                        out,
                        "{host:<w$}  {:>3}  {:>7}  {:>12}  {:>7}  {:>6}  {:>8}{}",
                        s.ok,
                        s.changed,
                        s.would_change,
                        s.skipped,
                        s.failed,
                        s.warnings,
                        match st.exit {
                            Some(c) if c != 0 && s.failed == 0 => format!("  exit {c}"),
                            _ => String::new(),
                        }
                    );
                }
                (None, None) => {
                    let _ = writeln!(
                        out,
                        "{host:<w$}  no summary (binary exited {} before finishing)",
                        st.exit.map(|c| c.to_string()).unwrap_or_else(|| "?".into())
                    );
                }
            }
        }
        let _ = self.w.write_all(out.as_bytes());
        let _ = self.w.flush();
        any_failed
    }
}

/// A command line on one line: arguments with whitespace or control
/// characters are shown quoted and escaped.
fn argv_text(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c.is_control()) {
                format!("{a:?}")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Changed => "changed",
        Status::WouldChange => "would change",
        Status::Failed => "FAILED",
    }
}

fn step_line(depth: u8, name: &str, status: &str, tail: &str) -> String {
    let label = format!("{}{name} ", "  ".repeat(depth as usize));
    if tail.is_empty() {
        format!("{label:.<NAME_WIDTH$} {status}")
    } else {
        format!("{label:.<NAME_WIDTH$} {status:<13}{tail}")
    }
}

/// `Ctx::step` wraps a failure as `` step `name`: cause ``; the frame's own
/// `step` field wins when set.
///
/// The chain still carries that layer even when the field is filled, so it
/// is dropped here rather than printed a second time after `FAILED at`. A
/// cancelled step's layer reads `` step `name` not applied ``, and the words
/// after the name are part of the cause and stay.
///
/// Parsing the chain is the fallback for an error that did not come from
/// `ctx.step` at all, and it is only a guess: a step name holding a backtick
/// followed by a colon splits in the wrong place. That is why the field
/// exists.
fn split_step<'a>(step: Option<&'a str>, error: &'a str) -> (Option<&'a str>, &'a str) {
    if let Some(s) = step {
        let head = format!("step `{s}`");
        let Some(rest) = error.strip_prefix(&head) else {
            return (Some(s), error);
        };
        let cause = rest
            .strip_prefix(": ")
            .or_else(|| rest.strip_prefix(' '))
            .unwrap_or(rest);
        return (Some(s), cause);
    }
    if let Some(rest) = error.strip_prefix("step `")
        && let Some((name, cause)) = rest.split_once("`: ")
    {
        return (Some(name), cause);
    }
    (None, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustible_sdk::CmdFailed;
    use rustible_sdk::event::Collect;
    use rustible_sdk::event::EventSink;

    fn step_started(id: u32, name: &str) -> Event {
        Event::StepStarted {
            id,
            depth: 0,
            name: name.into(),
            identity: "self".into(),
        }
    }

    fn step_finished(id: u32, name: &str, status: Status) -> Event {
        Event::StepFinished {
            id,
            depth: 0,
            name: name.into(),
            identity: "self".into(),
            status,
            diff: None,
            note: None,
            elapsed_ms: 3,
        }
    }

    fn render(verbosity: u8, feed: impl FnOnce(&mut Renderer<Vec<u8>>)) -> String {
        let mut r = Renderer::new(Vec::new(), &["local".into(), "arm".into()], verbosity);
        feed(&mut r);
        String::from_utf8(r.w).unwrap()
    }

    #[test]
    fn sub_events_nest_under_their_step_and_hosts_interleave_by_step() {
        // The binary's events for one host, collected the way a run does.
        let c = Collect::default();
        c.emit(step_started(1, "mc present"));
        c.emit(Event::CmdRan {
            identity: "self".into(),
            argv: vec!["dpkg-query".into(), "-W".into(), "mc".into()],
            status: 0,
            elapsed_ms: 12,
        });
        c.emit(Event::Log {
            level: Level::Debug,
            msg: "already there".into(),
        });
        c.emit(step_finished(1, "mc present", Status::Ok));
        c.emit(Event::Log {
            level: Level::Info,
            msg: "done".into(),
        });
        c.emit(Event::Finished(Summary {
            ok: 1,
            ..Default::default()
        }));
        let local = c.events();

        let out = render(2, |r| {
            // arm starts its step first, local's whole step lands while arm
            // is still inside its own: local prints as a unit, arm after.
            r.event("arm", &step_started(1, "mc present"));
            for ev in &local {
                r.event("local", ev);
            }
            r.event(
                "arm",
                &Event::CmdRan {
                    identity: "self".into(),
                    argv: vec!["apt-get".into()],
                    status: 0,
                    elapsed_ms: 4500,
                },
            );
            r.event("arm", &step_finished(1, "mc present", Status::Changed));
            r.event(
                "arm",
                &Event::Finished(Summary {
                    changed: 1,
                    ..Default::default()
                }),
            );
            assert!(!r.finish());
        });
        let expected = "\
[local]  mc present ................................. ok
[local]    $ dpkg-query -W mc (as self, exit 0, 12ms)
[local]    debug: already there
[local]    done
[arm  ]  mc present ................................. changed
[arm  ]    $ apt-get (as self, exit 0, 4500ms)

host    ok  changed  would change  skipped  failed  warnings
local    1        0             0        0       0         0
arm      0        1             0        0       0         0
";
        assert_eq!(out, expected);
    }

    #[test]
    fn cmd_ran_and_debug_hidden_below_their_verbosity() {
        let out = render(0, |r| {
            r.event("local", &step_started(1, "x"));
            r.event(
                "local",
                &Event::CmdRan {
                    identity: "self".into(),
                    argv: vec!["true".into()],
                    status: 0,
                    elapsed_ms: 1,
                },
            );
            r.event(
                "local",
                &Event::Log {
                    level: Level::Debug,
                    msg: "hidden".into(),
                },
            );
            r.event("local", &step_finished(1, "x", Status::Ok));
        });
        assert_eq!(out.lines().count(), 1, "{out}");
        assert!(!out.contains("hidden"));
    }

    #[test]
    fn failure_renders_per_vision_14() {
        let failed = Event::Failed {
            step: None,
            error: "step `nginx present`: installing nginx: `apt-get install -y nginx` exited 100"
                .into(),
            cmd: Some(CmdFailed {
                argv: vec![
                    "apt-get".into(),
                    "install".into(),
                    "-y".into(),
                    "nginx".into(),
                ],
                status: 100,
                stderr: "E: Unable to locate package nginx\n".into(),
            }),
        };
        let quiet = render(0, |r| {
            r.event("web1", &step_started(1, "nginx present"));
            r.event("web1", &step_finished(1, "nginx present", Status::Failed));
            r.event("web1", &failed);
            r.event(
                "web1",
                &Event::Finished(Summary {
                    failed: 1,
                    ..Default::default()
                }),
            );
            assert!(r.finish());
        });
        assert!(quiet.contains(
            "[web1 ]  FAILED at `nginx present`: installing nginx: `apt-get install -y nginx` exited 100\n"
        ), "{quiet}");
        assert!(!quiet.contains("Unable to locate"));

        let verbose = render(1, |r| {
            r.event("web1", &failed);
        });
        assert!(
            verbose.contains("[web1 ]    $ apt-get install -y nginx (exit 100)\n"),
            "{verbose}"
        );
        assert!(
            verbose.contains("[web1 ]      E: Unable to locate package nginx\n"),
            "{verbose}"
        );
    }

    #[test]
    fn orchestrator_failures_and_missing_summaries_fail_the_run() {
        let out = render(0, |r| {
            r.failed("arm", "ssh to cadu-cogram-vm-arm: Connection timed out");
            r.event("local", &Event::Finished(Summary::default()));
            r.exited("local", 0);
            assert!(r.finish());
        });
        assert!(out.contains("[arm  ]  FAILED: ssh to cadu-cogram-vm-arm: Connection timed out\n"));
        assert!(
            out.contains("arm    failed: ssh to cadu-cogram-vm-arm"),
            "{out}"
        );

        let out = render(0, |r| {
            r.exited("local", 101);
            assert!(r.finish());
        });
        assert!(
            out.contains("local  no summary (binary exited 101 before finishing)"),
            "{out}"
        );
    }

    #[test]
    fn failed_step_chain_prints_once_and_still_prints_when_swallowed() {
        let mut finished = step_finished(1, "x", Status::Failed);
        if let Event::StepFinished { note, .. } = &mut finished {
            *note = Some("boom: deeper".into());
        }
        let reported = render(0, |r| {
            r.event("local", &step_started(1, "x"));
            r.event("local", &finished);
            r.event(
                "local",
                &Event::Failed {
                    step: None,
                    error: "step `x`: boom: deeper".into(),
                    cmd: None,
                },
            );
            r.event(
                "local",
                &Event::Finished(Summary {
                    failed: 1,
                    ..Default::default()
                }),
            );
        });
        assert_eq!(reported.matches("boom: deeper").count(), 1, "{reported}");
        assert!(
            reported.contains(
                "[local]  x .......................................... FAILED
"
            ),
            "{reported}"
        );

        let swallowed = render(0, |r| {
            r.event("local", &step_started(1, "x"));
            r.event("local", &finished);
            r.event("local", &step_started(2, "y"));
            r.event("local", &step_finished(2, "y", Status::Ok));
            r.event(
                "local",
                &Event::Finished(Summary {
                    failed: 1,
                    ok: 1,
                    ..Default::default()
                }),
            );
        });
        assert!(
            swallowed.contains(
                "[local]  FAILED at `x`: boom: deeper
"
            ),
            "{swallowed}"
        );
        assert!(swallowed.find("FAILED at").unwrap() < swallowed.find("y ....").unwrap());
    }

    /// A step name holding a backtick and a colon splits the chain in the
    /// wrong place, which is why the frame carries the name itself. The
    /// filled field wins, and the chain's own `step `...`: ` layer is not
    /// printed twice.
    #[test]
    fn a_step_name_with_a_backtick_needs_the_frames_own_field() {
        let name = "odd `: name";
        let chain = format!("step `{name}`: deeper");
        assert_eq!(
            split_step(None, &chain),
            (Some("odd "), "name`: deeper"),
            "the fallback parser cannot do better than this"
        );
        assert_eq!(split_step(Some(name), &chain), (Some(name), "deeper"));

        let out = render(0, |r| {
            r.event("local", &step_started(1, name));
            r.event("local", &step_finished(1, name, Status::Failed));
            r.event(
                "local",
                &Event::Failed {
                    step: Some(name.into()),
                    error: chain.clone(),
                    cmd: None,
                },
            );
        });
        assert!(out.contains("FAILED at `odd `: name`: deeper\n"), "{out}");
    }

    /// A cancellation wraps the step differently (`` step `x` not started ``);
    /// the layer still goes, and what it said stays.
    #[test]
    fn a_cancelled_step_keeps_its_reason() {
        assert_eq!(
            split_step(Some("x"), "step `x` not started: cancelled"),
            (Some("x"), "not started: cancelled")
        );
    }

    #[test]
    fn argv_with_control_characters_stays_on_one_line() {
        let argv = vec![
            "dpkg-query".to_string(),
            "-f=${Status}\t${Version}\n".to_string(),
            "mc".to_string(),
        ];
        assert_eq!(
            argv_text(&argv),
            "dpkg-query \"-f=${Status}\\t${Version}\\n\" mc"
        );
        assert_eq!(argv_text(&["a b".to_string()]), "\"a b\"");
    }

    #[test]
    fn split_step_reads_the_context_chain() {
        assert_eq!(split_step(Some("a"), "x"), (Some("a"), "x"));
        assert_eq!(
            split_step(None, "step `a b`: cause: deeper"),
            (Some("a b"), "cause: deeper")
        );
        assert_eq!(split_step(None, "panic: boom"), (None, "panic: boom"));
    }
}
