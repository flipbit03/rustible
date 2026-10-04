//! Docker integration test for the `systemd` ops (vision 8, tier 3). Runs on
//! the systemd images with
//! `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_systemd`.

use std::sync::Arc;
use std::time::Duration;

use rustible::prelude::*;
use rustible::sdk::backend::Local;
use rustible::sdk::event::Collect;
use rustible::sdk::testing::changed_then_ok;
use rustible::sdk::{HostInfo, System};
use rustible_std::{apt, shell, systemd, user};

const UNIT: &str = "rustible-test";

#[rustible::integration_test(systemd_images = ["jrei/systemd-debian:12", "jrei/systemd-ubuntu:24.04"])]
fn unit_lifecycle_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    assert_eq!(ctx.facts().init, Init::Systemd);
    ctx.sys().write_atomic(
        format!("/etc/systemd/system/{UNIT}.service"),
        b"[Unit]\nDescription=rustible integration test\n\n\
          [Service]\nExecStart=/bin/sleep infinity\n\n\
          [Install]\nWantedBy=multi-user.target\n",
    )?;
    // Make systemd re-read its unit files after writing a new one. Before
    // `DaemonReload` existed this test had to reload an unrelated unit
    // (`systemd-journald`) just to carry a `.daemon_reload(true)` flag; now it
    // is one step that names no unit and returns nothing.
    let reloaded = ctx.step("systemd re-reads its units", systemd::DaemonReload::new())?;
    assert!(reloaded.changed);
    assert_eq!(
        reloaded.diff.as_ref().map(|d| d.render()),
        Some("systemctl daemon-reload".to_string())
    );

    let (first, second) = changed_then_ok(ctx, "unit enabled and started", || {
        systemd::Enabled::new(UNIT).now(true)
    })?;
    assert!(first.enabled && first.active);
    assert_eq!(*second, *first);

    let restarted = ctx.step("unit restarted", systemd::Restart::new(UNIT))?;
    assert!(restarted.changed && restarted.active);

    let (stopped, _) = changed_then_ok(ctx, "unit stopped", || systemd::Stopped::new(UNIT))?;
    assert!(stopped.enabled && !stopped.active);

    let (running, _) = changed_then_ok(ctx, "unit running", || systemd::Running::new(UNIT))?;
    assert!(running.active);

    let (disabled, _) = changed_then_ok(ctx, "unit disabled and stopped", || {
        systemd::Disabled::new(UNIT).now(true)
    })?;
    assert!(!disabled.enabled && !disabled.active);

    // Failure paths against the real systemctl.
    let err = ctx
        .step(
            "static unit cannot be disabled",
            systemd::Disabled::new("systemd-journald"),
        )
        .err()
        .map(|e| e.chain())
        .unwrap_or_default();
    assert!(err.contains("is static"), "{err}");
    let err = ctx
        .step("missing unit", systemd::Enabled::new("rustible-nope"))
        .err()
        .map(|e| e.chain())
        .unwrap_or_default();
    assert!(err.contains("not found"), "{err}");
    Ok(())
}

const SVC: &str = "rustible-svc";
const USER_UNIT: &str = "rustible-user-test";

