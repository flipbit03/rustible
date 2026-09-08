//! Docker integration test for the `systemd` ops (vision 8, tier 3). Runs on
//! the systemd images with
//! `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_systemd`.

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::systemd;

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
