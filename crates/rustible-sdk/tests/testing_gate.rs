//! The test API is unreachable without the `testing` feature (T1).
//!
//! A crate that depends on `rustible-sdk` and `rustible` with default
//! features, as every playbook and every collection's normal build does,
//! cannot name any of it; the same crate with the feature on can, and even
//! then cannot build a `Ctx` with `Ctx::new`. This spawns `cargo check` on a
//! scratch crate outside this workspace, because inside it a `cargo test`
//! turns the feature on for everything: the members' dev-dependencies enable
//! it, and features unify across one build. So the check fails if `testing`
//! becomes a default feature, or if an item loses its gate.

// This lays out a scratch crate and runs cargo on the test machine. Vision
// 7.2's `sys` rule governs operations running on a target, not a harness
// exercising the build; see clippy.toml.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// One gated item per line of `main`, so each error is placed by its line
/// number. Line 1 is `fn main() {`.
const GATED: [&str; 9] = [
    "let _ = rustible_sdk::testing::ctx;",
    "let _ = rustible_sdk::testing::dry;",
    "let _ = rustible_sdk::backend::Fake::new;",
    "let _ = rustible_sdk::event::Collect::default;",
    "let _ = rustible_sdk::System::fake;",
    "let _ = rustible_sdk::System::in_process_helper;",
    "let _ = rustible_sdk::System::with_check_mode;",
    "let _ = rustible_sdk::System::with_facts;",
    "let _ = rustible_sdk::launch::Launch::with_suffix;",
];

/// Private with or without the feature: a test builds its `Ctx` with
/// `testing::ctx`.
const CTX_NEW: &str = "let _ = rustible_sdk::Ctx::new;";

/// After `main`: the facade's re-export of the attribute, gated itself so a
/// missing feature fails where the author wrote it.
const ATTRIBUTE: &str = "#[rustible::integration_test(images = [\"debian:12\"])]\n\
                         fn t(_: &mut rustible::sdk::Ctx) -> rustible::sdk::Result<()> { Ok(()) }";

#[test]
fn the_test_api_needs_the_testing_feature() {
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("testing-gate");
    let mut lines = GATED.to_vec();
    lines.push(CTX_NEW);
    let at = |line: usize| format!("src/main.rs:{line}:");
    // `main`, its lines, its brace and a blank line come first.
    let attribute_line = lines.len() + 4;

    // Default features: every line fails, each at its own line.
    let out = check(&tmp, "default", "", &lines);
    let errors: Vec<&str> = out.lines().filter(|l| l.contains(": error")).collect();
    for (i, line) in lines.iter().enumerate() {
        assert!(
            errors.iter().any(|l| l.starts_with(&at(i + 2))),
            "`{line}` compiled without the `testing` feature:\n{out}"
        );
    }
    assert!(
        errors.iter().any(|l| l.starts_with(&at(attribute_line))),
        "`#[rustible::integration_test]` resolved without the `testing` feature:\n{out}"
    );
    assert!(
        out.contains("could not find `testing` in `rustible_sdk`"),
        "{out}"
    );

    // With the feature: everything resolves but `Ctx::new`.
    let out = check(&tmp, "testing", r#", features = ["testing"]"#, &lines);
    let errors: Vec<&str> = out.lines().filter(|l| l.contains(": error")).collect();
    assert!(
        errors.len() == 1 && errors[0].starts_with(&at(lines.len() + 1)),
        "with `testing`, only `Ctx::new` should fail:\n{out}"
    );
    assert!(errors[0].contains("private"), "{out}");
}

/// `cargo check` a binary crate in `<tmp>/<name>` whose `main` holds
/// `lines`, followed by [`ATTRIBUTE`], depending on this checkout's
/// `rustible-sdk` and `rustible` by path, with `features` appended to both
/// dependencies. Returns cargo's stderr; the check is expected to fail.
fn check(tmp: &Path, name: &str, features: &str, lines: &[&str]) -> String {
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR"));
    let facade = sdk.join("../rustible");
    let dir = tmp.join(name);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"gate-{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             publish = false\n\n[dependencies]\n\
             rustible-sdk = {{ path = {sdk:?}{features} }}\n\
             rustible = {{ path = {facade:?}{features} }}\n\n\
             # Not part of the workspace this sits under.\n[workspace]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        dir.join("src/main.rs"),
        format!("fn main() {{\n{}\n}}\n\n{ATTRIBUTE}\n", lines.join("\n")),
    )
    .unwrap();
    // The workspace's lock, so the scratch crate builds what the workspace
    // does and needs nothing from the network.
    std::fs::copy(sdk.join("../../Cargo.lock"), dir.join("Cargo.lock")).unwrap();

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        .args([
            "check",
            "--quiet",
            "--message-format=short",
            "--manifest-path",
        ])
        .arg(dir.join("Cargo.toml"))
        // One target directory for both, so the dependencies build once.
        .env("CARGO_TARGET_DIR", tmp.join("target"))
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(!out.status.success(), "the check passed:\n{stderr}");
    stderr
}
