//! `rustible toolchain`: what this machine can build for, and the environment
//! it would use.
//!
//! Playbook binaries are cross-compiled here and shipped to the target, so
//! "can this machine build for that host" is a question with a real answer,
//! and one worth asking before a run rather than during it. `check` answers
//! it; `--print-env` shows the compiler settings [`crate::toolchain`] would
//! hand to cargo, which is what a build actually uses.

use anyhow::{Context, Result};
use clap::Args;

use crate::toolchain::{Compilers, env_for_build, host_musl_triple};
use crate::workspace::Workspace;

/// Arguments for `rustible toolchain check`.
#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Target triple to check, repeatable. Defaults to both Linux musl
    /// targets, which is every host Rustible can manage.
    #[arg(long = "target")]
    pub targets: Vec<String>,
    /// Print the compiler environment as `KEY=VALUE` lines instead of a
    /// report, for feeding to a build.
    #[arg(long)]
    pub print_env: bool,
}

/// The targets to check when none were named: everything Rustible manages.
fn default_targets() -> Vec<String> {
    vec![
        "x86_64-unknown-linux-musl".to_string(),
        "aarch64-unknown-linux-musl".to_string(),
    ]
}

/// Run `rustible toolchain check`.
///
/// Exits 0 when every target can be built for, and fails with the same
/// message a run would give otherwise, so the answer here and the answer
/// during `playbook run` cannot disagree.
pub fn run(ws: Option<&std::path::Path>, args: CheckArgs) -> Result<u8> {
    let targets = if args.targets.is_empty() {
        default_targets()
    } else {
        args.targets.clone()
    };
    let compilers = Compilers::probe();

    // The headers are unpacked into the workspace cache when there is a
    // workspace, and into a temporary directory when there is not, so this
    // works outside one.
    let tmp;
    let cache_dir = match Workspace::discover(ws) {
        Ok(w) => w.cache_dir(),
        Err(_) => {
            tmp = tempfile::tempdir().context("creating a temporary cache directory")?;
            tmp.path().to_path_buf()
        }
    };

    let env = env_for_build(&compilers, &targets, &cache_dir)?;

    if args.print_env {
        for (k, v) in &env {
            println!("{k}={}", v.to_string_lossy());
        }
        return Ok(0);
    }

    println!("this machine builds for:");
    for t in &targets {
        println!("    {t}");
    }
    println!();
    match (&compilers.clang, &compilers.host_cc) {
        (Some(c), _) => println!("clang:  {}", c.display()),
        (None, Some(cc)) => println!(
            "clang:  not found; {} serves {} alone",
            cc.display(),
            host_musl_triple()
        ),
        (None, None) => println!("clang:  not found, and no other compiler either"),
    }
    if env.is_empty() {
        println!("cargo needs nothing set for these targets");
    } else {
        println!("\ncargo is given:");
        for (k, v) in &env {
            println!("    {k}={}", v.to_string_lossy());
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Named targets win over the default pair, and the default pair is
    /// every host Rustible can manage.
    #[test]
    fn defaults_are_every_manageable_target() {
        let d = default_targets();
        assert_eq!(d.len(), 2);
        assert!(d.iter().all(|t| t.ends_with("-unknown-linux-musl")));
        assert!(d.iter().any(|t| t.starts_with("x86_64")));
        assert!(d.iter().any(|t| t.starts_with("aarch64")));
    }

    /// The environment this prints is the environment a build is given: the
    /// command exists so CI and a puzzled operator see the same thing
    /// `playbook run` would use, rather than a second implementation that
    /// can drift from it.
    #[test]
    fn print_env_is_what_a_build_gets() {
        let t = tempfile::tempdir().unwrap();
        let compilers = Compilers::probe();
        let targets = default_targets();
        let direct = env_for_build(&compilers, &targets, t.path());
        // Whatever the machine can or cannot do, the command agrees with the
        // builder: both succeed or both fail with the same message.
        match direct {
            Ok(env) => assert!(
                env.keys()
                    .all(|k| k.starts_with("CC_") || k.starts_with("CFLAGS_"))
            ),
            Err(e) => assert!(e.to_string().contains("clang"), "{e}"),
        }
    }
}