/// Issue #55: `.user(true)` under `as_user(..)` manages another account's
/// user units, through the account's own user manager; #60: right after
/// `loginctl enable-linger`, with no step waiting for that manager.
///
/// The harness's `Ctx` cannot step to another account itself: on a real
/// system `as_user` starts this binary again as `--helper`, which a test
/// binary does not serve. A `System` built without escalation stands in for
/// it, prefixing each command with `sudo -n -u <account>`. The helper is
/// started by that same `sudo`, so the environment its commands inherit
/// lacks `XDG_RUNTIME_DIR` exactly as theirs does here, which is the bug.
///
/// One difference needs a fixture line. The helper sets the op's variables
/// on the command it spawns; the prefix sets them on `sudo`, which drops
/// them. So `sudoers` keeps `XDG_RUNTIME_DIR` and nothing else: root's own
/// environment here has none, so a step that did not set it still fails.
#[rustible::integration_test(systemd_images = ["jrei/systemd-debian:12", "jrei/systemd-ubuntu:24.04"])]
fn user_units_of_another_account_through_as_user(ctx: &mut Ctx) -> Result<()> {
    // Fixture: `sudo`, which the images lack, and the account with a unit
    // in its own `~/.config/systemd/user/`.
    ctx.step(
        "sudo installed",
        apt::Present::new(["sudo"]).update_cache(Duration::from_secs(3600)),
    )?;
    ctx.sys().write_atomic(
        "/etc/sudoers.d/rustible-xdg",
        b"Defaults env_keep += \"XDG_RUNTIME_DIR\"\n",
    )?;
    ctx.sys().set_mode("/etc/sudoers.d/rustible-xdg", 0o440)?;
    let svc = ctx.step("service account", user::Present::new(SVC).create_home(true))?;
    let dir = svc.home.join(".config/systemd/user");
    ctx.sys().mkdir_all(&dir)?;
    for d in [".config", ".config/systemd", ".config/systemd/user"] {
        ctx.sys().set_owner(svc.home.join(d), svc.uid, svc.gid)?;
    }
    let file = dir.join(format!("{USER_UNIT}.service"));
    ctx.sys().write_atomic(
        &file,
        b"[Unit]\nDescription=rustible user unit test\n\n\
          [Service]\nExecStart=/bin/sleep infinity\n\n\
          [Install]\nWantedBy=default.target\n",
    )?;
    ctx.sys().set_owner(&file, svc.uid, svc.gid)?;

    let facts = ctx.facts().clone();
    let as_svc = |check_mode: bool| {
        let sys = System::new(
            Arc::new(Local),
            facts.clone(),
            check_mode,
            Arc::new(Collect::default()),
        );
        Ctx::new(sys, HostInfo::local()).as_user(SVC)
    };
    let enabled = || systemd::Enabled::new(USER_UNIT).user(true).now(true);
    let runtime_dir = format!("/run/user/{}", svc.uid);

    // No session and no linger: no user manager. A real run refuses, naming
    // the account's own runtime directory, not root's.
    let err = as_svc(false)
        .step("refused without a user manager", enabled())
        .unwrap_err()
        .chain();
    assert!(
        err.contains(&format!(
            "no user manager for `{SVC}`: {runtime_dir} does not exist"
        )),
        "{err}"
    );
    // A dry run waits for it instead: an earlier step may enable linger.
    let planned = as_svc(true).step("waits for the user manager", enabled())?;
    assert!(planned.changed && !planned.is_available());
    let diff = planned
        .diff
        .as_ref()
        .map(|d| d.render())
        .unwrap_or_default();
    assert!(
        diff.ends_with(&format!(
            "waits for the user manager of `{SVC}` ({runtime_dir}/systemd/private does not \
             exist yet)"
        )),
        "{diff}"
    );

    // Linger gives the account a manager, but `loginctl enable-linger`
    // returns before logind has started it (#60). The step right after it
    // waits for the manager instead of refusing. In a container the manager
    // comes up within tens of milliseconds, a window this test could win or
    // lose by chance, so a drop-in holds it back two seconds after its
    // runtime directory exists: long enough that a step that did not wait
    // finds the directory and no manager, every time.
    ctx.sys().mkdir_all("/etc/systemd/system/user@.service.d")?;
    ctx.sys().write_atomic(
        "/etc/systemd/system/user@.service.d/rustible-slow.conf",
        b"[Service]\nExecStartPre=/bin/sleep 2\n",
    )?;
    ctx.step("systemd re-reads user@", systemd::DaemonReload::new())?;
    ctx.step(
        "linger enabled",
        shell::Command::new("loginctl")
            .args(["enable-linger", SVC])
            .creates(format!("/var/lib/systemd/linger/{SVC}")),
    )?;

    let mut svc_ctx = as_svc(false);
    let (first, second) = changed_then_ok(&mut svc_ctx, "user unit enabled and started", enabled)?;
    assert!(first.enabled && first.active);
    assert_eq!(*second, *first);
    let reloaded = svc_ctx.step(
        "user manager re-reads its units",
        systemd::DaemonReload::new().user(true),
    )?;
    assert!(reloaded.changed);
    // The unit is enabled in the account's own tree, not root's.
    assert!(
        ctx.sys()
            .exists(dir.join(format!("default.target.wants/{USER_UNIT}.service")))?
    );

    let restarted = svc_ctx.step(
        "user unit restarted",
        systemd::Restart::new(USER_UNIT).user(true),
    )?;
    assert!(restarted.changed && restarted.active);
    let (stopped, _) = changed_then_ok(&mut svc_ctx, "user unit stopped", || {
        systemd::Stopped::new(USER_UNIT).user(true)
    })?;
    assert!(stopped.enabled && !stopped.active);
    let (disabled, _) = changed_then_ok(&mut svc_ctx, "user unit disabled", || {
        systemd::Disabled::new(USER_UNIT).user(true).now(true)
    })?;
    assert!(!disabled.enabled && !disabled.active);
    Ok(())
}
