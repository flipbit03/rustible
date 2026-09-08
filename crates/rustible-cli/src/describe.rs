//! Vision doc 5.2 steps 2, 3 and 6 on the cargo side: the host-native
//! describe build and its cache, the vars pre-check through the binary's
//! `--check-vars` mode, and the one `dist` build for every target triple.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rustible_sdk::protocol::PROTOCOL_VERSION;
use rustible_sdk::runtime::{HostCheck, HostVars};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::workspace::Workspace;

/// One playbook's `--describe` entry: what the attribute said and the
/// JSON Schema of its vars (`null` when it takes none).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Describe {
    pub name: String,
    pub hosts: String,
    pub escalate: bool,
    pub vars_schema: Value,
}

#[derive(Debug, Deserialize)]
struct DescribeDoc {
    protocol: u32,
    playbooks: Vec<Describe>,
}

/// What `cargo metadata` knows about the workspace package: where
/// artifacts land and what the bin is called.
#[derive(Debug, Clone)]
pub struct Cargo {
    pub manifest: PathBuf,
    pub target_dir: PathBuf,
    pub bin: String,
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    target_directory: PathBuf,
}

#[derive(Deserialize)]
struct Package {
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
}

/// A workspace's playbook binary is the `src/main.rs` one. Taking the first
/// `bin` cargo lists would silently pick up a `src/bin/tool.rs` the user
/// added, and cargo metadata promises no order.
fn pick_bin<'a>(targets: impl Iterator<Item = &'a Target>, manifest: &Path) -> Result<String> {
    let bins: Vec<&Target> = targets
        .filter(|t| t.kind.iter().any(|k| k == "bin"))
        .collect();
    let from_main: Vec<&Target> = bins
        .iter()
        .copied()
        .filter(|t| t.src_path.ends_with("src/main.rs"))
        .collect();
    match (from_main.as_slice(), bins.as_slice()) {
        ([one], _) => Ok(one.name.clone()),
        ([], [only]) => Ok(only.name.clone()),
        ([], []) => bail!(
            "{} has no bin target; a rustible workspace has one, src/main.rs",
            manifest.display()
        ),
        (many, all) => {
            let listed = if many.is_empty() { all } else { many };
            let names: Vec<&str> = listed.iter().map(|t| t.name.as_str()).collect();
            bail!(
                "{} has several bin targets ({}); rustible drives the one built from \
                 src/main.rs, so keep exactly one",
                manifest.display(),
                names.join(", ")
            )
        }
    }
}

