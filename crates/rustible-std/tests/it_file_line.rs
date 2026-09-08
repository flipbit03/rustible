//! Docker integration test for `file::Line` (vision 8, tier 3).
//! Runs with `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_file_line`.

use rustible::prelude::*;
use rustible::sdk::testing::changed_then_ok;
use rustible_std::file::Line;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn line_changed_then_ok(ctx: &mut Ctx) -> Result<()> {
    let dir = "/etc/rustible-test";
    let path = "/etc/rustible-test/motd";
    ctx.sys().mkdir_all(dir)?;

    let (first, _) = changed_then_ok(ctx, "add greeting", || {
        Line::in_path(path).create(true).set("hello from rustible")
    })?;
    assert_eq!(first.line_no, 1);
    assert_eq!(ctx.sys().read_to_string(path)?, "hello from rustible\n");

    changed_then_ok(ctx, "replace greeting", || {
        Line::in_path(path)
            .matching("^hello ")
            .backup(true)
            .set("hello again")
    })?;
    assert_eq!(ctx.sys().read_to_string(path)?, "hello again\n");
    Ok(())
}
