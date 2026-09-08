//! The workspace (vision doc section 10.4): root discovery by walking up to
//! `rustible.toml`, the settings in that file, and the paths everything
//! else resolves against. Also the lexical path helpers and file writing
//! that `init` and `playbook create` share.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// `rustible.toml`: flat settings, every path relative to the root.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The inventory file.
    #[serde(default = "default_inventory")]
    pub inventory: PathBuf,
    /// Where describe output and per-triple artifacts are cached.
    #[serde(default = "default_cache_dir")]
    pub cache_dir: PathBuf,
}

fn default_inventory() -> PathBuf {
    "hosts.kdl".into()
}

fn default_cache_dir() -> PathBuf {
    ".rustible".into()
}

impl Default for Config {
    fn default() -> Self {
        Config {
            inventory: default_inventory(),
            cache_dir: default_cache_dir(),
        }
    }
}

impl Config {
    /// Parse the file's text. An unknown key is an error naming it, so a
    /// typo cannot silently fall back to a default.
    pub fn parse(src: &str) -> Result<Config> {
        toml::from_str(src).map_err(|e| anyhow::anyhow!("{}", e.message()))
    }
}

/// A located workspace.
#[derive(Debug, Clone)]
pub struct Workspace {
    /// Absolute directory holding `rustible.toml`.
    pub root: PathBuf,
    pub config: Config,
}

impl Workspace {
    /// `--workspace <dir>` when given, else walk up from the current
    /// directory (vision 10.4).
    pub fn discover(explicit: Option<&Path>) -> Result<Workspace> {
        let root = match explicit {
            Some(dir) => {
                let dir = absolute(dir)?;
                if !dir.join("rustible.toml").is_file() {
                    bail!("{} has no rustible.toml", dir.display());
                }
                dir
            }
            None => {
                let cwd = std::env::current_dir().context("reading the current directory")?;
                match find_workspace_root(&cwd) {
                    Some(r) => r,
                    None => bail!(
                        "not inside a rustible workspace: no rustible.toml in {} or any parent \
                         (run `rustible init`, or pass --workspace <dir>)",
                        cwd.display()
                    ),
                }
            }
        };
        let file = root.join("rustible.toml");
        let src =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        let config = Config::parse(&src).with_context(|| format!("{}", file.display()))?;
        Ok(Workspace { root, config })
    }

    pub fn inventory_path(&self) -> PathBuf {
        self.root.join(&self.config.inventory)
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.root.join(&self.config.cache_dir)
    }

    pub fn playbooks_dir(&self) -> PathBuf {
        self.root.join("playbooks")
    }

    pub fn manifest(&self) -> PathBuf {
        self.root.join("Cargo.toml")
    }

    /// A playbook's registry name from what the user typed: a path
    /// (`playbooks/cadu/mc.rs`, relative to the current directory or
    /// absolute) or the name itself (`cadu/mc`). Either way the file must
    /// exist under `playbooks/`.
    pub fn playbook_name(&self, arg: &str) -> Result<String> {
        let dir = self.playbooks_dir();
        let by_name = dir.join(format!("{arg}.rs"));
        if !arg.ends_with(".rs") && by_name.is_file() {
            return Ok(arg.to_string());
        }
        let abs = absolute(Path::new(arg))?;
        if !abs.is_file() {
            bail!(
                "no playbook `{arg}`: neither {} nor {} exists (see `rustible playbook list`)",
                abs.display(),
                by_name.display()
            );
        }
        if abs.strip_prefix(&dir).is_err() {
            bail!(
                "{} is outside {}; playbooks live under playbooks/ (vision doc section 9)",
                abs.display(),
                dir.display()
            );
        }
        Ok(rustible_build::name_of(&dir, &abs))
    }

    /// The source path of a playbook, relative to the root, for messages.
    pub fn playbook_file(&self, name: &str) -> String {
        format!("playbooks/{name}.rs")
    }
}

/// Walk up from `start` looking for `rustible.toml`.
pub(crate) fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join("rustible.toml").is_file())
        .map(Path::to_path_buf)
}

/// Resolve `.` and `..` lexically, without touching the filesystem, so a path
/// that does not exist yet (or is a symlink) is handled the same way.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Absolute, normalized form of a user-supplied path.
pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize(&joined))
}

/// Write a file, creating parent directories, and say so.
pub(crate) fn write_file(path: &Path, contents: &str, shown_as: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("    wrote {shown_as}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_lexically() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize(Path::new("../x/../y")), PathBuf::from("../y"));
    }

    #[test]
    fn config_defaults_and_overrides() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let c =
            Config::parse("inventory = \"inv/prod.kdl\"\ncache_dir = \"/tmp/rcache\"\n").unwrap();
        assert_eq!(c.inventory, PathBuf::from("inv/prod.kdl"));
        assert_eq!(c.cache_dir, PathBuf::from("/tmp/rcache"));
        let c = Config::parse("# only a comment\ncache_dir = \"cache\"\n").unwrap();
        assert_eq!(c.inventory, PathBuf::from("hosts.kdl"));
        assert_eq!(c.cache_dir, PathBuf::from("cache"));
    }

    #[test]
    fn config_rejects_unknown_keys_and_wrong_types() {
        let e = Config::parse("inventroy = \"hosts.kdl\"\n").unwrap_err();
        assert!(e.to_string().contains("inventroy"), "{e}");
        let e = Config::parse("inventory = 3\n").unwrap_err();
        assert!(e.to_string().contains("string"), "{e}");
    }

    #[test]
    fn workspace_paths_resolve_against_root() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("rustible.toml"),
            "inventory = \"inv.kdl\"\n",
        )
        .unwrap();
        fs::create_dir_all(tmp.path().join("playbooks/cadu")).unwrap();
        fs::write(tmp.path().join("playbooks/cadu/mc.rs"), "").unwrap();
        let ws = Workspace::discover(Some(tmp.path())).unwrap();
        assert_eq!(ws.inventory_path(), ws.root.join("inv.kdl"));
        assert_eq!(ws.cache_dir(), ws.root.join(".rustible"));
        assert_eq!(ws.playbook_name("cadu/mc").unwrap(), "cadu/mc");
        let abs = ws.root.join("playbooks/cadu/mc.rs");
        assert_eq!(ws.playbook_name(abs.to_str().unwrap()).unwrap(), "cadu/mc");
        let e = ws.playbook_name("cadu/nope").unwrap_err().to_string();
        assert!(e.contains("no playbook `cadu/nope`"), "{e}");
        let outside = tmp.path().join("rustible.toml");
        let e = ws
            .playbook_name(outside.to_str().unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("outside"), "{e}");
        assert!(Workspace::discover(Some(&tmp.path().join("playbooks"))).is_err());
    }
}
