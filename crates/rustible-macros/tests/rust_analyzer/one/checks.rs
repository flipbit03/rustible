use rustible::prelude::*;

#[rustible::playbook(hosts = "laptop")]
fn main(_ctx: &mut Ctx) -> Result<()> {
    // Found and checked, and right: no error.
    let _ = template!(include_str!("files/app.conf.j2"), port = 80)?;
    // A template file need not end in `.j2`.
    let _ = template!(include_str!("files/motd"), name = "x")?;
    let _ = template!(include_str!("files/motd"))?; //~ uses `name`, not passed to `template!`: add `name = <value>`

    let _ = template!(include_str!("files/app.conf.j2"))?; //~ files/app.conf.j2` uses `port`, not passed to `template!`
    let _ = template!(include_str!("files/app.conf.j2"), port = 80, extra = 1)?; //~ `extra` is passed to `template!` but not used
    let _ = template!(include_str!("files/missing.j2"))?; //~ template file not found: `playbooks/ra/one/files/missing.j2`
    let _ = template!(include_str!("files/app.conf.j3"), port = 80)?; //~ did you mean `files/app.conf.j2`?
    let _ = template!(include_str!("nowhere/app.conf.j2"), port = 80)?; //~ there is no directory `playbooks/ra/one/nowhere`
    let _ = template!(include_str!("files/bad.j2"), port = 80)?; //~ bad.j2` line 1: syntax error
    // Over several lines, with comments inside the call.
    let _ = template!(
        include_str!("files/app.conf.j2"), //~ uses `port`, not passed
        extra = 1, // unused, but reported only once `port` is passed
    )?;

    // A file's path written as the template: the literal is the text.
    let _ = template!("files/app.conf.j2", port = 80)?; //~ if you meant a file, write `template!(include_str!("files/app.conf.j2"), ..)`

    // A failed call is one error: the code after it still type-checks.
    let text: String = template!(include_str!("files/missing.j2"))?; //~ template file not found
    let _: &str = &text;
    Ok(())
}
