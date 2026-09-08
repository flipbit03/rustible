//! The process entry point every playbook binary shares (vision doc 5.5, 9).
//!
//! A workspace's generated `src/main.rs` is two lines: include the registry
//! the build script wrote, then `rustible::runtime::main(PLAYBOOKS)`. This
//! module implements the four modes of a playbook binary:
//!
//! - `--describe`: print metadata and vars schema for every playbook as JSON.
//! - `--remote`: driven by an orchestrator; read the `Start` frame on stdin,
//!   write `Hello` and event frames on stdout.
//! - `--helper`: serve `Backend` primitives to a sibling process (M5).
//! - a plain local run: `<name> [--check] [-v|-vv] [--json] [--var k=v]...`.

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::ctx::{Ctx, HostInfo};
use crate::error::Result;
use crate::event::{Event, EventSink, JsonLines, Pretty};
use crate::protocol::{self, Down, FrameSink, Up};
use crate::registry::{Named, describe_all};
use crate::system::System;

/// Exit codes: 0 ok, 2 a step or the playbook failed, 3 the binary was
/// misused (bad flags, unknown playbook, bad `Start` frame).
const EXIT_FAILED: u8 = 2;
const EXIT_USAGE: u8 = 3;

/// The generated `src/main.rs` calls this.
pub fn main(playbooks: &[Named]) -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--describe") => {
            println!(
                "{}",
                serde_json::to_string_pretty(&describe_all(playbooks)).expect("json")
            );
            ExitCode::SUCCESS
        }
        Some("--remote") => remote(playbooks),
        Some("--helper") => {
            eprintln!("rustible: helper mode is not implemented until M5");
            ExitCode::from(EXIT_USAGE)
        }
        _ => local(playbooks, &args),
    }
}

fn find<'a>(playbooks: &'a [Named], name: &str) -> Option<&'a Named> {
    playbooks.iter().find(|p| p.name == name)
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
        playbook,
        host,
        vars,
        check_mode,
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
    execute(named, host, vars, check_mode, sink)
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
            other => name = Some(other.to_string()),
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
    let sink: Arc<dyn EventSink> = if json {
        Arc::new(JsonLines(Mutex::new(std::io::stdout())))
    } else {
        Arc::new(Pretty::new(std::io::stdout(), "local", verbosity))
    };
    execute(
        named,
        HostInfo::local(),
        Value::Object(vars),
        check_mode,
        sink,
    )
}

fn print_usage(playbooks: &[Named]) {
    eprintln!("usage: <binary> <playbook> [--check] [-v|-vv] [--json] [--var key=value]...");
    eprintln!("       <binary> --describe");
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
fn execute(
    named: &Named,
    host: HostInfo,
    vars: Value,
    check_mode: bool,
    sink: Arc<dyn EventSink>,
) -> ExitCode {
    let sys = System::local(check_mode, sink.clone());
    sink.emit(Event::Facts(sys.facts().clone()));
    let mut ctx = Ctx::new(sys, host);
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
    sink.emit(Event::Finished(summary.clone()));
    if summary.failed > 0 {
        ExitCode::from(EXIT_FAILED)
    } else {
        ExitCode::SUCCESS
    }
}

// ---- compatibility with the spike playbook (deleted in M3) ----

/// Options for [`run`], the pre-macro entry point kept for `spike-playbook`.
pub struct RunOptions {
    pub check_mode: bool,
    pub verbosity: u8,
    pub json: bool,
    pub remote: bool,
    pub host: HostInfo,
    pub playbook_name: String,
}

impl RunOptions {
    pub fn from_args() -> Self {
        let mut o = RunOptions {
            check_mode: false,
            verbosity: 0,
            json: false,
            remote: false,
            host: HostInfo::local(),
            playbook_name: "spike".into(),
        };
        for a in std::env::args().skip(1) {
            match a.as_str() {
                "--check" => o.check_mode = true,
                "-v" => o.verbosity = 1,
                "-vv" => o.verbosity = 2,
                "--json" => o.json = true,
                "--remote" => o.remote = true,
                _ => {}
            }
        }
        o
    }
}

/// Pre-macro entry point. Wraps a plain function as a one-playbook registry.
pub fn run(opts: RunOptions, main_fn: impl FnOnce(&mut Ctx) -> Result<()> + 'static) -> ExitCode {
    // The registry wants a fn pointer; stash the closure in a thread-local.
    type MainFn = Box<dyn FnOnce(&mut Ctx) -> Result<()>>;
    thread_local! {
        static MAIN: std::cell::RefCell<Option<MainFn>> = const { std::cell::RefCell::new(None) };
    }
    MAIN.with(|m| *m.borrow_mut() = Some(Box::new(main_fn)));
    fn entry(ctx: &mut Ctx, _: Value) -> Result<()> {
        let f = MAIN.with(|m| m.borrow_mut().take()).expect("main set");
        f(ctx)
    }
    static PB: crate::registry::Playbook = crate::registry::Playbook {
        hosts: "local",
        escalate: false,
        schema: crate::vars::no_schema,
        entry,
    };
    let named = Named {
        name: "spike",
        playbook: &PB,
    };
    if opts.remote {
        return remote(std::slice::from_ref(&named));
    }
    let sink: Arc<dyn EventSink> = if opts.json {
        Arc::new(JsonLines(Mutex::new(std::io::stdout())))
    } else {
        Arc::new(Pretty::new(
            std::io::stdout(),
            opts.host.name.clone(),
            opts.verbosity,
        ))
    };
    execute(&named, opts.host, Value::Null, opts.check_mode, sink)
}