impl Cargo {
    pub async fn load(manifest: &Path) -> Result<Cargo> {
        let out = tokio::process::Command::new("cargo")
            .args([
                "metadata",
                "--no-deps",
                "--format-version",
                "1",
                "--manifest-path",
            ])
            .arg(manifest)
            .output()
            .await
            .context("running cargo metadata")?;
        if !out.status.success() {
            bail!(
                "cargo metadata failed for {}:\n{}",
                manifest.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let md: Metadata = serde_json::from_slice(&out.stdout).context("parsing cargo metadata")?;
        let bin = pick_bin(md.packages.iter().flat_map(|p| &p.targets), manifest)?;
        Ok(Cargo {
            manifest: manifest.to_path_buf(),
            target_dir: md.target_directory,
            bin,
        })
    }

    /// The host-native debug binary (`--describe`, `--check-vars`).
    pub fn debug_bin(&self) -> PathBuf {
        self.target_dir.join("debug").join(&self.bin)
    }

    /// The shipped binary for one triple.
    pub fn dist_bin(&self, triple: &str) -> PathBuf {
        self.target_dir.join(triple).join("dist").join(&self.bin)
    }

    /// One `cargo build`. `selected` sets `RUSTIBLE_PLAYBOOK` and
    /// `--features selected` (vision 9); `None` builds every playbook, as
    /// the editor does. `triples` empty means the host target, dev profile;
    /// otherwise `--profile dist` with one `--target` per triple.
    pub async fn build(&self, selected: Option<&str>, triples: &[String]) -> Result<()> {
        let mut cmd = tokio::process::Command::new("cargo");
        cmd.arg("build").arg("--manifest-path").arg(&self.manifest);
        if let Some(name) = selected {
            cmd.args(["--features", "selected"])
                .env("RUSTIBLE_PLAYBOOK", name);
        } else {
            cmd.env_remove("RUSTIBLE_PLAYBOOK");
        }
        if !triples.is_empty() {
            cmd.args(["--profile", "dist"]);
            for t in triples {
                cmd.args(["--target", t]);
            }
        }
        let status = cmd.status().await.context("running cargo build")?;
        if !status.success() {
            bail!("cargo build failed (exit {})", status.code().unwrap_or(-1));
        }
        Ok(())
    }
}

/// The describe cache key: the playbook source and `Cargo.lock` together
/// (vision 5.2 step 2). Lengths go in so `ab`+`c` and `a`+`bc` differ.
pub fn cache_key(source: &[u8], lock: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update((source.len() as u64).to_be_bytes());
    h.update(source);
    h.update((lock.len() as u64).to_be_bytes());
    h.update(lock);
    hex(&h.finalize())
}

/// `<cache_dir>/describe/<name>-<key>.json`.
pub fn cache_path(cache_dir: &Path, name: &str, key: &str) -> PathBuf {
    cache_dir
        .join("describe")
        .join(format!("{name}-{key}.json"))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_key(ws: &Workspace, name: &str) -> Result<String> {
    let src_path = ws.playbooks_dir().join(format!("{name}.rs"));
    let source =
        std::fs::read(&src_path).with_context(|| format!("reading {}", src_path.display()))?;
    let lock = std::fs::read(ws.root.join("Cargo.lock")).unwrap_or_default();
    Ok(cache_key(&source, &lock))
}

/// Run `<bin> --describe` and parse it, refusing a protocol we do not speak.
pub async fn describe_bin(bin: &Path) -> Result<Vec<Describe>> {
    let out = tokio::process::Command::new(bin)
        .arg("--describe")
        .output()
        .await
        .with_context(|| format!("running {} --describe", bin.display()))?;
    if !out.status.success() {
        bail!(
            "{} --describe failed:\n{}",
            bin.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let doc: DescribeDoc =
        serde_json::from_slice(&out.stdout).context("parsing --describe output")?;
    if doc.protocol != PROTOCOL_VERSION {
        bail!(
            "protocol mismatch: this rustible speaks {PROTOCOL_VERSION}, the workspace's binary \
             speaks {}; align the workspace's rustible dependency with the CLI",
            doc.protocol
        );
    }
    Ok(doc.playbooks)
}

/// The metadata of one playbook, from the cache when the source and
/// `Cargo.lock` are unchanged, else from a selected host-native build.
pub async fn describe_playbook(ws: &Workspace, cargo: &Cargo, name: &str) -> Result<Describe> {
    let key = read_key(ws, name)?;
    let cached = cache_path(&ws.cache_dir(), name, &key);
    if let Ok(text) = std::fs::read_to_string(&cached)
        && let Ok(d) = serde_json::from_str::<Describe>(&text)
        && d.name == name
    {
        return Ok(d);
    }
    cargo.build(Some(name), &[]).await?;
    let d = only(describe_bin(&cargo.debug_bin()).await?, name)?;
    // The build may have created or changed Cargo.lock; key on what is
    // there now so the next run hits.
    let key = read_key(ws, name)?;
    let cached = cache_path(&ws.cache_dir(), name, &key);
    if let Some(parent) = cached.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&cached, serde_json::to_string_pretty(&d)?)
        .with_context(|| format!("writing {}", cached.display()))?;
    Ok(d)
}

fn only(mut list: Vec<Describe>, name: &str) -> Result<Describe> {
    match list.len() {
        1 if list[0].name == name => Ok(list.remove(0)),
        _ => bail!(
            "expected the selected build to hold exactly `{name}`, got: {}",
            list.iter()
                .map(|d| d.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The host-native binary that validates vars for `name`: the debug binary
/// already there when it is a selected build of this very playbook with
/// this very schema (five milliseconds to ask), else a fresh build.
pub async fn check_binary(cargo: &Cargo, expected: &Describe) -> Result<PathBuf> {
    let bin = cargo.debug_bin();
    if bin.is_file()
        && let Ok(list) = describe_bin(&bin).await
        && list.len() == 1
        && list[0] == *expected
    {
        return Ok(bin);
    }
    cargo.build(Some(&expected.name), &[]).await?;
    only(describe_bin(&bin).await?, &expected.name)?;
    Ok(bin)
}

/// The pre-check (vision 10.3) through the binary's `--check-vars` mode, so
/// serde on the typed struct is the validator on both sides.
pub async fn check_vars(bin: &Path, name: &str, hosts: &[HostVars]) -> Result<Vec<HostCheck>> {
    let mut child = tokio::process::Command::new(bin)
        .args(["--check-vars", name])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running {} --check-vars", bin.display()))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    stdin.write_all(&serde_json::to_vec(hosts)?).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "{} --check-vars failed:\n{}",
            bin.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parsing --check-vars output")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(name: &str, src: &str) -> Target {
        Target {
            name: name.into(),
            kind: vec!["bin".into()],
            src_path: PathBuf::from(src),
        }
    }

    #[test]
    fn pick_bin_prefers_the_src_main_target() {
        let m = Path::new("/ws/Cargo.toml");
        let tool = target("tool", "/ws/src/bin/tool.rs");
        let main = target("infra", "/ws/src/main.rs");
        // Either order, the src/main.rs one wins.
        assert_eq!(pick_bin([&tool, &main].into_iter(), m).unwrap(), "infra");
        assert_eq!(pick_bin([&main, &tool].into_iter(), m).unwrap(), "infra");
        // A lone bin elsewhere is still the one to drive.
        assert_eq!(pick_bin([&tool].into_iter(), m).unwrap(), "tool");
        // A lib-only workspace, and two src/main.rs bins (two packages).
        let lib = Target {
            name: "helpers".into(),
            kind: vec!["lib".into()],
            src_path: PathBuf::from("/ws/src/lib.rs"),
        };
        assert!(
            pick_bin([&lib].into_iter(), m)
                .unwrap_err()
                .to_string()
                .contains("no bin target")
        );
        let other = target("second", "/ws/other/src/main.rs");
        let err = pick_bin([&main, &other].into_iter(), m)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("several bin targets") && err.contains("second"),
            "{err}"
        );
    }

    #[test]
    fn cache_key_covers_source_and_lock() {
        let a = cache_key(b"fn main() {}", b"lock v1");
        assert_eq!(a, cache_key(b"fn main() {}", b"lock v1"));
        assert_ne!(a, cache_key(b"fn main() { }", b"lock v1"));
        assert_ne!(a, cache_key(b"fn main() {}", b"lock v2"));
        assert_ne!(cache_key(b"ab", b"c"), cache_key(b"a", b"bc"));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn cache_path_shape() {
        let p = cache_path(Path::new("/ws/.rustible"), "cadu/mc", "abc");
        assert_eq!(p, PathBuf::from("/ws/.rustible/describe/cadu/mc-abc.json"));
    }

    #[test]
    fn describe_doc_parses_the_m1_shape() {
        let doc: DescribeDoc = serde_json::from_str(
            r#"{"protocol": 2, "playbooks": [
                {"name": "hello", "hosts": "local", "escalate": false, "vars_schema": null},
                {"name": "cadu/mc", "hosts": "lab", "escalate": true,
                 "vars_schema": {"type": "object", "properties": {"package": {"type": "string"}}, "required": ["package"]}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(doc.protocol, 2);
        assert_eq!(doc.playbooks.len(), 2);
        assert!(doc.playbooks[0].vars_schema.is_null());
        assert!(doc.playbooks[1].escalate);
        assert!(only(doc.playbooks.clone(), "hello").is_err());
        assert_eq!(
            only(vec![doc.playbooks[0].clone()], "hello").unwrap().hosts,
            "local"
        );
    }
}
