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

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;

use rustible_sdk::CmdFailed;
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
    /// the one that failed the host, the binary's `Failed` frame for the same
    /// step prints the chain. Otherwise (the playbook caught the error: the
    /// step is `recovered`) it prints from here, by `flush`: when the next
    /// step starts or is skipped, before a `Failed` frame for another
    /// failure, at `Finished`, or when the host ends. Lines from outside a
    /// step that arrive meanwhile queue in `after` and print after it.
    pending_fail: Option<PendingFail>,
    /// The failed steps whose chain was printed from `pending_fail`, with
    /// the chain as printed, so a `Failed` frame that names one later (an
    /// error the playbook kept and returned after other steps) closes it
    /// without printing it twice. Only when the frame's chain is that one:
    /// a `.context(..)` the playbook added on the way out was not printed.
    /// The failed command printed with the chain either way, so the frame
    /// never prints its own.
    shown: Vec<(FailKey, String)>,
}

/// Which failed step a chain or a `Failed` frame belongs to: its id, block
/// path and name. The id alone is not enough, because every `Ctx` a playbook
/// makes numbers its steps from 1; the name alone is not, because names
/// repeat.
#[derive(Clone, PartialEq, Eq)]
struct FailKey {
    id: u32,
    blocks: Vec<String>,
    step: String,
}

