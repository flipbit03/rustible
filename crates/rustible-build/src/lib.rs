//! Build-script helper for Rustible workspaces (vision doc section 9 and
//! `docs/05_SPIKE_PLAYBOOK_DISCOVERY.md`).
//!
//! The generated `build.rs` of a workspace is one line: `rustible_build::discover()`.
//! It walks `playbooks/**/*.rs`, registers every file that carries a function
//! marked `#[rustible::playbook]`, and writes `$OUT_DIR/playbooks.rs` with one
//! `#[path]` module per playbook and a `PLAYBOOKS` registry that the generated
//! `src/main.rs` hands to `rustible::runtime::main`.
//!
//! With `RUSTIBLE_PLAYBOOK=<name>` set *and* the `selected` Cargo feature
//! enabled, only that playbook is included, so a shipped binary carries exactly
//! one playbook and a broken sibling cannot block a run. Without the feature
//! the variable is refused, so a stray export never narrows an IDE or CI build.

use std::fmt;
use std::path::{Path, PathBuf};

/// One playbook file found under `playbooks/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovered {
    /// Path under `playbooks/` without the extension, `/`-separated: `cadu/mc`.
    pub name: String,
    /// Absolute path of the file.
    pub path: PathBuf,
}

#[derive(Debug)]
pub enum DiscoverError {
    Io(PathBuf, std::io::Error),
    Parse {
        path: PathBuf,
        line: usize,
        col: usize,
        msg: String,
    },
    TwoMarkers {
        path: PathBuf,
        fns: Vec<String>,
    },
    UnknownSelection {
        wanted: String,
        available: Vec<String>,
    },
    SelectedWithoutFeature(String),
}

impl fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscoverError::Io(p, e) => write!(f, "reading {}: {e}", p.display()),
            DiscoverError::Parse {
                path,
                line,
                col,
                msg,
            } => {
                write!(
                    f,
                    "playbook file {}:{line}:{col} does not parse: {msg}",
                    path.display()
                )
            }
            DiscoverError::TwoMarkers { path, fns } => write!(
                f,
                "playbook file {} has {} functions marked #[rustible::playbook] ({}); a playbook file has exactly one",
                path.display(),
                fns.len(),
                fns.join(", ")
            ),
            DiscoverError::UnknownSelection { wanted, available } => write!(
                f,
                "RUSTIBLE_PLAYBOOK=`{wanted}` does not name a playbook; available: {}",
                if available.is_empty() {
                    "(none)".to_string()
                } else {
                    available.join(", ")
                }
            ),
            DiscoverError::SelectedWithoutFeature(name) => write!(
                f,
                "RUSTIBLE_PLAYBOOK=`{name}` is set but the `selected` Cargo feature is not enabled. \
                 The rustible CLI passes `--features selected` with it; an IDE or CI build must not set \
                 RUSTIBLE_PLAYBOOK at all (vision doc section 9)."
            ),
        }
    }
}

impl std::error::Error for DiscoverError {}

