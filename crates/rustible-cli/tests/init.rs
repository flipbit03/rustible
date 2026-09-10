//! `rustible init` and `rustible playbook create` drive the built binary on
//! temp directories. The reference for what `init` generates is
//! `examples/workspace`; after a version bump, `rustible init --refresh
//! examples/workspace` brings the example's shims back in line.

// These drive the built `rustible` binary and lay out fixture workspaces on
// the test machine. Vision 7.2's `sys` rule governs operations running on a
// target, not a harness exercising the CLI; see clippy.toml.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn rustible(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rustible"))
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("running rustible")
}

fn example_workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/workspace")
        .canonicalize()
        .unwrap()
}

fn read(dir: &Path, rel: &str) -> String {
    fs::read_to_string(dir.join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The example workspace is the reference: regenerating it with the same
/// path dependencies must reproduce the shims, the cargo config, and the
/// manifest byte for byte. `src/lib.rs`, `hosts.kdl` and `playbooks/` carry
/// example content and are not compared.
#[test]
fn regenerates_the_example_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    // `../..` is what the example uses; relative to the new workspace it must
    // point at a checkout, so mirror the repository layout in the temp dir.
    let checkout = tmp.path().join("checkout");
    let dir = checkout.join("examples/workspace");
    for krate in ["rustible", "rustible-std", "rustible-build"] {
        let manifest = checkout.join("crates").join(krate).join("Cargo.toml");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, "").unwrap();
    }
    let out = rustible(
        tmp.path(),
        &["init", dir.to_str().unwrap(), "--path-deps", "../.."],
    );
    assert!(out.status.success(), "{}", stderr(&out));

    let example = example_workspace();
    for rel in [
        "Cargo.toml",
        "build.rs",
        "src/main.rs",
        ".cargo/config.toml",
    ] {
        assert_eq!(
            read(&dir, rel),
            read(&example, rel),
            "{rel} differs from examples/workspace; if the version was bumped, run \
             `cargo run -p rustible-cli -- init --refresh examples/workspace` for the shims; \
             for other files, delete the example's copy and rerun \
             `... init examples/workspace --force --path-deps ../..`"
        );
    }
    assert_eq!(read(&dir, ".gitignore"), "/target\n/.rustible\n");
    assert!(dir.join("playbooks/.gitkeep").is_file());
    assert!(dir.join("rustible.toml").is_file());
    assert!(dir.join("hosts.kdl").is_file());
    assert!(read(&dir, "src/lib.rs").contains("`workspace::helper()`"));
}

/// A fresh clone is not a conflict: `init` writes into it and leaves the
/// files it did not generate alone, appending to an existing `.gitignore`.
#[test]
fn a_cloned_repository_is_not_a_conflict() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();
    fs::write(tmp.path().join("README.md"), "# my infra\n").unwrap();
    fs::write(tmp.path().join("LICENSE"), "MIT\n").unwrap();
    fs::write(tmp.path().join(".gitignore"), "*.log\n/target\n").unwrap();

    let out = rustible(tmp.path(), &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(tmp.path().join("Cargo.toml").exists());
    assert_eq!(read(tmp.path(), "README.md"), "# my infra\n");
    assert_eq!(read(tmp.path(), "LICENSE"), "MIT\n");
    assert_eq!(
        read(tmp.path(), ".gitignore"),
        "*.log\n/target\n/.rustible\n"
    );
}

