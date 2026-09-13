//! The playbook the macOS-target spike proves itself with.
//!
//! It targets the `mac` group, which is not in `hosts.kdl`: like
//! `vagrant.rs`, the inventory naming a real machine is supplied with
//! `--inventory`. It exercises the parts of the pipeline that are not
//! obviously portable off Linux — the Darwin probe, a cross-built Mach-O
//! binary, escalation through the mac's own `sudo`, TLS from `ring` compiled
//! for Darwin — and deliberately touches no operation that speaks apt,
//! systemd or `/etc/passwd`.
//!
//! Run it twice: every step reports `changed` and then `ok`.

use rustible::prelude::*;
use rustible_std::{file, http, shell};

/// What the marker file says, so a second run has something to compare.
const MARKER: &str = "written by rustible, from a Linux box, to a mac\n";

#[rustible::playbook(hosts = "mac", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ensure!(
        f.os == Os::Macos,
        "this playbook is for macOS targets; this host reports {:?}",
        f.os
    );
    ensure!(f.is_root, "this playbook needs root, which means sudo worked");
    ctx.log(format!("{:?} on {:?}, root={}", f.os, f.arch, f.is_root));

    // What the mac says it is, through its own tools rather than through
    // facts, because `Facts` has no Darwin source for most of its fields.
    let vers = ctx.step("sw_vers", shell::Command::new("/usr/bin/sw_vers"))?;
    if !ctx.check_mode() {
        ctx.log(vers.stdout.replace('\n', " | ").trim().to_string());
    }

    // A directory and a file written as root: the escalation is the mac's
    // own sudo, over the real SSH transport.
    ctx.step(
        "spike directory",
        file::Directory::at("/etc/rustible-spike").mode(0o755).owner(0, 0),
    )?;
    ctx.step(
        "marker file",
        file::Copy::from_str(MARKER)
            .to("/etc/rustible-spike/marker")
            .mode(0o644)
            .owner(0, 0),
    )?;

    // A line edited in place, which is the single most-used shape in any
    // real inventory. Its own file: `file::Copy` above owns the whole of
    // `marker`, so editing a line of that one would make the two steps
    // undo each other on every run.
    ctx.step(
        "host line",
        file::Line::in_path("/etc/rustible-spike/hosts")
            .matching(r"^host=")
            .create(true)
            .set("host=cadumac"),
    )?;

    // TLS from the target, which is what proves `ring`'s C cross-compiled
    // for Darwin and runs there. The lookup happens on the target (vision
    // 5.1), so this is the mac's egress, not the controller's.
    let dl = ctx.step(
        "fetch a known file over https",
        http::Download::get("https://github.com/flipbit03.keys")
            .to("/etc/rustible-spike/keys")
            .mode(0o644),
    )?;
    if !ctx.check_mode() {
        ctx.log(format!("{} bytes over TLS", dl.bytes));
    }

    Ok(())
}
