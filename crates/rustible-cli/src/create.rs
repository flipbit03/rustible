//! `rustible playbook create <path>`: write a playbook skeleton (vision doc
//! sections 3 and 9) and print the rust-analyzer check-on-save hint.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::Args;

const PLAYBOOK: &str = include_str!("../templates/playbook.rs.tmpl");

/// Printed after every create (vision doc section 9): rust-analyzer links a
/// new playbook only after its build script re-runs.
pub const CHECK_ON_SAVE_HINT: &str = "\
rust-analyzer links a new playbook when its build script re-runs, which happens through
check-on-save (`cargo check`, on by default). With it on, the file is linked within seconds
of opening it; with it off, it shows as unlinked until you run \"Rebuild proc macros and
build scripts\" or any cargo command in a terminal.";

/// Scaffold a playbook file with the metadata attribute and a `main`.
#[derive(Args, Debug)]
pub struct CreateArgs {
    /// File to create, normally under `playbooks/`, e.g.
    /// `playbooks/cadu/ssh_enable_root_user.rs`. Missing directories are
    /// created; `.rs` is added when absent.
    pub path: PathBuf,
}

/// Run `rustible playbook create`.
pub fn run(args: CreateArgs) -> Result<()> {
    let mut path = args.path;
    if path.extension().is_none_or(|e| e != "rs") {
        path.set_extension("rs");
    }
    ensure!(
        !path.exists(),
        "{} already exists; not overwriting it",
        path.display()
    );

    let (name, warning) = playbook_name(&path)?;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&path, skeleton(&name)).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("    wrote {} (playbook `{name}`)", path.display());
    if let Some(w) = warning {
        eprintln!("warning: {w}");
    }
    eprintln!("\n{CHECK_ON_SAVE_HINT}");
    Ok(())
}

/// The file contents for a playbook called `name`.
pub fn skeleton(name: &str) -> String {
    PLAYBOOK.replace("{{name}}", name)
}

/// A playbook's name is its path under the workspace's `playbooks/` without
/// the extension (`cadu/x`). Outside a workspace, or outside `playbooks/`,
/// the name falls back to the file stem and a warning explains why the build
/// script will not find the file.
fn playbook_name(path: &Path) -> Result<(String, Option<String>)> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let stem = abs
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("{} has no file name", path.display()))?
        .to_string();

    let Some(root) = find_workspace_root(abs.parent().unwrap_or(&abs)) else {
        return Ok((
            stem,
            Some(format!(
                "{} is not inside a rustible workspace (no rustible.toml found walking up); \
                 only files under a workspace's playbooks/ are discovered",
                path.display()
            )),
        ));
    };
    let playbooks = root.join("playbooks");
    match abs.strip_prefix(&playbooks) {
        Ok(rel) => {
            let rel = rel.with_extension("");
            let name = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            Ok((name, None))
        }
        Err(_) => Ok((
            stem,
            Some(format!(
                "{} is outside {}; the build script only discovers playbooks there",
                path.display(),
                playbooks.display()
            )),
        )),
    }
}

/// Walk up from `start` looking for `rustible.toml` (vision doc section 10.4).
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join("rustible.toml").is_file() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skeleton_has_attribute_vars_and_log() {
        let s = skeleton("cadu/x");
        assert!(s.starts_with("//! cadu/x: "));
        assert!(s.contains("#[rustible::playbook(hosts = \"local\")]"));
        assert!(s.contains("// #[rustible::vars]\n// struct Vars {"));
        assert!(s.contains("fn main(ctx: &mut Ctx) -> Result<()> {"));
        assert!(s.contains("ctx.log(\"hello from cadu/x\");"));
        assert!(!s.contains("{{"));
    }

    #[test]
    fn names_come_from_the_playbooks_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        fs::write(root.join("rustible.toml"), "").unwrap();
        let (name, warn) = playbook_name(&root.join("playbooks/cadu/x.rs")).unwrap();
        assert_eq!(name, "cadu/x");
        assert!(warn.is_none());

        let (name, warn) = playbook_name(&root.join("src/x.rs")).unwrap();
        assert_eq!(name, "x");
        assert!(warn.unwrap().contains("outside"));

        let (name, warn) = playbook_name(Path::new("/nonexistent-rustible/y.rs")).unwrap();
        assert_eq!(name, "y");
        assert!(warn.unwrap().contains("not inside a rustible workspace"));
    }
}
