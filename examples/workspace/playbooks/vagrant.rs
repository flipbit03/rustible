//! The playbook the Vagrant spike proves itself with.
//!
//! It targets the `vagrant` group, whose hosts `dev/vagrant/Vagrantfile`
//! brings up, and exercises the four things a virtual machine buys over the
//! container harness in `crates/rustible-std/tests/`: the real SSH transport,
//! escalation through a real `sudo`, a live `/proc/sys` write, and a full
//! init system. Run it twice: every step reports `changed` and then `ok`.

use std::time::Duration;

use rustible::prelude::*;
use rustible_std::{apt, file, sysctl, systemd};

/// What the marker file says, so a second run has something to compare.
const MARKER: &str = "written by rustible from dev/vagrant\n";

#[rustible::vars]
struct Vars {
    /// The apt package to ensure; something small with no service.
    #[default = "mc"]
    package: String,
    /// The value `vm.swappiness` is driven to. The box ships 60.
    #[default = 61]
    swappiness: u8,
}

#[rustible::playbook(hosts = "vagrant", vars = Vars, escalate = true)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
    let f = ctx.facts();
    ensure!(f.package_manager == Pm::Apt, "this playbook needs apt; {} uses {:?}", f.hostname, f.package_manager);
    ensure!(f.is_root, "this playbook needs root, which means escalation worked");
    ctx.log(format!("{} is {:?} {} on {:?}, {} cpus", f.hostname, f.distro, f.distro_version, f.arch, f.cpus));

    // apt, over the real transport rather than inside a container.
    let pkg = ctx.step(format!("{} present", vars.package), apt::Present::new([vars.package.as_str()]).update_cache(Duration::from_secs(3600)))?;
    ctx.log(format!("{} packages installed", pkg.installed.len()));

    // A file written as root: the escalation is real sudo, not a container's
    // uid 0.
    ctx.step("marker file", file::Copy::from_str(MARKER).to("/etc/rustible-vagrant").mode(0o644).owner(0, 0))?;

    // A live /proc/sys write, which a container cannot do to the host kernel.
    ctx.step("swappiness", sysctl::Present::new("vm.swappiness", vars.swappiness.to_string()))?;

    // A full init system, queried through systemctl.
    ctx.step("time sync enabled", systemd::Enabled::new("systemd-timesyncd").now(true))?;

    Ok(())
}
