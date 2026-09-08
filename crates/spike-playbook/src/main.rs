//! Spike playbook: runs on the local machine against a scratch copy of an
//! sshd_config, to exercise the SDK end to end without SSH or cross-compiling.
//!
//! Usage: cargo run -p spike-playbook -- [--check] [-v|-vv] [--json]

use rustible_sdk::prelude::*;
use rustible_sdk::runtime::{RunOptions, run};
use rustible_std::{file, shell};

const SAMPLE: &str = "\
# sshd_config sample
Port 22
#PasswordAuthentication yes
PermitRootLogin prohibit-password
";

fn playbook(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ctx.log(format!(
        "running on {} ({:?} {}) as {}",
        f.hostname, f.distro, f.distro_version, f.user
    ));

    // Scratch area, set up outside the step model on purpose: it's test rigging.
    let scratch = std::env::var_os("RUSTIBLE_SPIKE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("rustible-spike-{}", std::process::id()))
        });
    std::fs::create_dir_all(&scratch)?;
    let sshd = scratch.join("sshd_config");
    if !sshd.exists() {
        std::fs::write(&sshd, SAMPLE)?;
    }

    ctx.step(
        "Scratch dir has restrictive mode",
        file::Directory::at(&scratch).mode(0o700),
    )?;

    let cfg = ctx.section("Harden sshd", |ctx| {
        let a = ctx.step(
            "Disable password auth",
            file::Line::in_path(&sshd)
                .matching(r"^#?PasswordAuthentication")
                .backup(true)
                .set("PasswordAuthentication no"),
        )?;
        ctx.debug(format!("PasswordAuthentication now at line {}", a.line_no));

        let b = ctx.step(
            "Disable root login",
            file::Line::in_path(&sshd)
                .matching(r"^#?PermitRootLogin")
                .set("PermitRootLogin no"),
        )?;

        let c = ctx.step(
            "Enable pubkey auth",
            file::Line::in_path(&sshd)
                .matching(r"^#?PubkeyAuthentication")
                .insert(file::Insert::After(regex::Regex::new(r"^Port").unwrap()))
                .set("PubkeyAuthentication yes"),
        )?;

        Ok(a.changed || b.changed || c.changed)
    })?;

    if cfg {
        ctx.step(
            "Validate config",
            shell::Command::new("grep").args(["-c", "Authentication", sshd.to_str().unwrap()]),
        )?;
    } else {
        ctx.skip("Validate config", "nothing changed");
    }

    ctx.step(
        "Marker file written once",
        shell::Command::new("touch")
            .arg(scratch.join("marker").to_str().unwrap())
            .creates(scratch.join("marker")),
    )?;

    if ctx.facts().cpus < 2 {
        ctx.warn("single-CPU host");
    }

    ctx.log(format!("scratch: {}", scratch.display()));
    Ok(())
}

fn main() -> std::process::ExitCode {
    run(RunOptions::from_args(), playbook)
}
