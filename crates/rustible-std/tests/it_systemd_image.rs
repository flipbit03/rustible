//! Docker integration test for the harness's systemd variant (vision 8,
//! tier 3; M6 brief "SystemdImage"). Proves the image boots with systemd as
//! PID 1 and that `systemctl` works inside, which is what the systemd ops
//! (`Enabled`, `Running`, ...) will build on. Runs with
//! `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_systemd_image`.

use rustible::prelude::*;
use rustible_std::shell;

#[rustible::integration_test(systemd_images = ["jrei/systemd-debian:12", "jrei/systemd-ubuntu:24.04"])]
fn systemd_is_pid1(ctx: &mut Ctx) -> Result<()> {
    assert_eq!(ctx.sys().read_to_string("/proc/1/comm")?.trim(), "systemd");
    let out = ctx.step(
        "journald is active",
        shell::Command::new("systemctl").args(["is-active", "systemd-journald"]),
    )?;
    assert_eq!(out.stdout.trim(), "active");
    Ok(())
}
