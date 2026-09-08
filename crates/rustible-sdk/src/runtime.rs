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
use crate::ctx::{Ctx, HostInfo};
use crate::event::{Compact, Event, EventSink, JsonLines, SharedSink};
use crate::protocol::{self, Down, FrameSink, Up};
use crate::registry::{Named, describe_all};
use crate::secret::Secret;
use crate::stream::WorkspaceFiles;
use crate::system::System;
use crate::vars;

/// Exit codes: 0 ok, 2 a step or the playbook failed, 3 the binary was
/// misused (bad flags, unknown playbook, bad `Start` frame).
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
    pub host: String,
    pub vars: Value,
}

/// One problem with one var on one host, as `--check-vars` reports it.
/// `var` is empty when the problem cannot be pinned to a var (a serde
/// error the schema walk did not anticipate).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VarProblem {
    pub var: String,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    /// An undeclared var: the bag is shared by every playbook targeting the
    /// host (vision 10.3), so this never fails a run.
    Warning,
}

/// `--check-vars` output for one host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostCheck {
    pub host: String,
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
    let mut ctx = Ctx::for_run(sys, host, channel, run_id);
    let entry = named.playbook.entry;

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| entry(&mut ctx, vars)));
    let failed = match outcome {
        Ok(Ok(())) => false,
        Ok(Err(e)) => {
            sink.emit(Event::failed(None, &e));
            true
        }
        Err(payload) => {
            let msg = payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            sink.emit(Event::Failed {
                step: None,
                error: format!("panic: {msg}"),
                cmd: None,
            });
            true
        }
    };

    let mut summary = ctx.summary();
    if failed && summary.failed == 0 {
        summary.failed += 1;
    }
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
}
