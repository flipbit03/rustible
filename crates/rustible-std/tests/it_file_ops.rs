//! Docker integration test for the `file` family (vision 8, tier 3):
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
