//! The test API is unreachable without the `testing` feature (T1).
//!
//! A crate that depends on `rustible-sdk`, `rustible` and `rustible-github`
//! with default features, as every playbook and every collection's normal
//! build does, cannot name any of it; the same crate with the feature on
//! can, and even then cannot build a `Ctx` with `Ctx::new`, `with_channel`
//! or `for_run`. This spawns `cargo check` on a scratch crate outside this
//! workspace, because inside it a `cargo test` turns the feature on for
//! everything: the members' dev-dependencies enable it, and features unify
//! across one build. So the check fails if `testing` becomes a default
//! feature, if a library turns it on in `[dependencies]`, or if an item
//! loses its gate.

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

/// The `Ctx` constructors, private with or without the feature: a test
/// builds its `Ctx` with `testing::ctx`, and a run is handed one. Without
/// the feature `new` and `with_channel` do not exist at all.
const PRIVATE: [&str; 3] = [
    "let _ = rustible_sdk::Ctx::new;",
    "let _ = rustible_sdk::Ctx::with_channel;",
    "let _ = rustible_sdk::Ctx::for_run;",
];

/// After `main`: the facade's re-export of the attribute, gated itself so a
/// missing feature fails where the author wrote it.
const ATTRIBUTE: &str = "#[rustible::integration_test(images = [\"debian:12\"])]\n\
                         fn t(_: &mut rustible::sdk::Ctx) -> rustible::sdk::Result<()> { Ok(()) }";

/// What rustc reports when a path or an associated function is not there:
/// the only reasons a gated line may fail without the feature. Anything
/// else means the scratch crate is broken, not that the gate held.
const NOT_FOUND: [&str; 4] = ["E0425", "E0432", "E0433", "E0599"];

/// What rustc reports for a private associated function.
const PRIVATE_FN: &str = "E0624";

#[test]
fn the_test_api_needs_the_testing_feature() {
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("testing-gate");
    let lines: Vec<&str> = GATED.iter().chain(&PRIVATE).copied().collect();
    // `fn main() {` is line 1, so `lines[i]` is line `i + 2`; then the
    // closing brace and a blank line, then the attribute.
    let line_of = |i: usize| i + 2;
    let private_lines: Vec<usize> = (GATED.len()..lines.len()).map(line_of).collect();
    let attribute_line = lines.len() + 4;

    // Default features: every line fails, each at its own line, and only
    // because the name is not there (or, for a constructor, is private).
    let out = check(&tmp, "default", "", &lines);
    out.expect_compile_errors();
    let mut expected: Vec<(usize, &str)> =
        (0..lines.len()).map(|i| (line_of(i), lines[i])).collect();
    expected.push((attribute_line, "#[rustible::integration_test]"));
    for (line, what) in &expected {
        let allowed = |e: &&str| {
            NOT_FOUND.iter().any(|c| e.contains(c))
                || (private_lines.contains(line) && e.contains(PRIVATE_FN))
        };
        match out.errors_at(*line).as_slice() {
            [] => panic!(
                "`{what}` compiled without the `testing` feature (no error at \
                 src/main.rs:{line}); the gate is gone:\n{}",
                out.stderr
            ),
            errs if !errs.iter().all(allowed) => panic!(
                "`{what}` failed without the `testing` feature, but not because it was \
                 not found, so this says nothing about the gate:\n{}",
                out.stderr
            ),
            _ => {}
        }
    }
    out.expect_no_errors_but_at(&expected.iter().map(|(l, _)| *l).collect::<Vec<_>>());
    assert!(
        out.stderr
            .contains("could not find `testing` in `rustible_sdk`"),
        "the compiler no longer says what `docs/USING_RUSTIBLE.md` quotes for a \
         missing feature:\n{}",
        out.stderr
    );

    // With the feature: everything resolves but the `Ctx` constructors,
    // which are private whatever the features.
    let out = check(&tmp, "testing", r#", features = ["testing"]"#, &lines);
    out.expect_compile_errors();
    out.expect_no_errors_but_at(&private_lines);
    for (line, what) in private_lines.iter().zip(PRIVATE) {
        let errs = out.errors_at(*line);
        assert!(
            !errs.is_empty() && errs.iter().all(|e| e.contains(PRIVATE_FN)),
            "with `testing`, `{what}` must still fail as private ({PRIVATE_FN}):\n{}",
            out.stderr
        );
    }
}

/// The outcome of one `cargo check` of the scratch crate.
struct Checked {
    passed: bool,
    stderr: String,
    /// `(line, message)` for every error rustc placed in `src/main.rs`.
    errors: Vec<(usize, String)>,
}

impl Checked {
    /// The check failed, and rustc reported errors in the scratch crate.
    /// A failure with none (a dependency that did not build, a lock cargo
    /// refused) is a broken setup, not a result.
    fn expect_compile_errors(&self) {
        assert!(
            !self.passed,
            "the scratch crate compiled, so nothing it names is gated:\n{}",
            self.stderr
        );
        assert!(
            !self.errors.is_empty(),
            "`cargo check` failed before reaching the scratch crate's code, so the \
             gate was not tested:\n{}",
            self.stderr
        );
    }

    fn errors_at(&self, line: usize) -> Vec<&str> {
        self.errors
            .iter()
            .filter(|(l, _)| *l == line)
            .map(|(_, e)| e.as_str())
            .collect()
    }

    fn expect_no_errors_but_at(&self, lines: &[usize]) {
        let stray: Vec<&(usize, String)> = self
            .errors
            .iter()
            .filter(|(l, _)| !lines.contains(l))
            .collect();
        assert!(
            stray.is_empty(),
            "unexpected errors in the scratch crate {stray:?}:\n{}",
            self.stderr
        );
    }
}

/// `cargo check` a binary crate in `<tmp>/<name>` whose `main` holds
/// `lines`, followed by [`ATTRIBUTE`], depending on this checkout's
/// `rustible-sdk` and `rustible` by path, with `features` appended to both
/// dependencies, and on `rustible-github` as a collection is used, with no
/// features: if it turned `testing` on for itself, the default build would
/// see the test API.
fn check(tmp: &Path, name: &str, features: &str, lines: &[&str]) -> Checked {
    let sdk = Path::new(env!("CARGO_MANIFEST_DIR"));
    let facade = sdk.join("../rustible");
    let github = sdk.join("../rustible-github");
    let dir = tmp.join(name);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"gate-{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
             publish = false\n\n[dependencies]\n\
             rustible-sdk = {{ path = {sdk:?}{features} }}\n\
             rustible = {{ path = {facade:?}{features} }}\n\
             rustible-github = {{ path = {github:?} }}\n\n\
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
    let mut cmd = Command::new(cargo);
    cmd.args([
        "check",
        "--quiet",
        "--color",
        "never",
        "--message-format=short",
        "--manifest-path",
    ])
    .arg(dir.join("Cargo.toml"))
    // One target directory for both, so the dependencies build once.
    .env("CARGO_TARGET_DIR", tmp.join("target"));
    // Hermetic: nothing from the outer build or CI's environment reaches
    // the scratch build (CI sets `CARGO_TERM_COLOR=always`, which put
    // escape codes in every message this parses).
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
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let errors = stderr
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("src/main.rs:")?;
            let (line, msg) = rest.split_once(':')?;
            if !msg.contains(": error") {
                return None;
            }
            Some((line.parse().ok()?, l.to_string()))
        })
        .collect();
    Checked {
        passed: out.status.success(),
        stderr,
        errors,
    }
}
