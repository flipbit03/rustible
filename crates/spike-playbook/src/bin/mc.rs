//! Playbook: on an apt-based host, ensure Midnight Commander is installed.
//! Needs root (escalate). Run locally with `sudo target/debug/mc`, or through
//! the orchestrator with `--escalate`.

use rustible_sdk::prelude::*;
use rustible_sdk::runtime::{RunOptions, run};
use rustible_std::apt;

fn playbook(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    if f.package_manager != Pm::Apt {
        bail!(
            "this playbook needs apt; {} uses {:?}",
            f.hostname,
            f.package_manager
        );
    }
    if !f.is_root {
        bail!("this playbook needs root (run with escalate)");
    }

    let mc = ctx.step("Midnight Commander present", apt::Present::new(["mc"]))?;

    if mc.changed && mc.predicted {
        ctx.log("would install mc");
    } else if mc.changed {
        ctx.log(format!("installed mc {}", mc.installed[0].version));
    } else {
        ctx.log(format!(
            "mc {} was already there",
            mc.already_present[0].version
        ));
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    run(RunOptions::from_args(), playbook)
}
