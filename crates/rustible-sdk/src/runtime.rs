//! The process entry point every playbook binary shares (vision doc 5.5, 9).
//!
//! A workspace's generated `src/main.rs` is two lines: include the registry
//! the build script wrote, then `rustible::runtime::main(PLAYBOOKS)`. This
//! module implements the four modes of a playbook binary:
//!
//! - `--describe`: print metadata and vars schema for every playbook as JSON.
//! - `--check-vars [<name>]`: the orchestrator's vars pre-check (vision 10.3).
//!   Reads a JSON array of `{ "host", "vars" }` on stdin and prints a JSON
//!   array of `{ "host", "problems": [ { "var", "severity", "message" } ] }`,
//!   validating each host's vars exactly as `Start` would (coercion, the
//!   schema walk for per-var attribution, then serde on the typed struct).
//! - `--remote`: driven by an orchestrator; read the `Start` frame on stdin,
//!   write `Hello` and event frames on stdout.
//! - `--helper`: serve `Backend` primitives to a sibling process that runs
//!   as another user (vision doc 11.3); this is the `Elevated` backend's
//!   other half.
//! - a plain local run: `<name> [--check] [-v|-vv] [--json] [--var k=v]...`,
//!   printed one line per event (`Compact`) or as JSON lines, with
//!   `local_file` served from the current directory.

use std::io::Read;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::serve_helper;
use crate::channel::{Channel, Feeder};
use crate::ctx::{Ctx, FailedStep, HostInfo, not_evaluated_further};
use crate::error::{OutputUnavailable, catching};
use crate::event::{Compact, Event, EventSink, JsonLines, SharedSink, WarnCounter};
use crate::protocol::{self, Down, FrameSink, Up};
use crate::registry::{Named, describe_all};
use crate::secret::Secret;
use crate::stream::WorkspaceFiles;
use crate::system::System;
use crate::vars;

/// Exit codes: 0 ok, 2 the host failed, 3 the binary was misused (bad
/// flags, unknown playbook, bad `Start` frame). The host failed when the
/// playbook returned an error or panicked, or the run was cancelled; a step
/// failure the playbook caught does not fail it (vision doc 14).
const EXIT_FAILED: u8 = 2;
const EXIT_USAGE: u8 = 3;

/// The generated `src/main.rs` calls this.
pub fn main(playbooks: &[Named]) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.iter().find(|a| {
        matches!(
            a.as_str(),
            "--describe" | "--check-vars" | "--remote" | "--helper"
        )
    });
    match mode.map(String::as_str) {
        Some("--describe") => {
            println!(
                "{}",
                serde_json::to_string_pretty(&describe_all(playbooks)).expect("json")
            );
            ExitCode::SUCCESS
        }
        Some("--check-vars") => {
            let name = args
                .iter()
                .find(|a| !a.starts_with('-'))
                .map(String::as_str);
            let mut input = String::new();
            if let Err(e) = std::io::stdin().lock().read_to_string(&mut input) {
                eprintln!("rustible: reading --check-vars input: {e}");
                return ExitCode::from(EXIT_USAGE);
            }
            match check_vars(playbooks, name, &input) {
                Ok(out) => {
                    println!("{}", serde_json::to_string(&out).expect("json"));
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("rustible: {e}");
                    ExitCode::from(EXIT_USAGE)
                }
            }
        }
        Some("--remote") => remote(playbooks),
        Some("--helper") => helper(),
        _ => local(playbooks, &args),
    }
}

/// Serve `Backend` requests from stdin until the parent closes it. Nothing
/// else may touch stdout here.
fn helper() -> ExitCode {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    match serve_helper(&mut stdin.lock(), &mut stdout.lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("rustible: helper: {e}");
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn find<'a>(playbooks: &'a [Named], name: &str) -> Option<&'a Named> {
    playbooks.iter().find(|p| p.name == name)
}

/// One host's vars as the orchestrator sends them to `--check-vars`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostVars {
    /// The inventory name, echoed back in the matching [`HostCheck`] so the
    /// orchestrator can pair results with hosts.
    pub host: String,
    /// The merged bag for this host, exactly as `Start` would carry it.
    /// `null` is read as an empty object, so a host with no vars is legal.
    pub vars: Value,
}

/// One problem with one var on one host, as `--check-vars` reports it.
/// `var` is empty when the problem cannot be pinned to a var (a serde
/// error the schema walk did not anticipate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarProblem {
    /// The var the problem is about: a declared name for an error, the
    /// inventory's own key for an unknown-key warning.
    pub var: String,
    /// Whether this stops the run; see [`Severity`].
    pub severity: Severity,
    /// One sentence, already written for a person to read, so the
    /// orchestrator prints it rather than rephrasing it.
    pub message: String,
}

/// How much a [`VarProblem`] matters. `--check-vars` reports both kinds; the
/// orchestrator refuses to start the run only on `Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// The vars would not deserialize into the playbook's struct, so the run
    /// would fail on the `Start` frame: a missing required var, a value of
    /// the wrong type, a nested object where vars must be flat.
    Error,
    /// An undeclared var: the bag is shared by every playbook targeting the
    /// host (vision 10.3), so this never fails a run.
    Warning,
}

/// `--check-vars` output for one host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCheck {
    /// Copied from the [`HostVars`] entry this answers; the output keeps the
    /// input's order.
    pub host: String,
    /// Empty when this host's vars would deserialize cleanly. Errors come
    /// first and warnings last, and a list holding only warnings still
    /// describes a runnable host.
    pub problems: Vec<VarProblem>,
}

/// The `--check-vars` mode as a function: `input` is the JSON array of
/// [`HostVars`]; `name` picks the playbook (optional when the binary holds
/// one). Errors are usage errors (bad input, unknown playbook).
pub fn check_vars(
    playbooks: &[Named],
    name: Option<&str>,
    input: &str,
) -> Result<Vec<HostCheck>, String> {
    let hosts: Vec<HostVars> = serde_json::from_str(input)
        .map_err(|e| format!("--check-vars input must be a JSON array of {{host, vars}}: {e}"))?;
    let named = match (name, playbooks) {
        (Some(n), _) => find(playbooks, n).ok_or_else(|| {
            format!(
                "this binary has no playbook `{n}`; it has: {}",
                names(playbooks)
            )
        })?,
        (None, [only]) => only,
        (None, _) => {
            return Err(format!(
                "--check-vars needs a playbook name; this binary has: {}",
                names(playbooks)
            ));
        }
    };
    Ok(hosts
        .into_iter()
        .map(|h| HostCheck {
            problems: check_host_vars(named.playbook, h.vars),
            host: h.host,
        })
        .collect())
}

