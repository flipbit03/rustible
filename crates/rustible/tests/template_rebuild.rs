//! Editing a file a template reaches only through includes rebuilds the
//! playbook (T1).
//!
//! `template!` embeds every file of a template's tree with `include_str!`,
//! and that is what tells cargo to rebuild when one changes: nothing else
//! records a file the macro read. This builds a scratch crate outside the
//! workspace whose template includes a file three levels deep, runs it,
//! edits only that file, and runs it again; the second run must print the
//! edit. A stale binary would print the first text twice.

// This lays out a scratch crate and runs cargo on the test machine. Vision
// 7.2's `sys` rule governs operations running on a target, not a harness
// exercising the build; see clippy.toml.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::process::Command;

#[test]
fn editing_a_nested_include_rebuilds_the_playbook() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("template-rebuild");
    let facade = Path::new(env!("CARGO_MANIFEST_DIR"));
    let _ = std::fs::remove_dir_all(dir.join("src"));
    write(
        &dir.join("Cargo.toml"),
        &format!(
            "[package]\nname = \"template-rebuild\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             publish = false\n\n[dependencies]\nrustible = {{ path = {facade:?} }}\n\n\
             # Not part of the workspace this sits under.\n[workspace]\n"
        ),
    );
    write(
        &dir.join("src/main.rs"),
        "fn main() {\n    \
         print!(\"{}\", rustible::template!(include_str!(\"t/one.j2\"), who = \"w\").unwrap());\n}\n",
    );
    write(&dir.join("src/t/one.j2"), "1[{% include \"a/two.j2\" %}]");
    write(
        &dir.join("src/t/a/two.j2"),
        "2[{% include \"b/three.j2\" %}]",
    );
    write(&dir.join("src/t/a/b/three.j2"), "3[before {{ who }}]");
    // The workspace's lock, so the scratch crate builds what the workspace
    // does and needs nothing from the network.
    std::fs::copy(facade.join("../../Cargo.lock"), dir.join("Cargo.lock")).unwrap();

    assert_eq!(run(&dir), "1[2[3[before w]]]");
    write(&dir.join("src/t/a/b/three.j2"), "3[after {{ who }}]");
    assert_eq!(run(&dir), "1[2[3[after w]]]");
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `cargo run` the scratch crate; its stdout.
fn run(dir: &Path) -> String {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["run", "--quiet", "--color", "never", "--manifest-path"])
        .arg(dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", dir.join("target"));
    // Hermetic: nothing from the outer build reaches the scratch build.
    for var in [
        "CARGO_TERM_COLOR",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        cmd.env_remove(var);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "the scratch crate did not build and run:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}