/// Entry point for a workspace's `build.rs`. Prints cargo directives, and on
/// error prints the message and exits non-zero so cargo shows it.
pub fn discover() {
    println!("cargo:rerun-if-changed=playbooks");
    println!("cargo:rerun-if-env-changed=RUSTIBLE_PLAYBOOK");
    if let Err(e) = discover_inner() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn discover_inner() -> Result<(), DiscoverError> {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let selected = std::env::var("RUSTIBLE_PLAYBOOK")
        .ok()
        .filter(|s| !s.is_empty());
    let feature_on = std::env::var_os("CARGO_FEATURE_SELECTED").is_some();
    if let (Some(name), false) = (&selected, feature_on) {
        return Err(DiscoverError::SelectedWithoutFeature(name.clone()));
    }

    let dir = root.join("playbooks");
    let found = if dir.is_dir() {
        scan(&dir)?
    } else {
        println!(
            "cargo:warning=no `playbooks/` directory in {}; zero playbooks registered",
            root.display()
        );
        Vec::new()
    };
    let chosen = select(found, selected.as_deref())?;
    let code = render(&chosen);
    std::fs::write(out_dir.join("playbooks.rs"), code)
        .map_err(|e| DiscoverError::Io(out_dir.clone(), e))?;
    Ok(())
}

/// Walk `dir` recursively and return every playbook file, sorted by name.
pub fn scan(dir: &Path) -> Result<Vec<Discovered>, DiscoverError> {
    let mut files = Vec::new();
    walk(dir, &mut files)?;
    files.sort();
    let mut found = Vec::new();
    for path in files {
        let src = std::fs::read_to_string(&path).map_err(|e| DiscoverError::Io(path.clone(), e))?;
        let file = syn::parse_file(&src).map_err(|e| {
            let span = e.span().start();
            DiscoverError::Parse {
                path: path.clone(),
                line: span.line,
                col: span.column + 1,
                msg: e.to_string(),
            }
        })?;
        let marked = marked_fns(&file);
        match marked.len() {
            0 => {}
            1 => {
                let rel = path
                    .strip_prefix(dir)
                    .expect("under dir")
                    .with_extension("");
                let name = rel
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                found.push(Discovered {
                    name,
                    path: path.clone(),
                });
            }
            _ => return Err(DiscoverError::TwoMarkers { path, fns: marked }),
        }
    }
    Ok(found)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), DiscoverError> {
    let entries = std::fs::read_dir(dir).map_err(|e| DiscoverError::Io(dir.to_path_buf(), e))?;
    for entry in entries {
        let entry = entry.map_err(|e| DiscoverError::Io(dir.to_path_buf(), e))?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// Names of functions carrying `#[rustible::playbook(..)]` or `#[playbook(..)]`.
fn marked_fns(file: &syn::File) -> Vec<String> {
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Fn(f) if f.attrs.iter().any(is_playbook_attr) => {
                Some(f.sig.ident.to_string())
            }
            _ => None,
        })
        .collect()
}

fn is_playbook_attr(attr: &syn::Attribute) -> bool {
    let segs: Vec<String> = attr
        .path()
        .segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect();
    matches!(segs.as_slice(), [p] if p == "playbook")
        || matches!(segs.as_slice(), [r, p] if r == "rustible" && p == "playbook")
}

/// Apply `RUSTIBLE_PLAYBOOK` selection.
pub fn select(
    found: Vec<Discovered>,
    selected: Option<&str>,
) -> Result<Vec<Discovered>, DiscoverError> {
    match selected {
        None => Ok(found),
        Some(wanted) => {
            let available: Vec<String> = found.iter().map(|d| d.name.clone()).collect();
            let chosen: Vec<Discovered> = found.into_iter().filter(|d| d.name == wanted).collect();
            if chosen.is_empty() {
                Err(DiscoverError::UnknownSelection {
                    wanted: wanted.to_string(),
                    available,
                })
            } else {
                Ok(chosen)
            }
        }
    }
}

/// The Rust source of `$OUT_DIR/playbooks.rs`.
pub fn render(found: &[Discovered]) -> String {
    let mut code = String::from("// Generated by rustible-build. Do not edit.\n");
    let mut registry = String::from(
        "#[allow(clippy::type_complexity)]\npub static PLAYBOOKS: &[::rustible::registry::Named] = &[\n",
    );
    for d in found {
        let ident = format!("__pb_{}", d.name.replace(['/', '-', '.'], "_"));
        code.push_str(&format!(
            "#[allow(non_snake_case, dead_code, clippy::all)]\n#[path = {:?}]\npub mod {ident};\n",
            d.path.display()
        ));
        registry.push_str(&format!(
            "    ::rustible::registry::Named {{ name: {:?}, playbook: &{ident}::__RUSTIBLE_PLAYBOOK }},\n",
            d.name
        ));
    }
    registry.push_str("];\n");
    code + &registry
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> tempfile::TempDir {
        let t = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let p = t.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
        t
    }

    const PB: &str = "use rustible::prelude::*;\n#[rustible::playbook(hosts = \"local\")]\nfn main(ctx: &mut Ctx) -> Result<()> { Ok(()) }\n";

    #[test]
    fn finds_nested_marked_files_and_names_them() {
        let t = tree(&[
            ("cadu/a.rs", PB),
            ("ops/deploy/b.rs", PB),
            ("top.rs", PB),
            ("cadu/util.rs", "pub fn helper() {}"),
        ]);
        let found = scan(t.path()).unwrap();
        let names: Vec<_> = found.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["cadu/a", "ops/deploy/b", "top"]);
    }

    #[test]
    fn bare_playbook_attribute_counts() {
        let t = tree(&[(
            "x.rs",
            "use rustible::playbook;\n#[playbook(hosts = \"local\")]\nfn main(ctx: &mut Ctx) -> Result<()> { Ok(()) }\n",
        )]);
        assert_eq!(scan(t.path()).unwrap().len(), 1);
    }

    #[test]
    fn attribute_in_comment_or_string_is_ignored() {
        let t = tree(&[(
            "x.rs",
            "// #[rustible::playbook(hosts = \"x\")]\nfn main() { let _s = \"#[rustible::playbook]\"; }\n",
        )]);
        assert!(scan(t.path()).unwrap().is_empty());
    }

    #[test]
    fn two_markers_in_one_file_is_an_error_naming_the_file() {
        let t = tree(&[(
            "two.rs",
            "#[rustible::playbook(hosts = \"a\")]\nfn main() {}\n#[playbook(hosts = \"b\")]\nfn main2() {}\n",
        )]);
        let e = scan(t.path()).unwrap_err().to_string();
        assert!(
            e.contains("two.rs") && e.contains("2 functions") && e.contains("main, main2"),
            "{e}"
        );
    }

    #[test]
    fn syntax_error_names_file_line_col() {
        let t = tree(&[("bad.rs", "fn main() {\n    let x = ;\n}\n")]);
        let e = scan(t.path()).unwrap_err().to_string();
        assert!(
            e.contains("bad.rs:2:13") && e.contains("does not parse"),
            "{e}"
        );
    }

    #[test]
    fn selection_filters_and_unknown_lists_available() {
        let t = tree(&[("a.rs", PB), ("b.rs", PB)]);
        let found = scan(t.path()).unwrap();
        assert_eq!(select(found.clone(), Some("a")).unwrap().len(), 1);
        let e = select(found, Some("zzz")).unwrap_err().to_string();
        assert!(e.contains("available: a, b"), "{e}");
    }

    #[test]
    fn render_emits_path_modules_and_registry() {
        let d = Discovered {
            name: "cadu/mc".into(),
            path: "/abs/playbooks/cadu/mc.rs".into(),
        };
        let code = render(&[d]);
        assert!(code.contains("#[path = \"/abs/playbooks/cadu/mc.rs\"]\npub mod __pb_cadu_mc;"));
        assert!(
            code.contains(
                "Named { name: \"cadu/mc\", playbook: &__pb_cadu_mc::__RUSTIBLE_PLAYBOOK }"
            )
        );
    }
}
