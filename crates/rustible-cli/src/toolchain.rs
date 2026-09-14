//! What a build needs from rustup: the target's `rust-std`.
//!
//! This module used to be the C toolchain — which compiler serves which
//! target, vendored musl headers, per-vendor workarounds, an Apple SDK to go
//! and find. All of that is zig's job now (`crate::zig`, M8): one compiler
//! carrying its own libc for every target, so there is nothing left to
//! choose. What remains is the one thing zig cannot do, which is teach rustc
//! a target it has no standard library for.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// Add any of `triples` that rustup does not already have, so a fleet with an
/// architecture the operator has never managed before builds instead of
/// failing on `can't find crate for \`core\``.
///
/// Rustible knows which triples a run needs — it probed the hosts — so asking
/// the operator to work that out and type `rustup target add` themselves is
/// busywork. This is additive and idempotent: an installed target is left
/// alone, and the install is announced rather than done silently.
///
/// A machine whose Rust did not come from rustup (a distribution package, a
/// container image) has no `rustup` on PATH. That is not an error here: it may
/// still have the target, and if it does not, cargo's own message names the
/// triple. Returns the triples it installed.
pub fn ensure_targets_installed(triples: &[String]) -> Result<Vec<String>> {
    if triples.is_empty() {
        return Ok(Vec::new());
    }
    let Some(installed) = rustup_installed_targets() else {
        return Ok(Vec::new());
    };

    let mut added = Vec::new();
    for triple in triples {
        if installed.iter().any(|t| t == triple) {
            continue;
        }
        eprintln!("  installing rust target {triple}");
        let status = Command::new("rustup")
            .args(["target", "add", triple])
            .status()
            .with_context(|| format!("running `rustup target add {triple}`"))?;
        if !status.success() {
            bail!(
                "`rustup target add {triple}` failed (exit {}).\n\
                 Install it by hand, or check that `{triple}` is a target this \
                 toolchain knows: `rustup target list`.",
                status.code().unwrap_or(-1)
            );
        }
        added.push(triple.clone());
    }
    Ok(added)
}

/// The triples `rustup` reports as installed, or `None` when there is no
/// rustup to ask.
fn rustup_installed_targets() -> Option<Vec<String>> {
    let out = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing to add is nothing done, and in particular no `rustup` run:
    /// the describe build asks with an empty list on every invocation.
    #[test]
    fn an_empty_list_installs_nothing() {
        assert!(ensure_targets_installed(&[]).unwrap().is_empty());
    }
}
