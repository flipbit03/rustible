//! Docker integration test for `sysctl::Present` (vision 8, tier 3).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_sysctl_present`.
//!
//! Containers cannot write `/proc/sys`, so the op runs with
//! `.apply_now(false)` and the assertions are about the drop-in file.

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::sysctl;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn present_persist_only_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    let dir = "/etc/sysctl.d";
    let file = "/etc/sysctl.d/99-rustible.conf";
    let key = "net.ipv4.ip_forward";

    // Stock debian:12 has no /etc/sysctl.d (procps is not installed): the
    // op refuses to create the directory itself (vision 6.7).
    if !ctx.sys().exists(dir)? {
        let err = ctx
            .step(
                "persist without the directory",
                sysctl::Present::new(key, "1").apply_now(false),
            )
            .unwrap_err()
            .chain();
        assert!(err.contains("/etc/sysctl.d does not exist"), "{err}");
        ctx.sys().mkdir_all(dir)?;
    }

    let (first, second) = changed_then_ok(ctx, "persist ip_forward", || {
        sysctl::Present::new(key, "1").apply_now(false)
    })?;
    assert_eq!(first.key, key);
    assert_eq!(first.value, "1");
    assert_eq!(first.file.to_str(), Some(file));
    assert_eq!(
        first.previous_live.as_deref(),
        Some(
            ctx.sys()
                .read_to_string("/proc/sys/net/ipv4/ip_forward")?
                .trim()
        ),
        "the live value is read even when not applied"
    );
    assert_eq!(second.previous_live, first.previous_live);
    assert_eq!(ctx.sys().read_to_string(file)?, "net.ipv4.ip_forward = 1\n");

    // A second key appends; changing the first rewrites its line in place.
    changed_then_ok(ctx, "persist swappiness", || {
        sysctl::Present::new("vm.swappiness", "10").apply_now(false)
    })?;
    changed_then_ok(ctx, "flip ip_forward", || {
        sysctl::Present::new(key, "0").apply_now(false)
    })?;
    assert_eq!(
        ctx.sys().read_to_string(file)?,
        "net.ipv4.ip_forward = 0\nvm.swappiness = 10\n"
    );

    // Applying live is refused for a key the kernel does not have, and
    // persist-only records no live value for it.
    let err = ctx
        .step(
            "live for a missing key",
            sysctl::Present::new("net.rustible.nope", "1"),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("does not exist on this kernel"), "{err}");
    let r = ctx.step(
        "persist a missing key",
        sysctl::Present::new("net.rustible.nope", "1").apply_now(false),
    )?;
    assert!(r.changed);
    assert_eq!(r.previous_live, None);
    Ok(())
}
