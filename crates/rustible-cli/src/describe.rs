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

use crate::toolchain;
use crate::workspace::Workspace;

/// One playbook's `--describe` entry: what the attribute said and the
/// JSON Schema of its vars (`null` when it takes none).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Describe {
    pub name: String,
    pub hosts: String,
    pub escalate: bool,
    /// The attribute's `ssh_user`, which replaces every host's own for this
    /// playbook. `None` (and absent from a cache file written before the
    /// field existed) leaves the login to the inventory.
    #[serde(default)]
    pub ssh_user: Option<String>,
    pub vars_schema: Value,
}

/// One describe cache file: the entry and the protocol of the binary that
/// wrote it. A CLI of another protocol reads the playbook's metadata
/// differently (a field it does not know, such as `ssh_user`, would be
/// dropped and the run would log in as the wrong account), so it builds
/// afresh instead of trusting the file.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    protocol: u32,
    #[serde(flatten)]
    describe: Describe,
}

/// The cache file's text for `d`, under this CLI's protocol.
fn cache_text(d: &Describe) -> Result<String> {
    Ok(serde_json::to_string_pretty(&CacheEntry {
        protocol: PROTOCOL_VERSION,
        describe: d.clone(),
    })?)
}

/// The cached entry for `name`, or `None` when the file is unreadable, is
/// for another playbook, or was written under another protocol (or before
/// the cache recorded one).
fn from_cache(text: &str, name: &str) -> Option<Describe> {
    let entry: CacheEntry = serde_json::from_str(text).ok()?;
    (entry.protocol == PROTOCOL_VERSION && entry.describe.name == name).then_some(entry.describe)
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
    /// The triple this machine is, from `rustc -vV`. Every build names a
    /// `--target`, including the host-native describe build, because that is
    /// the only way cargo-zigbuild puts zig in front of the C compiler and
    /// the linker; a build with no `--target` falls through to the system
    /// `cc`, which Rustible no longer asks anyone to have (M8 step 2).
    pub host: String,
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
    pub async fn load(ws: &Workspace) -> Result<Cargo> {
        let manifest = &ws.manifest();
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
            host: host_triple()?,
        })
    }

    /// The host-native debug binary (`--describe`, `--check-vars`). Under
    /// `target/<host>/debug/` rather than `target/debug/`, because the
    /// describe build passes `--target` like every other (see [`Cargo::host`]).
    pub fn debug_bin(&self) -> PathBuf {
        self.target_dir
            .join(&self.host)
            .join("debug")
            .join(&self.bin)
    }

    /// The shipped binary for one triple.
    pub fn dist_bin(&self, triple: &str) -> PathBuf {
        self.target_dir.join(triple).join("dist").join(&self.bin)
    }

    /// One `cargo build`. `selected` sets `RUSTIBLE_PLAYBOOK` and
    /// `--features selected` (vision 9); `None` builds every playbook, as
    /// the editor does. `triples` empty means the host target, dev profile;
    /// otherwise `--profile dist` with one `--target` per triple.
    ///
    /// Every build in Rustible funnels through here, and every one goes
    /// through zig (M8): `cargo_zigbuild::Build` hands back a `cargo build`
    /// command whose `-C linker=` and `CC_<triple>` point at wrapper scripts
    /// that exec `rustible zig cc …`. zig carries its own libc for every
    /// target Rustible ships to, so there is no compiler to choose, no
    /// header set to vendor and no SDK to obtain.
    pub async fn build(&self, selected: Option<&str>, triples: &[String]) -> Result<()> {
        // Rustible probed the hosts, so it already knows which architectures
        // this run needs. Making the operator work that out and run
        // `rustup target add` themselves is busywork, and the error they get
        // for not doing it is cargo's `can't find crate for \`core\``.
        toolchain::ensure_targets_installed(triples)?;

        let mut b = cargo_zigbuild::Build::new(Some(self.manifest.clone()));
        // The archiver too: cc-rs bundles ring's objects into a static
        // library with `ar`, and without this cargo-zigbuild leaves that to
        // whatever binutils the machine has — none, on a mac without the
        // command line tools. zig ships one.
        b.enable_zig_ar = true;
        if selected.is_some() {
            b.cargo.common.features = vec!["selected".into()];
        }
        if triples.is_empty() {
            // The host build, explicitly targeted: without `--target`,
            // cargo-zigbuild leaves the C compiler and linker alone and the
            // build quietly needs a system `cc` again.
            b.cargo.common.target = vec![self.host.clone()];
        } else {
            b.cargo.common.profile = Some("dist".into());
            b.cargo.common.target = triples.to_vec();
        }
        // Compiler messages come back as JSON on stdout, for `print_messages`
        // to show; cargo's own lines and progress bar stay on stderr.
        let format = if diagnostics_in_color() {
            "json-diagnostic-rendered-ansi"
        } else {
            "json"
        };
        b.cargo.common.message_format = vec![format.into()];
        // The describe build's host target counts: a mac controller is Apple.
        let apple = b.cargo.common.target.iter().any(|t| t.contains("-apple-"));
        // This is where zig is located and the wrappers written; a machine
        // with no zig fails here, by name, before cargo runs.
        let mut std_cmd = b
            .build_command()
            .context("preparing the zig-backed cargo build")?;
        wire_host_linker(&self.host, &mut std_cmd)?;
        let mut cmd = tokio::process::Command::from(std_cmd);
        match selected {
            Some(name) => {
                cmd.env("RUSTIBLE_PLAYBOOK", name);
            }
            None => {
                cmd.env_remove("RUSTIBLE_PLAYBOOK");
            }
        }
        cmd.stdout(std::process::Stdio::piped());
        let mut child = cmd.spawn().context("running cargo build")?;
        let stdout = child.stdout.take().expect("piped stdout");
        print_messages(stdout, apple).await?;
        let status = child.wait().await.context("running cargo build")?;
        if !status.success() {
            bail!("cargo build failed (exit {})", status.code().unwrap_or(-1));
        }
        Ok(())
    }
}

