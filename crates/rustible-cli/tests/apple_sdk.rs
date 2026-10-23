//! `rustible zig`, the compiler and linker shim, takes the empty `SDKROOT`
//! that `Cargo::build` names for rustc back out before zig runs, and leaves
//! any other `SDKROOT` alone (issue #155, `apple_sdk.rs`).

// These drive the built `rustible` binary and lay out fixtures on the test
// machine. Vision 7.2's `sys` rule governs operations running on a target,
// not a harness exercising the CLI; see clippy.toml.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

/// The `SDKROOT` a fake zig is handed when `rustible zig ar` runs with
/// `sdkroot` in its environment.
fn sdkroot_zig_sees(home: &Path, sdkroot: &Path) -> String {
    let zig = home.join("zig");
    let seen = home.join("seen");
    fs::write(
        &zig,
        format!(
            "#!/bin/sh\nprintf '%s' \"${{SDKROOT-unset}}\" > '{}'\n",
            seen.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&zig, fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_rustible"))
        .args(["zig", "ar", "x"])
        .env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        .env("SDKROOT", sdkroot)
        .env("CARGO_ZIGBUILD_ZIG_COMMAND", &zig)
        .output()
        .expect("running rustible");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(seen).unwrap()
}

#[test]
fn the_stand_in_never_reaches_zig() {
    let tmp = tempfile::tempdir().unwrap();
    let stand_in = tmp.path().join(".cache/rustible/no-apple-sdk");
    fs::create_dir_all(&stand_in).unwrap();
    assert_eq!(sdkroot_zig_sees(tmp.path(), &stand_in), "unset");
}

#[test]
fn an_sdkroot_the_operator_set_reaches_zig() {
    let tmp = tempfile::tempdir().unwrap();
    let sdk = tmp.path().join("MacOSX.sdk");
    fs::create_dir_all(&sdk).unwrap();
    assert_eq!(
        sdkroot_zig_sees(tmp.path(), &sdk),
        sdk.display().to_string()
    );
}
