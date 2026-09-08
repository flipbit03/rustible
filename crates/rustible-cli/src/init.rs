//! `rustible init`: create a Rustible workspace (vision doc sections 3, 9,
//! 10.4), or with `--refresh` rewrite only its two generated shims.
//!
//! The templates live in `templates/` next to this crate and are embedded at
//! compile time. `examples/workspace` in the repository is what this module
//! generates (with `--path-deps ../..`); a test keeps the two identical.

use std::fs;
use std::path::{Path, PathBuf};

use crate::workspace::normalize;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const CARGO_TOML: &str = include_str!("../templates/Cargo.toml.tmpl");
const BUILD_RS: &str = include_str!("../templates/build.rs.tmpl");
const MAIN_RS: &str = include_str!("../templates/main.rs.tmpl");
const LIB_RS: &str = include_str!("../templates/lib.rs.tmpl");
const CARGO_CONFIG: &str = include_str!("../templates/cargo-config.toml");
const HOSTS_KDL: &str = include_str!("../templates/hosts.kdl");
const RUSTIBLE_TOML: &str = include_str!("../templates/rustible.toml");
const GITIGNORE: &str = include_str!("../templates/gitignore");

/// The crates a workspace depends on, and the section each goes in.
const DEPENDENCIES: [&str; 2] = ["rustible", "rustible-std"];
const BUILD_DEPENDENCIES: [&str; 1] = ["rustible-build"];

/// Create a Rustible workspace: a Cargo package with the generated shims,
/// an inventory, and a `playbooks/` folder.
#[derive(Args, Debug)]
pub struct InitArgs {
    /// Directory to create the workspace in (created if missing). Defaults to
    /// the current directory, which must be empty.
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    /// Package name; defaults to the directory name, sanitized to a valid
    /// crate name.
    #[arg(long)]
    pub name: Option<String>,
    /// Write into a non-empty directory. Files that already exist are kept
    /// (only the two generated shims are rewritten); missing ones are added.
    #[arg(long, conflicts_with = "refresh")]
    pub force: bool,
    /// Rewrite only the two generated shims (`build.rs`, `src/main.rs`) of an
    /// existing workspace, for example after upgrading rustible.
    #[arg(long, conflicts_with_all = ["force", "path_deps", "name"])]
    pub refresh: bool,
    /// Development only: depend on the crates of a rustible checkout by path
    /// instead of crates.io versions. A relative path is relative to the new
    /// workspace.
    #[arg(long, value_name = "CHECKOUT")]
    pub path_deps: Option<PathBuf>,
}

/// One file `init` writes, relative to the workspace root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    pub path: &'static str,
    pub contents: String,
}

/// How the `[dependencies]` lines are written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Deps {
    /// `rustible = "<version>"`, the CLI's own version.
    CratesIo,
    /// `rustible = { path = "<checkout>/crates/rustible" }`, as given.
    Path(PathBuf),
}

/// Run `rustible init`.
pub fn run(args: InitArgs) -> Result<()> {
    if args.refresh {
        return refresh(&args.dir);
    }

    // An empty argument (`rustible init ""`) means the current directory,
    // and must go through the same emptiness check as `.`.
    let dir = if args.dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        args.dir.clone()
    };
    let dir = &dir;
    if dir.exists() {
        ensure!(
            dir.is_dir(),
            "{} exists and is not a directory",
            dir.display()
        );
        if !args.force && !is_empty_dir(dir)? {
            bail!(
                "{} is not empty; use --force to add the missing files (existing files are kept, only the two shims are rewritten)",
                dir.display()
            );
        }
    }

    let name = match &args.name {
        Some(n) => {
            validate_package_name(n).with_context(|| format!("--name `{n}`"))?;
            n.clone()
        }
        None => package_name_from_dir(dir)?,
    };

    let deps = match &args.path_deps {
        Some(checkout) => {
            // `dir` may not exist yet, so `..` must be resolved lexically.
            let resolved = normalize(&if checkout.is_absolute() {
                checkout.clone()
            } else {
                dir.join(checkout)
            });
            for krate in DEPENDENCIES.iter().chain(BUILD_DEPENDENCIES.iter()) {
                let manifest = resolved.join("crates").join(krate).join("Cargo.toml");
                ensure!(
                    manifest.is_file(),
                    "--path-deps {}: {} not found; expected a rustible checkout",
                    checkout.display(),
                    manifest.display()
                );
            }
            Deps::Path(checkout.clone())
        }
        None => Deps::CratesIo,
    };

    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let shim_paths: Vec<&str> = shims().iter().map(|g| g.path).collect();
    for file in generate(&name, &deps) {
        let target = dir.join(file.path);
        // With --force, user-owned files (Cargo.toml, src/lib.rs, hosts.kdl,
        // rustible.toml, .cargo/config.toml) are never overwritten; the shims
        // are ours and always rewritten.
        if args.force && target.exists() && !shim_paths.contains(&file.path) {
            eprintln!("    kept existing {}", file.path);
            continue;
        }
        write_file(dir, file.path, &file.contents)?;
    }
    ensure_gitignore(dir)?;
    ensure_gitkeep(dir)?;

    eprintln!(
        "\nWorkspace `{name}` is ready. Next:\n    cd {}\n    rustible playbook create playbooks/hello.rs\n    rustible playbook run playbooks/hello.rs",
        dir.display()
    );
    Ok(())
}

