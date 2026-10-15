use rustible::prelude::*;
use rustible::template as tpl;

#[rustible::playbook(hosts = "laptop")]
fn main(_ctx: &mut Ctx) -> Result<()> {
    // `one/alias.rs` writes this call as `template!`; here it goes through an
    // alias, which the editor's search does not take for a `template!` call,
    // so the editor cannot tell the two apart. Right here and wrong there,
    // it says nothing for either.
    let _ = tpl!(include_str!("files/alias.j2"), port = 80)?;
    Ok(())
}
