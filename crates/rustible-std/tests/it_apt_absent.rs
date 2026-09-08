//! Docker integration test for `apt::Absent` (vision 8, tier 3).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_apt_absent`.

use std::time::Duration;

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::apt;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn absent_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    assert_eq!(ctx.facts().package_manager, Pm::Apt);
    // Nothing to remove yet: `ok`, with the name in `not_present`.
    let before = ctx.step("remove sl (not installed)", apt::Absent::new(["sl"]))?;
    assert!(!before.changed);
    assert_eq!(before.not_present, vec!["sl"]);

    // Stock images ship without package lists; `update_cache` pays for
    // `apt-get update` once, in apply.
    let installed = ctx.step(
        "install sl",
        apt::Present::new(["sl"]).update_cache(Duration::ZERO),
    )?;
    assert!(installed.changed);
    assert!(ctx.sys().exists("/usr/games/sl")?);

    let (first, second) = changed_then_ok(ctx, "remove sl", || {
        apt::Absent::new(["sl"]).purge(true).autoremove(true)
    })?;
    assert_eq!(first.removed.len(), 1);
    assert_eq!(first.removed[0].name, "sl");
    assert!(
        !first.removed[0].version.is_empty(),
        "check knows the version from dpkg-query"
    );
    assert!(first.not_present.is_empty());
    assert_eq!(second.not_present, vec!["sl"]);
    assert!(second.removed.is_empty());
    assert!(!ctx.sys().exists("/usr/games/sl")?);

    // dpkg no longer reports it installed.
    let status = ctx
        .sys()
        .cmd("dpkg-query")
        .args(["-W", "-f=${Status}\n", "sl"])
        .allow_failure()
        .run()?;
    assert!(
        !status.stdout_str().contains("install ok installed"),
        "{}",
        status.stdout_str()
    );
    Ok(())
}
