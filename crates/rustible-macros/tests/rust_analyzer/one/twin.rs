use rustible::prelude::*;

#[rustible::playbook(hosts = "laptop")]
fn main(_ctx: &mut Ctx) -> Result<()> {
    // `two/twin.rs` has these calls too, token for token, so the editor
    // cannot tell the two apart. `files/twin.j2` reads `port` here and
    // `peer` there: right here, wrong there, so the editor says nothing.
    let _ = template!(include_str!("files/twin.j2"), port = 80)?;
    // Missing in both directories: wrong wherever the call is.
    let _ = template!(include_str!("files/gone.j2"))?; //~ template file not found
    Ok(())
}