/// Whether the compiler messages `Cargo::build` prints carry colour: as
/// cargo decides for its own lines, `CARGO_TERM_COLOR` first, else whether
/// stderr is a terminal.
fn diagnostics_in_color() -> bool {
    use std::io::IsTerminal;
    match std::env::var("CARGO_TERM_COLOR").as_deref() {
        Ok("always") => true,
        Ok("never") => false,
        _ => std::io::stderr().is_terminal(),
    }
}

/// Show the compiler messages from cargo's `--message-format=json` stdout on
/// our stderr, as each line arrives so they stay in step with cargo's own
/// lines. stdout stays clean for `--json`.
async fn print_messages(stdout: tokio::process::ChildStdout, apple: bool) -> Result<()> {
    use std::io::{IsTerminal, Write};
    use tokio::io::AsyncBufReadExt;

    // cargo draws its progress bar only on a terminal.
    let progress_bar = std::io::stderr().is_terminal();
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await.context("reading cargo's output")? {
        let text = match cargo_line(&line, apple) {
            CargoLine::Show(rendered) => rendered,
            CargoLine::Other => format!("{line}\n"),
            CargoLine::Quiet => continue,
        };
        let mut err = std::io::stderr().lock();
        // Clear cargo's progress bar first, as cargo does before a message;
        // it draws the bar again on its next tick.
        if progress_bar {
            let _ = err.write_all(b"\r\x1b[K");
        }
        let _ = err.write_all(text.as_bytes());
    }
    Ok(())
}

/// One line of cargo's `--message-format=json` stdout, decided.
#[derive(Debug, PartialEq)]
enum CargoLine {
    /// A compiler message, as rustc rendered it for a terminal.
    Show(String),
    /// Cargo bookkeeping (artifacts, build scripts, the end of the build),
    /// or the SDK lookup warning `is_sdk_lookup_warning` drops.
    Quiet,
    /// Not cargo's JSON at all: shown as it came.
    Other,
}

#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    message: Option<CompilerMessage>,
}

#[derive(Deserialize)]
struct CompilerMessage {
    level: String,
    message: String,
    code: Option<Value>,
    rendered: Option<String>,
}

/// What to do with one line of cargo's JSON stdout. `apple` says the build
/// has an Apple target, the only kind rustc looks for an SDK for.
fn cargo_line(line: &str, apple: bool) -> CargoLine {
    let Ok(msg) = serde_json::from_str::<CargoMessage>(line) else {
        return CargoLine::Other;
    };
    match msg.message {
        Some(m) if msg.reason == "compiler-message" => {
            if apple && is_sdk_lookup_warning(&m) {
                CargoLine::Quiet
            } else {
                CargoLine::Show(
                    m.rendered
                        .unwrap_or_else(|| format!("{}: {}\n", m.level, m.message)),
                )
            }
        }
        _ => CargoLine::Quiet,
    }
}

