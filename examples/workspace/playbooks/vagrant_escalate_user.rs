//! A playbook launched as an unprivileged `escalate_user`.
//!
//! `make vm-test` runs it after `vagrant.rs`, which creates the two accounts
//! it escalates to. The Vagrant inventory names every guest twice more, in
//! the `vagrant-escalate-user` group: with `escalate_user="rustible-login"`,
//! an account with a home, and with `escalate_user="rustible-nohome"`, a
//! system account without one. The login is the box's own, whose home
//! `vagrant.rs` made private, so neither account can run the login's copy of
//! this binary: the orchestrator streams it into the account's cache, or into
//! a private temp directory, and launches it from there. Run it twice:
//! `changed`, then `ok`.

use rustible::prelude::*;
use rustible_std::file;

/// The account with a home, which keeps a cached copy.
const LOGIN_ACCOUNT: &str = "rustible-login";

/// The system account without one, which runs from a temp copy every time.
const NOHOME_ACCOUNT: &str = "rustible-nohome";

/// Where `vagrant.rs` lets `NOHOME_ACCOUNT` write.
const NOHOME_DIR: &str = "/var/lib/rustible-nohome";

#[rustible::playbook(hosts = "vagrant-escalate-user", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    let user = ctx.facts().user.clone();
    ensure!(!ctx.facts().is_root, "running as root: the host's escalate_user did not reach the launch");
    let dir = match user.as_str() {
        LOGIN_ACCOUNT => format!("/home/{LOGIN_ACCOUNT}"),
        NOHOME_ACCOUNT => NOHOME_DIR.to_string(),
        other => bail!("running as `{other}`, not an escalate_user the Vagrant inventory names"),
    };

    // Written by the launched binary itself, so its owner is who ran it.
    ctx.step(format!("marker as {user}"), file::Copy::from_str("written by rustible as its escalate_user\n").to(format!("{dir}/escalate-user-marker")).mode(0o600))?;
    Ok(())
}
