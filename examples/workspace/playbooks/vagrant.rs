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
//!
//! And it steps into two unprivileged accounts with `ctx.as_user`, which
//! cannot run the login's copy of this binary: one with a home, where the
//! helper streams its own copy into the account's cache (cold on the first
//! run, cached on the second), and a system account with none, which gets a
//! private per-run copy in the temp directory every time.
//! `vagrant_escalate_user.rs` then launches a whole playbook as each of them.

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

/// A system account with no home, for an `as_user` helper that has nowhere
/// to cache its copy of the binary.
const NOHOME_ACCOUNT: &str = "rustible-nohome";

/// Somewhere `NOHOME_ACCOUNT` may write, since it has no home.
const NOHOME_DIR: &str = "/var/lib/rustible-nohome";

/// What the `as_user` steps write, each as its own account.
const AS_USER_MARKER: &str = "written by rustible's as_user helper\n";

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

    // `as_user` to an account that cannot read the login's home. Debian
    // makes homes 0755, which would hide that; Ubuntu makes them 0750, as
    // this one is made. The marker is 0600 and written by the helper, so its
    // owner is the proof of who wrote it.
    ctx.step("login home private", file::Attrs::at("/home/vagrant").mode(0o750))?;
    ctx.as_user(LOGIN_ACCOUNT).step(format!("marker as {LOGIN_ACCOUNT}"), file::Copy::from_str(AS_USER_MARKER).to(format!("/home/{LOGIN_ACCOUNT}/as-user-marker")).mode(0o600))?;

    // An account with no home at all: its helper runs from a private copy
    // in the temp directory, which removes itself.
    let nohome = ctx.step(format!("{NOHOME_ACCOUNT} account"), user::Present::new(NOHOME_ACCOUNT).system(true).create_home(false).home("/nonexistent"))?;
    ctx.step(NOHOME_DIR, file::Directory::at(NOHOME_DIR).owner(nohome.uid, nohome.gid).mode(0o700))?;
    ctx.as_user(NOHOME_ACCOUNT).step(format!("marker as {NOHOME_ACCOUNT}"), file::Copy::from_str(AS_USER_MARKER).to(format!("{NOHOME_DIR}/as-user-marker")).mode(0o600))?;
    // That copy deleted itself and its directory as the helper started.
    let left: Vec<_> = ctx.sys().read_dir("/tmp")?.into_iter().filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("rustible-"))).collect();
    ensure!(left.is_empty(), "the {NOHOME_ACCOUNT} helper's temp copy was left behind: {left:?}");

    Ok(())
}
