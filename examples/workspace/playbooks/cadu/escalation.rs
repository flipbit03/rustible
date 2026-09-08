//! Per-step escalation without `escalate = true` (vision doc 11.3): the
//! binary runs as the login user; one step runs as root through the helper
//! (`sudo -n -u root <this binary> --helper`), so it can write under `/etc`.

use rustible::prelude::*;
use rustible_std::file;

#[rustible::playbook(hosts = "lab")]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ensure!(!f.is_root, "run this playbook unescalated; it escalates per step");
    let line = format!("rustible m5 escalation test on {}", f.hostname);

    ctx.as_root().step(
        "marker line present",
        file::Line::in_path("/etc/rustible-m5-test")
            .create(true)
            .matching("^rustible m5 ")
            .set(line),
    )?;

    // Reads and commands through an escalated `sys` go through the same helper.
    let root = ctx.as_root();
    let who = root.sys().cmd("id").arg("-un").run()?;
    let me = ctx.sys().cmd("id").arg("-un").run()?;
    ctx.log(format!(
        "commands run as {} here and as {} through the helper",
        me.stdout_str().trim(),
        who.stdout_str().trim()
    ));
    // In `--check` the step above changed nothing, so there is nothing to
    // read back; the point of that run is that the helper refused the write.
    if ctx.check_mode() {
        ctx.log("check mode: the marker was not written, nothing to read back");
    } else {
        let content = root.sys().read_to_string("/etc/rustible-m5-test")?;
        ctx.log(format!("/etc/rustible-m5-test: {}", content.trim()));
    }
    Ok(())
}
