//! For the cancel test: a 30 s step, then a step that must never run once
//! the orchestrator sent `Cancel` (vision doc 5.5, 16.10).

use rustible::prelude::*;
use rustible_std::shell;

#[rustible::playbook(hosts = "lab")]
fn main(ctx: &mut Ctx) -> Result<()> {
    ctx.step("sleep 30 s", shell::Command::new("sleep").arg("30"))?;
    ctx.step(
        "marker after the sleep",
        shell::Command::new("touch").arg("/tmp/rustible-m5-next-step-ran"),
    )?;
    Ok(())
}