/// Only a file `init` itself writes stops it, and the message names it.
#[test]
fn refuses_only_on_conflicting_files_and_names_them() {
    let tmp = tempfile::tempdir().unwrap();
    fs::write(tmp.path().join("notes.txt"), "").unwrap();
    fs::write(tmp.path().join("hosts.kdl"), "// mine\n").unwrap();

    let out = rustible(tmp.path(), &["init"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("already has hosts.kdl"), "{err}");
    assert!(!err.contains("notes.txt"), "{err}");
    assert!(!tmp.path().join("Cargo.toml").exists());

    // --force adds the missing files and keeps the user-owned one.
    let out = rustible(tmp.path(), &["init", "--force"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(tmp.path().join("Cargo.toml").exists());
    assert_eq!(read(tmp.path(), "hosts.kdl"), "// mine\n");
    assert_eq!(read(tmp.path(), "notes.txt"), "");

    // A second run without --force now conflicts on everything it generated.
    let out = rustible(tmp.path(), &["init"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("Cargo.toml, build.rs, src/main.rs"), "{err}");
    assert!(err.contains("will not overwrite them"), "{err}");
}

#[test]
fn crates_io_versions_by_default_and_name_from_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("my infra");
    let out = rustible(tmp.path(), &["init", "my infra"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let manifest = read(&dir, "Cargo.toml");
    let version = env!("CARGO_PKG_VERSION");
    assert!(manifest.contains("name = \"my_infra\""));
    assert!(manifest.contains(&format!("rustible = \"{version}\"")));
    assert!(manifest.contains(&format!("rustible-std = \"{version}\"")));
    assert!(manifest.contains(&format!("rustible-build = \"{version}\"")));
    assert!(!manifest.contains("crates/rustible"));
}

#[test]
fn refresh_rewrites_only_the_shims() {
    let tmp = tempfile::tempdir().unwrap();
    let out = rustible(tmp.path(), &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));
    fs::write(tmp.path().join("build.rs"), "// edited\n").unwrap();
    fs::write(tmp.path().join("src/main.rs"), "// edited\n").unwrap();
    fs::write(tmp.path().join("src/lib.rs"), "// mine\n").unwrap();
    fs::write(tmp.path().join("hosts.kdl"), "// mine\n").unwrap();

    let out = rustible(tmp.path(), &["init", "--refresh"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(read(tmp.path(), "build.rs").starts_with("//! Generated by `rustible init`"));
    assert!(read(tmp.path(), "src/main.rs").starts_with("//! Generated by `rustible init`"));
    assert_eq!(read(tmp.path(), "src/lib.rs"), "// mine\n");
    assert_eq!(read(tmp.path(), "hosts.kdl"), "// mine\n");

    // Not a workspace: refused.
    let other = tempfile::tempdir().unwrap();
    let out = rustible(other.path(), &["init", "--refresh"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not a rustible workspace"));
}

#[test]
fn path_deps_must_point_at_a_checkout() {
    let tmp = tempfile::tempdir().unwrap();
    let out = rustible(tmp.path(), &["init", "--path-deps", "/nonexistent"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("expected a rustible checkout"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn playbook_create_writes_a_skeleton_and_prints_the_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let out = rustible(tmp.path(), &["init"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let out = rustible(
        tmp.path(),
        &["playbook", "create", "playbooks/cadu/hello.rs"],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    let src = read(tmp.path(), "playbooks/cadu/hello.rs");
    assert!(src.starts_with("//! cadu/hello: "));
    assert!(src.contains("#[rustible::playbook(hosts = \"local\")]"));
    assert!(src.contains("// #[rustible::vars]"));
    assert!(src.contains("ctx.log(\"hello from cadu/hello\");"));
    let err = stderr(&out);
    assert!(err.contains("check-on-save"), "{err}");
    assert!(!err.contains("warning:"), "{err}");

    // Existing file: refused, untouched.
    let out = rustible(
        tmp.path(),
        &["playbook", "create", "playbooks/cadu/hello.rs"],
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("already exists"));
    assert_eq!(read(tmp.path(), "playbooks/cadu/hello.rs"), src);

    // `.rs` added when missing; outside playbooks/ warns.
    let out = rustible(tmp.path(), &["playbook", "create", "src/stray"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(tmp.path().join("src/stray.rs").is_file());
    assert!(stderr(&out).contains("warning:"), "{}", stderr(&out));
}
