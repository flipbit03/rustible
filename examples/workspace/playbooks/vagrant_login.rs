//! A playbook that logs in as another account than the inventory's.
//!
//! `make vm-test` runs it after `vagrant.rs`, which creates `rustible-login`
//! with the keys the box's own login accepts. The inventory says
//! `ssh_user="vagrant"`; the attribute below replaces it for this playbook,
//! so everything here runs as `rustible-login` through a real ssh login,
//! which no container can show. Run it twice: `changed`, then `ok`.

use rustible::prelude::*;
use rustible_std::file;

/// The account `vagrant.rs` creates.
const ACCOUNT: &str = "rustible-login";

#[rustible::playbook(hosts = "vagrant", ssh_user = "rustible-login")]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ensure!(f.user == ACCOUNT, "logged in as `{}`, not `{ACCOUNT}`: the playbook's ssh_user did not reach ssh", f.user);
    ensure!(!f.is_root, "running as root, so the login was not `{ACCOUNT}`");

    // Written as the login user, into its own home: no escalation anywhere.
    ctx.step("marker in the login's home", file::Copy::from_str("written by rustible as rustible-login\n").to(format!("/home/{ACCOUNT}/rustible-login-marker")).mode(0o644))?;
    Ok(())
}
