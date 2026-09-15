//! Spike, part two: the parts of the runtime that are not operations —
//! the escalation helper, channel streaming, `fetch`, and the pure-Rust
//! archive extractor — driven against a mac.
//!
//! Deliberately *not* escalated at the playbook level: the binary runs as
//! the login user and reaches root one step at a time, which is the path
//! that spawns `sudo -n -u root <this binary> --helper` on the target.

use rustible::prelude::*;
use rustible_std::{archive, file, shell};

#[rustible::playbook(hosts = "mac")]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ensure!(!f.is_root, "run this one unescalated; it escalates per step");

    // The helper: a second copy of this binary, started by sudo as root.
    let mut root = ctx.as_root();
    let who = root.sys().cmd("/usr/bin/id").arg("-un").run()?;
    let me = ctx.sys().cmd("/usr/bin/id").arg("-un").run()?;
    ctx.log(format!(
        "HELPER commands run as {} here and as {} through the helper",
        me.stdout_str().trim(),
        who.stdout_str().trim()
    ));
    root.step(
        "root-owned line through the helper",
        file::Line::in_path("/etc/rustible-spike-helper")
            .create(true)
            .matching("^helper ")
            .set("helper reached root"),
    )?;

    // Channel streaming: a workspace file to the target's run directory.
    let path = ctx.local_file("hosts.kdl")?;
    let sum = ctx
        .sys()
        .cmd("/usr/bin/shasum")
        .args(["-a", "256"])
        .arg(path.to_string_lossy())
        .run()?;
    ctx.log(format!(
        "STREAM hosts.kdl arrived at {} sha256 {}",
        path.display(),
        sum.stdout_str().split_whitespace().next().unwrap_or("")
    ));

    // The pure-Rust extractor, over an archive the mac's own tar made.
    ctx.step(
        "make a tarball with the mac's tar",
        shell::Command::sh(
            "rm -rf /tmp/spike-src /tmp/spike.tar.gz /tmp/spike-out; \
             mkdir -p /tmp/spike-src && echo hello > /tmp/spike-src/a.txt && \
             /usr/bin/tar czf /tmp/spike.tar.gz -C /tmp spike-src",
        ),
    )?;
    ctx.step(
        "somewhere to extract into",
        file::Directory::at("/tmp/spike-out"),
    )?;
    ctx.step(
        "extract it with rustible's own extractor",
        archive::Extracted::from_path("/tmp/spike.tar.gz").to("/tmp/spike-out"),
    )?;
    let cat = ctx
        .sys()
        .cmd("/bin/cat")
        .arg("/tmp/spike-out/spike-src/a.txt")
        .run()?;
    ctx.log(format!("ARCHIVE extracted: {:?}", cat.stdout_str().trim()));

    // And back the other way.
    ctx.fetch("/tmp/spike-out/spike-src/a.txt", "fetched/")?;
    ctx.log("FETCH pulled a.txt back to the workspace".to_string());
    Ok(())
}
