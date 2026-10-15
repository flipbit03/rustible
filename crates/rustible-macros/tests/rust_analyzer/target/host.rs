use rustible::prelude::*;

#[rustible::playbook(hosts = "laptop")]
fn main(_ctx: &mut Ctx) -> Result<()> {
    // A host directory named `target` is searched like any other (only the
    // crate's own `target` is not), so this call and `two/host.rs`'s, token
    // for token the same, are both found. Right here and wrong there, the
    // editor says nothing for either.
    let _ = template!(include_str!("files/host.j2"), port = 80)?;
    Ok(())
}