/// Every file `init` generates from a template, in write order.
/// `.gitignore` and `playbooks/.gitkeep` are handled separately because they
/// are added to, never overwritten.
pub fn generate(name: &str, deps: &Deps) -> Vec<Generated> {
    let crate_ident = name.replace('-', "_");
    let [build_rs, main_rs] = shims();
    vec![
        Generated {
            path: "Cargo.toml",
            contents: CARGO_TOML
                .replace("{{name}}", name)
                .replace("{{dependencies}}", &render_deps(&DEPENDENCIES, deps))
                .replace(
                    "{{build_dependencies}}",
                    &render_deps(&BUILD_DEPENDENCIES, deps),
                ),
        },
        build_rs,
        main_rs,
        Generated {
            path: "src/lib.rs",
            contents: LIB_RS.replace("{{crate_ident}}", &crate_ident),
        },
        Generated {
            path: ".cargo/config.toml",
            contents: CARGO_CONFIG.to_string(),
        },
        Generated {
            path: "hosts.kdl",
            contents: HOSTS_KDL.to_string(),
        },
        Generated {
            path: "rustible.toml",
            contents: RUSTIBLE_TOML.to_string(),
        },
    ]
}

/// The two shims, the only files `--refresh` touches.
pub fn shims() -> [Generated; 2] {
    [
        Generated {
            path: "build.rs",
            contents: shim(BUILD_RS),
        },
        Generated {
            path: "src/main.rs",
            contents: shim(MAIN_RS),
        },
    ]
}

fn shim(template: &str) -> String {
    template.replace("{{version}}", VERSION)
}

fn render_deps(crates: &[&str], deps: &Deps) -> String {
    let lines: Vec<String> = crates
        .iter()
        .map(|krate| match deps {
            Deps::CratesIo => format!("{krate} = \"{VERSION}\""),
            Deps::Path(checkout) => format!(
                "{krate} = {{ path = \"{}/crates/{krate}\" }}",
                checkout.display()
            ),
        })
        .collect();
    lines.join("\n")
}

fn refresh(dir: &Path) -> Result<()> {
    ensure!(
        dir.join("rustible.toml").is_file() && dir.join("Cargo.toml").is_file(),
        "{} is not a rustible workspace (no rustible.toml and Cargo.toml); run `rustible init` first",
        dir.display()
    );
    for file in shims() {
        write_file(dir, file.path, &file.contents)?;
    }
    Ok(())
}

fn write_file(dir: &Path, rel: &str, contents: &str) -> Result<()> {
    crate::workspace::write_file(&dir.join(rel), contents, rel)
}

/// Create `.gitignore` with the template, or append the lines it lacks.
fn ensure_gitignore(dir: &Path) -> Result<()> {
    let path = dir.join(".gitignore");
    if !path.exists() {
        return write_file(dir, ".gitignore", GITIGNORE);
    }
    let existing =
        fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let missing: Vec<&str> = GITIGNORE
        .lines()
        .filter(|want| !existing.lines().any(|have| have.trim() == *want))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for line in &missing {
        out.push_str(line);
        out.push('\n');
    }
    fs::write(&path, out).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("    appended {} to .gitignore", missing.join(", "));
    Ok(())
}

fn ensure_gitkeep(dir: &Path) -> Result<()> {
    if dir.join("playbooks/.gitkeep").exists() {
        return Ok(());
    }
    write_file(dir, "playbooks/.gitkeep", "")
}

/// A directory counts as empty when it holds nothing but `.git`, so
/// `git init` before `rustible init` works.
fn is_empty_dir(dir: &Path) -> Result<bool> {
    for entry in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        if entry.file_name() != ".git" {
            return Ok(false);
        }
    }
    Ok(true)
}

fn package_name_from_dir(dir: &Path) -> Result<String> {
    // `.` and `..` have no file name; resolve them lexically (never
    // `canonicalize`, which would name a symlinked directory after its target).
    let abs = crate::workspace::absolute(dir)?;
    let raw = abs
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot derive a package name from {}; pass --name",
                dir.display()
            )
        })?;
    let name = sanitize_package_name(raw);
    validate_package_name(&name).with_context(|| {
        format!("directory name `{raw}` does not make a usable package name; pass --name")
    })?;
    Ok(name)
}

