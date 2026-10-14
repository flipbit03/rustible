//! rust-analyzer shows `template!`'s compile-time errors as the build does
//! (T1).
//!
//! rust-analyzer expands proc macros itself and by default does not tell a
//! macro which file the call is in, so there `template!` finds the call's
//! file another way (`caller_dirs` in `src/template.rs`), which no rustc test
//! reaches. This copies `examples/workspace`, adds the playbooks of
//! `tests/rust_analyzer/` as `playbooks/ra/`, and runs `cargo check` and
//! `rust-analyzer diagnostics` on it. Every error marked `//~` must come from
//! both, one marked `//~ build:` from the build alone, and no other error from
//! either, in those playbooks or the example's own.
//!
//! It needs rust-analyzer (`rustup component add rust-analyzer`, or a binary
//! named by `RUST_ANALYZER`) and runs only with `RUSTIBLE_RUST_ANALYZER=1`,
//! as `make rust-analyzer` and CI do.

// This lays out a scratch workspace and runs cargo and rust-analyzer on the
// test machine. Vision 7.2's `sys` rule governs operations running on a
// target, not a harness exercising the build; see clippy.toml.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::process::Command;

/// An error a tool reported: its file relative to the workspace, its line
/// (from 1), and the message.
#[derive(Debug)]
struct Reported {
    file: String,
    line: usize,
    message: String,
}

/// A `//~` mark: the line it is on, whether rust-analyzer must report the
/// error too, and a part of the message.
#[derive(Debug)]
struct Mark {
    file: String,
    line: usize,
    editor: bool,
    part: String,
}

#[test]
fn template_errors_in_rust_analyzer_match_the_build() {
    if std::env::var_os("RUSTIBLE_RUST_ANALYZER").is_none() {
        eprintln!("skipped: set RUSTIBLE_RUST_ANALYZER=1 to run it (needs rust-analyzer)");
        return;
    }
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repo = manifest.join("../..").canonicalize().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let ws = scratch.path().canonicalize().unwrap().join("workspace");
    copy_example(&repo, &ws);
    copy_tree(
        &manifest.join("tests/rust_analyzer"),
        &ws.join("playbooks/ra"),
    );
    let marks = marks(&ws, &ws.join("playbooks/ra"));
    // Shared by both tools and kept between runs, so that only the first
    // run builds the dependencies.
    let target = Path::new(env!("CARGO_TARGET_TMPDIR")).join("rust-analyzer-target");

    let build = cargo_check(&ws, &target);
    let editor = rust_analyzer(&ws, &target);

    let mut failures = Vec::new();
    compare("cargo check", &build, &marks, |_| true, &mut failures);
    compare(
        "rust-analyzer",
        &editor,
        &marks,
        |m| m.editor,
        &mut failures,
    );
    assert!(
        failures.is_empty(),
        "{}\n\ncargo check reported:\n{}\n\nrust-analyzer reported:\n{}",
        failures.join("\n"),
        list(&build),
        list(&editor)
    );
}

/// Every mark `applies` to has an error on its line containing its part,
/// and every error is one of those: a second error on a marked line (a
/// failed expansion's own) counts as unexpected.
fn compare(
    tool: &str,
    reported: &[Reported],
    marks: &[Mark],
    applies: impl Fn(&Mark) -> bool,
    failures: &mut Vec<String>,
) {
    let expected: Vec<&Mark> = marks.iter().filter(|m| applies(m)).collect();
    for m in &expected {
        let found = reported
            .iter()
            .any(|r| r.file == m.file && r.line == m.line && r.message.contains(&m.part));
        if !found {
            failures.push(format!(
                "{tool}: {}:{}: no error containing {:?}",
                m.file, m.line, m.part
            ));
        }
    }
    for r in reported {
        let marked = expected
            .iter()
            .any(|m| m.file == r.file && m.line == r.line && r.message.contains(&m.part));
        if !marked {
            failures.push(format!(
                "{tool}: {}:{}: unexpected error: {}",
                r.file, r.line, r.message
            ));
        }
    }
}

fn list(reported: &[Reported]) -> String {
    reported
        .iter()
        .map(|r| format!("  {}:{}: {}", r.file, r.line, r.message))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The example workspace's checked-in files, with its path dependencies on
/// this repository's crates made absolute.
fn copy_example(repo: &Path, ws: &Path) {
    let out = Command::new("git")
        .args(["ls-files", "-z", "examples/workspace"])
        .current_dir(repo)
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git ls-files failed");
    for file in out.stdout.split(|&b| b == 0).filter(|f| !f.is_empty()) {
        let file = std::str::from_utf8(file).unwrap();
        let to = ws.join(file.strip_prefix("examples/workspace/").unwrap());
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::copy(repo.join(file), &to).unwrap();
    }
    let manifest = ws.join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let crates = format!("path = \"{}/crates/", repo.display());
    std::fs::write(&manifest, text.replace("path = \"../../crates/", &crates)).unwrap();
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            copy_tree(&path, &to.join(entry.file_name()));
        } else {
            std::fs::copy(&path, to.join(entry.file_name())).unwrap();
        }
    }
}

