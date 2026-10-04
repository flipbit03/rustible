//! The per-host, per-step view of a run (vision doc 5.2 step 9 and 14).
//!
//! Every host's frames arrive on their own task; the renderer serializes
//! them into one stream where a step is printed as a unit: its line, then
//! everything that happened inside it (`CmdRan` at `-vv`, logs). Hosts
//! therefore interleave by whole steps, never by half a step. What happens
//! outside a step (facts, top-level logs) prints as it comes. A step inside
//! a `ctx.block` carries the block path as a `[outer][inner] ` prefix on its
//! own line, so every line stands on its own however hosts interleave; a
//! block prints no line of its own. A summary table closes the run.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;

use rustible_sdk::event::{Event, Level, Status, Summary, block_prefix};

/// Column the status starts in on a step line, counted from the start of the
/// label. 57 is where the mock-ups in issue #47 put it: a block prefix plus
/// a step name, `[DCIM folder is receive-only] Read folder config `, is 49
/// characters, and still gets a dot leader. With a host label of a dozen
/// characters and `would change`, a status-only line ends near column 85,
/// inside a 100-column terminal. A longer label still pushes the status
/// right rather than being cut.
const NAME_WIDTH: usize = 57;

#[derive(Default)]
struct HostState {
    /// Lines buffered under the step in flight, already indented.
    open: Option<Vec<String>>,
    summary: Option<Summary>,
    /// Something outside the playbook went wrong: connect, upload, protocol.
    error: Option<String>,
    exit: Option<i32>,
    /// A step reported `Failed` with its chain in `note`. If its error is
    /// the one that failed the host, the binary's `Failed` frame carrying the
    /// same step id follows and prints the chain once. If it does not (the playbook
    /// caught the error: the step is `recovered`), the chain prints from
    /// here, before whatever comes next.
    pending_fail: Option<PendingFail>,
}