/// A failed step's chain, held back until it is known whether a `Failed`
/// frame will print it.
struct PendingFail {
    key: FailKey,
    chain: String,
    /// The failed command in the chain, if any, printed under it at `-v`.
    cmd: Option<CmdFailed>,
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
                &format!(
                    "FAILED at {}: {}",
                    failed_at(&p.key.blocks, &p.key.step),
                    p.chain
                ),
            );
            if let Some(c) = &p.cmd {
                self.cmd_block(host, c);
            }
            for l in p.after {
                self.line(host, &l);
            }
            self.state(host).shown.push((p.key, p.chain));
        }
    }

    /// The failed command under its `FAILED` line at `-v`: `$ argv (exit
    /// N)`, then its stderr. Printed once per failure, wherever its chain is.
    fn cmd_block(&mut self, host: &str, c: &CmdFailed) {
        if self.verbosity >= 1 {
            self.line(
                host,
                &format!("  $ {} (exit {})", argv_text(&c.argv), c.status),
            );
            for l in c.stderr.lines() {
                self.line(host, &format!("    {l}"));
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
                cmd,
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
                            key: FailKey {
                                id: *id,
                                blocks: blocks.clone(),
                                step: name.clone(),
                            },
                            chain: chain.clone(),
                            cmd: cmd.clone(),
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
                // Whatever the status: a step carries a diff only when its
                // `check` planned something (changed, would change, failed
                // in `apply`, cancelled between `check` and `apply`, or ran
                // and changed nothing), and the step line shows only the
                // diff's first line. One line is already whole there.
                if self.verbosity >= 1
                    && let Some(d) = diff
                    && !on_the_step_line(d)
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
                // The failed step this frame closes, when it names one of
                // this run's: by id, blocks and name together (see `FailKey`).
                let key = match (id, step) {
                    (Some(id), Some(step)) => Some(FailKey {
                        id: *id,
                        blocks: blocks.clone(),
                        step: step.to_string(),
                    }),
                    _ => None,
                };
                let st = self.state(host);
                let pending = st.pending_fail.take_if(|p| key.as_ref() == Some(&p.key));
                // A frame with no id cannot take the held reason, but may
                // still be its failure: same name and blocks.
                let same_step =
                    |k: &FailKey| key.is_none() && step == Some(&*k.step) && k.blocks == *blocks;
                let flushed_same = st.pending_fail.as_ref().is_some_and(|p| same_step(&p.key));
                // Anything else held back is a different failure, which the
                // playbook caught: its reason prints first, in order.
                self.flush(host);
                // Its reason was printed already, word for word, and no other
                // failed step of the same name and blocks was, so "above" can
                // only mean it. With such a twin the reason is printed again:
                // repeated, but never ambiguous. So is a reason the playbook
                // added words to with `.context(..)`, which were not above.
                let st = self.state(host);
                // Its chain printed before this frame, and its command with
                // it: earlier, when a later step began, or just now by the
                // flush, for a frame that names the step but not its id. A
                // frame that took the held chain prints the command, even
                // beside a twin from another `Ctx` printed with the same key.
                let printed = flushed_same
                    || pending.is_none()
                        && key
                            .as_ref()
                            .is_some_and(|k| st.shown.iter().any(|(o, _)| o == k));
                let shown = key.as_ref().is_some_and(|k| {
                    st.shown.iter().any(|(o, chain)| o == k && *chain == *error)
                        && !st
                            .shown
                            .iter()
                            .any(|(o, _)| o.step == k.step && o.blocks == k.blocks && o.id != k.id)
                });
                let text = match (step, shown) {
                    // Printed already, when a later step began; this line
                    // only says it is the one that failed the host.
                    (Some(s), true) => format!("FAILED: {} (reason above)", failed_at(blocks, s)),
                    (Some(s), false) => format!("FAILED at {}: {error}", failed_at(blocks, s)),
                    (None, _) => format!("FAILED: {error}"),
                };
                self.line(host, &text);
                if !printed && let Some(c) = cmd {
                    self.cmd_block(host, c);
                }
                // The reason first, then what the playbook printed after
                // it, as for a failure it caught.
                for l in pending.map(|p| p.after).unwrap_or_default() {
                    self.line(host, &l);
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

/// Whether `d`'s full render says no more than the step line does: one line
/// with something on it, the one `short()` shows. Blank lines do not count,
/// as they do not for `short()`. The SDK's `Compact` has its own copy.
fn on_the_step_line(d: &rustible_sdk::Diff) -> bool {
    let full = d.render();
    let mut lines = full.lines().filter(|l| !l.trim().is_empty());
    matches!(
        (lines.next(), lines.next()),
        (Some(l), None) if l.trim() == d.short().trim()
    )
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
fn split_step<'a>(step: Option<&'a str>, error: &'a str) -> (Option<&'a str>, Cow<'a, str>) {
    if let Some(s) = step {
        return (Some(s), Cow::Owned(drop_step_layer(s, error)));
    }
    if let Some(rest) = error.strip_prefix("step `")
        && let Some((name, cause)) = rest.split_once("`: ")
    {
        return (Some(name), Cow::Borrowed(cause));
    }
    (None, Cow::Borrowed(error))
}

/// `error` without the `` step `name` `` layer, wherever it sits: outermost
/// for a bare `?`, further in when the playbook wrapped the step's error with
/// `.context(..)` on its way out (`deploying the app: step `deploy`: ..`). A
/// layer starts the chain or follows a `": "`, and is followed by `": "` or,
/// for a cancelled step, by a space and the words after the name, which are
/// part of the cause and stay. A chain without the layer is returned whole.
fn drop_step_layer(step: &str, error: &str) -> String {
    let head = format!("step `{step}`");
    let mut from = 0;
    while let Some(at) = error[from..].find(&head).map(|i| from + i) {
        let rest = &error[at + head.len()..];
        let rest = rest
            .strip_prefix(": ")
            .or_else(|| rest.strip_prefix(' '))
            .or_else(|| rest.is_empty().then_some(rest));
        if let Some(rest) = rest
            && (at == 0 || error[..at].ends_with(": "))
        {
            return format!("{}{rest}", &error[..at]);
        }
        from = at + 1;
    }
    error.to_string()
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
            cmd: None,
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

    /// What the playbook says about a failed step comes after the reason it
    /// failed, never before it: when the playbook caught the error, and when
    /// the error escapes and the frame prints the reason instead.
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
[local]  FAILED at `optional thing`: `false` exited 1
[local]    WARNING: skipping: step `optional thing`: `false` exited 1
"
        );
    }

    fn failing(id: u32, name: &str, why: &str) -> Event {
        let mut ev = step_finished(id, name, Status::Failed);
        if let Event::StepFinished { note, .. } = &mut ev {
            *note = Some(why.into());
        }
        ev
    }

    fn failed_frame(step: Option<&str>, id: Option<u32>, error: &str) -> Event {
        Event::Failed {
            step: step.map(String::from),
            id,
            blocks: vec![],
            error: error.into(),
            cmd: None,
        }
    }

    // ---- the failed command ----

    fn sh_failed() -> CmdFailed {
        CmdFailed {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo nope >&2; exit 3".into(),
            ],
            status: 3,
            signal: None,
            stderr: "nope\nstill nope\n".into(),
        }
    }

    /// `failing`, with the failed command and a diff of two lines.
    fn failing_cmd(id: u32, name: &str) -> Event {
        let mut ev = failing(id, name, "`/bin/sh -c echo nope >&2; exit 3` exited 3");
        if let Event::StepFinished { cmd, diff, .. } = &mut ev {
            *cmd = Some(sh_failed());
            *diff = Some(rustible_sdk::Diff::summary("PATCH http://h/x\n{}"));
        }
        ev
    }

    fn failed_frame_cmd(step: Option<&str>, id: Option<u32>, error: &str) -> Event {
        let mut ev = failed_frame(step, id, error);
        if let Event::Failed { cmd, .. } = &mut ev {
            *cmd = Some(sh_failed());
        }
        ev
    }

    const STEP_X: &str = "\
[local]  x ....................................................... FAILED          PATCH http://h/x …
[local]      | PATCH http://h/x
[local]      | {}
";

    const CMD: &str = "\
[local]    $ /bin/sh -c \"echo nope >&2; exit 3\" (exit 3)
[local]      nope
[local]      still nope
";

    /// A failure the playbook caught shows its command and stderr at `-v`,
    /// after its reason and before what the playbook said about it: the
    /// step line, its diff, the reason, the command, then the warning.
    /// Below `-v`, the reason alone.
    #[test]
    fn a_caught_failure_shows_its_command_at_v() {
        let feed = |r: &mut Renderer<Vec<u8>>| {
            r.event("local", &step_started(1, "x"));
            r.event("local", &failing_cmd(1, "x"));
            r.event(
                "local",
                &Event::Log {
                    level: Level::Warn,
                    msg: "skipping x".into(),
                },
            );
            r.event("local", &step_started(2, "next"));
            r.event("local", &step_finished(2, "next", Status::Ok));
        };
        let reason = "[local]  FAILED at `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3\n";
        let rest = "\
[local]    WARNING: skipping x
[local]  next .................................................... ok
";
        assert_eq!(render(1, feed), format!("{STEP_X}{reason}{CMD}{rest}"));
        let step_line = STEP_X.lines().next().unwrap();
        assert_eq!(render(0, feed), format!("{step_line}\n{reason}{rest}"));
    }

    /// The failure that escaped: the `Failed` frame takes the held reason
    /// and prints the command with it, once, though both the step and the
    /// frame carry it.
    #[test]
    fn an_escaped_failure_shows_its_command_once() {
        let out = render(1, |r| {
            r.event("local", &step_started(1, "x"));
            r.event("local", &failing_cmd(1, "x"));
            r.event(
                "local",
                &failed_frame_cmd(
                    Some("x"),
                    Some(1),
                    "step `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3",
                ),
            );
        });
        assert_eq!(
            out,
            format!(
                "{STEP_X}[local]  FAILED at `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3\n{CMD}"
            )
        );
    }

    /// An error the playbook kept and returned after other steps: the
    /// command printed with the reason when the next step began, so the
    /// frame prints neither again. With context the playbook added on the
    /// way out, the frame prints the reason again for its new words, but
    /// the command still only once.
    #[test]
    fn a_reason_above_shows_its_command_once() {
        for (error, closing) in [
            (
                "step `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3",
                "[local]  FAILED: `x` (reason above)\n",
            ),
            (
                "deploying: step `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3",
                "[local]  FAILED at `x`: deploying: `/bin/sh -c echo nope >&2; exit 3` exited 3\n",
            ),
        ] {
            let out = render(1, |r| {
                r.event("local", &step_started(1, "x"));
                r.event("local", &failing_cmd(1, "x"));
                r.event("local", &step_started(2, "cleanup"));
                r.event("local", &step_finished(2, "cleanup", Status::Ok));
                r.event("local", &failed_frame_cmd(Some("x"), Some(1), error));
            });
            assert_eq!(
                out,
                format!(
                    "{STEP_X}[local]  FAILED at `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3\n\
                     {CMD}\
                     [local]  cleanup ................................................. ok\n\
                     {closing}"
                )
            );
            assert_eq!(out.matches("still nope").count(), 1, "{out}");
        }
    }

    /// A command that failed outside any step, the playbook's own
    /// `ctx.sys()` call with `?`: no step carries it, so the `Failed` frame
    /// prints it.
    #[test]
    fn a_command_failing_outside_any_step_shows_it_from_the_frame() {
        let out = render(1, |r| {
            r.event("local", &step_started(1, "first"));
            r.event("local", &step_finished(1, "first", Status::Ok));
            r.event(
                "local",
                &failed_frame_cmd(None, None, "`/bin/sh -c echo nope >&2; exit 3` exited 3"),
            );
        });
        assert_eq!(
            out,
            format!(
                "[local]  first ................................................... ok\n\
                 [local]  FAILED: `/bin/sh -c echo nope >&2; exit 3` exited 3\n{CMD}"
            )
        );
    }

    /// A twin of the failed step, same id, name and blocks from a second
    /// `Ctx`, was caught and printed its command. The run's own step then
    /// escapes: its `Failed` frame takes the held reason, and prints its own
    /// command with it, once, beside the twin's. (The reason is word for
    /// word the twin's, so the line points above, as it did before.)
    #[test]
    fn an_escaped_failure_beside_a_caught_twin_shows_its_command_once() {
        let with_stderr = |mut ev: Event, stderr: &str| {
            if let Event::StepFinished { cmd: Some(c), .. } | Event::Failed { cmd: Some(c), .. } =
                &mut ev
            {
                c.stderr = stderr.into();
            }
            ev
        };
        let out = render(1, |r| {
            r.event("local", &step_started(1, "x"));
            r.event("local", &with_stderr(failing_cmd(1, "x"), "BBB\n"));
            r.event("local", &step_started(1, "x"));
            r.event("local", &with_stderr(failing_cmd(1, "x"), "AAA\n"));
            r.event(
                "local",
                &with_stderr(
                    failed_frame_cmd(
                        Some("x"),
                        Some(1),
                        "step `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3",
                    ),
                    "AAA\n",
                ),
            );
        });
        assert_eq!(out.matches("BBB").count(), 1, "{out}");
        assert_eq!(out.matches("AAA").count(), 1, "{out}");
        assert!(
            out.ends_with(
                "[local]  FAILED: `x` (reason above)\n\
                 [local]    $ /bin/sh -c \"echo nope >&2; exit 3\" (exit 3)\n\
                 [local]      AAA\n"
            ),
            "{out}"
        );
    }

    /// A frame naming the step but not its id (an error another `Ctx`
    /// returned, or one the runtime could not pair) leaves the held reason
    /// to print on its own, command and all; the frame does not print the
    /// same command again under its own line.
    #[test]
    fn a_frame_without_an_id_does_not_repeat_the_held_steps_command() {
        let out = render(1, |r| {
            r.event("local", &step_started(1, "x"));
            r.event("local", &failing_cmd(1, "x"));
            r.event(
                "local",
                &failed_frame_cmd(
                    Some("x"),
                    None,
                    "step `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3",
                ),
            );
        });
        let reason = "[local]  FAILED at `x`: `/bin/sh -c echo nope >&2; exit 3` exited 3\n";
        assert_eq!(out, format!("{STEP_X}{reason}{CMD}{reason}"));
    }

    /// Two `Ctx` values both number from 1, so `a` (this run's) and `b` (a
    /// second context's) share an id. `b`'s reason was held back when `a`'s
    /// frame arrived; it is a different step, and must still print.
    #[test]
    fn a_frame_pairs_with_a_held_back_chain_by_id_name_and_blocks() {
        let out = render(0, |r| {
            r.event("local", &step_started(1, "a"));
            r.event("local", &failing(1, "a", "`exit 3` exited 3"));
            r.event("local", &step_started(1, "b"));
            r.event("local", &failing(1, "b", "`exit 4` exited 4"));
            r.event(
                "local",
                &failed_frame(Some("a"), Some(1), "step `a`: `exit 3` exited 3"),
            );
        });
        assert_eq!(
            out,
            "\
[local]  a ....................................................... FAILED
[local]  FAILED at `a`: `exit 3` exited 3
[local]  b ....................................................... FAILED
[local]  FAILED at `b`: `exit 4` exited 4
[local]  FAILED: `a` (reason above)
"
        );
    }

    /// A host that fails without naming a step of this run (a panic, a
    /// `bail!`, a swallowed cancellation, another context's error) does not
    /// swallow the reason of the step the playbook caught just before.
    #[test]
    fn a_frame_with_no_step_id_keeps_the_held_back_reason() {
        for frame in [
            failed_frame(None, None, "panic: boom"),
            failed_frame(Some("x"), None, "step `x`: made up"),
        ] {
            let out = render(0, |r| {
                r.event("local", &step_started(1, "x"));
                r.event("local", &failing(1, "x", "`false` exited 1"));
                r.event("local", &frame);
            });
            let lines: Vec<&str> = out.lines().collect();
            assert_eq!(
                lines[1], "[local]  FAILED at `x`: `false` exited 1",
                "{out}"
            );
            assert!(
                lines[2] == "[local]  FAILED: panic: boom"
                    || lines[2] == "[local]  FAILED at `x`: made up",
                "{out}"
            );
            assert_eq!(lines.len(), 3, "{out}");
        }
    }

    /// An error the playbook kept and returned after other steps: its reason
    /// printed when the next step began, so the frame's line says only that
    /// this step failed the host, without the reason a second time.
    #[test]
    fn an_escaping_reason_already_printed_is_not_printed_twice() {
        let out = render(0, |r| {
            r.event("local", &in_block(step_started(1, "x"), &["b"]));
            r.event(
                "local",
                &in_block(failing(1, "x", "`false` exited 3"), &["b"]),
            );
            r.event("local", &step_started(2, "cleanup"));
            r.event("local", &step_finished(2, "cleanup", Status::Ok));
            r.event(
                "local",
                &Event::Failed {
                    step: Some("x".into()),
                    id: Some(1),
                    blocks: vec!["b".into()],
                    error: "step `x`: `false` exited 3".into(),
                    cmd: None,
                },
            );
        });
        assert_eq!(
            out,
            "\
[local]  [b] x ................................................... FAILED
[local]  FAILED at [b] `x`: `false` exited 3
[local]  cleanup ................................................. ok
[local]  FAILED: [b] `x` (reason above)
"
        );
        assert_eq!(out.matches("exited 3").count(), 1, "{out}");
    }

    /// An error kept, then returned with words the playbook added by
    /// `.context(..)`: what printed above was the step's own chain, without
    /// them, so the frame's line prints its chain in full rather than
    /// pointing above and losing the words.
    #[test]
    fn a_reason_given_context_after_it_printed_is_printed_again_with_it() {
        let out = render(0, |r| {
            r.event("local", &step_started(1, "deploy"));
            r.event(
                "local",
                &failing(1, "deploy", "`/bin/sh -c exit 5` exited 5"),
            );
            r.event("local", &step_started(2, "report"));
            r.event("local", &step_finished(2, "report", Status::Changed));
            r.event(
                "local",
                &failed_frame(
                    Some("deploy"),
                    Some(1),
                    "deploying the app: step `deploy`: `/bin/sh -c exit 5` exited 5",
                ),
            );
        });
        assert!(
            out.ends_with(
                "[local]  FAILED at `deploy`: `/bin/sh -c exit 5` exited 5\n\
                 [local]  report .................................................. changed\n\
                 [local]  FAILED at `deploy`: deploying the app: `/bin/sh -c exit 5` exited 5\n"
            ),
            "{out}"
        );
        assert!(!out.contains("reason above"), "{out}");
    }

    /// The manual's idiom for adding words, `.context(..)`, puts the step's
    /// layer inside the chain; it is dropped there too, so the name is not
    /// printed twice.
    #[test]
    fn a_step_layer_inside_the_chain_is_dropped_too() {
        let out = render(0, |r| {
            r.event("local", &step_started(1, "deploy"));
            r.event(
                "local",
                &failing(1, "deploy", "`/bin/sh -c exit 5` exited 5"),
            );
            r.event(
                "local",
                &failed_frame(
                    Some("deploy"),
                    Some(1),
                    "deploying the app: step `deploy`: `/bin/sh -c exit 5` exited 5",
                ),
            );
        });
        assert!(
            out.ends_with(
                "[local]  FAILED at `deploy`: deploying the app: `/bin/sh -c exit 5` exited 5\n"
            ),
            "{out}"
        );
        assert_eq!(
            split_step(
                Some("x"),
                "while waiting: step `x` not applied: cancelled: by ctrl-c"
            ),
            (
                Some("x"),
                "while waiting: not applied: cancelled: by ctrl-c".into()
            )
        );
        // Only a whole layer goes: the same words elsewhere stay.
        assert_eq!(
            split_step(Some("x"), "copied step `x`s file: step `x`: nope"),
            (Some("x"), "copied step `x`s file: nope".into())
        );
        assert_eq!(
            split_step(Some("x"), "said step `x`: done: step `x`: nope"),
            (Some("x"), "said step `x`: done: nope".into())
        );
        assert_eq!(
            split_step(Some("x"), "a: step `x`"),
            (Some("x"), "a: ".into())
        );
        assert_eq!(
            split_step(Some("x"), "no layer: here"),
            (Some("x"), "no layer: here".into())
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
            (Some("odd "), "name`: deeper".into()),
            "the fallback parser cannot do better than this"
        );
        assert_eq!(
            split_step(Some(name), &chain),
            (Some(name), "deeper".into())
        );

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
            (Some("x"), "not started: cancelled".into())
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
        assert_eq!(split_step(Some("a"), "x"), (Some("a"), "x".into()));
        assert_eq!(
            split_step(None, "step `a b`: cause: deeper"),
            (Some("a b"), "cause: deeper".into())
        );
        assert_eq!(
            split_step(None, "panic: boom"),
            (None, "panic: boom".into())
        );
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
                cmd,
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
                cmd,
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

    /// A failed step's diff is what it attempted, and the step line shows
    /// only its first line: `-v` prints the rest under it, as it does for a
    /// changed step, and the default verbosity does not.
    #[test]
    fn a_failed_steps_full_diff_prints_at_v() {
        let mut ev = failing(1, "patch folder", "PATCH http://h/x returned 409 Conflict");
        if let Event::StepFinished { diff, .. } = &mut ev {
            *diff = Some(rustible_sdk::Diff::summary(
                "PATCH http://h/x\n{\n  \"type\": \"receiveonly\"\n}",
            ));
        }
        let feed = |r: &mut Renderer<Vec<u8>>| {
            r.event("local", &step_started(1, "patch folder"));
            r.event("local", &ev);
            r.event(
                "local",
                &failed_frame(
                    Some("patch folder"),
                    Some(1),
                    "step `patch folder`: PATCH http://h/x returned 409 Conflict",
                ),
            );
        };
        let quiet = render(0, feed);
        let lines: Vec<&str> = quiet.lines().collect();
        assert_eq!(lines.len(), 2, "{quiet}");
        assert!(
            lines[0].ends_with(" FAILED          PATCH http://h/x …"),
            "the step line keeps the first line, marked as cut: {quiet}"
        );
        assert_eq!(
            lines[1],
            "[local]  FAILED at `patch folder`: PATCH http://h/x returned 409 Conflict"
        );
        let verbose = render(1, feed);
        assert_eq!(
            verbose,
            format!(
                "{}\n\
                 [local]      | PATCH http://h/x\n\
                 [local]      | {{\n\
                 [local]      |   \"type\": \"receiveonly\"\n\
                 [local]      | }}\n\
                 {}\n",
                lines[0], lines[1]
            )
        );
    }

    /// A request that ran and changed nothing (a GET with a body, or
    /// `.changed_when` saying no) is `ok` with a diff: `-v` prints it whole
    /// too, and the default verbosity only its cut first line.
    #[test]
    fn an_ok_steps_full_diff_prints_at_v() {
        let mut ev = step_finished(1, "query", Status::Ok);
        if let Event::StepFinished { diff, note, .. } = &mut ev {
            *diff = Some(rustible_sdk::Diff::summary(
                "GET http://h/x\n{\n  \"q\": 1\n}",
            ));
            *note = Some("ran, unchanged".into());
        }
        let feed = |r: &mut Renderer<Vec<u8>>| {
            r.event("local", &step_started(1, "query"));
            r.event("local", &ev);
        };
        let quiet = render(0, feed);
        assert_eq!(quiet.lines().count(), 1, "{quiet}");
        assert!(
            quiet.ends_with(" ok              GET http://h/x …   ran, unchanged\n"),
            "{quiet}"
        );
        let verbose = render(1, feed);
        assert_eq!(
            verbose,
            format!(
                "{quiet}\
                 [local]      | GET http://h/x\n\
                 [local]      | {{\n\
                 [local]      |   \"q\": 1\n\
                 [local]      | }}\n"
            )
        );
    }

    /// A diff of one line is already whole on the step line: `-v` does not
    /// repeat it underneath, whatever the status.
    #[test]
    fn a_one_line_diff_is_not_repeated_at_v() {
        for status in [
            Status::Ok,
            Status::Changed,
            Status::WouldChange,
            Status::Failed,
        ] {
            let mut ev = step_finished(1, "query", status);
            if let Event::StepFinished { diff, .. } = &mut ev {
                *diff = Some(rustible_sdk::Diff::summary("GET http://h/x\n"));
            }
            let out = render(1, |r| {
                r.event("local", &step_started(1, "query"));
                r.event("local", &ev);
            });
            assert_eq!(out.lines().count(), 1, "{status:?}: {out}");
            assert!(out.contains("GET http://h/x"), "{out}");
        }
        // Lines with nothing on them do not count, as for `short()`.
        let mut ev = step_finished(1, "query", Status::Changed);
        if let Event::StepFinished { diff, .. } = &mut ev {
            *diff = Some(rustible_sdk::Diff::summary("one\n \n"));
        }
        let out = render(1, |r| {
            r.event("local", &step_started(1, "query"));
            r.event("local", &ev);
        });
        assert_eq!(out.lines().count(), 1, "{out}");
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
