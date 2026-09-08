//! Docker integration test for `apt::Latest` (vision 8, tier 3).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_apt_latest`.
//!
//! The point of interest is the check-time cache refresh: a stock image ships
//! with no package lists, so a dry run that does not refresh them cannot name
//! a candidate version and its answer is worthless.

use std::time::Duration;

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::apt;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn latest_refreshes_the_cache_in_check(ctx: &mut Ctx) -> Result<()> {
    assert_eq!(ctx.facts().package_manager, Pm::Apt);

    // Stock images ship without package lists, so apt names no candidate.
    let policy = |ctx: &mut Ctx| -> Result<apt::Policy> {
        let out = ctx
            .sys()
            .cmd("apt-cache")
            .args(["policy", "sl"])
            .allow_failure()
            .run()?;
        Ok(apt::parse_policy(&out.stdout_str()))
    };
    assert_eq!(
        policy(ctx)?.candidate,
        None,
        "the image already has package lists; the rest of this test proves nothing"
    );

    // A dry run with `.update_cache` refreshes them first, so it can plan.
    let dry = ctx.sys().clone().with_check_mode(true);
    let op = apt::Latest::new(["sl"]).update_cache(Duration::ZERO);
    let Plan::Change(change) = op.check(&dry)? else {
        panic!("expected a change: `sl` is not installed");
    };
    let predicted = change.predicted.expect("Latest predicts its output");
    assert_eq!(predicted.installed.len(), 1);
    assert!(
        !predicted.installed[0].version.is_empty(),
        "the refreshed lists name a candidate version"
    );

    // The refresh really happened on the target, which is the honest cost of
    // the dry run and what the op warns about.
    assert!(
        policy(ctx)?.candidate.is_some(),
        "the check-mode run left the package lists refreshed"
    );

    // And for real: install, then report `ok` at the candidate version.
    let (first, second) = changed_then_ok(ctx, "sl at the latest version", || {
        apt::Latest::new(["sl"]).update_cache(Duration::from_secs(3600))
    })?;
    assert_eq!(first.installed.len(), 1);
    assert!(!first.installed[0].version.is_empty());
    assert!(first.upgraded.is_empty());
    assert_eq!(second.current.len(), 1);
    assert_eq!(second.current[0].name, "sl");
    assert!(ctx.sys().exists("/usr/games/sl")?);
    Ok(())
}
