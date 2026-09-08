//! Playbook: on an apt-based host, ensure Midnight Commander is installed.
//! Needs root (escalate). Run locally with `sudo target/debug/mc mc`. The
//! orchestrator drives `examples/workspace` now, not this crate (M1 item 7).

use rustible_sdk::prelude::*;
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

// The spike predates the `#[rustible::playbook]` macro: it registers by hand.
static PB: rustible_sdk::registry::Playbook = rustible_sdk::registry::Playbook {
    hosts: "local",
    escalate: false,
    schema: rustible_sdk::vars::no_schema,
    entry: |ctx, _| playbook(ctx),
};

fn main() -> std::process::ExitCode {
    rustible_sdk::runtime::main(&[rustible_sdk::registry::Named {
        name: "mc",
        playbook: &PB,
    }])
}
