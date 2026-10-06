//! Docker integration test for `apt::Latest` and the `.update_cache(..)`
//! refresh it shares with `apt::Present` (vision 8, T2).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_apt_latest`.
//!
//! The first point of interest is the cache refresh under `--check`: a stock
//! image ships with no package lists, so a dry run cannot name a candidate
//! version without fetching them, and a dry run fetches nothing (vision 12).
//! It says it cannot decide instead, and the lists are still empty
//! afterwards.
//!
//! The second is the lists' age (#70). An `apt-get update` that changes no
//! index leaves `/var/lib/apt/lists` as old as it was, so a refresh records
//! itself in `/var/lib/apt/periodic/update-success-stamp`, and the age is
//! the newer of the stamp and the lists, with `pkgcache.bin` only when
//! neither can be read. These tests back-date with `touch -d`, and tell
//! whether `apt-get update` ran from `/var/lib/apt/lists/partial`, which
//! every update rewrites and nothing else here touches.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustible::prelude::*;
use rustible::sdk::HostInfo;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::apt;

const STAMP: &str = "/var/lib/apt/periodic/update-success-stamp";
const LISTS: &str = "/var/lib/apt/lists";
const PARTIAL: &str = "/var/lib/apt/lists/partial";
const PKGCACHE: &str = "/var/cache/apt/pkgcache.bin";
const TWO_DAYS: u64 = 2 * 86_400;
const HOUR: Duration = Duration::from_secs(3_600);

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `stat -c %Y`, or `None` when the path does not exist.
fn mtime(ctx: &mut Ctx, path: &str) -> Result<Option<u64>> {
    let Some(out) = ctx.sys().cmd("stat").args(["-c", "%Y", path]).ok()? else {
        return Ok(None);
    };
    Ok(Some(out.stdout_str().trim().parse()?))
}

/// Set `path`'s mtime to `ago` seconds before now, and return it. `touch`
/// creates a file that is missing.
fn back_date(ctx: &mut Ctx, path: &str, ago: u64) -> Result<u64> {
    let at = now() - ago;
    ctx.sys()
        .cmd("touch")
        .args(["-d", &format!("@{at}"), path])
        .run()?;
    Ok(at)
}

/// Make the lists directory and `pkgcache.bin` two days old, so that what
/// keeps the lists fresh afterwards can only be the stamp. The images carry
/// no `pkgcache.bin` (their `docker-clean` turns it off), so it is dated
/// only if present.
fn back_date_lists(ctx: &mut Ctx) -> Result<()> {
    back_date(ctx, LISTS, TWO_DAYS)?;
    if ctx.sys().exists(PKGCACHE)? {
        back_date(ctx, PKGCACHE, TWO_DAYS)?;
    }
    Ok(())
}

/// Make the lists two days old by every source of their age, as a box a
/// refresh stamped two days ago and then left alone is.
fn make_lists_stale(ctx: &mut Ctx) -> Result<u64> {
    back_date_lists(ctx)?;
    ctx.sys().mkdir_all("/var/lib/apt/periodic")?;
    back_date(ctx, STAMP, TWO_DAYS)
}

/// Plain `apt-get update`, outside any op, so the lists exist to go stale.
fn apt_get_update(ctx: &mut Ctx) -> Result<()> {
    ctx.sys().cmd("apt-get").arg("update").run()?;
    Ok(())
}

/// A `Ctx` for a dry run over the same machine: harness bodies run with
/// check mode off.
fn dry(ctx: &mut Ctx) -> Ctx {
    Ctx::new(ctx.sys().clone().with_check_mode(true), HostInfo::local())
}

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
            && rendered.ends_with("and the lists are not refreshed under --check"),
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
    let fresh = apt::Latest::new(["sl"]).update_cache(HOUR);
    let Plan::Satisfied(report) = fresh.check(&dry)? else {
        panic!("expected satisfied: sl is at the candidate and the lists are fresh");
    };
    assert_eq!(report.current[0].version, second.current[0].version);
    Ok(())
}

