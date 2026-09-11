//! `rustible init`: create a Rustible workspace (vision doc sections 3, 9,
//! 10.4), or with `--refresh` rewrite only its two generated shims.
//!
//! The templates live in `templates/` next to this crate and are embedded at
//! compile time. `examples/workspace` in the repository is what this module
//! generates (with `--path-deps ../..`); a test keeps the two identical.

use std::fs;
use std::path::{Path, PathBuf};

use crate::toolchain::Compilers;
use crate::workspace::normalize;

use anyhow::{Context, Result, bail, ensure};
use clap::Args;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The version in the workspace manifest between releases. Nobody can publish
/// `0.0.0`, so it means exactly one thing: built from source, not released.
/// `release.yml` rewrites it from the tag at publish time, so a binary still
/// reporting it came from a checkout.
const PLACEHOLDER_VERSION: &str = "0.0.0";

const CARGO_TOML: &str = include_str!("../templates/Cargo.toml.tmpl");
const BUILD_RS: &str = include_str!("../templates/build.rs.tmpl");
const MAIN_RS: &str = include_str!("../templates/main.rs.tmpl");
const LIB_RS: &str = include_str!("../templates/lib.rs.tmpl");
const CARGO_CONFIG: &str = include_str!("../templates/cargo-config.toml");
const HOSTS_KDL: &str = include_str!("../templates/hosts.kdl");
const RUSTIBLE_TOML: &str = include_str!("../templates/rustible.toml");
const GITIGNORE: &str = include_str!("../templates/gitignore");
const README_MD: &str = include_str!("../templates/README.md.tmpl");

/// The paths [`generate`] writes, in write order. Kept equal to `generate`'s
/// own paths by a test; [`conflicts`] needs them before a package name or a
/// dependency style has been decided.
const GENERATED_PATHS: [&str; 7] = [
    "Cargo.toml",
    "build.rs",
    "src/main.rs",
    "src/lib.rs",
    ".cargo/config.toml",
    "hosts.kdl",
    "rustible.toml",
];

/// The crates a workspace depends on, and the section each goes in.
const DEPENDENCIES: [&str; 2] = ["rustible", "rustible-std"];
const BUILD_DEPENDENCIES: [&str; 1] = ["rustible-build"];

