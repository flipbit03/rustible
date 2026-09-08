//! For the cancel test: a streamed file, then a 30 s step, then a step that
//! must never run once the orchestrator sent `Cancel` (vision doc 5.5, 16.10).
//!
//! The `local_file` at the top is what makes this cover the interaction M5
//! owns: a binary killed after ignoring `Cancel` runs no destructor, so its
//! run temp directory and the file in it are the orchestrator's to remove.
//! `hosts.kdl` is committed, so this playbook needs no generated fixture.

use rustible::prelude::*;
use rustible_std::shell;

#[rustible::playbook(hosts = "lab")]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.local_file("hosts.kdl")?;
    ctx.log(format!("streamed hosts.kdl to {}", f.display()));
    ctx.step("sleep 30 s", shell::Command::new("sleep").arg("30"))?;
    ctx.step(
        "marker after the sleep",
        shell::Command::new("touch").arg("/tmp/rustible-m5-next-step-ran"),
    )?;
    Ok(())
}
