//! Stand-in for what the `#[rustible::playbook]` macro will generate: parse the
//! run options, gather facts, build the context, run `main`, report, exit.

use std::process::ExitCode;
use std::sync::Arc;

use crate::ctx::{Ctx, HostInfo};
use crate::error::Result;
use crate::event::{Event, EventSink, JsonLines, Pretty};
use crate::system::System;

pub struct RunOptions {
    pub check_mode: bool,
    pub verbosity: u8,
    /// Emit JSON lines instead of the pretty renderer.
    pub json: bool,
    pub host_name: String,
}

impl RunOptions {
    /// Minimal flag parsing for the spike: --check, -v/-vv, --json.
    pub fn from_args() -> Self {
        let mut o = RunOptions {
            check_mode: false,
            verbosity: 0,
            json: false,
            host_name: "local".into(),
        };
        for a in std::env::args().skip(1) {
            match a.as_str() {
                "--check" => o.check_mode = true,
                "-v" => o.verbosity = 1,
                "-vv" => o.verbosity = 2,
                "--json" => o.json = true,
                _ => {}
            }
        }
        o
    }
}

pub fn run(opts: RunOptions, main: impl FnOnce(&mut Ctx) -> Result<()>) -> ExitCode {
    let sink: Arc<dyn EventSink> = if opts.json {
        Arc::new(JsonLines(std::sync::Mutex::new(std::io::stdout())))
    } else {
        Arc::new(Pretty::new(
            std::io::stdout(),
            opts.host_name.clone(),
            opts.verbosity,
        ))
    };

    let sys = System::local(opts.check_mode, sink.clone());
    sink.emit(Event::Facts(sys.facts().clone()));

    let mut ctx = Ctx::new(
        sys,
        HostInfo {
            name: opts.host_name,
            groups: vec![],
        },
    );

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| main(&mut ctx)));
    let failed = match outcome {
        Ok(Ok(())) => false,
        Ok(Err(e)) => {
            sink.emit(Event::Failed {
                step: None,
                error: e.to_string(),
            });
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
