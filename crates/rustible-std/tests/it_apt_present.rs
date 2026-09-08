//! Docker integration test for `apt::Present` (vision 8, tier 3).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_apt_present`.

use std::time::Duration;

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::apt;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn present_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    assert_eq!(ctx.facts().package_manager, Pm::Apt);
    // Stock images ship without package lists; `update_cache` runs
    // `apt-get update` in apply, so only the first step pays for it.
    let (first, second) = changed_then_ok(ctx, "install sl", || {
        apt::Present::new(["sl"]).update_cache(Duration::ZERO)
    })?;
    assert_eq!(first.installed.len(), 1);
    assert_eq!(first.installed[0].name, "sl");
    assert!(
        !first.installed[0].version.is_empty(),
        "apply resolves the version"
    );
    assert_eq!(second.already_present.len(), 1);
    assert!(ctx.sys().exists("/usr/games/sl")?);
    Ok(())
}
