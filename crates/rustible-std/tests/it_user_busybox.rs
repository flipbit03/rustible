//! Docker integration test for `user` and `group` on BusyBox (vision 8, tier
//! 3). The Debian and Ubuntu legs live in `it_user_group`; Alpine is a
//! separate binary because BusyBox is a different toolset, not a variation:
//! `adduser`/`addgroup` instead of `useradd`/`groupadd`, and no `usermod` at
//! all. Two of this branch's findings only exist here, so a Fake test is not
//! enough evidence for either:
//!
//! * `user::Present` must not predict a shell it was not given, because
//!   BusyBox `adduser` takes it from `$SHELL`, else the invoking user's own
//!   passwd entry, neither of which the op can see. The test shows both
//!   answers on the same image, so `/bin/sh` was a guess, not a default.
//! * a group named after a new account must become that account's primary
//!   group, because BusyBox `adduser` dies with "group name is in use" when
//!   it tries to create a private group that already exists.
//!
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_user_busybox`.

use std::path::Path;

use std::sync::Arc;

use rustible::prelude::*;
use rustible::sdk::event::Collect;
use rustible::sdk::testing::changed_then_ok;
use rustible::sdk::{HostInfo, System};
use rustible_std::shell::Command;
use rustible_std::{group, user};

fn passwd_line(ctx: &mut Ctx, name: &str) -> Result<Option<String>> {
    Ok(ctx
        .sys()
        .read_to_string("/etc/passwd")?
        .lines()
        .find(|l| l.split(':').next() == Some(name))
        .map(str::to_string))
}

#[rustible::integration_test(images = ["alpine:3.20"])]
fn busybox_user_and_group(ctx: &mut Ctx) -> Result<()> {
    assert!(ctx.sys().is_root());

    // No `.shell()`: the op must neither predict the account nor name a shell
    // in the diff, because on BusyBox it cannot know which shell it will get.
    // Prediction only means anything in check mode, so this claim is made
    // against a dry `Ctx` over the same real machine; a plain `ctx.step` here
    // would report `predicted: false` no matter what the op decided, and the
    // assertion would hold even if the `/bin/sh` guess came back.
    let mut dry = Ctx::new(
        System::local(true, Arc::new(Collect::default())),
        HostInfo::local(),
    );
    // uid and gid are pinned in both dry steps (`users` is gid 100 on this
    // image), so the shell is the only thing left that can block a
    // prediction. Without one: no prediction, and no `shell=` in the diff.
    let planned = dry.step(
        "dry user without a shell",
        user::Present::new("rustible-ash").uid(4100).gid("users"),
    )?;
    assert!(
        planned.changed && !planned.predicted,
        "an unknown shell blocks prediction (vision 12)"
    );
    let short = planned.diff.as_ref().unwrap().short();
    assert!(
        !short.contains("shell="),
        "the diff must not name a shell the op was not given: {short}"
    );
    // With one, everything is knowable and the step predicts. The only
    // difference between the two steps is `.shell()`, which is what makes the
    // assertion above a real one rather than a tautology.
    let planned = dry.step(
        "dry user with a shell",
        user::Present::new("rustible-ash")
            .uid(4100)
            .gid("users")
            .shell("/bin/sh"),
    )?;
    assert!(planned.predicted, "an explicit shell is knowable");
    assert_eq!(planned.shell, Path::new("/bin/sh"));
    assert!(
        planned
            .diff
            .as_ref()
            .unwrap()
            .short()
            .contains("shell=/bin/sh"),
        "{:?}",
        planned.diff
    );

    // Now for real. The second, read-back run reports the shell BusyBox chose.
    let (account, _) = changed_then_ok(ctx, "user without a shell", || {
        user::Present::new("rustible-ash")
    })?;
    let line = passwd_line(ctx, "rustible-ash")?.expect("account created");
    let real_shell = line.rsplit(':').next().unwrap();
    assert_eq!(
        real_shell,
        account.shell.display().to_string(),
        "the second, read-back run reports the shell BusyBox chose"
    );

    // Why the op refuses to guess: the same `adduser -D`, with nothing but
    // `$SHELL` set in its environment, produces a different shell. The op
    // cannot see the environment it will be run under, so `/bin/sh` (what it
    // used to predict) is only right by accident.
    ctx.step(
        "adduser under SHELL=/bin/ash",
        Command::new("adduser")
            .args(["-D", "rustible-env"])
            .env("SHELL", "/bin/ash"),
    )?;
    assert!(
        passwd_line(ctx, "rustible-env")?
            .unwrap()
            .ends_with(":/bin/ash"),
        "BusyBox took the shell from $SHELL"
    );
    assert_eq!(
        real_shell, "/bin/sh",
        "and with no $SHELL it took root's, which happens to be /bin/sh here"
    );

    // An explicit shell is still predicted and still applied (`adduser -s`).
    let (explicit, _) = changed_then_ok(ctx, "user with a shell", || {
        user::Present::new("rustible-sh").shell("/bin/sh")
    })?;
    assert_eq!(explicit.shell, Path::new("/bin/sh"));
    assert!(
        passwd_line(ctx, "rustible-sh")?
            .unwrap()
            .ends_with(":/bin/sh")
    );

    // The vision 6.1 shape: a group of the account's name created first.
    // Without the fix, `adduser` dies with "group name 'x' is in use".
    let (grp, _) = changed_then_ok(ctx, "same-named group", || {
        group::Present::new("rustible-same")
    })?;
    let (same, _) = changed_then_ok(ctx, "same-named user", || {
        user::Present::new("rustible-same").shell("/bin/sh")
    })?;
    assert_eq!(
        same.gid, grp.gid,
        "the existing group became the primary group (`adduser -G`)"
    );

    // A supplementary group goes on with `addgroup <user> <group>`.
    let (other, _) = changed_then_ok(ctx, "group", || group::Present::new("rustible-grp"))?;
    changed_then_ok(ctx, "membership", || {
        user::Membership::of(&same).in_group(&other)
    })?;
    let etc_group = ctx.sys().read_to_string("/etc/group")?;
    assert!(
        etc_group
            .lines()
            .any(|l| l.starts_with("rustible-grp:") && l.ends_with(":rustible-same")),
        "{etc_group}"
    );

    // Vision 6.7 on BusyBox too: a missing group is refused, not created.
    let err = ctx
        .step(
            "user with missing group",
            user::Present::new("rustible-nope").groups(["rustible-missing"]),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("does not exist"), "{err}");
    assert!(passwd_line(ctx, "rustible-nope")?.is_none());

    // BusyBox has no `usermod`, so changing an existing account's attributes
    // is refused with a message that says why rather than silently skipped.
    let err = ctx
        .step(
            "modify an existing account",
            user::Present::new("rustible-sh").comment("Rustible"),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("BusyBox"), "{err}");

    // Absent through `deluser`/`delgroup`.
    for u in [
        "rustible-ash",
        "rustible-env",
        "rustible-sh",
        "rustible-same",
    ] {
        changed_then_ok(ctx, "user absent", || {
            user::Absent::new(u).remove_home(true)
        })?;
        assert!(passwd_line(ctx, u)?.is_none());
    }
    changed_then_ok(ctx, "group absent", || group::Absent::new("rustible-grp"))?;
    Ok(())
}
