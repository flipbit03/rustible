use rustible::prelude::*;

#[rustible::playbook(hosts = "laptop")]
fn main(_ctx: &mut Ctx) -> Result<()> {
    // The same call as `target/host.rs`'s (see there).
    let _ = template!(include_str!("files/host.j2"), port = 80)?; //~ build: uses `peer`, not passed
    Ok(())
}
