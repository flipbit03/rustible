// Both lists may be given; systemd images become `Image::Systemd`.
use rustible::prelude::*;

#[rustible::integration_test(images = ["debian:12"], systemd_images = ["jrei/systemd-debian:12"])]
fn something(ctx: &mut Ctx) -> Result<()> {
    ctx.log("inside the container");
    Ok(())
}

fn main() {}
