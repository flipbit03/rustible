//! Docker integration test for `ssh::authorized_keys` (vision 8, tier 3):
//! `Present` (plain and `exclusive`) and `Absent` for a user created in the
//! same test, with the file mode, `.ssh` mode and ownership checked on the
//! real filesystem. Runs with
//! `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_authorized_keys`.

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::ssh::authorized_keys;
use rustible_std::{file, user};

const K1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE cadu@x86";
const K2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITWO cadu@arm";
const STRANGER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOLD someone@else";

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn authorized_keys_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    // The vision 6.1 playbook, step by step.
    let account = ctx.step("user", user::Present::new("rustible-ak").shell("/bin/bash"))?;
    assert!(account.changed);
    let ssh_dir = account.home.join(".ssh");
    let keys_file = ssh_dir.join("authorized_keys");

    // `~/.ssh` is the playbook's job (vision 6.7, decided for this op).
    let err = ctx
        .step(
            "keys without ~/.ssh",
            authorized_keys::Present::for_user(&account).keys([K1]),
        )
        .unwrap_err()
        .chain();
    assert!(err.contains(".ssh"), "{err}");
    ctx.step(
        "~/.ssh",
        file::Directory::at(&ssh_dir)
            .owner(account.uid, account.gid)
            .mode(0o700),
    )?;

    let (first, second) = changed_then_ok(ctx, "install keys", || {
        authorized_keys::Present::for_user(&account).keys([K1, K2])
    })?;
    assert_eq!(first.path, keys_file);
    assert_eq!(first.added.len(), 2);
    assert_eq!(second.already_present.len(), 2);
    let st = ctx.sys().stat(&keys_file)?.expect("file exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o600, account.uid, account.gid));
    let st = ctx.sys().stat(&ssh_dir)?.expect("dir exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o700, account.uid, account.gid));
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
    let st = ctx.sys().stat(&keys_file)?.expect("file exists");
    assert_eq!(
        (st.mode, st.uid),
        (0o600, account.uid),
        "rewrite keeps attrs"
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