/// Every `//~` mark in the `.rs` files under `dir`.
fn marks(ws: &Path, dir: &Path) -> Vec<Mark> {
    let mut found = Vec::new();
    for path in rs_files(dir) {
        let file = relative(ws, &path);
        let text = std::fs::read_to_string(&path).unwrap();
        for (i, line) in text.lines().enumerate() {
            let Some((_, mark)) = line.split_once("//~ ") else {
                continue;
            };
            let (editor, part) = match mark.strip_prefix("build: ") {
                Some(part) => (false, part),
                None => (true, mark),
            };
            found.push(Mark {
                file: file.clone(),
                line: i + 1,
                editor,
                part: part.trim().to_string(),
            });
        }
    }
    found
}

fn rs_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            files.extend(rs_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs") {
            files.push(path);
        }
    }
    files
}

/// `path` relative to the workspace, `/`-separated.
fn relative(ws: &Path, path: &Path) -> String {
    path.strip_prefix(ws)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The errors `cargo check` reports in the workspace's own files.
fn cargo_check(ws: &Path, target: &Path) -> Vec<Reported> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let out = Command::new(cargo)
        // Plain text whatever the caller's CARGO_TERM_COLOR (CI sets it).
        .args(["check", "--message-format", "short", "--color", "never"])
        .current_dir(ws)
        .env("CARGO_TARGET_DIR", target)
        .output()
        .expect("cargo runs");
    let stderr = String::from_utf8_lossy(&out.stderr);
    // `<file>:<line>:<column>: error[<code>]: <message>`
    let reported: Vec<Reported> = stderr
        .lines()
        .filter_map(|l| {
            let mut parts = l.splitn(4, ':');
            let (file, line, _column, rest) =
                (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            let (kind, message) = rest.trim_start().split_once(": ")?;
            if !kind.starts_with("error") {
                return None;
            }
            Some(Reported {
                file: relative(ws, Path::new(file)),
                line: line.parse().ok()?,
                message: message.to_string(),
            })
        })
        .filter(|r| !r.file.starts_with('/'))
        .collect();
    assert!(
        !reported.is_empty() || out.status.success(),
        "cargo check failed with no error this test can read:\n{stderr}"
    );
    reported
}

/// The errors `rust-analyzer diagnostics` reports in the workspace's own
/// files. Its output format is unstable, so a run this cannot read fails
/// with the output.
fn rust_analyzer(ws: &Path, target: &Path) -> Vec<Reported> {
    let binary = std::env::var("RUST_ANALYZER").unwrap_or_else(|_| "rust-analyzer".into());
    let out = Command::new(&binary)
        .arg("diagnostics")
        .arg(ws)
        .env("CARGO_TARGET_DIR", target)
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{binary}`: {e}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("diagnostic scan complete"),
        "`{binary} diagnostics` did not finish:\n{text}"
    );
    // `at crate <name>, file <path>: <Severity> <kind> from LineCol { line:
    // <from 0>, col: <n> } to LineCol { .. }: <message>`, the message
    // possibly over several lines.
    let mut blocks: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.starts_with("at crate ") {
            blocks.push(line.to_string());
        } else if let Some(last) = blocks.last_mut()
            && !line.starts_with("diagnostic scan complete")
            && !line.is_empty()
        {
            last.push('\n');
            last.push_str(line);
        }
    }
    blocks
        .iter()
        .map(|block| {
            parse_ra(ws, block)
                .unwrap_or_else(|| panic!("cannot read this rust-analyzer diagnostic:\n{block}"))
        })
        .filter_map(|(severity, r)| (severity == "Error").then_some(r))
        .filter(|r| !r.file.starts_with('/'))
        .collect()
}

fn parse_ra(ws: &Path, block: &str) -> Option<(String, Reported)> {
    let rest = block.strip_prefix("at crate ")?;
    let (_, rest) = rest.split_once(", file ")?;
    let (file, rest) = rest.split_once(": ")?;
    let (severity, rest) = rest.split_once(' ')?;
    let (_, rest) = rest.split_once(" from LineCol { line: ")?;
    let (line, rest) = rest.split_once(',')?;
    let (_, message) = rest.split_once("}: ")?;
    Some((
        severity.to_string(),
        Reported {
            file: relative(ws, Path::new(file)),
            line: line.trim().parse::<usize>().ok()? + 1,
            message: message.to_string(),
        },
    ))
}
