//! `rustible toolchain check`: which zig a build would use, and the
//! environment it would hand cargo.
//!
//! The environment is the point. It is the same one `playbook run` gives
//! cargo — the same `cargo_zigbuild::Build`, the same host-linker wiring —
//! so `eval "$(rustible toolchain check --target <t> --print-env)"` followed
//! by a plain `cargo build --target <t>` is a Rustible build without the
//! run around it. CI uses that to cross-build where there is no host to ship
//! to, and an operator can use it to give rust-analyzer or a bare `cargo`
//! the zig that `rustible` would use, on a machine with no other C compiler.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;

use crate::describe::{host_triple, wire_host_linker};
use crate::toolchain::ensure_targets_installed;
use crate::zig;

/// Arguments for `rustible toolchain check`.
#[derive(Args, Debug)]
pub struct CheckArgs {
    /// Target triple to check, repeatable. Defaults to every target Rustible
    /// can manage: both Linux musl triples and both macOS ones.
    #[arg(long = "target")]
    pub targets: Vec<String>,
    /// Print the build environment as `export KEY='value'` lines instead of
    /// a report, ready for `eval`. Values contain spaces, so they are
    /// quoted; do not split this output on whitespace.
    #[arg(long)]
    pub print_env: bool,
}

/// The targets to check when none were named: everything Rustible manages.
fn default_targets() -> Vec<String> {
    [
        "x86_64-unknown-linux-musl",
        "aarch64-unknown-linux-musl",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Run `rustible toolchain check`. zig was provisioned in `main` before the
/// runtime started (that is where the environment can be exported soundly);
/// provisioning again here is a cache hit and gives the report its first line.
pub fn run(_ws: Option<&Path>, args: CheckArgs) -> Result<u8> {
    let targets = if args.targets.is_empty() {
        default_targets()
    } else {
        args.targets.clone()
    };
    ensure_targets_installed(&targets)?;
    let host = host_triple()?;
    let located = zig::provision(&|line| eprintln!("  {line}"))?;

    // One environment per target, because the wrapper scripts are per
    // target. Printed in target order, so `--target a --target b` gives a's
    // block then b's; a reader who wants one target names one.
    let mut blocks = Vec::new();
    for target in &targets {
        let mut b = cargo_zigbuild::Build::new(None);
        b.enable_zig_ar = true;
        b.cargo.common.target = vec![target.clone()];
        let mut cmd = b
            .build_command()
            .with_context(|| format!("preparing the zig-backed build for {target}"))?;
        wire_host_linker(&host, &mut cmd)?;
        let mut env: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        // The compiler and linker wrappers are scripts with the zig path
        // baked in, but `ar` is this binary reached through a symlink, with
        // nothing baked in: it finds zig from the environment. `playbook run`
        // inherits that from `main`; a shell that `eval`s this output has
        // to be given it too, or every `ring` archive step dies with
        // "Failed to find zig".
        env.push((
            "CARGO_ZIGBUILD_ZIG_COMMAND".into(),
            located.path().to_string_lossy().into_owned(),
        ));
        blocks.push((target.clone(), env));
    }

    if args.print_env {
        // Shell-quoted and `export`-prefixed, so `eval "$(rustible toolchain
        // check --print-env)"` is the whole recipe. Values contain spaces.
        for (_, env) in &blocks {
            for (k, v) in env {
                println!("export {k}='{}'", shell_quote(v));
            }
        }
        return Ok(0);
    }

    println!("{}", located.describe());
    println!("this machine builds for:");
    for t in &targets {
        println!("    {t}");
    }
    for (target, env) in &blocks {
        println!("\n{target}: cargo is given");
        for (k, v) in env {
            println!("    {k}={v}");
        }
    }
    Ok(0)
}

/// Escape for a single-quoted shell string.
fn shell_quote(s: &str) -> String {
    s.replace('\'', r"'\''")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Named targets win over the defaults, and the defaults are every host
    /// Rustible can manage — Linux and macOS, both architectures.
    #[test]
    fn defaults_are_every_manageable_target() {
        let d = default_targets();
        assert_eq!(d.len(), 4);
        assert!(d.iter().any(|t| t == "x86_64-unknown-linux-musl"));
        assert!(d.iter().any(|t| t == "aarch64-unknown-linux-musl"));
        assert!(d.iter().any(|t| t == "aarch64-apple-darwin"));
        assert!(d.iter().any(|t| t == "x86_64-apple-darwin"));
    }

    /// A value with a quote in it survives `eval`: the first CI recipe split
    /// `--print-env` on whitespace and executed a path. Quoting is the fix
    /// and it has to hold for the one character single quotes cannot hold.
    #[test]
    fn quoting_survives_a_single_quote() {
        assert_eq!(shell_quote("-idirafter /a b"), "-idirafter /a b");
        assert_eq!(shell_quote("it's"), r"it'\''s");
    }
}
