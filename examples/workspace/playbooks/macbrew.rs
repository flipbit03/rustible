//! Homebrew on a mac, the way `vagrant.rs` drives apt on a Debian guest.
//!
//! Deliberately **not** escalated: Homebrew refuses to run as root, so this
//! is the one playbook here whose ops need the login user. Run it twice; the
//! second run reports `ok`.
//!
//!     rustible playbook run macbrew
//!     rustible playbook run macbrew --var present=false

use rustible::prelude::*;
use rustible_std::brew;

#[rustible::vars]
struct Vars {
    /// A small curses game, chosen because it is quick to build and has no
    /// service to leave running.
    #[default = "ninvaders"]
    package: String,
    /// False removes it again, which is how the `Absent` path is exercised.
    #[default = true]
    present: bool,
}

#[rustible::playbook(hosts = "mac", vars = Vars)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
    let f = ctx.facts();
    ensure!(
        f.has_pm(&Pm::Brew),
        "this playbook needs Homebrew; {} has {:?}",
        f.hostname,
        f.package_managers
    );
    ensure!(
        !f.is_root,
        "run this playbook unescalated: Homebrew refuses to run as root"
    );
    ctx.log(format!(
        "{} is {:?} {} on {:?}, {} cpus, {} MB, pm {:?}, init {:?}",
        f.hostname, f.distro, f.distro_version, f.arch, f.cpus, f.memory_mb, f.package_managers, f.init
    ));

    if vars.present {
        let out = ctx.step(
            format!("{} present", vars.package),
            brew::Present::new([vars.package.as_str()]),
        )?;
        if !ctx.check_mode() {
            ctx.log(format!(
                "installed {:?}, already there {:?}",
                out.installed, out.already_present
            ));
        }
    } else {
        let out = ctx.step(
            format!("{} absent", vars.package),
            brew::Absent::new([vars.package.as_str()]),
        )?;
        if !ctx.check_mode() {
            ctx.log(format!("removed {:?}", out.removed));
        }
    }
    Ok(())
}
