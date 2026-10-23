//! The Apple SDK that a Darwin build does not need (issue #155).
//!
//! zig links Mach-O without an SDK (M8), but rustc does not know that: when
//! nothing names one, it asks `xcrun` on every Darwin link and warns when
//! `xcrun` has none to give — on every Linux controller, on every run. rustc
//! takes an existing absolute directory in `SDKROOT` without asking, so
//! `Cargo::build` names an empty one of Rustible's own. rustc hands it to the
//! linker as `SDKROOT` too, and cargo-zigbuild would read that as a real SDK
//! and point zig at its missing `usr/lib`, which only trades one warning for
//! another. So `rustible`, run as the compiler, linker or archiver, drops it
//! from its environment first, and zig builds exactly as it did without it.
//!
//! A machine with an SDK is left alone: an `SDKROOT` the operator set, or an
//! Xcode or command line tools install that `xcrun` answers for.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::zig;

/// The stand-in: an empty directory in Rustible's cache.
fn stand_in() -> Option<PathBuf> {
    Some(zig::cache_root().ok()?.join("no-apple-sdk"))
}

/// The `SDKROOT` for a build that links for Darwin, or `None` to leave rustc
/// to find the SDK the way it always has. Never fails a build: a cache that
/// cannot be written only brings the warning back.
pub fn sdkroot_for_build() -> Option<PathBuf> {
    if std::env::var_os("SDKROOT").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    // The question rustc is about to ask. A machine that can answer it keeps
    // its real SDK.
    let xcrun = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if xcrun.is_ok_and(|s| s.success()) {
        return None;
    }
    let dir = stand_in()?;
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Whether `sdkroot` is the stand-in rather than an SDK.
pub fn is_stand_in(sdkroot: Option<&OsStr>) -> bool {
    sdkroot.is_some_and(|s| stand_in().is_some_and(|d| d.as_os_str() == s))
}