/// #70, the case that motivated it: lists two days old, a real
/// `Latest::update_cache(1h)` refreshes them, and the refresh changes no
/// index, so `/var/lib/apt/lists` stays two days old. Before the stamp, a dry
/// run straight after kept reporting "candidate versions unknown" (and every
/// real run refreshed again) until a mirror published a changed index.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn latest_refresh_that_changes_no_index_counts_as_fresh(ctx: &mut Ctx) -> Result<()> {
    apt_get_update(ctx)?;
    make_lists_stale(ctx)?;
    let op = || apt::Latest::new(["sl"]).update_cache(HOUR);

    // The setup took: a dry run sees stale lists and cannot decide.
    let Plan::Change(c) = op().check(dry(ctx).sys())? else {
        panic!("expected a change: the lists were made two days old");
    };
    let rendered = c.diff().render();
    assert!(
        rendered.contains("candidate versions unknown"),
        "{rendered}"
    );

    let first = ctx.step("sl at the latest version", op())?;
    assert!(first.changed);
    // The refresh stamped: the stamp is the only source that is fresh now.
    let stamped = mtime(ctx, STAMP)?.expect("a successful refresh writes the stamp");
    assert!(now() - stamped < 600, "the stamp is not fresh: {stamped}");

    // The refresh usually changes no index, leaving the lists directory two
    // days old; a mirror that published in the last few seconds would have
    // moved it. Pin the usual case regardless, and move the stamp back half
    // an hour, still within the hour, so a second refresh would show.
    back_date_lists(ctx)?;
    let stamp = back_date(ctx, STAMP, 1_800)?;
    let partial = back_date(ctx, PARTIAL, TWO_DAYS)?;

    // A second real run trusts the stamp: `ok`, and no refresh.
    let second = ctx.step("sl at the latest version (again)", op())?;
    assert!(!second.changed, "the second run changed something");
    assert_eq!(
        mtime(ctx, PARTIAL)?,
        Some(partial),
        "the second real run ran apt-get update again"
    );

    // The dry run now plans from the lists, as the real run did: `ok`.
    let report = match op().check(dry(ctx).sys())? {
        Plan::Satisfied(report) => report,
        Plan::Change(c) => panic!(
            "expected satisfied: a real run refreshed the lists just now, but the dry run \
             planned {:?}",
            c.diff().render()
        ),
    };
    assert_eq!(report.current[0].name, "sl");
    assert_eq!(mtime(ctx, STAMP)?, Some(stamp), "the stamp moved");
    Ok(())
}

/// The same for `Present`, which refreshes in `apply`: after one real run
/// refreshed, the next install (another package, since a satisfied step
/// never reaches `apply`) does not refresh again.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn present_does_not_refresh_again_after_a_refresh_that_changed_nothing(
    ctx: &mut Ctx,
) -> Result<()> {
    apt_get_update(ctx)?;
    make_lists_stale(ctx)?;
    let partial = back_date(ctx, PARTIAL, TWO_DAYS)?;

    let sl = ctx.step("sl present", apt::Present::new(["sl"]).update_cache(HOUR))?;
    assert!(sl.changed);
    // The detector works: this refresh rewrote `partial`.
    assert_ne!(
        mtime(ctx, PARTIAL)?,
        Some(partial),
        "the first step should have run apt-get update"
    );

    let stamped = mtime(ctx, STAMP)?.expect("the refresh wrote no stamp");
    assert!(now() - stamped < 600, "the stamp is not fresh: {stamped}");

    // As in the `Latest` case: the lists two days old, only the stamp fresh,
    // and moved back half an hour so a second refresh would show. Then
    // watch `partial` and the stamp.
    back_date_lists(ctx)?;
    let stamp = back_date(ctx, STAMP, 1_800)?;
    let partial = back_date(ctx, PARTIAL, TWO_DAYS)?;
    let hello = ctx.step(
        "hello present",
        apt::Present::new(["hello"]).update_cache(HOUR),
    )?;
    assert!(hello.changed);
    assert!(ctx.sys().exists("/usr/bin/hello")?);
    assert_eq!(
        mtime(ctx, PARTIAL)?,
        Some(partial),
        "the second install ran apt-get update again"
    );
    assert_eq!(mtime(ctx, STAMP)?, Some(stamp), "the stamp moved");
    Ok(())
}

