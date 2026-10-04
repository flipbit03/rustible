//! A playbook whose launch must be refused: `escalate = true` under an
//! `ssh_user` that has no sudo.
//!
//! `make vm-test` runs it once, after `vagrant_login.rs`, and expects the run
//! to fail. The orchestrator logs in as `rustible-login` and launches the
//! binary behind `sudo -n`, which refuses, so the binary never starts; the
//! failure must say that the login came from this attribute and what it
//! replaced (`launch_escalation_failure` in `rustible-cli`'s `run.rs`). The
//! body below is never reached, and if it is, it fails.

use rustible::prelude::*;

#[rustible::playbook(hosts = "vagrant", ssh_user = "rustible-login", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    bail!("launched as `{}` with escalate = true, but `rustible-login` has no sudo: the launch should have been refused", ctx.facts().user)
}
