//! The smallest playbook: no vars, no escalation, logs the facts.

use rustible::prelude::*;

#[rustible::playbook(hosts = "local")]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ctx.log(workspace::greeting(&f.hostname));
    ctx.log(format!("{:?} {} on {:?}, {} cpus, {} MB, pm {:?}", f.distro, f.distro_version, f.arch, f.cpus, f.memory_mb, f.package_manager));
    if f.is_root {
        ctx.warn("running as root; hello does not need it");
    }
    Ok(())
}
