use rustible::prelude::*;

#[rustible::playbook(hosts = "laptop")]
fn main(_ctx: &mut Ctx) -> Result<()> {
    // `one/twin.rs` has these calls too, token for token, so the editor
    // cannot tell the two apart. `files/twin.j2` reads `peer` here and
    // `port` there: wrong here, right there, so the editor says nothing.
    let _ = template!(include_str!("files/twin.j2"), port = 80)?; //~ build: uses `peer`, not passed
    // Missing in both directories: wrong wherever the call is.
    let _ = template!(include_str!("files/gone.j2"))?; //~ template file not found
    // An inline template including a file missing in both, alike.
    let _ = template!("{% include \"files/gone.j2\" %}")?; //~ `{% include %}`: no such file
    Ok(())
}