/// Create a Rustible workspace: a Cargo package with the generated shims,
/// an inventory, and a `playbooks/` folder.
#[derive(Args, Debug)]
pub struct InitArgs {
    /// Directory to create the workspace in (created if missing). Defaults to
    /// the current directory. Other files in it are left alone; only a file
    /// `init` would itself write is a conflict.
    #[arg(default_value = ".")]
    pub dir: PathBuf,
    /// Package name; defaults to the directory name, sanitized to a valid
    /// crate name.
    #[arg(long)]
    pub name: Option<String>,
    /// Write even when files `init` generates already exist. Those files are
    /// kept (only the two generated shims are rewritten); missing ones are
    /// added.
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
        if !args.force {
            let clashes = conflicts(dir);
            if !clashes.is_empty() {
                bail!(
                    "{} already has {}; `rustible init` will not overwrite {}. \
                     Use --force to add only the missing files (existing files are kept, \
                     only the two generated shims are rewritten)",
                    dir.display(),
                    clashes.join(", "),
                    if clashes.len() == 1 { "it" } else { "them" }
                );
            }
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
        None => {
            // The workspace manifest carries PLACEHOLDER_VERSION and the
            // release workflow rewrites it from the tag before publishing, so
            // a binary still reporting it was built from a checkout rather
            // than installed from crates.io. Its `{krate} = "<version>"` lines
            // would then pin the workspace to the name-reservation releases,
            // which contain almost nothing, and the failure arrives much later
            // as a cargo error about a missing generated file.
            if VERSION == PLACEHOLDER_VERSION {
                eprintln!(
                    "warning: this `rustible` was built from a checkout, so the workspace it \
                     writes\n         would depend on rustible {PLACEHOLDER_VERSION}, which is \
                     not published and\n         never will be, and cargo will refuse to \
                     resolve it.\n\n         Point it at your checkout instead:\n\n           \
                     rustible init --path-deps /path/to/rustible {}\n\n         \
                     Or install a published build: cargo install rustible-cli\n",
                    dir.display()
                );
            }
            Deps::CratesIo
        }
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
    ensure_readme(dir, &name)?;

    // A warning, not a failure: `init` compiles nothing, and a user who has
    // just created a workspace would rather hear about a missing compiler now
    // than at their first `playbook run` (vision 5.3, and
    // `docs/plan/reports/C-TOOLCHAIN-SPIKE.md`).
    if let Some(w) = Compilers::probe().init_warning() {
        eprintln!("\n{w}");
    }

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

/// Create `README.md` when the directory has none, and never touch one that
/// is already there.
///
/// It is not in `GENERATED_PATHS`, so a clone that already has a README is
/// still a valid `init` target and keeps its own — the same treatment
/// `.gitignore` gets, and for the same reason: refusing would make `init`
/// unusable in exactly the repositories people run it in.
///
/// The point of writing one at all is the link it carries. A workspace is
/// nine files of shims and inventory with nothing saying what they are, so
/// anyone — or any agent — landing in a shared repository has no way to learn
/// what Rustible is or how to drive it. The README names the project and
/// points at `docs/USING_RUSTIBLE.md`, which is enough to start from cold.
fn ensure_readme(dir: &Path, name: &str) -> Result<()> {
    if dir.join("README.md").symlink_metadata().is_ok() {
        return Ok(());
    }
    write_file(dir, "README.md", &README_MD.replace("{{name}}", name))
}

fn ensure_gitkeep(dir: &Path) -> Result<()> {
    if dir.join("playbooks/.gitkeep").exists() {
        return Ok(());
    }
    write_file(dir, "playbooks/.gitkeep", "")
}

/// The files already in `dir` that `init` would overwrite, in write order.
///
/// Cargo's rule (`cargo init`): a directory is not refused for being
/// non-empty, only for holding a file the generator itself writes. A clone
/// with a `README.md`, a `LICENSE` and its own `.gitignore` is therefore
/// fine, and those files are never read or touched. `.gitignore` and
/// `playbooks/.gitkeep` are not conflicts either: the first is appended to,
/// the second only created when missing.
///
/// A dangling symlink counts, hence `symlink_metadata` rather than `exists`.
pub fn conflicts(dir: &Path) -> Vec<&'static str> {
    GENERATED_PATHS
        .iter()
        .copied()
        .filter(|rel| dir.join(rel).symlink_metadata().is_ok())
        .collect()
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
///
/// Also lowercased. A directory called `MyInfra` is an entirely reasonable
/// thing to run `rustible init` in, and a package named `MyInfra` makes rustc
/// warn `crate MyInfra should have a snake case name` on **every** build
/// thereafter — a permanent papercut from a directory name. Cargo itself does
/// not lowercase, but cargo is not generating a package whose builds a
/// person will watch scroll past for months.
pub fn sanitize_package_name(raw: &str) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
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

    /// The generated manifest pins whatever version this binary reports. A
    /// release build reports the tag; a checkout build reports
    /// `PLACEHOLDER_VERSION`, which is why `init` warns in that case.
    #[test]
    fn crates_io_deps_pin_this_binarys_own_version() {
        let files = generate("rx", &Deps::CratesIo);
        let manifest = files
            .iter()
            .find(|f| f.path == "Cargo.toml")
            .expect("a manifest");
        let text = &manifest.contents;
        // Every rustible dependency line carries this binary's own version,
        // and none carries anything else — a hardcoded version in the template
        // would show up here as a line that disagrees.
        let pinned: Vec<&str> = text.lines().filter(|l| l.starts_with("rustible")).collect();
        assert!(!pinned.is_empty(), "no rustible dependency lines: {text}");
        for line in pinned {
            assert!(
                line.contains(&format!("\"{VERSION}\"")),
                "`{line}` does not pin this binary's version ({VERSION})"
            );
        }
    }

    #[test]
    fn sanitizes_directory_names() {
        assert_eq!(sanitize_package_name("my_infra"), "my_infra");
        assert_eq!(sanitize_package_name("my-infra"), "my-infra");
        assert_eq!(sanitize_package_name("my infra.v2"), "my_infra_v2");
        assert_eq!(sanitize_package_name("2026"), "_2026");
        assert_eq!(sanitize_package_name("ação"), "a__o");
        // Lowercased, so `rustible init MyInfra` does not leave rustc warning
        // `crate MyInfra should have a snake case name` on every later build.
        assert_eq!(sanitize_package_name("MyInfra"), "myinfra");
        assert_eq!(sanitize_package_name("My-Infra"), "my-infra");
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
    fn readme_is_written_when_absent_and_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        ensure_readme(dir, "myinfra").unwrap();
        let written = fs::read_to_string(dir.join("README.md")).unwrap();
        assert!(written.starts_with("# myinfra\n"), "{written}");
        // The whole point: an agent landing here can find out what this is.
        assert!(
            written
                .contains("https://github.com/flipbit03/rustible/blob/main/docs/USING_RUSTIBLE.md"),
            "the guide link is the reason this file exists: {written}"
        );
        assert!(!written.contains("{{name}}"), "unsubstituted placeholder");

        // A second run keeps whatever is there, including a user's own.
        fs::write(dir.join("README.md"), "mine\n").unwrap();
        ensure_readme(dir, "myinfra").unwrap();
        assert_eq!(fs::read_to_string(dir.join("README.md")).unwrap(), "mine\n");
    }

    #[test]
    fn a_clone_with_its_own_readme_is_still_a_valid_init_target() {
        // README.md is deliberately outside GENERATED_PATHS. If it were in,
        // `init` would refuse every repository that already has one, which is
        // most of them.
        assert!(
            !GENERATED_PATHS.contains(&"README.md"),
            "adding README.md to GENERATED_PATHS makes `init` refuse ordinary clones"
        );
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("README.md"), "theirs\n").unwrap();
        assert!(conflicts(tmp.path()).is_empty());
    }

