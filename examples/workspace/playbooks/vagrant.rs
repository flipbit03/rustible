//! The playbook the Vagrant spike proves itself with.
//!
//! It targets the `vagrant` group, whose hosts `dev/vagrant/Vagrantfile`
//! brings up, and exercises the four things a virtual machine buys over the
//! container harness in `crates/rustible-std/tests/`: the real SSH transport,
//! escalation through a real `sudo`, a live `/proc/sys` write, and a full
//! init system. Run it twice: every step reports `changed` and then `ok`.
//!
//! It also makes the account `vagrant_login.rs` logs in as, with the keys
//! the box's own login accepts, so that playbook's `ssh_user` can reach it
//! with the inventory's key.

use std::time::Duration;

use rustible::prelude::*;
use rustible_std::ssh::authorized_keys;
use rustible_std::{apt, file, sysctl, systemd, user};

/// What the marker file says, so a second run has something to compare.
const MARKER: &str = "written by rustible from dev/vagrant\n";

/// The second account, which `vagrant_login.rs` names in its `ssh_user`.
const LOGIN_ACCOUNT: &str = "rustible-login";

/// The keys the box's login accepts; the inventory's `-i` is one of them.
const BOX_KEYS: &str = "/home/vagrant/.ssh/authorized_keys";

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
    ensure!(f.has_pm(&Pm::Apt), "this playbook needs apt; {} has {:?}", f.hostname, f.package_managers);
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

    // The account a playbook's `ssh_user` logs in as, reachable with the
    // same key as the box's own login.
    let keys = ctx.sys().read_to_string(BOX_KEYS)?;
    let keys: Vec<&str> = keys.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    ensure!(!keys.is_empty(), "{BOX_KEYS} has no keys to give {LOGIN_ACCOUNT}");
    let account = ctx.step(format!("{LOGIN_ACCOUNT} account"), user::Present::new(LOGIN_ACCOUNT).create_home(true))?;
    ctx.step(format!("{LOGIN_ACCOUNT} keys"), authorized_keys::Present::for_user(&account).keys(keys))?;

    Ok(())
}
