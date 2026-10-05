//! Docker integration test for `apt::Latest` (vision 8, T2).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_apt_latest`.
//!
//! The point of interest is the cache refresh under `--check`: a stock image
//! ships with no package lists, so a dry run cannot name a candidate version
//! without fetching them, and a dry run fetches nothing (vision 12). It says
//! it cannot decide instead, and the lists are still empty afterwards.

use std::time::Duration;

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::apt;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn latest_does_not_refresh_the_cache_under_check(ctx: &mut Ctx) -> Result<()> {
    assert!(ctx.facts().has_pm(&Pm::Apt));

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

    // A dry run with `.update_cache` does not refresh them, so it cannot
    // plan: it reports a change whose diff says why. The harness body runs
    // with check mode off, so the dry run gets a `System` of its own.
    let dry = ctx.sys().clone().with_check_mode(true);
    let op = apt::Latest::new(["sl"]).update_cache(Duration::ZERO);
    let Plan::Change(change) = op.check(&dry)? else {
        panic!("expected a change: with Duration::ZERO the lists are always stale");
    };
    let rendered = change.diff().render();
    assert!(
        rendered.starts_with("apt packages sl: candidate versions unknown; ")
            && rendered.ends_with("and they are not refreshed under --check"),
        "{rendered:?}"
    );

    // And nothing was fetched or written: apt still names no candidate.
    assert_eq!(
        policy(ctx)?.candidate,
        None,
        "the dry run refreshed the package lists"
    );

    // For real: refresh, install, then report `ok` at the candidate version.
    let (first, second) = changed_then_ok(ctx, "sl at the latest version", || {
        apt::Latest::new(["sl"]).update_cache(Duration::from_secs(3600))
    })?;
    assert_eq!(first.installed.len(), 1);
    assert!(!first.installed[0].version.is_empty());
    assert!(first.upgraded.is_empty());
    assert_eq!(second.current.len(), 1);
    assert_eq!(second.current[0].name, "sl");
    assert!(ctx.sys().exists("/usr/games/sl")?);

    // The lists are fresh now, so a dry run needs no refresh and plans from
    // them as the real run did: `ok`, at the version the real run reported.
    let fresh = apt::Latest::new(["sl"]).update_cache(Duration::from_secs(3600));
    let Plan::Satisfied(report) = fresh.check(&dry)? else {
        panic!("expected satisfied: sl is at the candidate and the lists are fresh");
    };
    assert_eq!(report.current[0].version, second.current[0].version);
    Ok(())
}
