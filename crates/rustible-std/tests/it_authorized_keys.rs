//! Docker integration test for `ssh::authorized_keys` (vision 8, tier 3):
//! `Present` (plain and `exclusive`) and `Absent` for a user created in the
//! same test, with the file mode, `.ssh` mode and ownership checked on the
//! real filesystem. Runs with
//! `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_authorized_keys`.
//!
//! What this tier adds over the `Fake`, having checked rather than assumed:
//! a missing `mkdir_all` *is* caught at tier 2 (the fake's `set_mode` errors
//! on an absent path), so that is not the reason. The reasons are real
//! `chmod`/`chown` semantics on a real inode, a real `/etc/passwd` that
//! `useradd` wrote, the fact that `useradd -m` does not make `~/.ssh` — which
//! is the premise the whole change rests on and which only a real `useradd`
//! can establish — and symlink resolution through a directory component,
//! which `Fake::resolve` does not model.

use std::sync::Arc;

use rustible::prelude::*;
use rustible::sdk::event::Collect;
use rustible::sdk::testing::changed_then_ok;
use rustible::sdk::{HostInfo, System};
use rustible_std::ssh::authorized_keys;
use rustible_std::user;

const K1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE cadu@x86";
const K2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITWO cadu@arm";
const STRANGER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOLD someone@else";

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn authorized_keys_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    // The vision 6.1 playbook, minus its `file::Directory` step: `~/.ssh` is
    // the keys step's own business now (issue #40). Vision 6.1 itself still
    // shows three steps and is unamended — the amendment is proposed, not
    // applied, because CLAUDE.md reserves that edit for the author.
    let account = ctx.step("user", user::Present::new("rustible-ak").shell("/bin/bash"))?;
    assert!(account.changed);
    let ssh_dir = account.home.join(".ssh");
    let keys_file = ssh_dir.join("authorized_keys");
    ensure!(
        !ctx.sys().exists(&ssh_dir)?,
        "useradd -m does not make ~/.ssh; if it did, this test would prove nothing"
    );

    let (first, second) = changed_then_ok(ctx, "install keys", || {
        authorized_keys::Present::for_user(&account).keys([K1, K2])
    })?;
    assert_eq!(first.path, keys_file);
    assert_eq!(first.added.len(), 2);
    assert_eq!(first.created_dir.as_ref(), Some(&ssh_dir));
    assert_eq!(second.already_present.len(), 2);
    assert_eq!(second.created_dir, None, "only the first pass creates it");

    // The whole point: the directory is there, 0700, owned by the account,
    // with the file inside it.
    let st = ctx.sys().stat(&ssh_dir)?.expect("dir exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o700, account.uid, account.gid));
    let st = ctx.sys().stat(&keys_file)?.expect("file exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o600, account.uid, account.gid));
    assert_eq!(
        ctx.sys().read_to_string(&keys_file)?,
        format!("{K1}\n{K2}\n")
    );

    // A stranger's key appears; `exclusive` removes it and keeps ours.
    ctx.sys()
        .write_atomic(&keys_file, format!("{K1}\n{STRANGER}\n{K2}\n").as_bytes())?;
    let (first, _) = changed_then_ok(ctx, "exactly these keys", || {
        authorized_keys::Present::for_user(&account)
            .exclusive(true)
            .keys([K1, K2])
    })?;
    assert_eq!(first.removed.len(), 1);
    assert_eq!(first.removed[0].comment.as_deref(), Some("someone@else"));
    assert!(first.added.is_empty());
    assert_eq!(
        ctx.sys().read_to_string(&keys_file)?,
        format!("{K1}\n{K2}\n")
    );

    // Absent: one key goes, the other stays. Ansible gates its
    // directory-and-ownership pass on `do_write`, so a revocation that writes
    // takes it along — break both first and watch the removal fix them.
    ctx.sys().set_mode(&ssh_dir, 0o755)?;
    ctx.sys().set_mode(&keys_file, 0o644)?;
    let (first, second) = changed_then_ok(ctx, "revoke a key", || {
        authorized_keys::Absent::for_user(&account).keys([K1])
    })?;
    assert_eq!(first.removed.len(), 1);
    assert_eq!(second.not_present.len(), 1);
    assert_eq!(ctx.sys().read_to_string(&keys_file)?, format!("{K2}\n"));
    let st = ctx.sys().stat(&ssh_dir)?.expect("dir exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o700, account.uid, account.gid));
    let st = ctx.sys().stat(&keys_file)?.expect("file exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o600, account.uid, account.gid));

    // But a revocation with nothing to revoke writes nothing, so it takes no
    // pass with it: Ansible's `do_write` stays false and so does ours.
    ctx.sys().set_mode(&keys_file, 0o644)?;
    let r = ctx.step(
        "revoke a key that is already gone",
        authorized_keys::Absent::for_user(&account).keys([K1]),
    )?;
    assert!(!r.changed);
    assert_eq!(
        ctx.sys().stat(&keys_file)?.expect("file exists").mode,
        0o644,
        "a revocation that removes nothing must not repair anything"
    );
    ctx.sys().set_mode(&keys_file, 0o600)?;

    // Same file by name, as a playbook without the account in scope does it.
    let r = ctx.step(
        "by user name",
        authorized_keys::Present::for_user_name("rustible-ak").keys([K2]),
    )?;
    assert!(!r.changed);
    Ok(())
}

/// The attribute half, against a real `chmod`/`chown`.
///
/// The `.ssh` is planted 0775 deliberately: that is group-writable, which is
/// the state `sshd(8)` actually refuses to read keys out of under
/// `StrictModes` (its manual: writable by other users means "sshd will not
/// allow it to be used"). So this is the case where installing keys and
/// leaving the mode alone reports a clean `changed` over an account that
/// still cannot log in.
///
/// Ansible repairs these only on a run that is already rewriting the file;
/// here the keys are already correct, so this is the case its `do_write` gate
/// misses and the reason `Present` checks on every run.
#[rustible::integration_test(images = ["debian:12"])]
fn wrong_modes_and_ownership_are_repaired(ctx: &mut Ctx) -> Result<()> {
    let account = ctx.step(
        "user",
        user::Present::new("rustible-ak2").shell("/bin/bash"),
    )?;
    let ssh_dir = account.home.join(".ssh");
    let keys_file = ssh_dir.join("authorized_keys");

    // A `.ssh` and a key file as a careless hand would leave them: world
    // readable, group writable, owned by root.
    ctx.sys().mkdir_all(&ssh_dir)?;
    ctx.sys().set_mode(&ssh_dir, 0o775)?;
    ctx.sys().set_owner(&ssh_dir, 0, 0)?;
    ctx.sys()
        .write_atomic(&keys_file, format!("{K1}\n").as_bytes())?;
    ctx.sys().set_mode(&keys_file, 0o644)?;
    ctx.sys().set_owner(&keys_file, 0, 0)?;

    // The keys are already right, so the *only* thing wrong is the
    // attributes — and that is still a change.
    let (first, _) = changed_then_ok(ctx, "repair", || {
        authorized_keys::Present::for_user(&account).keys([K1])
    })?;
    assert!(first.added.is_empty(), "no key moved: only attributes");
    assert_eq!(first.created_dir, None, "the directory was already there");

    let st = ctx.sys().stat(&ssh_dir)?.expect("dir exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o700, account.uid, account.gid));
    let st = ctx.sys().stat(&keys_file)?.expect("file exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o600, account.uid, account.gid));
    // The contents were never rewritten.
    assert_eq!(ctx.sys().read_to_string(&keys_file)?, format!("{K1}\n"));
    Ok(())
}

/// The two refusals worth keeping, and the one line this op will not cross.
#[rustible::integration_test(images = ["debian:12"])]
fn refusals_that_need_a_human(ctx: &mut Ctx) -> Result<()> {
    // No home: `mkdir_all` would have made it root-owned and 0755, which is
    // an account that cannot log in. Ansible's module fails here too, using
    // `os.mkdir` rather than `os.makedirs`.
    let homeless = ctx.step(
        "user without a home",
        user::Present::new("rustible-ak3")
            .shell("/bin/bash")
            .create_home(false),
    )?;
    ensure!(
        !ctx.sys().exists(&homeless.home)?,
        "create_home(false) left a home behind; this case is not being tested"
    );
    let err = ctx
        .step(
            "keys with no home",
            authorized_keys::Present::for_user(&homeless).keys([K1]),
        )
        .unwrap_err()
        .chain();
    assert!(
        err.contains(&format!(
            "home directory {} does not exist",
            homeless.home.display()
        )),
        "must be the missing-home refusal, not the not-absolute one: {err}"
    );
    assert!(err.contains("create_home(true)"), "names the fix: {err}");
    assert!(
        !ctx.sys().exists(&homeless.home)?,
        "the refusal must not have created the home on its way out"
    );

    // A dangling `.ssh` symlink: `mkdir` would fail with a bare EEXIST on a
    // path a "does not exist" message had just named.
    let linked = ctx.step(
        "user with a linked .ssh",
        user::Present::new("rustible-ak4"),
    )?;
    let ssh_dir = linked.home.join(".ssh");
    ctx.sys().symlink("/mnt/gone/ssh", &ssh_dir)?;
    let err = ctx
        .step(
            "keys behind a dangling link",
            authorized_keys::Present::for_user(&linked).keys([K1]),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("symlink pointing at something"), "{err}");

    // And something that is not a directory at all.
    let blocked = ctx.step(
        "user with a file in the way",
        user::Present::new("rustible-ak5"),
    )?;
    let ssh_path = blocked.home.join(".ssh");
    ctx.sys().write_atomic(&ssh_path, b"not a directory\n")?;
    let err = ctx
        .step(
            "keys with a file in the way",
            authorized_keys::Present::for_user(&blocked).keys([K1]),
        )
        .unwrap_err()
        .chain();
    assert!(
        err.contains(&format!(
            "{} exists and is not a directory",
            ssh_path.display()
        )),
        "must name the .ssh path, not some other parent: {err}"
    );
    Ok(())
}

/// Symlink resolution through a directory component, which the `Fake` does
/// not model: `Fake::resolve` follows only a path's final component, so tier
/// 2 cannot say where the file actually lands when `~/.ssh` is a link. Here a
/// real kernel answers.
#[rustible::integration_test(images = ["debian:12"])]
fn symlinked_ssh_dir_is_followed_to_the_real_directory(ctx: &mut Ctx) -> Result<()> {
    let account = ctx.step("user", user::Present::new("rustible-ak7"))?;
    let ssh_dir = account.home.join(".ssh");
    ctx.sys().mkdir_all("/srv/keys/rustible-ak7")?;
    ctx.sys().set_mode("/srv/keys/rustible-ak7", 0o755)?;
    ctx.sys().symlink("/srv/keys/rustible-ak7", &ssh_dir)?;

    let (first, _) = changed_then_ok(ctx, "keys behind a link", || {
        authorized_keys::Present::for_user(&account).keys([K1])
    })?;
    assert_eq!(first.created_dir, None, "the link resolves to a directory");

    // The file is in the real directory, and the mode landed on the target
    // rather than on the link.
    let real = ctx.sys().stat("/srv/keys/rustible-ak7")?.expect("target");
    assert_eq!(
        (real.mode, real.uid, real.gid),
        (0o700, account.uid, account.gid)
    );
    assert_eq!(
        ctx.sys()
            .read_to_string("/srv/keys/rustible-ak7/authorized_keys")?,
        format!("{K1}\n")
    );
    let link = ctx.sys().stat(&ssh_dir)?.expect("link");
    assert_eq!(
        link.kind,
        rustible::sdk::backend::FileKind::Symlink,
        "the link itself was not replaced"
    );
    Ok(())
}

/// The reason issue #40 was filed, proven on a real machine rather than
/// against a fake: a dry run of a **first provision** must not fail.
///
/// This used to be the wart. `--check` runs `check` and stops, so the
/// account's home does not exist when the keys step is reached, and an op
/// that stats the filesystem sees a machine the real run would never present
/// it with. The old refusal told the author to add a `file::Directory` step —
/// advice they had already taken — and sent them hunting for a bug in a
/// playbook that converges in one pass.
///
/// Check mode only means anything against a dry `Ctx`, since harness bodies
/// run with it off, so this builds one over the same container.
#[rustible::integration_test(images = ["debian:12"])]
fn a_dry_run_of_a_first_provision_does_not_fail(ctx: &mut Ctx) -> Result<()> {
    // An account with no home at all: the shape a dry run sees before
    // `user::Present` has run for real.
    let account = ctx.step(
        "user without a home",
        user::Present::new("rustible-ak6")
            .shell("/bin/bash")
            .create_home(false),
    )?;
    ensure!(!ctx.sys().exists(&account.home)?, "the home must be absent");

    let mut dry = Ctx::new(
        System::local(true, Arc::new(Collect::default())),
        HostInfo::local(),
    );
    let planned = dry.step(
        "dry keys",
        authorized_keys::Present::for_user(&account).keys([K1]),
    )?;
    assert!(planned.changed, "the dry run reports work, not a failure");
    assert_eq!(
        planned.created_dir.as_ref(),
        Some(&account.home.join(".ssh")),
        "and says it would make the directory"
    );
    let short = planned.diff.as_ref().unwrap().short();
    assert!(short.contains("exists=yes"), "{short}");
    assert!(short.contains("mode=0700"), "{short}");
    assert!(
        short.contains(&format!("owner={}:{}", account.uid, account.gid)),
        "a dry run that silently dropped the ownership line would still log in \
         nowhere: {short}"
    );

    // Nothing was touched: check mode is a dry run, not a rehearsal.
    assert!(!ctx.sys().exists(&account.home)?);

    // And the same op in a run that can act still refuses, so the tolerance
    // above is confined to the mode that writes nothing.
    let err = ctx
        .step(
            "real keys",
            authorized_keys::Present::for_user(&account).keys([K1]),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains("home directory"), "{err}");
    Ok(())
}
