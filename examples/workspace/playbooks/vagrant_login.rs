//! A playbook that logs in as another account than the inventory's.
//!
//! `make vm-test` runs it after `vagrant.rs`, which creates `rustible-login`
//! with the keys the box's own login accepts. The inventory says
//! `ssh_user="vagrant"`; the attribute below replaces it for this playbook,
//! so everything here runs as `rustible-login` through a real ssh login,
//! which no container can show. Run it twice: `changed`, then `ok`.
//!
//! It also fails on purpose, every run: `rustible-login` has no sudo, so a
//! step as root cannot escalate, and the failure has to say that the login
//! came from this attribute and what it replaced. The playbook catches it,
//! so the recap counts it `recovered`, once per host per run, and
//! `vm-test.sh` expects exactly that.

use rustible::prelude::*;
use rustible_std::file;

/// The account `vagrant.rs` creates.
const ACCOUNT: &str = "rustible-login";

/// What a failed escalation says about where the login came from
/// (`LoginOverride::note`): the attribute's account, and the inventory's,
/// which the Vagrantfile writes on each `host` node.
const NOTE: &str = "the login user `rustible-login` comes from the playbook's `ssh_user` attribute, which overrides the inventory's `vagrant` (from host)";

#[rustible::playbook(hosts = "vagrant", ssh_user = "rustible-login")]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ensure!(f.user == ACCOUNT, "logged in as `{}`, not `{ACCOUNT}`: the playbook's ssh_user did not reach ssh", f.user);
    ensure!(!f.is_root, "running as root, so the login was not `{ACCOUNT}`");

    // Written as the login user, into its own home: no escalation anywhere.
    ctx.step("marker in the login's home", file::Copy::from_str("written by rustible as rustible-login\n").to(format!("/home/{ACCOUNT}/rustible-login-marker")).mode(0o644))?;

    // Escalation runs from the login, and this one has no sudo, so the
    // helper's `sudo -n` is refused before the op reads anything. The op only
    // asks that `/root` exists, so were sudo ever to let it through, nothing
    // would change before the `bail!` below says so.
    match ctx.as_root().step("root is out of reach", file::Directory::at("/root")) {
        Ok(_) => bail!("`{ACCOUNT}` escalated to root, but the Vagrantfile gives it no sudo: the escalation this step exists to see fail did not"),
        Err(e) => {
            let chain = e.chain();
            ensure!(chain.matches(NOTE).count() == 1, "the failed escalation should name where the login came from, exactly once: `{NOTE}`; it said: {chain}");
            ctx.log("escalating as `rustible-login` failed, naming the playbook's ssh_user");
        }
    }
    Ok(())
}