/// rustc's warning that it could not find an Apple SDK (`XcrunError` in
/// `rustc_codegen_ssa`): there is no `xcrun` (any non-mac), or `xcrun`
/// failed (a mac without the command line tools). zig links against its own
/// libSystem and never reads the SDK, so under Rustible it is always noise,
/// and it is not a lint, so nothing else can silence it (issue #155).
/// rustc's lookup of an SDK it did find (`output of xcrun ...`) is not this.
fn is_sdk_lookup_warning(m: &CompilerMessage) -> bool {
    let text = &m.message;
    m.level == "warning"
        && m.code.as_ref().is_none_or(Value::is_null)
        && (text.starts_with("invoking `\"xcrun\" ")
            || text.starts_with("failed running `\"xcrun\" "))
        && text.contains(" \"--show-sdk-path\"` to find ")
        && text.contains(".sdk")
}

/// Point the *host* linker at zig too, so build scripts and proc-macros
/// stop needing a system `cc`.
///
/// Those are host artifacts, and cargo links them with the host's linker
/// regardless of `--target`. cargo-zigbuild only ever wires the target
/// triple, so on a cross build every build script still links with `cc`;
/// and when host == target it goes further and turns cargo's
/// `target-applies-to-host` off through a nightly-channel override — for
/// glibc-versioned host triples like `x86_64-unknown-linux-gnu.2.17`,
/// which Rustible never builds — so even there the host linker is `cc`.
/// Measured on M8 step 2: with no `cc` on `PATH`, `serde_core`'s build
/// script failed with "linker `cc` not found" while ring's C compiled
/// fine.
///
/// The fix is the same for both cases: a zig wrapper for the host triple
/// in `CARGO_TARGET_<HOST>_LINKER`, which stable cargo applies to host
/// artifacts, and the override removed so that it can.
fn wire_host_linker(host_triple: &str, cmd: &mut std::process::Command) -> Result<()> {
    let config = cargo_config2::Config::load().context("loading cargo config for zig")?;
    let host = cargo_zigbuild::zig::prepare_zig_linker(host_triple, &config)
        .with_context(|| format!("preparing zig as the linker for host {host_triple}"))?;
    let env_host = host_triple.replace('-', "_");
    cmd.env(
        format!("CARGO_TARGET_{}_LINKER", env_host.to_uppercase()),
        &host.cc,
    );
    // A build script that compiles C for the host (rare; cc-rs builds
    // for TARGET) gets zig as well.
    cmd.env(format!("CC_{env_host}"), &host.cc);
    cmd.env(format!("CXX_{env_host}"), &host.cxx);
    for var in [
        "__CARGO_TEST_CHANNEL_OVERRIDE_DO_NOT_USE_THIS",
        "CARGO_UNSTABLE_TARGET_APPLIES_TO_HOST",
        "CARGO_TARGET_APPLIES_TO_HOST",
    ] {
        cmd.env_remove(var);
    }
    Ok(())
}

/// The triple `rustc` itself runs on, from `rustc -vV`'s `host:` line.
fn host_triple() -> Result<String> {
    let out = std::process::Command::new("rustc")
        .arg("-vV")
        .output()
        .context("running `rustc -vV` to learn the host triple")?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("host: "))
        .map(|s| s.trim().to_string())
        .context("no `host:` line in `rustc -vV`")
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

