//! Streaming over the channel (vision doc 5.6): a large workspace file to a
//! temp path on the target, a secret into memory only, and a fetch back.

use rustible::prelude::*;

#[rustible::vars]
struct Vars {
    /// Workspace-relative file to stream to the target.
    #[default = "files/big.bin"]
    file: String,
    /// Workspace-relative file to load as a secret.
    #[default = "files/secret.txt"]
    secret: String,
}

#[rustible::playbook(hosts = "lab", vars = Vars)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
    let path = ctx.local_file(&vars.file)?;
    let sum = ctx.sys().cmd("sha256sum").arg(path.to_string_lossy()).run()?;
    ctx.log(format!(
        "{} arrived at {} sha256 {}",
        vars.file,
        path.display(),
        sum.stdout_str().split_whitespace().next().unwrap_or("")
    ));

    let secret = ctx.local_secret(&vars.secret)?;
    // Its digest proves it arrived intact without printing it; the bytes go
    // to a pipe, never to a file.
    let sum = ctx.sys().cmd("sha256sum").stdin(secret.as_bytes()).run()?;
    ctx.log(format!(
        "{}: {} bytes in memory, sha256 {}",
        vars.secret,
        secret.len(),
        sum.stdout_str().split_whitespace().next().unwrap_or("")
    ));
    let run_dir = path.parent().and_then(|p| p.parent()).unwrap_or(&path);
    let ls = ctx.sys().cmd("find").arg(run_dir.to_string_lossy()).arg("-type").arg("f").run()?;
    ctx.log(format!("files under the run's temp dir: {}", ls.stdout_str().split_whitespace().collect::<Vec<_>>().join(" ")));

    ctx.fetch("/etc/hostname", "out/")?;
    Ok(())
}