    #[test]
    fn refresh_does_not_touch_the_readme() {
        // `--refresh` rewrites the two shims and nothing else; the README is
        // the user's once it exists.
        assert!(shims().iter().all(|g| g.path != "README.md"));
    }

    #[test]
    fn generated_paths_match_what_generate_writes() {
        let written: Vec<&str> = generate("rx", &Deps::CratesIo)
            .iter()
            .map(|g| g.path)
            .collect();
        assert_eq!(written, GENERATED_PATHS.to_vec());
    }

    #[test]
    fn only_generated_files_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // A freshly cloned repository: nothing here is ours.
        fs::create_dir(dir.join(".git")).unwrap();
        for rel in ["README.md", "LICENSE", ".gitignore", "notes.txt"] {
            fs::write(dir.join(rel), "").unwrap();
        }
        fs::create_dir_all(dir.join("playbooks")).unwrap();
        fs::write(dir.join("playbooks/.gitkeep"), "").unwrap();
        assert!(conflicts(dir).is_empty(), "{:?}", conflicts(dir));

        // One file we would write is enough, and it is named.
        fs::create_dir_all(dir.join("src")).unwrap();
        fs::write(dir.join("src/main.rs"), "").unwrap();
        assert_eq!(conflicts(dir), vec!["src/main.rs"]);

        // Reported in write order, not discovery order.
        fs::write(dir.join("Cargo.toml"), "").unwrap();
        assert_eq!(conflicts(dir), vec!["Cargo.toml", "src/main.rs"]);
    }

    #[test]
    fn a_dangling_symlink_still_conflicts() {
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink("nowhere", tmp.path().join("hosts.kdl")).unwrap();
            assert!(!tmp.path().join("hosts.kdl").exists());
            assert_eq!(conflicts(tmp.path()), vec!["hosts.kdl"]);
        }
    }

    #[test]
    fn lib_doc_uses_the_crate_identifier() {
        let files = generate("my-infra", &Deps::CratesIo);
        let lib = files.iter().find(|f| f.path == "src/lib.rs").unwrap();
        assert!(lib.contents.contains("`my_infra::helper()`"));
    }
}