/// Where a protocol mismatch sends the reader: the guide's upgrade procedure,
/// at the same URL the generated `README.md` links, so a crates.io user can
/// open it.
pub const UPGRADING_URL: &str = "https://github.com/flipbit03/rustible/blob/main/docs/USING_RUSTIBLE.md#upgrading-rustible-and-a-workspace";

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
             speaks {}; the CLI and every rustible crate in the workspace must be the same \
             release. To move the workspace to this CLI's version, see {UPGRADING_URL}",
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
        && let Some(d) = from_cache(&text, name)
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
    std::fs::write(&cached, cache_text(&d)?)
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
        let p = cache_path(Path::new("/ws/.rustible"), "demo/mc", "abc");
        assert_eq!(p, PathBuf::from("/ws/.rustible/describe/demo/mc-abc.json"));
    }

    #[test]
    fn describe_doc_parses_the_m1_shape() {
        let doc: DescribeDoc = serde_json::from_str(
            r#"{"protocol": 2, "playbooks": [
                {"name": "hello", "hosts": "local", "escalate": false, "vars_schema": null},
                {"name": "demo/mc", "hosts": "lab", "escalate": true,
                 "vars_schema": {"type": "object", "properties": {"package": {"type": "string"}}, "required": ["package"]}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(doc.protocol, 2);
        assert_eq!(doc.playbooks.len(), 2);
        assert!(doc.playbooks[0].vars_schema.is_null());
        assert!(doc.playbooks[1].escalate);
        assert_eq!(doc.playbooks[0].ssh_user, None, "absent before protocol 6");
        assert!(only(doc.playbooks.clone(), "hello").is_err());
        assert_eq!(
            only(vec![doc.playbooks[0].clone()], "hello").unwrap().hosts,
            "local"
        );
    }

    /// The protocol-mismatch errors link a heading of the guide by its
    /// anchor; renaming the heading would leave them pointing at nothing.
    #[test]
    fn describe_doc_parses_the_protocol_6_shape() {
        let doc: DescribeDoc = serde_json::from_str(
            r#"{"protocol": 6, "playbooks": [
                {"name": "games/minecraft", "hosts": "games", "escalate": false,
                 "ssh_user": "minecraft", "vars_schema": null},
                {"name": "hello", "hosts": "local", "escalate": false,
                 "ssh_user": null, "vars_schema": null}
            ]}"#,
        )
        .unwrap();
        assert_eq!(doc.playbooks[0].ssh_user.as_deref(), Some("minecraft"));
        assert_eq!(doc.playbooks[1].ssh_user, None);
        // The cache stores `Describe` itself, so it must survive the trip.
        let text = cache_text(&doc.playbooks[0]).unwrap();
        assert_eq!(
            from_cache(&text, "games/minecraft"),
            Some(doc.playbooks[0].clone())
        );
    }

    /// A cache file from a CLI of another protocol is not trusted: a newer
    /// one's `ssh_user` read by an older CLI would vanish, and the run would
    /// log in as the inventory's account until the binary's `Hello`.
    #[test]
    fn a_cache_entry_from_another_protocol_is_rebuilt() {
        let d = Describe {
            name: "games/minecraft".into(),
            hosts: "games".into(),
            escalate: false,
            ssh_user: Some("minecraft".into()),
            vars_schema: Value::Null,
        };
        let text = cache_text(&d).unwrap();
        let mut entry: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(entry["protocol"], PROTOCOL_VERSION);
        assert_eq!(entry["ssh_user"], "minecraft");

        entry["protocol"] = Value::from(PROTOCOL_VERSION + 1);
        assert_eq!(from_cache(&entry.to_string(), "games/minecraft"), None);
        entry["protocol"] = Value::from(PROTOCOL_VERSION - 1);
        assert_eq!(from_cache(&entry.to_string(), "games/minecraft"), None);
        // Written before the cache recorded a protocol.
        let old = serde_json::to_string(&d).unwrap();
        assert_eq!(from_cache(&old, "games/minecraft"), None);
        // Another playbook's file.
        assert_eq!(from_cache(&text, "hello"), None);
    }

    #[test]
    fn upgrading_url_names_a_heading_of_the_guide() {
        let guide = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/USING_RUSTIBLE.md");
        let text = std::fs::read_to_string(&guide).unwrap();
        let (_, anchor) = UPGRADING_URL.split_once('#').unwrap();
        let heading = text
            .lines()
            .filter_map(|l| l.strip_prefix("### "))
            .find(|h| h.to_lowercase().replace(' ', "-") == anchor);
        assert!(
            heading.is_some(),
            "no `### ` heading in the guide for #{anchor}"
        );
    }

    // ---- cargo's JSON messages ----

    /// A `compiler-message` line as cargo prints it, trimmed to the fields
    /// `cargo_line` reads plus the ones around them.
    fn compiler_message(level: &str, message: &str, code: Value, rendered: &str) -> String {
        serde_json::json!({
            "reason": "compiler-message",
            "package_id": "path+file:///ws#0.1.0",
            "manifest_path": "/ws/Cargo.toml",
            "target": { "kind": ["bin"], "name": "infra" },
            "message": {
                "$message_type": "diagnostic",
                "level": level,
                "message": message,
                "code": code,
                "spans": [],
                "children": [],
                "rendered": rendered,
            },
        })
        .to_string()
    }

    /// rustc 1.99's message when there is no `xcrun` at all (a Linux
    /// controller), byte for byte.
    const NO_XCRUN: &str = "invoking `\"xcrun\" \"--sdk\" \"macosx\" \"--show-sdk-path\"` to find \
                            MacOSX.sdk failed: No such file or directory (os error 2)";
    /// And when `xcrun` is there but fails (a mac without the command line
    /// tools).
    const XCRUN_FAILED: &str =
        "failed running `\"xcrun\" \"--sdk\" \"macosx\" \"--show-sdk-path\"` to find MacOSX.sdk";

    #[test]
    fn the_sdk_lookup_warning_is_dropped_for_an_apple_build() {
        for text in [NO_XCRUN, XCRUN_FAILED] {
            let line =
                compiler_message("warning", text, Value::Null, &format!("warning: {text}\n"));
            assert_eq!(cargo_line(&line, true), CargoLine::Quiet, "{text}");
        }
    }

    #[test]
    fn the_sdk_lookup_warning_shows_when_no_target_is_apple() {
        let line = compiler_message("warning", NO_XCRUN, Value::Null, "warning: xcrun\n");
        assert_eq!(
            cargo_line(&line, false),
            CargoLine::Show("warning: xcrun\n".into())
        );
    }

    #[test]
    fn everything_else_rustc_says_is_shown_as_rendered() {
        let unused = compiler_message(
            "warning",
            "unused variable: `x`",
            serde_json::json!({ "code": "unused_variables", "explanation": null }),
            "\u{1b}[33mwarning\u{1b}[0m: unused variable: `x`\n",
        );
        assert_eq!(
            cargo_line(&unused, true),
            CargoLine::Show("\u{1b}[33mwarning\u{1b}[0m: unused variable: `x`\n".into())
        );
        // An error mentioning xcrun is still an error, and is shown.
        let error = compiler_message("error", NO_XCRUN, Value::Null, "error: xcrun\n");
        assert_eq!(
            cargo_line(&error, true),
            CargoLine::Show("error: xcrun\n".into())
        );
        // rustc passing on what a working `xcrun` printed is not a failed
        // lookup.
        let found = compiler_message(
            "warning",
            "output of `xcrun` while finding MacOSX.sdk",
            Value::Null,
            "warning: output of `xcrun`\n",
        );
        assert_eq!(
            cargo_line(&found, true),
            CargoLine::Show("warning: output of `xcrun`\n".into())
        );
        // A lint whose text happens to match still has a code, and shows.
        let lint = compiler_message(
            "warning",
            NO_XCRUN,
            serde_json::json!({ "code": "some_lint", "explanation": null }),
            "warning: lint\n",
        );
        assert_eq!(
            cargo_line(&lint, true),
            CargoLine::Show("warning: lint\n".into())
        );
    }

    #[test]
    fn a_message_without_rendered_text_is_shown_from_its_parts() {
        let line = serde_json::json!({
            "reason": "compiler-message",
            "message": { "level": "error", "message": "boom", "code": null, "rendered": null },
        })
        .to_string();
        assert_eq!(
            cargo_line(&line, true),
            CargoLine::Show("error: boom\n".into())
        );
    }

    #[test]
    fn cargo_bookkeeping_is_quiet_and_anything_else_is_passed_on() {
        for line in [
            r#"{"reason":"compiler-artifact","package_id":"x","target":{},"filenames":[]}"#,
            r#"{"reason":"build-script-executed","package_id":"x","out_dir":"/o"}"#,
            r#"{"reason":"build-finished","success":false}"#,
        ] {
            assert_eq!(cargo_line(line, true), CargoLine::Quiet, "{line}");
        }
        for line in ["", "hello from somewhere", r#"{"not":"cargo"}"#] {
            assert_eq!(cargo_line(line, true), CargoLine::Other, "{line}");
        }
    }
}