/// A failed step's chain, held back until it is known whether a `Failed`
/// frame will print it.
struct PendingFail {
    /// The step's id, which a `Failed` frame for the same failure carries.
    /// Names repeat (a retry reuses one), so the name is never matched on.
    id: u32,
    blocks: Vec<String>,
    step: String,
    chain: String,
    /// Lines from outside any step that arrived while the chain was held
    /// back, typically the playbook's own word on the failure it caught
    /// (`ctx.warn(format!("skipping: {e:#}"))`). They print after the chain,
    /// so the reason a step failed comes before what was done about it.
    after: Vec<String>,
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
        let st = self.state(host);
        match (&mut st.open, &mut st.pending_fail) {
            (Some(buf), _) => buf.push(text),
            (None, Some(p)) => p.after.push(text),
            (None, None) => self.line(host, &text),
        }
    }

    fn flush(&mut self, host: &str) {
        if let Some(buf) = self.state(host).open.take() {
            for l in buf {
                self.line(host, &l);
            }
        }
        if let Some(p) = self.state(host).pending_fail.take() {
            self.line(
                host,
                &format!("FAILED at {}: {}", failed_at(&p.blocks, &p.step), p.chain),
            );
            for l in p.after {
                self.line(host, &l);
            }
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
        match ev {
            Event::Facts(f) => {
                if self.verbosity >= 1 {
                    let text = format!(
                        "facts: {:?} {} {:?} {:?} cpus={} mem={}MB user={}",
                        f.distro,
                        f.distro_version,
                        f.arch,
                        f.package_managers,
                        f.cpus,
                        f.memory_mb,
                        f.user
                    );
                    self.line(host, &text);
                }
            }
            // A block is a grouping, not a step: the steps inside carry its
            // path, so it has no line of its own.
            Event::BlockStarted { .. } | Event::BlockFinished { .. } => {}
            Event::StepStarted { .. } => {
                self.flush(host);
                self.state(host).open = Some(vec![]);
            }
            Event::StepFinished {
                id,
                blocks,
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
                        pending = Some(PendingFail {
                            id: *id,
                            blocks: blocks.clone(),
                            step: name.clone(),
                            chain: chain.clone(),
                            after: vec![],
                        });
                    }
                    Some(n) => {
                        let _ = write!(tail, "   {n}");
                    }
                    None => {}
                }
                if identity != "self" {
                    let _ = write!(tail, "   as {identity}");
                }
                let text = step_line(blocks, name, status_word(*status), &tail);
                let buffered = self.state(host).open.take().unwrap_or_default();
                self.line(host, &text);
                if self.verbosity >= 1
                    && let Some(d) = diff
                    && matches!(status, Status::Changed | Status::WouldChange)
                {
                    for l in d.render().lines() {
                        self.line(host, &format!("    | {l}"));
                    }
                }
                for l in buffered {
                    self.line(host, &l);
                }
                self.state(host).pending_fail = pending;
            }
            Event::StepSkipped {
                blocks,
                name,
                reason,
                ..
            } => {
                self.flush(host);
                let text = step_line(blocks, name, "skipped", &format!("   {reason}"));
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
            Event::Failed {
                step,
                id,
                blocks,
                error,
                cmd,
            } => {
                let (step, error) = split_step(step.as_deref(), error);
                // The frame carries the chain; the step line's copy is not
                // needed when it is the same failure, which only the step id
                // says. What was held back behind it still prints, before
                // this line.
                if let Some(p) = &self.state(host).pending_fail
                    && *id == Some(p.id)
                {
                    let held = self.state(host).pending_fail.take();
                    for l in held.map(|p| p.after).unwrap_or_default() {
                        self.line(host, &l);
                    }
                }
                self.flush(host);
                let text = match step {
                    Some(s) => format!("FAILED at {}: {error}", failed_at(blocks, s)),
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

    /// The summary table. Returns whether any host failed: a `failed`
    /// count (never `recovered`, which is a failure the playbook caught), a
    /// non-zero exit, no summary at all, or an orchestrator-side error.
    pub fn finish(&mut self) -> bool {
        let mut any_failed = false;
        let mut out = String::new();
        let w = self.width.max(4);
        let _ = writeln!(
            out,
            "\n{:<w$}  {:>3}  {:>7}  {:>12}  {:>7}  {:>6}  {:>9}  {:>8}",
            "host", "ok", "changed", "would change", "skipped", "failed", "recovered", "warnings"
        );
        for host in &self.order {
            let st = &self.hosts[host];
            let failed = st.error.is_some()
                || st.exit.is_some_and(|c| c != 0)
                || st.summary.as_ref().is_none_or(|s| s.failed > 0);
            any_failed |= failed;
            match (&st.summary, &st.error) {
                (_, Some(e)) => {
                    // One line, whatever the reason is. The full text was
                    // printed as the FAILED line above; a multi-line reason
                    // here (a toolchain refusal, a long connect error) would
                    // break the table it sits in.
                    let _ = writeln!(out, "{host:<w$}  failed: {}", one_line(e, 96));
                }
                (Some(s), None) => {
                    let _ = writeln!(
                        out,
                        "{host:<w$}  {:>3}  {:>7}  {:>12}  {:>7}  {:>6}  {:>9}  {:>8}{}",
                        s.ok,
                        s.changed,
                        s.would_change,
                        s.skipped,
                        s.failed,
                        s.recovered,
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

/// Where a step failed, after `FAILED at`: its block path as the step line
/// prints it, then its name quoted, so a repeated name inside a block is
/// told apart from the same name elsewhere.
fn failed_at(blocks: &[String], step: &str) -> String {
    if blocks.is_empty() {
        format!("`{step}`")
    } else {
        format!("{} `{step}`", block_prefix(blocks))
    }
}

fn status_word(s: Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::Changed => "changed",
        Status::WouldChange => "would change",
        Status::Failed => "FAILED",
    }
}

/// `[outer][inner] name ....... status   tail`. The block prefix is part of
/// the label, so it counts toward `NAME_WIDTH` like the name does.
fn step_line(blocks: &[String], name: &str, status: &str, tail: &str) -> String {
    let label = if blocks.is_empty() {
        format!("{name} ")
    } else {
        format!("{} {name} ", block_prefix(blocks))
    };
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

/// A reason reduced to one line of at most `max` characters, for the summary
/// table. The whole text is already on screen above, so this only has to say
/// which host failed and roughly why.
fn one_line(text: &str, max: usize) -> String {
    let first = text.lines().next().unwrap_or("").trim_end();
    let multi = text.lines().nth(1).is_some();
    if first.chars().count() <= max && !multi {
        return first.to_string();
    }
    let cut: String = first.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod one_line_tests {
    use super::one_line;

    /// The summary table is a table. A reason with newlines in it, which is
    /// what a toolchain refusal or a long connect error looks like, must not
    /// turn one row into five.
    #[test]
    fn a_multi_line_reason_becomes_one_line() {
        let reason = "curl is missing and is needed to download zig 0.15.2, and this playbook \
                      has to be built for aarch64-unknown-linux-musl.\nInstall curl, or set \
                      RUSTIBLE_ZIG to a zig already on this machine, and run this again";
        let got = one_line(reason, 96);
        assert!(!got.contains('\n'), "{got}");
        assert!(
            got.chars().count() <= 96,
            "{} chars: {got}",
            got.chars().count()
        );
        assert!(got.starts_with("curl is missing"), "{got}");
        assert!(
            got.ends_with('…'),
            "elided, so the reader knows there is more: {got}"
        );
    }

    /// A short single-line reason is left exactly as it is.
    #[test]
    fn a_short_reason_is_untouched() {
        assert_eq!(one_line("connection refused", 96), "connection refused");
    }
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
            blocks: vec![],
            name: name.into(),
            identity: "self".into(),
        }
    }

    fn step_finished(id: u32, name: &str, status: Status) -> Event {
        Event::StepFinished {
            id,
            blocks: vec![],
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
[local]  mc present .............................................. ok
[local]    $ dpkg-query -W mc (as self, exit 0, 12ms)
[local]    debug: already there
[local]    done
[arm  ]  mc present .............................................. changed
[arm  ]    $ apt-get (as self, exit 0, 4500ms)

host    ok  changed  would change  skipped  failed  recovered  warnings
local    1        0             0        0       0          0         0
arm      0        1             0        0       0          0         0
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
            id: None,
            blocks: vec![],
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
                signal: None,
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

    /// The table stays a table. A toolchain refusal is two paragraphs and a
    /// connect error can carry a chain; both arrive here through the same
    /// orchestrator-level path, and both used to print in full inside one
    /// column. This pins the table, where `one_line_tests` pins the helper.
    /// The full text is still above, as the `FAILED:` line.
    #[test]
    fn a_multi_line_reason_does_not_break_the_summary_table() {
        let reason = "curl is missing and is needed to download zig 0.15.2, and this playbook \
                      has to be built for aarch64-unknown-linux-musl. Rustible's TLS provider \
                      (ring) compiles a little C, and zig is what compiles it.\nInstall curl, \
                      or set RUSTIBLE_ZIG to a zig already on this machine, and run this again";
        let out = render(0, |r| {
            r.failed("arm", reason);
            r.event("local", &Event::Finished(Summary::default()));
            r.exited("local", 0);
            assert!(r.finish());
        });
        // The whole reason is above the table, newlines and all.
        assert!(out.contains("and run this again"), "{out}");
        // The table itself is the header plus exactly one row per host.
        let table: Vec<&str> = out
            .lines()
            .skip_while(|l| !l.trim_start().starts_with("host "))
            .filter(|l| !l.trim().is_empty())
            .collect();
        assert_eq!(table.len(), 3, "header plus two hosts, got {table:#?}");
        let arm = table.iter().find(|l| l.starts_with("arm")).unwrap();
        assert!(arm.starts_with("arm    failed: curl is missing"), "{arm}");
        assert!(arm.ends_with('…'), "elided: {arm}");
        assert!(!arm.contains("apt install"), "{arm}");
    }

    #[test]
    fn orchestrator_failures_and_missing_summaries_fail_the_run() {
        let out = render(0, |r| {
            r.failed("arm", "ssh to 10.0.3.11: Connection timed out");
            r.event("local", &Event::Finished(Summary::default()));
            r.exited("local", 0);
            assert!(r.finish());
        });
        assert!(out.contains("[arm  ]  FAILED: ssh to 10.0.3.11: Connection timed out\n"));
        assert!(out.contains("arm    failed: ssh to 10.0.3.11"), "{out}");

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
                    step: Some("x".into()),
                    id: Some(1),
                    blocks: vec![],
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
                "[local]  x ....................................................... FAILED
"
            ),
            "{reported}"
        );

        // The playbook caught the error and carried on: the step is
        // `recovered`, the host did not fail, and the chain still prints.
        let swallowed = render(0, |r| {
            r.event("local", &step_started(1, "x"));
            r.event("local", &finished);
            r.event("local", &step_started(2, "y"));
            r.event("local", &step_finished(2, "y", Status::Ok));
            r.event(
                "local",
                &Event::Finished(Summary {
                    recovered: 1,
                    ok: 1,
                    ..Default::default()
                }),
            );
            r.exited("local", 0);
            r.event("arm", &Event::Finished(Summary::default()));
            r.exited("arm", 0);
            assert!(!r.finish(), "a recovered failure does not fail the host");
        });
        assert!(
            swallowed.contains(
                "[local]  FAILED at `x`: boom: deeper
"
            ),
            "{swallowed}"
        );
        assert!(swallowed.find("FAILED at").unwrap() < swallowed.find("y ....").unwrap());
        assert!(
            swallowed.ends_with(
                "\nhost    ok  changed  would change  skipped  failed  recovered  warnings
local    1        0             0        0       0          1         0
arm      0        0             0        0       0          0         0
"
            ),
            "{swallowed}"
        );
    }

    /// The issue's own example: a retry loop that heals on its third
    /// attempt. Both failed attempts print `FAILED` and their chain, live;
    /// the last attempt prints its status; the recap counts the two
    /// failures as `recovered`, and the host is not failed.
    #[test]
    fn a_retry_that_heals_shows_every_failure_and_fails_nothing() {
        let attempt = |id: u32, status: Status| {
            let mut ev = step_finished(id, "wait for the api", status);
            if let Event::StepFinished { note, .. } = &mut ev
                && status == Status::Failed
            {
                *note = Some("`curl -fsS http://127.0.0.1:8080/health` exited 7".into());
            }
            ev
        };
        let mut r = Renderer::new(Vec::new(), &["web1".into()], 0);
        for (id, status) in [
            (1, Status::Failed),
            (2, Status::Failed),
            (3, Status::Changed),
        ] {
            r.event("web1", &step_started(id, "wait for the api"));
            r.event("web1", &attempt(id, status));
        }
        r.event(
            "web1",
            &Event::Finished(Summary {
                ok: 4,
                changed: 1,
                recovered: 2,
                ..Default::default()
            }),
        );
        r.exited("web1", 0);
        assert!(!r.finish());
        let out = String::from_utf8(r.w).unwrap();
        let expected = "\
[web1]  wait for the api ........................................ FAILED
[web1]  FAILED at `wait for the api`: `curl -fsS http://127.0.0.1:8080/health` exited 7
[web1]  wait for the api ........................................ FAILED
[web1]  FAILED at `wait for the api`: `curl -fsS http://127.0.0.1:8080/health` exited 7
[web1]  wait for the api ........................................ changed

host   ok  changed  would change  skipped  failed  recovered  warnings
web1    4        1             0        0       0          2         0
";
        assert_eq!(out, expected);
    }

    /// A host that failed is failed whatever else it recovered from, and
    /// both columns say so.
    #[test]
    fn failed_and_recovered_on_one_host_fail_it() {
        let out = render(0, |r| {
            r.event(
                "local",
                &Event::Finished(Summary {
                    failed: 1,
                    recovered: 4,
                    ..Default::default()
                }),
            );
            r.exited("local", 2);
            r.event("arm", &Event::Finished(Summary::default()));
            r.exited("arm", 0);
            assert!(r.finish());
        });
        assert!(
            out.contains(
                "local    0        0             0        0       1          4         0\n"
            ),
            "{out}"
        );
    }

    /// The closing `FAILED at` line names the step's block path, as its
    /// step line does: names repeat, and blocks make repeats likelier.
    #[test]
    fn the_failed_at_line_names_the_steps_blocks() {
        let path = ["outer", "inner"];
        let mut finished = in_block(step_finished(1, "boom", Status::Failed), &path);
        if let Event::StepFinished { note, .. } = &mut finished {
            *note = Some("`/bin/sh -c exit 3` exited 3".into());
        }
        let failed = Event::Failed {
            step: Some("boom".into()),
            id: Some(1),
            blocks: path.map(String::from).to_vec(),
            error: "step `boom`: `/bin/sh -c exit 3` exited 3".into(),
            cmd: None,
        };
        let out = render(0, |r| {
            r.event("local", &in_block(step_started(1, "boom"), &path));
            r.event("local", &finished);
            r.event("local", &failed);
        });
        assert_eq!(
            out,
            "\
[local]  [outer][inner] boom ..................................... FAILED
[local]  FAILED at [outer][inner] `boom`: `/bin/sh -c exit 3` exited 3
"
        );

        // Caught, the held-back chain carries the path the same way.
        let out = render(0, |r| {
            r.event("local", &in_block(step_started(1, "boom"), &path));
            r.event("local", &finished);
            r.event("local", &Event::Finished(Summary::default()));
        });
        assert!(
            out.ends_with(
                "[local]  FAILED at [outer][inner] `boom`: `/bin/sh -c exit 3` exited 3\n"
            ),
            "{out}"
        );
    }

    /// What the playbook says about a failure it caught comes after the
    /// reason the step failed, not before it; and when the error escapes
    /// instead, the chain still prints once, from the frame.
    #[test]
    fn a_line_after_a_held_back_chain_prints_after_it() {
        let mut finished = step_finished(1, "optional thing", Status::Failed);
        if let Event::StepFinished { note, .. } = &mut finished {
            *note = Some("`false` exited 1".into());
        }
        let warn = Event::Log {
            level: Level::Warn,
            msg: "skipping: step `optional thing`: `false` exited 1".into(),
        };
        let caught = render(0, |r| {
            r.event("local", &step_started(1, "optional thing"));
            r.event("local", &finished);
            r.event("local", &warn);
            r.event("local", &step_started(2, "next"));
            r.event("local", &step_finished(2, "next", Status::Ok));
        });
        assert_eq!(
            caught,
            "\
[local]  optional thing .......................................... FAILED
[local]  FAILED at `optional thing`: `false` exited 1
[local]    WARNING: skipping: step `optional thing`: `false` exited 1
[local]  next .................................................... ok
"
        );

        let escaped = render(0, |r| {
            r.event("local", &step_started(1, "optional thing"));
            r.event("local", &finished);
            r.event("local", &warn);
            r.event(
                "local",
                &Event::Failed {
                    step: Some("optional thing".into()),
                    id: Some(1),
                    blocks: vec![],
                    error: "step `optional thing`: `false` exited 1".into(),
                    cmd: None,
                },
            );
        });
        assert_eq!(
            escaped,
            "\
[local]  optional thing .......................................... FAILED
[local]    WARNING: skipping: step `optional thing`: `false` exited 1
[local]  FAILED at `optional thing`: `false` exited 1
"
        );
    }

    /// The held-back chain is dropped only for the `Failed` frame of the
    /// same failure, by step id: names repeat. `first` and `second` both
    /// fail as `x`; `first?` returns the earlier error after `second`'s chain
    /// was held back, and `second`'s reason must still print.
    #[test]
    fn a_held_back_chain_is_dropped_only_for_the_same_step_id() {
        let failing = |id: u32, why: &str| {
            let mut ev = step_finished(id, "x", Status::Failed);
            if let Event::StepFinished { note, .. } = &mut ev {
                *note = Some(why.into());
            }
            ev
        };
        let out = render(0, |r| {
            r.event("local", &step_started(1, "x"));
            r.event("local", &failing(1, "`false` exited 3"));
            r.event("local", &step_started(2, "x"));
            r.event("local", &failing(2, "`false` exited 4"));
            r.event(
                "local",
                &Event::Failed {
                    step: Some("x".into()),
                    id: Some(1),
                    blocks: vec![],
                    error: "step `x`: `false` exited 3".into(),
                    cmd: None,
                },
            );
        });
        assert_eq!(
            out,
            "\
[local]  x ....................................................... FAILED
[local]  FAILED at `x`: `false` exited 3
[local]  x ....................................................... FAILED
[local]  FAILED at `x`: `false` exited 4
[local]  FAILED at `x`: `false` exited 3
"
        );

        // The same step id: the frame prints the chain, once.
        let out = render(0, |r| {
            r.event("local", &in_block(step_started(2, "x"), &["b"]));
            r.event("local", &in_block(failing(2, "`false` exited 4"), &["b"]));
            r.event(
                "local",
                &Event::Failed {
                    step: Some("x".into()),
                    id: Some(2),
                    blocks: vec!["b".into()],
                    error: "step `x`: `false` exited 4".into(),
                    cmd: None,
                },
            );
        });
        assert_eq!(out.matches("exited 4").count(), 1, "{out}");
        assert!(
            out.ends_with("FAILED at [b] `x`: `false` exited 4\n"),
            "{out}"
        );
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
                    id: Some(1),
                    blocks: vec![],
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

    // ---- blocks ----

    fn in_block(ev: Event, path: &[&str]) -> Event {
        let path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
        match ev {
            Event::StepStarted {
                id, name, identity, ..
            } => Event::StepStarted {
                id,
                blocks: path,
                name,
                identity,
            },
            Event::StepFinished {
                id,
                name,
                identity,
                status,
                diff,
                note,
                elapsed_ms,
                ..
            } => Event::StepFinished {
                id,
                blocks: path,
                name,
                identity,
                status,
                diff,
                note,
                elapsed_ms,
            },
            other => other,
        }
    }

    fn block(started: bool, path: &[&str]) -> Event {
        let blocks = path.iter().map(|s| s.to_string()).collect();
        if started {
            Event::BlockStarted { blocks }
        } else {
            Event::BlockFinished { blocks }
        }
    }

    /// Every line stands on its own: a step inside a block carries the whole
    /// path, nested blocks included, aligned like any other step, and the
    /// block itself prints nothing. Two hosts interleaving their blocks is
    /// exactly the case a heading line plus indentation could not survive.
    #[test]
    fn steps_in_blocks_carry_the_path_and_hosts_interleave_cleanly() {
        const DCIM: &str = "DCIM";
        let mut read = step_finished(1, "Read folder config", Status::WouldChange);
        if let Event::StepFinished { diff, .. } = &mut read {
            *diff = Some(rustible_sdk::Diff::summary(
                "GET http://x (not sent under --check)",
            ));
        }
        let out = render(0, |r| {
            r.event("local", &block(true, &[DCIM]));
            r.event("arm", &block(true, &[DCIM]));
            r.event(
                "local",
                &in_block(step_started(1, "Read folder config"), &[DCIM]),
            );
            r.event(
                "arm",
                &in_block(step_started(1, "Read folder config"), &[DCIM]),
            );
            r.event("arm", &in_block(read.clone(), &[DCIM]));
            r.event(
                "arm",
                &Event::Log {
                    level: Level::Warn,
                    msg: "[DCIM] not evaluated further under --check: needs the output of \
                          step `Read folder config`, which would change and so has none"
                        .into(),
                },
            );
            r.event("arm", &block(false, &[DCIM]));
            r.event(
                "local",
                &in_block(step_finished(1, "Read folder config", Status::Ok), &[DCIM]),
            );
            r.event("local", &block(true, &[DCIM, "inner"]));
            r.event(
                "local",
                &in_block(step_started(2, "Set type"), &[DCIM, "inner"]),
            );
            r.event(
                "local",
                &in_block(
                    step_finished(2, "Set type", Status::Changed),
                    &[DCIM, "inner"],
                ),
            );
            r.event("local", &block(false, &[DCIM, "inner"]));
            r.event("local", &block(false, &[DCIM]));
            r.event("arm", &step_started(2, "Restart syncthing"));
            r.event(
                "arm",
                &step_finished(2, "Restart syncthing", Status::WouldChange),
            );
        });
        let expected = "\
[arm  ]  [DCIM] Read folder config ............................... would change    GET http://x (not sent under --check)
[arm  ]    WARNING: [DCIM] not evaluated further under --check: needs the output of step `Read folder config`, which would change and so has none
[local]  [DCIM] Read folder config ............................... ok
[local]  [DCIM][inner] Set type .................................. changed
[arm  ]  Restart syncthing ....................................... would change
";
        assert_eq!(out, expected);
    }

    /// A prefix longer than the name column pushes the status right, as a
    /// long name does; it is never cut.
    #[test]
    fn a_long_prefix_counts_toward_the_name_column() {
        let path = ["a block with a rather long name", "and another one, longer"];
        assert_eq!(
            step_line(&path.map(String::from), "step", "ok", ""),
            "[a block with a rather long name][and another one, longer] step  ok"
        );
        assert_eq!(
            step_line(&["b".to_string()], "step", "ok", ""),
            "[b] step ................................................ ok"
        );
        assert_eq!(
            step_line(&[], "step", "skipped", "   why"),
            "step .................................................... skipped         why"
        );
    }

    /// Under a prefixed step the diff keeps a fixed indent: there is no depth
    /// to indent by any more.
    #[test]
    fn diff_lines_under_a_prefixed_step_have_a_fixed_indent() {
        let mut ev = step_finished(1, "conf", Status::Changed);
        if let Event::StepFinished { diff, .. } = &mut ev {
            *diff = Some(rustible_sdk::Diff::text("/etc/x", "a\n", "b\n"));
        }
        let out = render(1, |r| {
            r.event("local", &in_block(step_started(1, "conf"), &["a", "b"]));
            r.event("local", &in_block(ev, &["a", "b"]));
        });
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with("[local]  [a][b] conf ......"), "{out}");
        assert!(lines.len() > 1, "{out}");
        for l in &lines[1..] {
            assert!(l.starts_with("[local]      | "), "{out}");
        }
    }

    #[test]
    fn a_skipped_step_in_a_block_carries_the_prefix() {
        let out = render(0, |r| {
            r.event(
                "local",
                &Event::StepSkipped {
                    id: 1,
                    blocks: vec!["a".into()],
                    name: "restart".into(),
                    reason: "config unchanged".into(),
                },
            );
        });
        assert_eq!(
            out,
            "[local]  [a] restart ............................................. skipped         config unchanged\n"
        );
    }

    #[test]
    fn block_events_print_nothing() {
        let out = render(2, |r| {
            r.event("local", &block(true, &["a"]));
            r.event("local", &block(true, &["a", "b"]));
            r.event("local", &block(false, &["a", "b"]));
            r.event("local", &block(false, &["a"]));
        });
        assert_eq!(out, "");
    }
}
