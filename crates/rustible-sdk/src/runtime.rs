//! Stand-in for what the `#[rustible::playbook]` macro will generate: parse the
//! run options (from flags locally, or from the `Start` frame when driven by
//! the orchestrator), gather facts, build the context, run `main`, report, exit.

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use crate::ctx::{Ctx, HostInfo};
use crate::error::Result;
use crate::event::{Event, EventSink, JsonLines, Pretty};
use crate::protocol::{self, Down, FrameSink, Up};
use crate::system::System;

pub struct RunOptions {
    pub check_mode: bool,
    pub verbosity: u8,
    /// Emit JSON lines instead of the pretty renderer (local mode only).
    pub json: bool,
    /// Driven by an orchestrator: read `Start` from stdin, write frames to stdout.
    pub remote: bool,
    pub host: HostInfo,
    pub playbook_name: String,
}

impl RunOptions {
    /// Minimal flag parsing for the spike: --check, -v/-vv, --json, --remote.
    pub fn from_args() -> Self {
        let mut o = RunOptions {
            check_mode: false,
            verbosity: 0,
            json: false,
            remote: false,
            host: HostInfo {
                name: "local".into(),
                groups: vec![],
            },
            playbook_name: std::env::args()
                .next()
                .and_then(|a| {
                    std::path::Path::new(&a)
                        .file_name()
                        .map(|f| f.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
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

pub fn run(mut opts: RunOptions, main: impl FnOnce(&mut Ctx) -> Result<()>) -> ExitCode {
    let sink: Arc<dyn EventSink> = if opts.remote {
        // Wait for the orchestrator's Start frame. Everything about this run
        // comes from it, never from the binary or the target's disk.
        let start: Option<Down> = match protocol::read_frame(&mut std::io::stdin().lock()) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("rustible: bad Start frame: {e}");
                return ExitCode::from(3);
            }
        };
        match start {
            Some(Down::Start {
                host,
                check_mode,
                verbosity,
                ..
            }) => {
                opts.host = host;
                opts.check_mode = check_mode;
                opts.verbosity = verbosity;
            }
            other => {
                eprintln!("rustible: expected Start frame, got {other:?}");
                return ExitCode::from(3);
            }
        }
        let sink = Arc::new(FrameSink(Mutex::new(std::io::stdout())));
        let _ = protocol::write_frame(
            &mut *sink.0.lock().unwrap(),
            &Up::Hello {
                protocol: protocol::PROTOCOL_VERSION,
                playbook: opts.playbook_name.clone(),
            },
        );
        sink
    } else if opts.json {
        Arc::new(JsonLines(Mutex::new(std::io::stdout())))
    } else {
        Arc::new(Pretty::new(
            std::io::stdout(),
            opts.host.name.clone(),
            opts.verbosity,
        ))
    };

    let sys = System::local(opts.check_mode, sink.clone());
    sink.emit(Event::Facts(sys.facts().clone()));

    let mut ctx = Ctx::new(sys, opts.host);

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| main(&mut ctx)));
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
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}