/// An old stamp next to fresh lists cannot make them look stale: the newest
/// source wins, so nothing refreshes and the stamp is left as it was.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn a_stale_stamp_does_not_outweigh_fresh_lists(ctx: &mut Ctx) -> Result<()> {
    apt_get_update(ctx)?;
    let lists = mtime(ctx, LISTS)?.expect("apt-get update made the lists");
    assert!(now() - lists < 600, "the lists are not fresh: {lists}");
    ctx.sys().mkdir_all("/var/lib/apt/periodic")?;
    let stamp = back_date(ctx, STAMP, TWO_DAYS)?;
    let partial = back_date(ctx, PARTIAL, TWO_DAYS)?;

    let op = || apt::Latest::new(["sl"]).update_cache(HOUR);
    let Plan::Change(c) = op().check(dry(ctx).sys())? else {
        panic!("expected a change: sl is not installed");
    };
    assert!(
        c.diff()
            .render()
            .starts_with("apt packages:\n  sl: absent -> "),
        "{}",
        c.diff().render()
    );
    changed_then_ok(ctx, "sl at the latest version", op)?;
    ctx.step(
        "hello present",
        apt::Present::new(["hello"]).update_cache(HOUR),
    )?;

    assert_eq!(
        mtime(ctx, PARTIAL)?,
        Some(partial),
        "a step ran apt-get update"
    );
    assert_eq!(mtime(ctx, STAMP)?, Some(stamp), "a step rewrote the stamp");
    Ok(())
}

/// The stamp says an update succeeded, so a failed one writes none. A
/// malformed sources entry fails `apt-get update` before it fetches
/// anything, exit 100. Without the entry, the same steps stamp.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn a_failed_refresh_writes_no_stamp(ctx: &mut Ctx) -> Result<()> {
    const BROKEN: &str = "/etc/apt/sources.list.d/rustible-broken.list";
    assert!(!ctx.sys().exists(STAMP)?, "the image already has a stamp");
    ctx.sys().write_atomic(BROKEN, b"deb garbage\n")?;

    let err = ctx
        .step(
            "sl present, sources broken",
            apt::Present::new(["sl"]).update_cache(Duration::ZERO),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("apt-get update"), "{err}");
    let err = ctx
        .step(
            "sl latest, sources broken",
            apt::Latest::new(["sl"]).update_cache(Duration::ZERO),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("apt-get update"), "{err}");
    assert!(
        !ctx.sys().exists(STAMP)?,
        "a failed refresh wrote the stamp"
    );

    ctx.sys().remove(BROKEN)?;
    ctx.step(
        "sl latest",
        apt::Latest::new(["sl"]).update_cache(Duration::ZERO),
    )?;
    assert!(
        ctx.sys().exists(STAMP)?,
        "a successful refresh wrote no stamp"
    );
    Ok(())
}

/// A dry run against stale lists runs no `apt-get update` and writes no
/// stamp (#48), for either op.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn a_dry_run_against_stale_lists_neither_refreshes_nor_stamps(ctx: &mut Ctx) -> Result<()> {
    apt_get_update(ctx)?;
    let stamp = make_lists_stale(ctx)?;
    let lists = mtime(ctx, LISTS)?;
    let partial = back_date(ctx, PARTIAL, TWO_DAYS)?;

    let mut dry = dry(ctx);
    let latest = dry.step("sl latest", apt::Latest::new(["sl"]).update_cache(HOUR))?;
    assert!(latest.changed && !latest.is_available());
    let present = dry.step("sl present", apt::Present::new(["sl"]).update_cache(HOUR))?;
    assert!(present.changed && !present.is_available());

    assert_eq!(
        mtime(ctx, PARTIAL)?,
        Some(partial),
        "the dry run ran apt-get update"
    );
    assert_eq!(mtime(ctx, LISTS)?, lists);
    assert_eq!(
        mtime(ctx, STAMP)?,
        Some(stamp),
        "the dry run wrote the stamp"
    );
    Ok(())
}

