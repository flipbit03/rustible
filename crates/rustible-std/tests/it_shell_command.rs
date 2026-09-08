//! Docker integration test for `shell::Command` (vision 8, tier 3): the
//! action itself plus `creates`, `removes`, `stdin`, `env`, `cwd` and
//! `changed_when` against a real `/bin/sh`. Runs with
//! `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_shell_command`.

use std::sync::Arc;

use rustible::prelude::*;
use rustible::sdk::event::Collect;
use rustible::sdk::testing::changed_then_ok;
use rustible::sdk::{HostInfo, System};
use rustible_std::shell::Command;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn command_options_against_a_real_shell(ctx: &mut Ctx) -> Result<()> {
    let dir = "/etc/rustible-test";
    let marker = "/etc/rustible-test/marker";
    ctx.sys().mkdir_all(dir)?;

    // An action: runs and changes every time, output captured, LANG=C.
    let out = ctx.step("echo", Command::sh("echo hi; echo $LANG; echo err >&2"))?;
    assert!(out.changed);
    assert_eq!(out.status, 0);
    assert_eq!(out.stdout, "hi\nC\n");
    assert_eq!(out.stderr, "err\n");
    let again = ctx.step("echo again", Command::sh("echo hi"))?;
    assert!(again.changed, "no escape hatch, so it always changes");

    // `creates`: the second run does not execute (the marker exists).
    let (first, second) = changed_then_ok(ctx, "touch marker", || {
        Command::new("touch").arg(marker).creates(marker)
    })?;
    assert!(first.status == 0 && second.status == 0);
    assert!(ctx.sys().exists(marker)?);

    // `removes`: the second run does not execute (the marker is gone).
    changed_then_ok(ctx, "rm marker", || {
        Command::new("rm").arg(marker).removes(marker)
    })?;
    assert!(!ctx.sys().exists(marker)?);

    // `stdin` reaches the process; a large input round-trips in full.
    let out = ctx.step("cat stdin", Command::new("cat").stdin("fed via stdin"))?;
    assert_eq!(out.stdout, "fed via stdin");
    let big = "x".repeat(2 * 1024 * 1024);
    let out = ctx.step("wc stdin", Command::new("wc").arg("-c").stdin(big.as_str()))?;
    assert_eq!(out.stdout.trim(), big.len().to_string());

    // `env` and `cwd`.
    let out = ctx.step(
        "env",
        Command::sh("echo $RUSTIBLE_TEST_VAR").env("RUSTIBLE_TEST_VAR", "papoi"),
    )?;
    assert_eq!(out.stdout, "papoi\n");
    let out = ctx.step("cwd", Command::new("pwd").cwd(dir))?;
    assert_eq!(out.stdout.trim(), dir);

    // `changed_when`: the command runs either way, the predicate decides.
    let out = ctx.step(
        "up to date",
        Command::sh("echo Already up to date.")
            .changed_when(|o| !o.stdout.contains("Already up to date")),
    )?;
    assert!(!out.changed, "ran, but the predicate said ok");
    assert_eq!(
        out.stdout, "Already up to date.\n",
        "the output is still there"
    );
    let out = ctx.step(
        "updated",
        Command::sh("echo Updating 1..2")
            .changed_when(|o| !o.stdout.contains("Already up to date")),
    )?;
    assert!(out.changed);

    // A non-zero exit fails the step; `.ok()` on the step is `ignore_errors`.
    let err = ctx
        .step("fails", Command::sh("echo boom >&2; exit 7"))
        .unwrap_err()
        .chain();
    assert!(err.contains('7') && err.contains("boom"), "{err}");
    assert!(ctx.step("ignored", Command::new("false")).is_err());

    // Check mode over the real machine: nothing runs, nothing is created.
    let never = "/etc/rustible-test/never";
    let mut dry = Ctx::new(
        System::local(true, Arc::new(Collect::default())),
        HostInfo::local(),
    );
    let r = dry.step("would touch", Command::new("touch").arg(never))?;
    assert!(r.changed && !r.is_available());
    let r = dry.step(
        "would touch with changed_when",
        Command::new("touch").arg(never).changed_when(|_| false),
    )?;
    assert!(r.changed, "check mode cannot run the predicate");
    assert!(!ctx.sys().exists(never)?);
    Ok(())
}
