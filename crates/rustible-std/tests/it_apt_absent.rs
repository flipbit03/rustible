//! Docker integration test for `apt::Absent` (vision 8, T2).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_apt_absent`.

use std::time::Duration;

use rustible::prelude::*;
use rustible::sdk::testing::{self, changed_then_ok};
use rustible_std::apt;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn absent_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    assert!(ctx.facts().has_pm(&Pm::Apt));
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

/// #157. dpkg keeps a `not-installed` record for a package another installed
/// package names, and `dpkg-query` answers for it with exit 0. libc6's
/// `Breaks:` names `nscd` on both images, so a stock image has the record
/// with nothing set up.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn absent_ok_for_a_package_dpkg_keeps_only_a_record_of(ctx: &mut Ctx) -> Result<()> {
    let record = ctx
        .sys()
        .cmd("dpkg-query")
        .args(["-W", "-f=${Status}\n", "nscd"])
        .allow_failure()
        .run()?;
    assert!(record.success(), "dpkg-query has a record of nscd");
    assert_eq!(record.stdout_str(), "unknown ok not-installed\n");

    for purge in [false, true] {
        let r = ctx.step("nscd absent", apt::Absent::new(["nscd"]).purge(purge))?;
        assert!(!r.changed, "purge {purge}");
        assert_eq!(r.not_present, vec!["nscd"]);
    }
    Ok(())
}

/// A held package is installed for `Present` and `Latest`, and a step that
/// would change its version is refused, under check mode too, because
/// apt-get would refuse it.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn held_package_is_present_and_absent_refuses_it(ctx: &mut Ctx) -> Result<()> {
    ctx.step(
        "install sl",
        apt::Present::new(["sl"]).update_cache(Duration::ZERO),
    )?;
    ctx.sys().cmd("apt-mark").args(["hold", "sl"]).run()?;

    let present = ctx.step("sl present while held", apt::Present::new(["sl"]))?;
    assert!(!present.changed);
    assert_eq!(present.already_present[0].name, "sl");
    let latest = ctx.step("sl latest while held", apt::Latest::new(["sl"]))?;
    assert!(!latest.changed);

    let mut dry = testing::dry(ctx);
    for purge in [false, true] {
        for (mode, c) in [("real", &mut *ctx), ("dry", &mut dry)] {
            let err = c
                .step(
                    "sl absent while held",
                    apt::Absent::new(["sl"]).purge(purge),
                )
                .unwrap_err()
                .chain();
            assert!(err.contains("package `sl` is held"), "{mode}: {err}");
            assert!(err.contains("apt-mark unhold sl"), "{mode}: {err}");
        }
    }
    assert!(ctx.sys().exists("/usr/games/sl")?);

    // Unheld, the same step goes through.
    ctx.sys().cmd("apt-mark").args(["unhold", "sl"]).run()?;
    changed_then_ok(ctx, "sl absent once unheld", || {
        apt::Absent::new(["sl"]).purge(true)
    })?;

    // Installing a held package sets its version, which apt-get refuses,
    // whether nothing of it is on disk or only its configuration files.
    // Purging those files changes no version, so that goes through.
    ctx.step("install nano", apt::Present::new(["nano"]))?;
    ctx.step("remove nano", apt::Absent::new(["nano"]))?;
    for name in ["cowsay", "nano"] {
        ctx.sys().cmd("apt-mark").args(["hold", name]).run()?;
        let mut dry = testing::dry(ctx);
        for (mode, c) in [("real", &mut *ctx), ("dry", &mut dry)] {
            let err = c
                .step("install while held", apt::Present::new([name]))
                .unwrap_err()
                .chain();
            assert!(
                err.contains(&format!("package `{name}` is held")),
                "{mode}: {err}"
            );
        }
    }
    let status = ctx
        .sys()
        .cmd("dpkg-query")
        .args(["-W", "-f=${Status}\n", "nano"])
        .run()?;
    assert_eq!(status.stdout_str(), "hold ok config-files\n");
    changed_then_ok(ctx, "purge nano's config files while held", || {
        apt::Absent::new(["nano"]).purge(true)
    })?;
    Ok(())
}
