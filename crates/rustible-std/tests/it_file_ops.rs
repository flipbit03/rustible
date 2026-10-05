//! Docker integration test for the `file` family (vision 8, T2):
//! `Directory`, `Copy`, `Attrs`, `Symlink`, `Line`, `Block`, `Absent`, each
//! applied twice on the real filesystem. Runs with
//! `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_file_ops`.

use std::path::Path;

use rustible::prelude::*;
use rustible::sdk::backend::FileKind;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::file;

fn mode(ctx: &Ctx, p: &str) -> Result<u32> {
    Ok(ctx.sys().stat(p)?.expect("exists").mode)
}

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn file_family_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    let dir = "/etc/rustible-test/files";
    let conf = "/etc/rustible-test/files/app.conf";
    let link = "/etc/rustible-test/files/app.link";
    let cfg = "/etc/rustible-test/files/managed.cfg";

    // Directory: created with the asked mode, then ok.
    let (first, _) = changed_then_ok(ctx, "directory", || {
        file::Directory::at(dir).mode(0o750).owner(0, 0)
    })?;
    assert!(first.created);
    assert_eq!(mode(ctx, dir)?, 0o750);
    // Mode drift is repaired.
    ctx.sys().set_mode(dir, 0o755)?;
    let (again, _) = changed_then_ok(ctx, "directory mode", || {
        file::Directory::at(dir).mode(0o750)
    })?;
    assert!(!again.created);

    // Copy: content and mode, then a content change with a backup.
    let (first, _) = changed_then_ok(ctx, "copy", || {
        file::Copy::from_str("port = 80\n").to(conf).mode(0o640)
    })?;
    assert!(first.content_changed && first.backup_path.is_none());
    assert_eq!(first.bytes, 10);
    assert_eq!(ctx.sys().read_to_string(conf)?, "port = 80\n");
    assert_eq!(mode(ctx, conf)?, 0o640);
    let (second, _) = changed_then_ok(ctx, "copy new content", || {
        file::Copy::from_str("port = 8080\n")
            .to(conf)
            .mode(0o640)
            .backup(true)
    })?;
    let backup = second.backup_path.clone().expect("a backup was taken");
    assert_eq!(ctx.sys().read_to_string(&backup)?, "port = 80\n");
    assert_eq!(ctx.sys().read_to_string(conf)?, "port = 8080\n");
    // Attributes only: the file is not rewritten.
    let (third, _) = changed_then_ok(ctx, "copy mode only", || {
        file::Copy::from_str("port = 8080\n").to(conf).mode(0o600)
    })?;
    assert!(!third.content_changed);
    assert_eq!(mode(ctx, conf)?, 0o600);

    // Setuid survives a rewrite. On a real kernel every `chown` of a file
    // clears setuid, and setgid with group execute, even to the ids it
    // already has. With `.mode().owner()`, `apply` sets both again after the
    // rewrite, owner first (`copy_rewrite_of_a_setuid_file_keeps_the_bit` is
    // the `Fake`'s half).
    let suid = "/etc/rustible-test/files/suid";
    changed_then_ok(ctx, "suid v1", || {
        file::Copy::from_str("v1\n")
            .to(suid)
            .mode(0o4755)
            .owner(65534, 65534)
    })?;
    changed_then_ok(ctx, "suid v2", || {
        file::Copy::from_str("v2\n")
            .to(suid)
            .mode(0o4755)
            .owner(65534, 65534)
    })?;
    assert_eq!(mode(ctx, suid)? & 0o7777, 0o4755);

    // Without `.mode()`, and in `Line` and `Block`, which set none, nothing
    // in the op sets the mode again: the rewrite itself has to keep it, by
    // giving the replacement the old owner before the old mode (issue #51).
    // A rewrite that `chown`ed after copying the mode left 0755, and the
    // second run was `ok` all the same, because no mode was asked for.
    changed_then_ok(ctx, "suid v3 without mode", || {
        file::Copy::from_str("v3\n").to(suid)
    })?;
    let st = ctx.sys().stat(suid)?.expect("exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o4755, 65534, 65534));
    // With `.owner()` already right and still no `.mode()`: no `chown` is
    // issued after the rewrite, because one to the same owner clears setuid
    // on this kernel with nothing to set it back.
    changed_then_ok(ctx, "suid v4 owner only", || {
        file::Copy::from_str("v4\n").to(suid).owner(65534, 65534)
    })?;
    let st = ctx.sys().stat(suid)?.expect("exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o4755, 65534, 65534));
    changed_then_ok(ctx, "suid line", || file::Line::in_path(suid).set("exit 0"))?;
    assert_eq!(ctx.sys().read_to_string(suid)?, "v4\nexit 0\n");
    let st = ctx.sys().stat(suid)?.expect("exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o4755, 65534, 65534));
    let sgid = "/etc/rustible-test/files/sgid";
    changed_then_ok(ctx, "sgid v1", || {
        file::Copy::from_str("#!/bin/sh\n").to(sgid).mode(0o2755)
    })?;
    changed_then_ok(ctx, "sgid block", || {
        file::Block::in_path(sgid).set("exit 0")
    })?;
    assert!(ctx.sys().read_to_string(sgid)?.contains("exit 0\n"));
    assert_eq!(mode(ctx, sgid)?, 0o2755);

    // A backup, as root, of another user's setuid file, with a symlink
    // planted at its predictable name (issue #75). The name is
    // `<file>.~rustible.<unix seconds>`, so a link sits at every second from
    // just before now to two minutes on, each pointing at a root file the
    // backup must not write through. The backup goes to a free name with a
    // random suffix instead, is a new regular file with the file's owner,
    // and carries no setuid: `std::fs::copy` wrote the old contents through
    // the link and gave a root-owned copy mode 4755, and a root-owned copy of
    // another user's content is one a tool trusting root-owned files would
    // act on as root.
    let victim = "/etc/rustible-test/files/victim";
    ctx.sys().write_atomic(victim, b"victim\n")?;
    ctx.sys().set_mode(victim, 0o600)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    for ts in now - 2..now + 120 {
        ctx.sys()
            .symlink(victim, format!("{suid}.~rustible.{ts}"))?;
    }
    let (first, _) = changed_then_ok(ctx, "suid backup past a planted link", || {
        file::Copy::from_str("v5\n").to(suid).backup(true)
    })?;
    assert_eq!(ctx.sys().read_to_string(victim)?, "victim\n");
    let st = ctx.sys().stat(victim)?.expect("exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o600, 0, 0), "{:o}", st.mode);
    let backup = first.backup_path.clone().expect("a backup was taken");
    let name = backup.to_string_lossy().into_owned();
    let suffix = name
        .strip_prefix(&format!("{suid}.~rustible."))
        .and_then(|rest| rest.split_once('.'))
        .map(|(_, random)| random);
    assert!(
        suffix.is_some_and(|r| r.len() == 8 && r.chars().all(|c| c.is_ascii_hexdigit())),
        "the planted name is skipped for a random one: {name}"
    );
    let st = ctx.sys().stat(&backup)?.expect("the backup exists");
    assert_eq!(
        (st.kind, st.mode, st.uid, st.gid),
        (FileKind::File, 0o755, 65534, 65534),
        "{:o}",
        st.mode
    );
    assert_eq!(ctx.sys().read_to_string(&backup)?, "v4\nexit 0\n");
    let st = ctx.sys().stat(suid)?.expect("exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o4755, 65534, 65534));

    // Attrs: mode and owner on the existing file (uid 1 = daemon everywhere).
    changed_then_ok(ctx, "attrs", || {
        file::Attrs::at(conf).mode(0o644).owner(1, 1)
    })?;
    let st = ctx.sys().stat(conf)?.expect("exists");
    assert_eq!((st.mode, st.uid, st.gid), (0o644, 1, 1));

    // Symlink: created, then ok; a wrong target is repointed.
    changed_then_ok(ctx, "symlink", || file::Symlink::at(link).pointing_to(conf))?;
    assert_eq!(ctx.sys().read_link(link)?, Path::new(conf));
    assert_eq!(
        ctx.sys().stat(link)?.expect("exists").kind,
        FileKind::Symlink
    );
    changed_then_ok(ctx, "symlink repointed", || {
        file::Symlink::at(link).pointing_to(dir)
    })?;
    assert_eq!(ctx.sys().read_link(link)?, Path::new(dir));

    // Line: insert, then replace by regex.
    let (first, _) = changed_then_ok(ctx, "line", || {
        file::Line::in_path(conf)
            .matching("^port")
            .set("port = 9090")
    })?;
    assert_eq!(first.line_no, 1);
    assert_eq!(ctx.sys().read_to_string(conf)?, "port = 9090\n");
    changed_then_ok(ctx, "line appended", || {
        file::Line::in_path(conf).set("workers = 4")
    })?;
    assert_eq!(
        ctx.sys().read_to_string(conf)?,
        "port = 9090\nworkers = 4\n"
    );

    // Block: created with markers, changed, then removed with an empty block.
    let (first, _) = changed_then_ok(ctx, "block", || {
        file::Block::in_path(cfg).create(true).set("a = 1\nb = 2")
    })?;
    assert_eq!(first.line_no, 1);
    let text = ctx.sys().read_to_string(cfg)?;
    assert!(
        text.contains("BEGIN") && text.contains("a = 1\nb = 2\n") && text.contains("END"),
        "{text}"
    );
    changed_then_ok(ctx, "block changed", || {
        file::Block::in_path(cfg).backup(true).set("a = 2")
    })?;
    let text = ctx.sys().read_to_string(cfg)?;
    assert!(
        text.contains("a = 2\n") && !text.contains("b = 2"),
        "{text}"
    );
    changed_then_ok(ctx, "block removed", || file::Block::in_path(cfg).set(""))?;
    let text = ctx.sys().read_to_string(cfg)?;
    assert!(!text.contains("BEGIN"), "{text:?}");

    // Absent: a link goes without its target; a populated directory is
    // refused unless recursive.
    let (first, second) = changed_then_ok(ctx, "absent link", || file::Absent::at(link))?;
    assert!(first.removed && !second.removed);
    assert!(ctx.sys().exists(dir)?, "the link's target stays");
    let err = ctx
        .step("absent populated dir", file::Absent::at(dir))
        .unwrap_err()
        .chain();
    assert!(err.contains("recursive"), "{err}");
    assert!(ctx.sys().exists(conf)?);
    changed_then_ok(ctx, "absent tree", || file::Absent::at(dir).recursive(true))?;
    assert!(!ctx.sys().exists(dir)?);
    Ok(())
}