/// Turn a directory name into a crate name the way `cargo new` would accept:
/// anything but ASCII alphanumerics, `-` and `_` becomes `_`, and a leading
/// digit gets a `_` prefix.
pub fn sanitize_package_name(raw: &str) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name
}

/// Rust keywords and the names Cargo or this workspace reserve.
const RESERVED: &[&str] = &[
    "as",
    "break",
    "const",
    "continue",
    "crate",
    "else",
    "enum",
    "extern",
    "false",
    "fn",
    "for",
    "if",
    "impl",
    "in",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "pub",
    "ref",
    "return",
    "self",
    "Self",
    "static",
    "struct",
    "super",
    "trait",
    "true",
    "type",
    "unsafe",
    "use",
    "where",
    "while",
    "async",
    "await",
    "dyn",
    "abstract",
    "become",
    "box",
    "do",
    "final",
    "macro",
    "override",
    "priv",
    "typeof",
    "unsized",
    "virtual",
    "yield",
    "try",
    "gen",
    "test",
    "core",
    "std",
    "alloc",
    "proc_macro",
    "proc-macro",
    // Cargo forbids these as binary target names (they collide with its
    // build directory layout).
    "build",
    "deps",
    "examples",
    "incremental",
];

/// The generated package's lib would shadow one of its own dependencies.
fn shadows_dependency(name: &str) -> bool {
    let ident = name.replace('-', "_");
    DEPENDENCIES
        .iter()
        .chain(BUILD_DEPENDENCIES.iter())
        .any(|d| d.replace('-', "_") == ident)
}

/// Reject names Cargo would refuse or that would shadow a dependency.
pub fn validate_package_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "package name is empty");
    ensure!(
        name.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "package name `{name}` may only contain ASCII letters, digits, `-` and `_`"
    );
    ensure!(
        !name.starts_with(|c: char| c.is_ascii_digit()),
        "package name `{name}` cannot start with a digit"
    );
    ensure!(
        !RESERVED.contains(&name),
        "`{name}` is a Rust keyword or a name Cargo reserves and cannot be a package name"
    );
    ensure!(
        !shadows_dependency(name),
        "`{name}` would shadow a dependency of the generated workspace; pick another name"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_directory_names() {
        assert_eq!(sanitize_package_name("my_infra"), "my_infra");
        assert_eq!(sanitize_package_name("my-infra"), "my-infra");
        assert_eq!(sanitize_package_name("my infra.v2"), "my_infra_v2");
        assert_eq!(sanitize_package_name("2026"), "_2026");
        assert_eq!(sanitize_package_name("ação"), "a__o");
    }

    #[test]
    fn normalizes_lexically_via_workspace() {
        assert_eq!(
            normalize(Path::new("/a/b/new/../../crates")),
            PathBuf::from("/a/crates")
        );
        assert_eq!(normalize(Path::new("x/./y/..")), PathBuf::from("x"));
        assert_eq!(normalize(Path::new("../../z")), PathBuf::from("../../z"));
    }

    #[test]
    fn rejects_reserved_names() {
        assert!(validate_package_name("type").is_err());
        assert!(validate_package_name("rustible").is_err());
        assert!(validate_package_name("test").is_err());
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("my-infra").is_ok());
    }

    #[test]
    fn crates_io_deps_use_the_cli_version() {
        let files = generate("rx", &Deps::CratesIo);
        let manifest = &files[0].contents;
        assert!(manifest.contains(&format!("rustible = \"{VERSION}\"")));
        assert!(manifest.contains(&format!("rustible-std = \"{VERSION}\"")));
        assert!(manifest.contains(&format!("rustible-build = \"{VERSION}\"")));
        assert!(manifest.contains("name = \"rx\""));
        assert!(manifest.contains("[features]\nselected = []"));
        assert!(manifest.contains("[profile.dist]"));
        assert!(!manifest.contains("{{"));
    }

    #[test]
    fn path_deps_point_at_the_checkout() {
        let files = generate("rx", &Deps::Path(PathBuf::from("../..")));
        let manifest = &files[0].contents;
        assert!(manifest.contains("rustible = { path = \"../../crates/rustible\" }"));
        assert!(manifest.contains("rustible-build = { path = \"../../crates/rustible-build\" }"));
    }

    #[test]
    fn shims_carry_the_header() {
        for shim in shims() {
            let first = shim.contents.lines().next().unwrap();
            assert_eq!(
                first,
                format!("//! Generated by `rustible init` (rustible {VERSION}). Do not edit.")
            );
            assert!(shim.contents.contains("`rustible init --refresh`"));
            assert!(shim.contents.contains("`playbooks/`"));
            assert!(shim.contents.contains("`src/lib.rs`"));
        }
    }

    #[test]
    fn lib_doc_uses_the_crate_identifier() {
        let files = generate("my-infra", &Deps::CratesIo);
        let lib = files.iter().find(|f| f.path == "src/lib.rs").unwrap();
        assert!(lib.contents.contains("`my_infra::helper()`"));
    }
}
