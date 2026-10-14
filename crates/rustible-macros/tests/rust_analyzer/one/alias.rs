use rustible::prelude::*;

#[rustible::playbook(hosts = "laptop")]
fn main(_ctx: &mut Ctx) -> Result<()> {
    // `three/alias.rs` makes this call through an alias (see there).
    let _ = template!(include_str!("files/alias.j2"), port = 80)?; //~ build: uses `peer`, not passed
    Ok(())
}
