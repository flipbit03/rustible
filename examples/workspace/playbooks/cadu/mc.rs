//! On an apt-based host, ensure a package (Midnight Commander by default) is
//! installed. Needs root, so the attribute says `escalate = true`.

use rustible::prelude::*;
use rustible_std::apt;

mod helpers; // sibling file: playbooks/cadu/helpers.rs (vision doc section 9)

#[rustible::vars]
struct Vars {
    /// The apt package to ensure.
    package: String,
    #[default = false]
    update_cache: bool,
}

#[rustible::playbook(hosts = "lab", vars = Vars, escalate = true)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
    let f = ctx.facts();
    ensure!(f.package_manager == Pm::Apt, "this playbook needs apt; {} uses {:?}", f.hostname, f.package_manager);
    ensure!(f.is_root, "this playbook needs root (escalate)");

    let name = format!("{} present", vars.package);
    let pkg = ctx.step(name, apt::Present::new([vars.package.as_str()]).update_cache(vars.update_cache))?;
    ctx.log(helpers::describe(&pkg));
    Ok(())
}
