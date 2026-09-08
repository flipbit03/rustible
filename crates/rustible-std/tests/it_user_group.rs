//! Docker integration test for `user` and `group` (vision 8, tier 3): the
//! real `groupadd`/`useradd`/`usermod`/`userdel` as root, with `/etc/passwd`
//! and `/etc/group` checked afterwards, plus a check-mode dry run of the
//! vision 6.6 shape (group step, then user step) over the real machine.
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_user_group`.

use std::path::Path;
use std::sync::Arc;

use rustible::prelude::*;
use rustible::sdk::event::Collect;
use rustible::sdk::testing::changed_then_ok;
use rustible::sdk::{HostInfo, System};
use rustible_std::{group, user};

// Distinct names: `useradd` on Debian creates a private group named after
// the user, so a group step with the user's name would make it fail.
const GRP: &str = "rustible-grp";
const GRP2: &str = "rustible-grp2";
const USR: &str = "rustible-usr";

fn line_of(text: &str, name: &str) -> Option<String> {
    text.lines()
        .find(|l| l.split(':').next() == Some(name))
        .map(str::to_string)
}

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn users_and_groups_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    assert!(ctx.sys().is_root());

    // group::Present, then user::Present with a supplementary group.
    let (grp, _) = changed_then_ok(ctx, "group", || group::Present::new(GRP))?;
    let etc_group = ctx.sys().read_to_string("/etc/group")?;
    assert_eq!(
        line_of(&etc_group, GRP).as_deref(),
        Some(format!("{GRP}:x:{}:", grp.gid).as_str())
    );

    let (account, second) = changed_then_ok(ctx, "user", || {
        user::Present::new(USR)
            .shell("/bin/bash")
            .groups([GRP])
            .comment("Rustible test")
    })?;
    assert_eq!(*second, *account, "the second run reads the same account");
    assert_eq!(account.shell, Path::new("/bin/bash"));
    assert_eq!(account.home, Path::new("/home/rustible-usr"));
    assert_eq!(account.groups, vec![GRP]);
    assert!(
        ctx.sys().exists(&account.home)?,
        "create_home defaults to true"
    );
    let passwd = ctx.sys().read_to_string("/etc/passwd")?;
    assert_eq!(
        line_of(&passwd, USR).as_deref(),
        Some(
            format!(
                "{USR}:x:{}:{}:Rustible test:/home/rustible-usr:/bin/bash",
                account.uid, account.gid
            )
            .as_str()
        )
    );
    let etc_group = ctx.sys().read_to_string("/etc/group")?;
    assert_eq!(
        line_of(&etc_group, GRP).as_deref(),
        Some(format!("{GRP}:x:{}:{USR}", grp.gid).as_str())
    );

    // Modify an existing account: shell and comment through usermod.
    let (modified, _) = changed_then_ok(ctx, "user modified", || {
        user::Present::new(USR).shell("/bin/sh").comment("Rustible")
    })?;
    assert_eq!(modified.shell, Path::new("/bin/sh"));
    assert_eq!(modified.groups, vec![GRP], "memberships untouched");
    let passwd = ctx.sys().read_to_string("/etc/passwd")?;
    assert!(
        line_of(&passwd, USR)
            .unwrap()
            .ends_with(":Rustible:/home/rustible-usr:/bin/sh"),
        "{passwd}"
    );

    // A group with a fixed gid, then Membership chained from both outputs.
    let (grp2, _) = changed_then_ok(ctx, "group with gid", || {
        group::Present::new(GRP2).gid(4242)
    })?;
    assert_eq!(grp2.gid, 4242);
    changed_then_ok(ctx, "membership", || {
        user::Membership::of(&account).in_group(&grp2)
    })?;
    let etc_group = ctx.sys().read_to_string("/etc/group")?;
    assert_eq!(
        line_of(&etc_group, GRP2).as_deref(),
        Some(format!("{GRP2}:x:4242:{USR}").as_str())
    );
    let looked_up = ctx.step("existing", user::Existing::named(USR))?;
    assert!(!looked_up.changed);
    assert_eq!(looked_up.groups, vec![GRP, GRP2]);

    // Vision 6.7 against the real tools: a missing group is refused, not created.
    let err = ctx
        .step(
            "user with missing group",
            user::Present::new("rustible-nope").groups(["rustible-missing"]),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("does not exist"), "{err}");
    assert!(line_of(&ctx.sys().read_to_string("/etc/passwd")?, "rustible-nope").is_none());

    // Check mode over the real machine (vision 6.6 shape): the group step
    // reports "would create" and the user, membership and gid steps that
    // depend on it continue instead of aborting the dry run.
    let mut dry = Ctx::new(
        System::local(true, Arc::new(Collect::default())),
        HostInfo::local(),
    );
    let planned = dry.step("dry group", group::Present::new("rustible-dry"))?;
    assert!(
        planned.changed && !planned.is_available(),
        "no gid to predict"
    );
    let dry_user = dry.step(
        "dry user in planned group",
        user::Present::new("rustible-dry-usr").groups(["rustible-dry"]),
    )?;
    assert!(dry_user.changed && !dry_user.is_available());
    let member = dry.step(
        "dry membership in planned group",
        user::Membership::of(&account).in_group_named("rustible-dry"),
    )?;
    assert!(member.changed && member.is_available());
    let planned_gid = dry.step(
        "dry group with gid",
        group::Present::new("rustible-dry2").gid(4343),
    )?;
    assert!(planned_gid.changed && planned_gid.predicted);
    let dry_user2 = dry.step(
        "dry user with planned primary group",
        user::Present::new("rustible-dry-usr2")
            .uid(4343)
            .gid(&*planned_gid)
            .shell("/bin/sh"),
    )?;
    assert!(dry_user2.predicted);
    assert_eq!(dry_user2.gid, 4343);
    // The same steps fail outside check mode, and nothing was created.
    let err = ctx
        .step(
            "real user in planned-only group",
            user::Present::new("rustible-dry-usr").groups(["rustible-dry"]),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("does not exist"), "{err}");
    let etc_group = ctx.sys().read_to_string("/etc/group")?;
    assert!(line_of(&etc_group, "rustible-dry").is_none());
    assert!(line_of(&etc_group, "rustible-dry2").is_none());

    // Absent: the user with its home, then the groups.
    let (removed, _) = changed_then_ok(ctx, "user absent", || {
        user::Absent::new(USR).remove_home(true)
    })?;
    assert_eq!(
        removed.home.as_deref(),
        Some(Path::new("/home/rustible-usr"))
    );
    assert!(!ctx.sys().exists("/home/rustible-usr")?);
    assert!(line_of(&ctx.sys().read_to_string("/etc/passwd")?, USR).is_none());
    for g in [GRP, GRP2] {
        let (removed, _) = changed_then_ok(ctx, "group absent", || group::Absent::new(g))?;
        assert!(removed.gid.is_some());
    }
    let etc_group = ctx.sys().read_to_string("/etc/group")?;
    assert!(line_of(&etc_group, GRP).is_none() && line_of(&etc_group, GRP2).is_none());
    // useradd's private group went with the user.
    assert!(line_of(&etc_group, USR).is_none(), "{etc_group}");
    Ok(())
}
