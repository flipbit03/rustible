//! Docker integration test for `ssh::authorized_keys` (vision 8, tier 3):
//! `Present` (plain and `exclusive`) and `Absent` for a user created in the
//! same test, with the file mode, `.ssh` mode and ownership checked on the
//! real filesystem. Runs with
//! `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_authorized_keys`.
//!
//! This tier is the only one that can prove the directory work. The `Fake`'s
//! `write` does not need a parent to exist, so at tier 2 a `check` that
//! planned the directory and an `apply` that forgot to create it look
//! identical; here the write fails. Real `mkdir`, `chmod` and `chown`, and a
//! real `/etc/passwd`, are also what says whether the attribute repair does
//! on a machine what it does against the fake.

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
    // The vision 6.1 playbook, which is now two steps rather than three:
    // `~/.ssh` is the keys step's own business (issue #40).
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

    // Absent: one key goes, the other stays.
    let (first, second) = changed_then_ok(ctx, "revoke a key", || {
        authorized_keys::Absent::for_user(&account).keys([K1])
    })?;
    assert_eq!(first.removed.len(), 1);
    assert_eq!(second.not_present.len(), 1);
    assert_eq!(ctx.sys().read_to_string(&keys_file)?, format!("{K2}\n"));

    // Same file by name, as a playbook without the account in scope does it.
    let r = ctx.step(
        "by user name",
        authorized_keys::Present::for_user_name("rustible-ak").keys([K2]),
    )?;
    assert!(!r.changed);
    Ok(())
}

/// The attribute half, against a real `chmod`/`chown`. sshd's `StrictModes`
/// refuses keys out of a group-writable `.ssh` or a file the account does not
/// own, so an op that installed keys and left those alone would report a
/// clean `changed` over an account that still cannot log in.
/// `ansible.posix.authorized_key` repairs both on every run; so does this.
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
    assert!(err.contains("home directory"), "{err}");
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
    assert!(err.contains("exists and is not a directory"), "{err}");
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