/// `pkgcache.bin` is not a refresh: any `apt-get install` rewrites it
/// without fetching an index, so lists two days old stay stale after one.
/// With the images' `docker-clean` removed, apt keeps `pkgcache.bin` as a
/// stock install does.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn an_install_that_rewrites_pkgcache_does_not_freshen_the_lists(ctx: &mut Ctx) -> Result<()> {
    const DOCKER_CLEAN: &str = "/etc/apt/apt.conf.d/docker-clean";
    if ctx.sys().exists(DOCKER_CLEAN)? {
        ctx.sys().remove(DOCKER_CLEAN)?;
    }
    apt_get_update(ctx)?;
    make_lists_stale(ctx)?;
    let pkgcache = back_date(ctx, PKGCACHE, TWO_DAYS)?;

    // An install with no update, outside any op.
    ctx.sys()
        .cmd("apt-get")
        .args(["install", "-y", "--no-install-recommends", "hello"])
        .env("DEBIAN_FRONTEND", "noninteractive")
        .run()?;
    let rewritten = mtime(ctx, PKGCACHE)?.expect("apt keeps pkgcache.bin without docker-clean");
    assert_ne!(
        rewritten, pkgcache,
        "the install did not rewrite pkgcache.bin; the rest of this test proves nothing"
    );
    assert!(
        now() - rewritten < 600,
        "pkgcache.bin is not fresh: {rewritten}"
    );

    // The lists are as stale as they were: the dry run cannot decide.
    let Plan::Change(c) = apt::Latest::new(["sl"])
        .update_cache(HOUR)
        .check(dry(ctx).sys())?
    else {
        panic!("expected a change: the lists are two days old");
    };
    let rendered = c.diff().render();
    assert!(
        rendered.contains("candidate versions unknown") && rendered.contains("2d"),
        "the dry run took the lists for fresh after an install rewrote pkgcache.bin: {rendered}"
    );
    Ok(())
}

/// An `apt-get update` that cannot reach its mirror exits 0, warning
/// "Failed to fetch" and "Some index files failed to download". That
/// refreshed nothing, so it writes no stamp, and the lists stay stale. The
/// only source is an unreachable one (nothing listens on port 9), and the
/// op is `Latest` of `apt`, which is installed and whose candidate the
/// unreachable source cannot change, so the step is `ok` once the refresh
/// is behind it.
#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn an_update_that_fetched_nothing_writes_no_stamp(ctx: &mut Ctx) -> Result<()> {
    apt_get_update(ctx)?;
    let stamp = make_lists_stale(ctx)?;
    ctx.sys()
        .cmd("sh")
        .args([
            "-c",
            "mkdir -p /root/sources.off && \
             mv /etc/apt/sources.list.d/* /root/sources.off/ && \
             rm -f /etc/apt/sources.list",
        ])
        .run()?;
    ctx.sys().write_atomic(
        "/etc/apt/sources.list",
        b"deb http://127.0.0.1:9/debian bookworm main\n",
    )?;
    let partial = back_date(ctx, PARTIAL, TWO_DAYS)?;

    let op = || apt::Latest::new(["apt"]).update_cache(HOUR);
    let r = ctx.step("apt latest, mirror unreachable", op())?;
    assert!(!r.changed, "the step changed something");
    assert_ne!(
        mtime(ctx, PARTIAL)?,
        Some(partial),
        "the step should have run apt-get update"
    );
    assert_eq!(
        mtime(ctx, STAMP)?,
        Some(stamp),
        "an update that fetched nothing wrote the stamp"
    );

    // The next dry run still sees the lists two days old.
    let Plan::Change(c) = op().check(dry(ctx).sys())? else {
        panic!("expected a change: no refresh happened, so the lists are stale");
    };
    let rendered = c.diff().render();
    assert!(
        rendered.contains("candidate versions unknown"),
        "{rendered}"
    );
    Ok(())
}
