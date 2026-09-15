// An integration test file is a test crate; the attribute expands to a
// plain `#[test]` that only does something under RUSTIBLE_INTEGRATION=1.
use rustible::prelude::*;

#[rustible::integration_test(images = ["debian:12", "ubuntu:24.04"])]
fn something(ctx: &mut Ctx) -> Result<()> {
    ctx.log("inside the container");
    Ok(())
}

fn main() {}