fn names(playbooks: &[Named]) -> String {
    playbooks
        .iter()
        .map(|p| p.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// What `Start` will do to these vars, without running anything: coerce,
/// walk the schema so every problem names its var, then, when the walk is
/// clean, deserialize into the typed struct. Serde is the authority; the
/// walk only attributes. A playbook without vars has no problems.
pub fn check_host_vars(playbook: &crate::registry::Playbook, raw: Value) -> Vec<VarProblem> {
    let schema = (playbook.schema)();
    if schema.is_null() {
        return vec![];
    }
    let raw = if raw.is_null() {
        Value::Object(Default::default())
    } else {
        raw
    };
    let raw = vars::coerce_scalars_to_lists(&schema, raw);
    let error = |var: String, message: String| VarProblem {
        var,
        severity: Severity::Error,
        message,
    };
    let mut out = vec![];
    for name in vars::non_flat_vars(&schema) {
        let message = format!("var `{name}` is an object; vars are flat scalars, lists, or enums");
        out.push(error(name, message));
    }
    for name in vars::missing_required(&schema, &raw) {
        let message = format!("missing required var `{name}`");
        out.push(error(name, message));
    }
    for m in vars::type_mismatches(&schema, &raw) {
        let message = m.to_string();
        out.push(error(m.var, message));
    }
    if out.is_empty()
        && let Err(e) = (playbook.check_vars)(raw.clone())
    {
        out.push(error(String::new(), e.chain()));
    }
    for u in vars::unknown_keys(&schema, &raw) {
        let message = u.to_string();
        out.push(VarProblem {
            var: u.key,
            severity: Severity::Warning,
            message,
        });
    }
    out
}

fn remote(playbooks: &[Named]) -> ExitCode {
    let start: Option<Down> = match protocol::read_frame(&mut std::io::stdin().lock()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("rustible: bad Start frame: {e}");
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let Some(Down::Start {
        run_id,
        playbook,
        host,
        vars,
        check_mode,
        escalate_password,
        ..
    }) = start
    else {
        eprintln!("rustible: expected a Start frame first, got {start:?}");
        return ExitCode::from(EXIT_USAGE);
    };
    let Some(named) = find(playbooks, &playbook) else {
        eprintln!(
            "rustible: this binary has no playbook `{playbook}`; it has: {}",
            playbooks
                .iter()
                .map(|p| p.name)
                .collect::<Vec<_>>()
                .join(", ")
        );
        return ExitCode::from(EXIT_USAGE);
    };
    let sink = Arc::new(FrameSink(Mutex::new(std::io::stdout())));
    let _ = protocol::write_frame(
        &mut *sink.0.lock().unwrap(),
        &Up::Hello {
            protocol: protocol::PROTOCOL_VERSION,
            playbook: named.name.to_string(),
        },
    );
    let (channel, feeder) = Channel::remote(sink.clone());
    // Everything after Start (Cancel, file chunks) arrives on this thread;
    // EOF means the orchestrator is gone, which cancels the run.
    std::thread::spawn(move || read_down_frames(feeder));
    execute(
        named,
        host,
        vars,
        check_mode,
        sink,
        channel,
        escalate_password,
        run_id,
    )
}

fn read_down_frames(feeder: Feeder) {
    let mut stdin = std::io::stdin().lock();
    loop {
        match protocol::read_frame::<_, Down>(&mut stdin) {
            Ok(Some(frame)) => feeder.feed(frame),
            Ok(None) => break,
            Err(e) => {
                eprintln!("rustible: bad frame from the orchestrator: {e}");
                break;
            }
        }
    }
    feeder.close();
}

fn local(playbooks: &[Named], args: &[String]) -> ExitCode {
    let mut name: Option<String> = None;
    let mut check_mode = false;
    let mut verbosity = 0u8;
    let mut json = false;
    let mut vars = serde_json::Map::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => check_mode = true,
            "-v" => verbosity = 1,
            "-vv" => verbosity = 2,
            "--json" => json = true,
            "--var" => {
                let Some(kv) = it.next() else {
                    eprintln!("rustible: --var needs key=value");
                    return ExitCode::from(EXIT_USAGE);
                };
                let Some((k, v)) = kv.split_once('=') else {
                    eprintln!("rustible: --var needs key=value, got `{kv}`");
                    return ExitCode::from(EXIT_USAGE);
                };
                // A value that parses as JSON (number, bool, list) is taken as
                // such; anything else is a string. Same rule the CLI will use.
                let value =
                    serde_json::from_str::<Value>(v).unwrap_or(Value::String(v.to_string()));
                vars.insert(k.to_string(), value);
            }
            "-h" | "--help" => {
                print_usage(playbooks);
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("rustible: unknown flag `{other}`");
                print_usage(playbooks);
                return ExitCode::from(EXIT_USAGE);
            }
            other => {
                if let Some(first) = &name {
                    eprintln!(
                        "rustible: got two playbook names, `{first}` and `{other}`; give one"
                    );
                    print_usage(playbooks);
                    return ExitCode::from(EXIT_USAGE);
                }
                name = Some(other.to_string());
            }
        }
    }
    let named = match (name, playbooks) {
        (Some(n), _) => match find(playbooks, &n) {
            Some(p) => p,
            None => {
                eprintln!("rustible: no playbook `{n}` in this binary");
                print_usage(playbooks);
                return ExitCode::from(EXIT_USAGE);
            }
        },
        (None, [only]) => only,
        (None, _) => {
            eprintln!("rustible: which playbook?");
            print_usage(playbooks);
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let sink: SharedSink = if json {
        Arc::new(JsonLines(Mutex::new(std::io::stdout())))
    } else {
        Arc::new(Compact::new(std::io::stdout(), verbosity))
    };
    // A local run serves `local_file` from the working directory (the
    // workspace root when run from there) under the orchestrator's rules.
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let channel = match WorkspaceFiles::new(&cwd) {
        Ok(files) => Channel::local(files).0,
        Err(e) => {
            eprintln!("rustible: cannot serve files from {}: {e}", cwd.display());
            Channel::detached()
        }
    };
    execute(
        named,
        HostInfo::local(),
        Value::Object(vars),
        check_mode,
        sink,
        channel,
        None,
        // No orchestrator, so no run id: unique among live runs on this
        // host, which is all the temp directory's name needs.
        format!("pid{}", std::process::id()),
    )
}

fn print_usage(playbooks: &[Named]) {
    eprintln!("usage: <binary> <playbook> [--check] [-v|-vv] [--json] [--var key=value]...");
    eprintln!("       <binary> --describe");
    eprintln!(
        "       <binary> --check-vars [<playbook>]   (JSON array of {{host, vars}} on stdin)"
    );
    eprintln!("playbooks in this binary:");
    for p in playbooks {
        eprintln!(
            "  {}  (hosts = {:?}{})",
            p.name,
            p.playbook.hosts,
            if p.playbook.escalate {
                ", escalate"
            } else {
                ""
            }
        );
    }
}

/// The `failed` and `recovered` counts for a host, once its playbook has
/// returned (vision doc 14).
///
/// A failed step is `failed` when its error is the one that left the
/// playbook (`escaped`, read off the error's `StepFailed` layer) or when the
/// run was cancelled as it failed; every other failed step was caught by the
/// playbook, which carried on, and is `recovered`. A host that failed
/// (`host_failed`) with no failed step to show for it, because the playbook
/// `bail!`ed on its own, panicked, or was cancelled before a step started,
/// still counts one, so `failed > 0` is exactly "the host failed".
///
/// Identification is by step id, never by name, which repeats in a retry
/// loop. An `escaped` id that names no recorded failure (a layer built by
/// hand) matches nothing and falls back to the floor of one.
fn classify(failures: &[FailedStep], escaped: Option<u32>, host_failed: bool) -> (u32, u32) {
    let (mut failed, mut recovered) = (0u32, 0u32);
    for f in failures {
        if f.cancelled || escaped == Some(f.id) {
            failed = failed.saturating_add(1);
        } else {
            recovered = recovered.saturating_add(1);
        }
    }
    if host_failed && failed == 0 {
        failed = 1;
    }
    (failed, recovered)
}

/// The warning a dry run gives when this binary cannot unwind: under
/// `panic = "abort"`, a read of a would-change step's output kills the host
/// instead of ending the enclosing `ctx.block`.
fn abort_notice(abort: bool, check_mode: bool) -> Option<&'static str> {
    (abort && check_mode).then_some(
        "this playbook binary was built with panic = \"abort\", so under --check a read of a \
         would-change step's output ends this host's run instead of the enclosing ctx.block; \
         set panic = \"unwind\" in [profile.dist]",
    )
}

/// Gather facts, build the context, run the entry, report, and map the
/// outcome to an exit code. Shared by every mode.
#[allow(clippy::too_many_arguments)]
fn execute(
    named: &Named,
    host: HostInfo,
    vars: Value,
    check_mode: bool,
    sink: Arc<dyn EventSink>,
    channel: Arc<Channel>,
    escalate_password: Option<Secret>,
    run_id: String,
) -> ExitCode {
    // Every warning is counted here, at the one point every event passes,
    // rather than by each of the three places that write one.
    let counter = Arc::new(WarnCounter::new(sink));
    let sink: SharedSink = counter.clone();
    let sys = System::local(check_mode, sink.clone())
        .with_escalation(&host.escalate_method, escalate_password);
    sink.emit(Event::Facts(sys.facts().clone()));
    // A host's var bag is shared by every playbook that targets it, so keys
    // this playbook does not declare are legitimate; still, a near-miss of a
    // declared name is almost always a typo, so say so (vision 10.3).
    for w in vars::unknown_key_warnings(&(named.playbook.schema)(), &vars) {
        sink.emit(Event::Log {
            level: crate::event::Level::Warn,
            msg: w,
        });
    }
    if let Some(w) = abort_notice(cfg!(panic = "abort"), check_mode) {
        sink.emit(Event::Log {
            level: crate::event::Level::Warn,
            msg: w.into(),
        });
    }
    let mut ctx = Ctx::for_run(sys, host, channel, run_id);
    let entry = named.playbook.entry;

    let outcome = catching(|| entry(&mut ctx, vars));
    // The playbook body is the outermost block (vision doc 12): under
    // `--check`, a read of a would-change step's output that no `ctx.block`
    // absorbed ends this host's dry run here, with the same warning a block
    // gives and no prefix. Nothing failed, so the host is not counted as
    // failed. Outside check mode it is an error like any other, and so is
    // one that carries `StepFailed`: a step read the missing output inside
    // its op, failed, and that stays a failure.
    //
    // `escaped` is the id of the failed step whose error left the playbook,
    // when the error came from one: that step is `failed`, and every other
    // failed step the playbook caught is `recovered` (vision doc 14).
    let check_mode = ctx.check_mode();
    let (failed, escaped) = match outcome {
        Ok(Ok(())) => (false, None),
        Ok(Err(e)) => match e.downcast_ref::<OutputUnavailable>() {
            Some(u) if check_mode && e.step_failed().is_none() => {
                ctx.warn(not_evaluated_further("", &u.step));
                (false, None)
            }
            _ => {
                sink.emit(Event::failed(&e));
                (true, e.step_failed().and_then(|s| s.id()))
            }
        },
        Err(payload) => match payload.downcast::<OutputUnavailable>() {
            Ok(u) if check_mode => {
                ctx.warn(not_evaluated_further("", &u.step));
                (false, None)
            }
            Ok(u) => {
                // `Deref` unwinds with this typed payload; reported as the
                // error it stands for rather than as `panic: panic`.
                sink.emit(Event::Failed {
                    step: None,
                    blocks: vec![],
                    error: u.to_string(),
                    cmd: None,
                });
                (true, None)
            }
            Err(payload) => {
                let msg = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "panic".into());
                sink.emit(Event::Failed {
                    step: None,
                    blocks: vec![],
                    error: format!("panic: {msg}"),
                    cmd: None,
                });
                (true, None)
            }
        },
    };
    // A run the operator stopped never reports success, even when the
    // playbook swallowed every cancelled step's error and returned `Ok`.
    // Said here, since nothing else would say why the host failed.
    let failed = match ctx.check_cancelled() {
        Err(cancelled) if !failed => {
            sink.emit(Event::failed(&cancelled));
            true
        }
        Err(_) => true,
        Ok(()) => failed,
    };

    let mut summary = ctx.summary();
    summary.warnings = counter.count();
    (summary.failed, summary.recovered) = classify(&ctx.failures(), escaped, failed);
    // Dropping the last `Ctx` removes streamed files and closes the helpers
    // (their `CmdRan` events are already in the sink).
    drop(ctx);
    sink.emit(Event::Finished(summary.clone()));
    if summary.failed > 0 {
        ExitCode::from(EXIT_FAILED)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Playbook;

    #[derive(serde::Deserialize, schemars::JsonSchema)]
    #[allow(dead_code)]
    struct Vars {
        package: String,
        #[serde(default)]
        retries: u8,
        /// Something the schema walk only sees as a string.
        bind: Option<std::net::IpAddr>,
        ports: Vec<u16>,
    }

    static WITH_VARS: Playbook = Playbook {
        hosts: "lab",
        escalate: false,
        schema: vars::schema_for::<Vars>,
        entry: |_, _| Ok(()),
        check_vars: |raw| vars::from_value::<Vars>(raw).map(|_| ()),
    };

    static NO_VARS: Playbook = Playbook {
        hosts: "local",
        escalate: false,
        schema: vars::no_schema,
        entry: |_, _| Ok(()),
        check_vars: |_| Ok(()),
    };

    fn registry() -> Vec<Named> {
        vec![
            Named {
                name: "cadu/mc",
                playbook: &WITH_VARS,
            },
            Named {
                name: "hello",
                playbook: &NO_VARS,
            },
        ]
    }

    /// Through the JSON the mode reads and writes, as the orchestrator does.
    fn round_trip(name: Option<&str>, input: serde_json::Value) -> Vec<HostCheck> {
        let out = check_vars(&registry(), name, &input.to_string()).unwrap();
        let text = serde_json::to_string(&out).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn check_vars_round_trip() {
        let out = round_trip(
            Some("cadu/mc"),
            serde_json::json!([
                { "host": "a", "vars": { "package": "mc", "ports": 22 } },
                { "host": "b", "vars": { "retries": 300, "extra": 1, "pakage": "x" } },
                { "host": "c", "vars": { "package": "mc", "ports": [1], "bind": "not-an-ip" } }
            ]),
        );
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].host, "a");
        assert!(
            out[0].problems.is_empty(),
            "scalar coerced to list: {:?}",
            out[0].problems
        );

        let b = &out[1];
        let errors: Vec<&VarProblem> = b
            .problems
            .iter()
            .filter(|p| p.severity == Severity::Error)
            .collect();
        let vars: Vec<&str> = errors.iter().map(|p| p.var.as_str()).collect();
        assert_eq!(vars, ["package", "ports", "retries"], "{:?}", b.problems);
        assert!(errors[0].message.contains("missing required var `package`"));
        assert!(
            errors[2].message.contains("retries"),
            "{}",
            errors[2].message
        );
        let warnings: Vec<&VarProblem> = b
            .problems
            .iter()
            .filter(|p| p.severity == Severity::Warning)
            .collect();
        assert_eq!(warnings.len(), 2);
        assert!(
            warnings[1].message.contains("did you mean `package`"),
            "{}",
            warnings[1].message
        );

        // The walk passes `bind` (a string is a string); serde does not.
        let c = &out[2];
        assert_eq!(c.problems.len(), 1, "{:?}", c.problems);
        assert_eq!(c.problems[0].severity, Severity::Error);
        assert_eq!(c.problems[0].var, "");
        assert!(
            c.problems[0].message.contains("bind") || c.problems[0].message.contains("IP"),
            "{}",
            c.problems[0].message
        );
    }

    /// Every producer of a `WARNING:` line reaches the summary's warning
    /// column: `Ctx::warn`, `System::warn` from inside an op, and the
    /// runtime's own undeclared-var notice. A column that disagrees with
    /// what the operator just read on screen is worse than no column.
    #[test]
    fn every_warning_producer_reaches_the_summary() {
        use crate::event::{Collect, Level};
        use crate::op::{Op, Plan};
        use crate::system::System;

        struct WarnsFromCheck;
        impl Op for WarnsFromCheck {
            type Output = ();
            type Intent = std::convert::Infallible;
            fn check(&self, sys: &System) -> crate::Result<Plan<Self>> {
                sys.warn("the op has an opinion");
                Ok(Plan::Satisfied(()))
            }
            fn apply(&self, _: &System, intent: Self::Intent) -> crate::Result<()> {
                match intent {}
            }
        }

        static WARNS: Playbook = Playbook {
            hosts: "local",
            escalate: false,
            schema: vars::schema_for::<Vars>,
            entry: |ctx, _| {
                ctx.warn("the playbook has an opinion");
                ctx.step("op that warns", WarnsFromCheck)?;
                Ok(())
            },
            check_vars: |_| Ok(()),
        };
        let named = Named {
            name: "warns",
            playbook: &WARNS,
        };

        let sink = Arc::new(Collect::default());
        // `pakage` is undeclared, which is the runtime's own warning.
        let host_vars = serde_json::json!({ "package": "mc", "ports": [22], "pakage": "mc" });
        execute(
            &named,
            HostInfo::local(),
            host_vars,
            false,
            sink.clone(),
            Channel::detached(),
            None,
            "test".into(),
        );

        let events = sink.events();
        let printed = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::Log {
                        level: Level::Warn,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(printed, 3, "three WARNING: lines: {events:#?}");
        let summary = events
            .iter()
            .find_map(|e| match e {
                Event::Finished(s) => Some(s.clone()),
                _ => None,
            })
            .expect("the run finished");
        assert_eq!(
            summary.warnings, 3,
            "the summary counts every warning the operator saw"
        );
    }

    /// The `Failed` frame names the step itself. The renderer's fallback
    /// recovers the name by parsing the `` step `...`: `` prefix off the
    /// chain, which a name containing a backtick and a colon splits in the
    /// wrong place, so the field has to be filled where the name is known.
    #[test]
    fn failed_frame_carries_the_step_name() {
        use crate::event::Collect;
        use crate::op::{Op, Plan};
        use crate::system::System;

        struct Boom;
        impl Op for Boom {
            type Output = ();
            type Intent = std::convert::Infallible;
            fn check(&self, _: &System) -> crate::Result<Plan<Self>> {
                Err(crate::Error::msg("deeper"))
            }
            fn apply(&self, _: &System, intent: Self::Intent) -> crate::Result<()> {
                match intent {}
            }
        }

        static FAILS: Playbook = Playbook {
            hosts: "local",
            escalate: false,
            schema: vars::no_schema,
            entry: |ctx, _| {
                ctx.step("odd `: name", Boom)?;
                Ok(())
            },
            check_vars: |_| Ok(()),
        };
        let named = Named {
            name: "fails",
            playbook: &FAILS,
        };

        let sink = Arc::new(Collect::default());
        execute(
            &named,
            HostInfo::local(),
            Value::Null,
            false,
            sink.clone(),
            Channel::detached(),
            None,
            "test".into(),
        );

        let (step, error) = sink
            .events()
            .into_iter()
            .find_map(|e| match e {
                Event::Failed { step, error, .. } => Some((step, error)),
                _ => None,
            })
            .expect("the run failed");
        assert_eq!(step.as_deref(), Some("odd `: name"));
        assert_eq!(error, "step `odd `: name`: deeper");
    }

    #[test]
    fn check_vars_playbook_selection_and_no_vars() {
        let out = round_trip(
            Some("hello"),
            serde_json::json!([{ "host": "a", "vars": { "anything": true } }]),
        );
        assert!(out[0].problems.is_empty());
        let e = check_vars(&registry(), None, "[]").unwrap_err();
        assert!(e.contains("needs a playbook name"), "{e}");
        let e = check_vars(&registry(), Some("nope"), "[]").unwrap_err();
        assert!(e.contains("no playbook `nope`"), "{e}");
        let e = check_vars(&registry(), Some("hello"), "{}").unwrap_err();
        assert!(e.contains("JSON array"), "{e}");
        let only = vec![Named {
            name: "hello",
            playbook: &NO_VARS,
        }];
        assert!(check_vars(&only, None, "[]").is_ok());
    }

    // ---- the playbook body is the outermost block ----

    /// A step that would change, so under `--check` it has no output.
    struct WouldChange;

    #[derive(Debug)]
    struct Go;

    impl crate::op::Intent for Go {
        fn diff(&self) -> crate::Diff {
            crate::Diff::summary("go")
        }
    }

    impl crate::op::Op for WouldChange {
        type Output = u32;
        type Intent = Go;
        fn check(&self, _: &crate::system::System) -> crate::Result<crate::op::Plan<Self>> {
            Ok(crate::op::Plan::Change(Go))
        }
        fn apply(&self, _: &crate::system::System, Go: Go) -> crate::Result<u32> {
            Ok(1)
        }
    }

    /// Runs `playbook` through `execute`, as a host run does, and returns the
    /// exit code with every event.
    fn run(playbook: &'static Playbook, check_mode: bool) -> (ExitCode, Vec<Event>) {
        let sink = Arc::new(crate::event::Collect::default());
        let named = Named {
            name: "under-test",
            playbook,
        };
        let code = execute(
            &named,
            HostInfo::local(),
            Value::Null,
            check_mode,
            sink.clone(),
            Channel::detached(),
            None,
            "test".into(),
        );
        (code, sink.events())
    }

    fn summary_of(events: &[Event]) -> crate::event::Summary {
        events
            .iter()
            .find_map(|e| match e {
                Event::Finished(s) => Some(s.clone()),
                _ => None,
            })
            .expect("the run finished")
    }

    fn warnings_in(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Log {
                    level: crate::event::Level::Warn,
                    msg,
                } => Some(msg.clone()),
                _ => None,
            })
            .collect()
    }

    fn failed_in(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Failed { error, .. } => Some(error.clone()),
                _ => None,
            })
            .collect()
    }

    const TOP_LEVEL: &str = "not evaluated further under --check: needs the output of step \
                             `read`, which would change and so has none";

    static DEREF_AT_TOP: Playbook = Playbook {
        hosts: "local",
        escalate: false,
        schema: vars::no_schema,
        entry: |ctx, _| {
            let got = ctx.step("read", WouldChange)?;
            let n = *got;
            ctx.step("never reached", WouldChange)?;
            let _ = n;
            Ok(())
        },
        check_vars: |_| Ok(()),
    };

    static QUESTION_MARK_AT_TOP: Playbook = Playbook {
        hosts: "local",
        escalate: false,
        schema: vars::no_schema,
        entry: |ctx, _| {
            let got = ctx.step("read", WouldChange)?;
            let n = *got.output()?;
            ctx.step("never reached", WouldChange)?;
            let _ = n;
            Ok(())
        },
        check_vars: |_| Ok(()),
    };

    /// Under `--check` nothing failed: the dry run could not see further.
    /// The host ends with the unprefixed warning, succeeds, and counts no
    /// failure, whichever way the output was read.
    #[test]
    fn under_check_a_top_level_missing_output_ends_the_dry_run_without_failing() {
        for playbook in [&DEREF_AT_TOP, &QUESTION_MARK_AT_TOP] {
            let (code, events) = run(playbook, true);
            assert_eq!(code, ExitCode::SUCCESS);
            let s = summary_of(&events);
            assert_eq!(
                (s.failed, s.recovered, s.would_change, s.warnings),
                (0, 0, 1, 1)
            );
            assert_eq!(warnings_in(&events), [TOP_LEVEL]);
            assert!(failed_in(&events).is_empty(), "{events:#?}");
        }
    }

    static RETURNS_UNAVAILABLE: Playbook = Playbook {
        hosts: "local",
        escalate: false,
        schema: vars::no_schema,
        entry: |_, _| {
            Err(crate::error::OutputUnavailable {
                step: "by hand".into(),
            }
            .into())
        },
        check_vars: |_| Ok(()),
    };

    /// Outside `--check` the runtime absorbs nothing: the error fails the
    /// host as before.
    #[test]
    fn a_real_run_fails_the_host_on_output_unavailable() {
        let (code, events) = run(&RETURNS_UNAVAILABLE, false);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
        assert_eq!(summary_of(&events).failed, 1);
        assert_eq!(
            failed_in(&events),
            ["step `by hand` would have changed; its output is unavailable in check mode"]
        );
        assert!(warnings_in(&events).is_empty());
    }

    static PANICS_IN_A_BLOCK: Playbook = Playbook {
        hosts: "local",
        escalate: false,
        schema: vars::no_schema,
        entry: |ctx, _| {
            ctx.block("b", |_| -> crate::Result<()> { panic!("boom") })?;
            Ok(())
        },
        check_vars: |_| Ok(()),
    };

    /// A real panic is nobody's to absorb, in check mode included: the block
    /// resumes it and the runtime reports it as it always has.
    #[test]
    fn under_check_a_panic_in_a_block_still_fails_the_host() {
        let (code, events) = run(&PANICS_IN_A_BLOCK, true);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
        assert_eq!(summary_of(&events).failed, 1);
        assert_eq!(failed_in(&events), ["panic: boom"]);
        assert!(warnings_in(&events).is_empty());
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::BlockFinished { blocks } if blocks == &["b"])),
            "BlockFinished before the panic was resumed"
        );
    }

    static DEREF_IN_A_REAL_RUN: Playbook = Playbook {
        hosts: "local",
        escalate: false,
        schema: vars::no_schema,
        entry: |_, _| {
            // A real run cannot produce this; the payload is thrown directly
            // to show how the runtime words it if it ever does.
            crate::error::OutputUnavailable::throw("by hand")
        },
        check_vars: |_| Ok(()),
    };

    /// The typed payload, unabsorbed, is reported by its own message rather
    /// than as an anonymous panic.
    #[test]
    fn an_unabsorbed_deref_payload_reports_a_clean_message() {
        let (code, events) = run(&DEREF_IN_A_REAL_RUN, false);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
        assert_eq!(
            failed_in(&events),
            ["step `by hand` would have changed; its output is unavailable in check mode"]
        );
    }

    /// An op that reads another step's output inside `check`.
    struct ReadsInCheck(crate::op::Applied<u32>);

    impl crate::op::Op for ReadsInCheck {
        type Output = u32;
        type Intent = Go;
        fn check(&self, _: &crate::system::System) -> crate::Result<crate::op::Plan<Self>> {
            Ok(crate::op::Plan::Satisfied(*self.0))
        }
        fn apply(&self, _: &crate::system::System, Go: Go) -> crate::Result<u32> {
            Ok(1)
        }
    }

    static READS_INSIDE_AN_OP: Playbook = Playbook {
        hosts: "local",
        escalate: false,
        schema: vars::no_schema,
        entry: |ctx, _| {
            let got = ctx.step("read", WouldChange)?;
            ctx.step("uses it", ReadsInCheck(got))?;
            Ok(())
        },
        check_vars: |_| Ok(()),
    };

    /// A read inside an op is that step's failure, counted, and the runtime
    /// does not relabel it as a gap in the dry run: the host fails, with
    /// the step and the reason, and there is no "not evaluated" warning.
    #[test]
    fn under_check_a_read_inside_an_op_fails_the_host() {
        let (code, events) = run(&READS_INSIDE_AN_OP, true);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
        assert_eq!(verdict(&events), (1, 0));
        assert_eq!(
            failed_in(&events),
            [
                "step `uses it`: step `read` would have changed; its output is unavailable in \
              check mode"
            ]
        );
        assert!(warnings_in(&events).is_empty(), "{events:#?}");
    }

    /// Only a dry run of a binary that cannot unwind is warned.
    #[test]
    fn only_a_dry_run_built_to_abort_is_warned() {
        assert!(abort_notice(true, true).is_some_and(|w| w.contains("panic = \"unwind\"")));
        assert!(abort_notice(true, false).is_none());
        assert!(abort_notice(false, true).is_none());
        assert!(abort_notice(false, false).is_none());
    }

    static RESTART_GUARDED_BY_AN_ABSORBED_BLOCK: Playbook = Playbook {
        hosts: "local",
        escalate: false,
        schema: vars::no_schema,
        entry: |ctx, _| {
            let got = ctx.step("read", WouldChange)?;
            let cfg = ctx.block("uses it", |ctx| {
                let _ = *got;
                ctx.step("conf", WouldChange)
            })?;
            // `cfg.changed` is the `Applied`'s field, through `Block`'s Deref.
            if cfg.changed {
                ctx.step("restart", WouldChange)?;
            }
            Ok(())
        },
        check_vars: |_| Ok(()),
    };

    /// At the top level, reading through an absorbed block (`cfg.changed`
    /// on the `Applied` it would have returned) ends the host's dry run with
    /// the unprefixed warning naming the original step; the guarded restart
    /// does not run, and nothing fails.
    #[test]
    fn under_check_a_read_through_an_absorbed_block_ends_the_dry_run_without_failing() {
        let (code, events) = run(&RESTART_GUARDED_BY_AN_ABSORBED_BLOCK, true);
        assert_eq!(code, ExitCode::SUCCESS);
        let s = summary_of(&events);
        assert_eq!(
            (s.failed, s.recovered, s.would_change, s.warnings),
            (0, 0, 1, 2)
        );
        let missing = "not evaluated further under --check: needs the output of step `read`, \
                       which would change and so has none";
        assert_eq!(
            warnings_in(&events),
            [format!("[uses it] {missing}"), missing.to_string()]
        );
        assert!(!events.iter().any(|e| matches!(
            e,
            Event::StepStarted { name, .. } if name == "restart"
        )));
    }

    // ---- the host's verdict is what the playbook returns (#44) ----

    /// A step that succeeds or fails as it is told, so a playbook can play
    /// out a retry.
    struct Attempt(bool);

    impl crate::op::Op for Attempt {
        type Output = ();
        type Intent = std::convert::Infallible;
        fn check(&self, _: &crate::system::System) -> crate::Result<crate::op::Plan<Self>> {
            if self.0 {
                Ok(crate::op::Plan::Satisfied(()))
            } else {
                Err(crate::Error::msg("not yet"))
            }
        }
        fn apply(&self, _: &crate::system::System, intent: Self::Intent) -> crate::Result<()> {
            match intent {}
        }
    }

    macro_rules! playbook {
        ($entry:expr) => {
            Playbook {
                hosts: "local",
                escalate: false,
                schema: vars::no_schema,
                entry: $entry,
                check_vars: |_| Ok(()),
            }
        };
    }

    /// `run`, on a channel the test holds, so it can cancel the run.
    fn run_on(
        playbook: &'static Playbook,
        check_mode: bool,
        channel: Arc<Channel>,
    ) -> (ExitCode, Vec<Event>) {
        let sink = Arc::new(crate::event::Collect::default());
        let named = Named {
            name: "under-test",
            playbook,
        };
        let code = execute(
            &named,
            HostInfo::local(),
            Value::Null,
            check_mode,
            sink.clone(),
            channel,
            None,
            "test".into(),
        );
        (code, sink.events())
    }

    /// `(failed, recovered)` from the run's `Finished` frame.
    fn verdict(events: &[Event]) -> (u32, u32) {
        let s = summary_of(events);
        (s.failed, s.recovered)
    }

    /// Every `Failed` frame as `(step, blocks)`.
    fn failed_frames(events: &[Event]) -> Vec<(Option<String>, Vec<String>)> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Failed { step, blocks, .. } => Some((step.clone(), blocks.clone())),
                _ => None,
            })
            .collect()
    }

    /// The process-level mapping every case below relies on: exit 2 exactly
    /// when the summary counts a failed step, whatever it recovered from.
    fn assert_exit_matches(code: ExitCode, events: &[Event]) {
        let expected = if summary_of(events).failed > 0 {
            ExitCode::from(EXIT_FAILED)
        } else {
            ExitCode::SUCCESS
        };
        assert_eq!(code, expected, "{events:#?}");
    }

    static RETRY_HEALS: Playbook = playbook!(|ctx, _| {
        let mut attempt = 0;
        while let Err(e) = ctx.step("wait for the api", Attempt(attempt == 2)) {
            attempt += 1;
            if attempt == 5 {
                return Err(e);
            }
        }
        Ok(())
    });

    /// The issue's first scenario: a retry that succeeds on its third
    /// attempt. Two failed attempts, both caught by the loop: the host did
    /// not fail.
    #[test]
    fn a_retry_that_heals_recovers_and_does_not_fail_the_host() {
        let (code, events) = run(&RETRY_HEALS, false);
        assert_eq!(verdict(&events), (0, 2));
        assert_eq!(summary_of(&events).ok, 1);
        assert!(failed_frames(&events).is_empty(), "{events:#?}");
        assert_eq!(code, ExitCode::SUCCESS);
        // Both failed attempts were still reported failed, live.
        let failed_lines = events
            .iter()
            .filter(|e| {
                matches!(
                    e,
                    Event::StepFinished {
                        status: crate::event::Status::Failed,
                        ..
                    }
                )
            })
            .count();
        assert_eq!(failed_lines, 2);
    }

    static ESCAPES: Playbook = playbook!(|ctx, _| {
        ctx.step("boom", Attempt(false))?;
        Ok(())
    });

    #[test]
    fn a_step_error_returned_with_question_mark_fails_the_host() {
        let (code, events) = run(&ESCAPES, false);
        assert_eq!(verdict(&events), (1, 0));
        assert_eq!(failed_frames(&events), [(Some("boom".into()), vec![])]);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
    }

    static RECOVER_THEN_ESCAPE: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("a", Attempt(false)).ok();
        ctx.step("b", Attempt(false))?;
        Ok(())
    });

    #[test]
    fn a_caught_failure_then_an_escaping_one_is_one_of_each() {
        let (code, events) = run(&RECOVER_THEN_ESCAPE, false);
        assert_eq!(verdict(&events), (1, 1));
        assert_eq!(failed_frames(&events), [(Some("b".into()), vec![])]);
        assert_exit_matches(code, &events);
    }

    static RETRIES_EXHAUSTED: Playbook = playbook!(|ctx, _| {
        let mut attempt = 0;
        while let Err(e) = ctx.step("wait for the api", Attempt(false)) {
            attempt += 1;
            if attempt == 5 {
                return Err(e);
            }
        }
        Ok(())
    });

    /// Five failures of one step name, the last returned: the literal rule.
    /// The loop caught four, and the fifth left the playbook. Telling them
    /// apart takes the step id; the name is the same all five times.
    #[test]
    fn retries_exhausted_count_the_last_failed_and_the_rest_recovered() {
        let (code, events) = run(&RETRIES_EXHAUSTED, false);
        assert_eq!(verdict(&events), (1, 4));
        assert_eq!(
            failed_frames(&events),
            [(Some("wait for the api".into()), vec![])]
        );
        assert_exit_matches(code, &events);
    }

    static CONTEXT_PRESERVED: Playbook = playbook!(|ctx, _| {
        use crate::error::Context as _;
        ctx.step("boom", Attempt(false))
            .context("while bringing the api up")?;
        Ok(())
    });

    /// A layer the playbook adds on the way out does not hide which step
    /// failed.
    #[test]
    fn a_context_layer_added_by_the_playbook_keeps_the_step_identified() {
        let (code, events) = run(&CONTEXT_PRESERVED, false);
        assert_eq!(verdict(&events), (1, 0));
        assert_eq!(failed_frames(&events), [(Some("boom".into()), vec![])]);
        assert_exit_matches(code, &events);
    }

    static BAILS_AFTER_A_CAUGHT_FAILURE: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("a", Attempt(false)).ok();
        crate::bail!("gave up on my own")
    });

    #[test]
    fn a_bail_without_a_step_fails_the_host_and_the_caught_step_stays_recovered() {
        let (code, events) = run(&BAILS_AFTER_A_CAUGHT_FAILURE, false);
        assert_eq!(verdict(&events), (1, 1));
        assert_eq!(failed_frames(&events), [(None, vec![])]);
        assert_eq!(failed_in(&events), ["gave up on my own"]);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
    }

    static PANICS_AFTER_A_CAUGHT_FAILURE: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("a", Attempt(false)).ok();
        panic!("boom")
    });

    #[test]
    fn a_panic_fails_the_host_whatever_was_caught_before_it() {
        let (code, events) = run(&PANICS_AFTER_A_CAUGHT_FAILURE, false);
        let (failed, recovered) = verdict(&events);
        assert!(failed >= 1, "{events:#?}");
        assert_eq!(recovered, 1);
        assert_eq!(failed_in(&events), ["panic: boom"]);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
    }

    static HAND_BUILT_STEP_FAILED: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("boom", Attempt(false)).ok();
        Err(crate::Error::msg("made up").context(crate::error::StepFailed::at("boom")))
    });

    /// A `StepFailed` the playbook built itself names a step but no step
    /// id, so it claims none of the recorded failures: the caught one stays
    /// recovered and the host still fails, by the floor of one.
    #[test]
    fn a_hand_built_step_failed_layer_claims_no_recorded_failure() {
        let (code, events) = run(&HAND_BUILT_STEP_FAILED, false);
        assert_eq!(verdict(&events), (1, 1));
        assert_exit_matches(code, &events);
    }

    static OPTIONAL_STEP: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("optional thing", Attempt(false)).ok();
        ctx.step("the rest", Attempt(true))?;
        Ok(())
    });

    /// The issue's third scenario: `.ok()` on an optional step.
    #[test]
    fn an_optional_step_wrapped_in_ok_does_not_fail_the_host() {
        let (code, events) = run(&OPTIONAL_STEP, false);
        assert_eq!(verdict(&events), (0, 1));
        assert!(failed_frames(&events).is_empty(), "{events:#?}");
        assert_eq!(code, ExitCode::SUCCESS);
    }

    /// `grep -q <pattern> <file>`, run for real: it exits 1 when nothing
    /// matches, which `Cmd::run` turns into a `CmdFailed`.
    struct GrepQ(&'static str, &'static str);

    impl crate::op::Op for GrepQ {
        type Output = ();
        type Intent = std::convert::Infallible;
        fn check(&self, sys: &crate::system::System) -> crate::Result<crate::op::Plan<Self>> {
            sys.cmd("grep").args(["-q", self.0, self.1]).run()?;
            Ok(crate::op::Plan::Satisfied(()))
        }
        fn apply(&self, _: &crate::system::System, intent: Self::Intent) -> crate::Result<()> {
            match intent {}
        }
    }

    static GREP_NOT_FOUND: Playbook = playbook!(|ctx, _| {
        // `grep -q` exits 1 for "no match"; the playbook reads that as an
        // answer rather than as a failure.
        let found = ctx
            .step("is the line there", GrepQ("^nope$", "/dev/null"))
            .is_ok();
        if !found {
            ctx.step("add the line", Attempt(true))?;
        }
        Ok(())
    });

    /// The issue's second scenario: a real `grep -q` that finds nothing,
    /// caught as "not found".
    #[test]
    fn grep_caught_as_not_found_does_not_fail_the_host() {
        let (code, events) = run(&GREP_NOT_FOUND, false);
        assert_eq!(verdict(&events), (0, 1));
        assert_eq!(summary_of(&events).ok, 1);
        assert!(failed_frames(&events).is_empty(), "{events:#?}");
        assert_eq!(code, ExitCode::SUCCESS);
    }

    static ACROSS_BLOCKS_AND_IDENTITIES: Playbook = playbook!(|ctx, _| {
        let _ = ctx
            .block("outer", |ctx| {
                let _ = ctx.as_root().step("as root", Attempt(false)).ok();
                ctx.block("inner", |ctx| {
                    ctx.step("deep", Attempt(false))?;
                    Ok(())
                })?;
                Ok(())
            })
            .ok();
        ctx.as_user("nobody").step("as nobody", Attempt(false))?;
        Ok(())
    });

    /// Failures through `as_user` clones and inside blocks land in the one
    /// shared record and are classified like any other: the two caught ones
    /// recovered (one by the playbook, one by `.ok()` on the outer block
    /// after `?` carried it out of the inner one), the escaping one failed.
    #[test]
    fn failures_in_blocks_and_as_user_share_one_classification() {
        let (code, events) = run(&ACROSS_BLOCKS_AND_IDENTITIES, false);
        assert_eq!(verdict(&events), (1, 2));
        assert_eq!(failed_frames(&events), [(Some("as nobody".into()), vec![])]);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
    }

    static ESCAPES_FROM_NESTED_BLOCKS: Playbook = playbook!(|ctx, _| {
        ctx.block("outer", |ctx| {
            ctx.step("before", Attempt(true))?;
            ctx.block("inner", |ctx| ctx.step("boom", Attempt(false)).map(drop))?;
            Ok(())
        })?;
        Ok(())
    });

    /// `?` across two block boundaries and out of the playbook: failed, and
    /// the `Failed` frame names where, so the closing line can print
    /// `FAILED at [outer][inner] `boom``.
    #[test]
    fn a_failure_escaping_nested_blocks_fails_the_host_and_names_its_blocks() {
        let (code, events) = run(&ESCAPES_FROM_NESTED_BLOCKS, false);
        assert_eq!(verdict(&events), (1, 0));
        assert_eq!(
            failed_frames(&events),
            [(Some("boom".into()), vec!["outer".into(), "inner".into()])]
        );
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
    }

    static CAUGHT_BY_THE_BLOCK_BODY: Playbook = playbook!(|ctx, _| {
        ctx.block("outer", |ctx| {
            if ctx
                .block("inner", |ctx| ctx.step("boom", Attempt(false)).map(drop))
                .is_err()
            {
                ctx.step("fallback", Attempt(true))?;
            }
            Ok(())
        })?;
        Ok(())
    });

    /// A block whose `Err` its enclosing block catches: recovered, and the
    /// host succeeds.
    #[test]
    fn a_failure_caught_by_an_enclosing_block_is_recovered() {
        let (code, events) = run(&CAUGHT_BY_THE_BLOCK_BODY, false);
        assert_eq!(verdict(&events), (0, 1));
        assert!(failed_frames(&events).is_empty(), "{events:#?}");
        assert_eq!(code, ExitCode::SUCCESS);
    }

    static READ_INSIDE_AN_OP_CAUGHT: Playbook = playbook!(|ctx, _| {
        let got = ctx.step("read", WouldChange)?;
        let _ = ctx.step("uses it", ReadsInCheck(got)).ok();
        ctx.step("after", WouldChange)?;
        Ok(())
    });

    /// Decision (b)'s step failure follows the same rule: caught, it is
    /// recovered, and the host succeeds.
    #[test]
    fn under_check_a_caught_read_inside_an_op_is_recovered() {
        let (code, events) = run(&READ_INSIDE_AN_OP_CAUGHT, true);
        assert_eq!(verdict(&events), (0, 1));
        assert_eq!(summary_of(&events).would_change, 2);
        assert!(failed_frames(&events).is_empty(), "{events:#?}");
        assert_eq!(code, ExitCode::SUCCESS);
    }

    static CAUGHT_THEN_ABSORBED: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("optional", Attempt(false)).ok();
        let got = ctx.step("read", WouldChange)?;
        let _ = *got;
        ctx.step("never reached", WouldChange)?;
        Ok(())
    });

    /// Absorption is neither failed nor recovered: the caught step is the
    /// only one counted, and the dry run's early end fails nothing.
    #[test]
    fn under_check_an_absorbed_read_is_neither_failed_nor_recovered() {
        let (code, events) = run(&CAUGHT_THEN_ABSORBED, true);
        assert_eq!(verdict(&events), (0, 1));
        assert_eq!(warnings_in(&events), [TOP_LEVEL]);
        assert_eq!(code, ExitCode::SUCCESS);
    }

    thread_local! {
        /// The channel the cancelling ops below cancel: `execute` runs the
        /// playbook on the test's own thread.
        static CHANNEL: std::cell::RefCell<Option<Arc<Channel>>> =
            const { std::cell::RefCell::new(None) };
    }

    fn cancel_this_run() {
        CHANNEL.with(|c| {
            c.borrow()
                .as_ref()
                .expect("the test set the channel")
                .cancel("cancelled by the test")
        });
    }

    /// Cancels the run from inside `check`, as a `Cancel` frame arriving
    /// mid-step does, then plans a change: the step is stopped before
    /// `apply`, "not applied".
    struct CancelledMidStep;

    impl crate::op::Op for CancelledMidStep {
        type Output = u32;
        type Intent = Go;
        fn check(&self, _: &crate::system::System) -> crate::Result<crate::op::Plan<Self>> {
            cancel_this_run();
            Ok(crate::op::Plan::Change(Go))
        }
        fn apply(&self, _: &crate::system::System, Go: Go) -> crate::Result<u32> {
            Ok(1)
        }
    }

    /// An op that gives up because the run was cancelled under it.
    struct GivesUpOnCancel;

    impl crate::op::Op for GivesUpOnCancel {
        type Output = ();
        type Intent = std::convert::Infallible;
        fn check(&self, _: &crate::system::System) -> crate::Result<crate::op::Plan<Self>> {
            cancel_this_run();
            Err(crate::Error::msg("stopped waiting: the run was cancelled"))
        }
        fn apply(&self, _: &crate::system::System, intent: Self::Intent) -> crate::Result<()> {
            match intent {}
        }
    }

    fn cancellable() -> Arc<Channel> {
        let channel = Channel::detached();
        CHANNEL.with(|c| *c.borrow_mut() = Some(channel.clone()));
        channel
    }

    static SWALLOWS_EVERYTHING: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("a", Attempt(false)).ok();
        let _ = ctx.step("b", CancelledMidStep).ok();
        let _ = ctx.step("c", Attempt(true)).ok();
        Ok(())
    });

    /// A cancelled run fails the host even when the playbook swallows every
    /// error and returns `Ok`: the step the cancellation stopped is failed,
    /// never recovered, and the one caught before it stays recovered. The
    /// `Failed` frame says why, since no error left the playbook to say it.
    #[test]
    fn a_cancelled_run_fails_the_host_even_when_every_error_is_swallowed() {
        let (code, events) = run_on(&SWALLOWS_EVERYTHING, false, cancellable());
        assert_eq!(verdict(&events), (1, 1));
        assert_eq!(failed_in(&events), ["cancelled: cancelled by the test"]);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
        // `c` was refused before it started: no step events, no count.
        assert!(!events.iter().any(|e| matches!(
            e,
            Event::StepStarted { name, .. } if name == "c"
        )));
    }

    static SWALLOWS_AN_OP_THAT_GAVE_UP: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("waits", GivesUpOnCancel).ok();
        Ok(())
    });

    /// A step whose own op gave up because of the cancellation failed
    /// because the run was cancelled, and is never recovered.
    #[test]
    fn a_step_that_failed_once_the_run_was_cancelled_is_failed_even_when_caught() {
        let (code, events) = run_on(&SWALLOWS_AN_OP_THAT_GAVE_UP, false, cancellable());
        assert_eq!(verdict(&events), (1, 0));
        assert_exit_matches(code, &events);
    }

    static ONLY_REFUSED_STEPS: Playbook = playbook!(|ctx, _| {
        let _ = ctx.step("a", Attempt(true)).ok();
        Ok(())
    });

    /// Cancelled before any step started: nothing is recorded, and the host
    /// fails by the floor of one.
    #[test]
    fn a_run_cancelled_before_any_step_still_fails_the_host() {
        let channel = cancellable();
        channel.cancel("cancelled by the test");
        let (code, events) = run_on(&ONLY_REFUSED_STEPS, false, channel);
        assert_eq!(verdict(&events), (1, 0));
        assert_eq!(failed_in(&events), ["cancelled: cancelled by the test"]);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
    }

    static CANCELLED_AND_ESCAPED: Playbook = playbook!(|ctx, _| {
        ctx.step("b", CancelledMidStep)?;
        Ok(())
    });

    /// A cancelled step whose error leaves the playbook is reported once:
    /// the frame for the escaped error, no second one for the cancellation.
    #[test]
    fn a_cancelled_step_that_escapes_is_reported_once() {
        let (code, events) = run_on(&CANCELLED_AND_ESCAPED, false, cancellable());
        assert_eq!(verdict(&events), (1, 0));
        assert_eq!(failed_frames(&events), [(Some("b".into()), vec![])]);
        assert_eq!(code, ExitCode::from(EXIT_FAILED));
    }

    // ---- classify: tier 1 ----

    fn step(id: u32) -> FailedStep {
        FailedStep {
            id,
            cancelled: false,
        }
    }

    #[test]
    fn classify_is_by_id_and_floors_a_failed_host_at_one() {
        let five = [step(1), step(2), step(3), step(4), step(5)];
        assert_eq!(classify(&five, Some(5), true), (1, 4));
        assert_eq!(classify(&five, None, false), (0, 5));
        assert_eq!(classify(&five, None, true), (1, 5), "bail! after catching");
        assert_eq!(
            classify(&five, Some(99), true),
            (1, 5),
            "an id no step drew"
        );
        assert_eq!(classify(&[], None, true), (1, 0), "a panic with no step");
        assert_eq!(classify(&[], None, false), (0, 0));
        let cancelled = [
            step(1),
            FailedStep {
                id: 2,
                cancelled: true,
            },
        ];
        assert_eq!(classify(&cancelled, None, true), (1, 1));
        assert_eq!(classify(&cancelled, Some(2), true), (1, 1), "counted once");
        assert_eq!(classify(&cancelled, Some(1), true), (2, 0));
    }
}
