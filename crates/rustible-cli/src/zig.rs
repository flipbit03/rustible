//! Where zig comes from (M8).
//!
//! zig is the C toolchain: `ring` compiles a little C, and zig compiles it
//! for every target Rustible ships to, carrying its own libc for each. This
//! module answers one question — *which* `zig` this process builds with — in
//! the order that lets an operator's choice win:
//!
//! 1. `RUSTIBLE_ZIG`, a path to a zig binary. Used as-is, never
//!    version-checked, because someone who set it meant it.
//! 2. Whatever cargo-zigbuild finds by itself: `zig` on `PATH`, or the
//!    `ziglang` Python package.
//! 3. The one Rustible fetched earlier into its own cache.
//! 4. Fetch it: the pinned release for this host, over `curl`, verified
//!    against a SHA-256 written in this file, unpacked in pure Rust.
//!
//! The cache is `$XDG_CACHE_HOME/rustible/zig/<version>/`, else
//! `$HOME/.cache/rustible/zig/<version>/` — the same place the remote binary
//! cache already lives on every target. It is a property of the machine, not
//! of the workspace: 380 MB once, not once per checkout. Rustible never edits
//! `PATH`, never writes a shell rc, never installs to a system directory and
//! never elevates; zig is invoked by absolute path and is invisible to
//! everything else on the machine. Deleting the cache is always safe — the
//! next build fetches again.
//!
//! `curl` is the one system tool named here, and the only one the dependency
//! rule names beyond rustup. The CLI links no TLS stack of its own so that
//! `cargo install rustible-cli` needs no C compiler, which would be a strange
//! thing to need in order to fetch the C compiler. Integrity comes from the
//! pinned checksum, not from the transport.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

/// The zig release Rustible builds with. Bumping it is a deliberate change:
/// zig is pre-1.0, cargo-zigbuild tracks it, and the two are a matched pair.
pub const ZIG_VERSION: &str = "0.15.2";

/// One host's tarball, as `https://ziglang.org/download/index.json` lists
/// it. Written down rather than fetched at run time: the index is a live
/// document, and reading it would let upstream change what gets installed.
struct Pin {
    /// zig's own name for the host: `<arch>-<os>`.
    host: &'static str,
    /// SHA-256 of the `.tar.xz`, lowercase hex.
    sha256: &'static str,
    /// Bytes, for the announcement only.
    size: u64,
}

const PINS: &[Pin] = &[
    Pin {
        host: "x86_64-linux",
        sha256: "02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239",
        size: 53_733_924,
    },
    Pin {
        host: "aarch64-linux",
        sha256: "958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f",
        size: 49_471_996,
    },
    Pin {
        host: "x86_64-macos",
        sha256: "375b6909fc1495d16fc2c7db9538f707456bfc3373b14ee83fdd3e22b3d43f7f",
        size: 55_800_460,
    },
    Pin {
        host: "aarch64-macos",
        sha256: "3cc2bab367e185cdfb27501c4b30b1b0653c28d9f73df8dc91488e66ece5fa6b",
        size: 50_635_984,
    },
];

impl Pin {
    fn url(&self) -> String {
        format!(
            "https://ziglang.org/download/{ZIG_VERSION}/zig-{}-{ZIG_VERSION}.tar.xz",
            self.host
        )
    }
}

/// Which zig this process will build with, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// `RUSTIBLE_ZIG` named it.
    Env(PathBuf),
    /// cargo-zigbuild found it on its own — `PATH` or the Python package —
    /// and will find it again the same way, so nothing needs exporting.
    Found(PathBuf),
    /// Already in Rustible's cache from an earlier fetch.
    Cached(PathBuf),
    /// Fetched just now.
    Fetched(PathBuf),
}

impl Located {
    /// The path to export as `CARGO_ZIGBUILD_ZIG_COMMAND`, which is how
    /// cargo-zigbuild is told about a zig it would not find itself. `None`
    /// when it would.
    pub fn export(&self) -> Option<&Path> {
        match self {
            Located::Env(p) | Located::Cached(p) | Located::Fetched(p) => Some(p),
            Located::Found(_) => None,
        }
    }

    /// The zig binary, wherever it came from.
    pub fn path(&self) -> &Path {
        match self {
            Located::Env(p) | Located::Found(p) | Located::Cached(p) | Located::Fetched(p) => p,
        }
    }

    /// One line for `rustible toolchain install`.
    pub fn describe(&self) -> String {
        match self {
            Located::Env(p) => format!("zig from RUSTIBLE_ZIG: {}", p.display()),
            Located::Found(p) => format!(
                "zig found on this machine: {} (an operator's zig wins; rustible fetched nothing)",
                p.display()
            ),
            Located::Cached(p) => {
                format!("zig {ZIG_VERSION} from rustible's cache: {}", p.display())
            }
            Located::Fetched(p) => format!("zig {ZIG_VERSION} fetched to {}", p.display()),
        }
    }
}

/// Find or fetch zig, in the order the module header gives. `announce` gets
/// one line per thing worth telling the operator about (a download, an
/// install), the way `ensure_targets_installed` announces a `rustup target
/// add`; it is never called for a silent hit.
pub fn provision(announce: &dyn Fn(&str)) -> Result<Located> {
    if let Some(p) = std::env::var_os("RUSTIBLE_ZIG").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(p);
        if !is_executable(&p) {
            bail!(
                "RUSTIBLE_ZIG={} is not an executable file; it should be the path of a zig binary",
                p.display()
            );
        }
        return Ok(Located::Env(p));
    }
    if let Ok((path, _args)) = cargo_zigbuild::Zig::find_zig() {
        return Ok(Located::Found(path));
    }
    let root = cache_root()?.join("zig").join(ZIG_VERSION);
    if root.join(STAMP).is_file()
        && let Some(bin) = binary_in(&root)
    {
        return Ok(Located::Cached(bin));
    }
    let bin = fetch_with(&root, announce, which("curl"), &host_key()?)?;
    Ok(Located::Fetched(bin))
}

/// `rustible toolchain install`: provision deliberately, ahead of the first
/// build, and say what happened.
pub fn install() -> Result<u8> {
    let located = provision(&|line| eprintln!("  {line}"))?;
    println!("{}", located.describe());
    Ok(0)
}

const STAMP: &str = ".complete";

/// zig's name for this machine, which is also the pin key.
fn host_key() -> Result<String> {
    let key = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "x86_64-linux",
        ("linux", "aarch64") => "aarch64-linux",
        ("macos", "x86_64") => "x86_64-macos",
        ("macos", "aarch64") => "aarch64-macos",
        (os, arch) => bail!(
            "rustible has no pinned zig for this machine ({os} {arch}); install zig yourself and \
             set RUSTIBLE_ZIG to it"
        ),
    };
    Ok(key.to_string())
}

/// `$XDG_CACHE_HOME/rustible`, else `$HOME/.cache/rustible`.
pub fn cache_root() -> Result<PathBuf> {
    cache_root_from(std::env::var_os("XDG_CACHE_HOME"), std::env::var_os("HOME"))
}

fn cache_root_from(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("rustible"));
    }
    let home = home
        .filter(|v| !v.is_empty())
        .context("neither XDG_CACHE_HOME nor HOME is set, so rustible has nowhere to keep zig")?;
    Ok(PathBuf::from(home).join(".cache").join("rustible"))
}

/// Download, verify, unpack, and move into place. `curl` and `host` are
/// parameters so the refusal paths are testable without a network.
fn fetch_with(
    root: &Path,
    announce: &dyn Fn(&str),
    curl: Option<PathBuf>,
    host: &str,
) -> Result<PathBuf> {
    let pin = PINS
        .iter()
        .find(|p| p.host == host)
        .with_context(|| format!("no pinned zig {ZIG_VERSION} for host {host}"))?;
    let Some(curl) = curl else {
        bail!(
            "curl is missing and is needed to download zig {ZIG_VERSION}. Install curl, or set \
             RUSTIBLE_ZIG to a zig already on this machine"
        );
    };
    let url = pin.url();

    // Unpack beside the target and rename into place, the same way the
    // vendored musl headers used to be: the rename is atomic, a concurrent
    // run's finished work is never destroyed, and the scratch carries this
    // pid so two fetchers never share one.
    let scratch = root.with_extension(format!("tmp.{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).with_context(|| format!("creating {}", scratch.display()))?;
    let tarball = scratch.join("zig.tar.xz");

    announce(&format!(
        "downloading zig {ZIG_VERSION} for {host} ({} MB) from {url}",
        pin.size / 1_000_000
    ));
    let status = Command::new(&curl)
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
        ])
        .arg(&tarball)
        .arg(&url)
        .status()
        .with_context(|| format!("running {}", curl.display()))?;
    if !status.success() {
        bail!(
            "curl exited {} downloading {url}",
            status.code().unwrap_or(-1)
        );
    }

    let bytes =
        std::fs::read(&tarball).with_context(|| format!("reading {}", tarball.display()))?;
    verify_sha256(&bytes, pin.sha256).with_context(|| format!("verifying {url}"))?;
    unpack_tar_xz(&bytes[..], &scratch).with_context(|| format!("unpacking {url}"))?;
    drop(bytes);
    std::fs::remove_file(&tarball).ok();
    binary_in(&scratch).context("the zig tarball unpacked without a `zig` binary in it")?;
    std::fs::write(scratch.join(STAMP), format!("zig {ZIG_VERSION}\n"))
        .with_context(|| format!("writing the stamp in {}", scratch.display()))?;

    if let Some(parent) = root.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // A fetch that died half way leaves a directory with no stamp, and
    // `rename` will not replace a directory. Clear that wreckage; a complete
    // one is left alone.
    if root.exists() && !root.join(STAMP).is_file() {
        let _ = std::fs::remove_dir_all(root);
    }
    match std::fs::rename(&scratch, root) {
        Ok(()) => {}
        Err(_) if root.join(STAMP).is_file() => {
            // Another run finished first; ours is surplus.
            let _ = std::fs::remove_dir_all(&scratch);
        }
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "moving {} into place at {}",
                    scratch.display(),
                    root.display()
                )
            });
        }
    }
    let bin = binary_in(root).context("zig went missing between unpacking and installing")?;
    announce(&format!("zig {ZIG_VERSION} installed at {}", bin.display()));
    Ok(bin)
}

/// The `zig` binary inside an unpacked release: the tarball carries one
/// top-level directory (`zig-x86_64-linux-0.15.2/`) with `zig` in it.
fn binary_in(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join("zig");
        if entry.path().is_dir() && is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Pure: the bytes hash to `expected`, or an error naming both digests.
fn verify_sha256(bytes: &[u8], expected: &str) -> Result<()> {
    let actual = format!("{:x}", Sha256::digest(bytes));
    if actual != expected {
        bail!(
            "SHA-256 mismatch: expected {expected}, got {actual}. The download is not the \
             release rustible pinned; nothing was installed"
        );
    }
    Ok(())
}

/// Unpack a `.tar.xz` into `dst`, keeping modes: without
/// `set_preserve_permissions` the `tar` crate strips them, and a zig that
/// is not executable is not a zig.
fn unpack_tar_xz(xz: impl Read, dst: &Path) -> Result<()> {
    let reader = lzma_rust2::XzReader::new(xz, true);
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive
        .unpack(dst)
        .with_context(|| format!("unpacking into {}", dst.display()))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The first executable `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    // ---- pure ----

    #[test]
    fn pins_are_well_formed() {
        assert_eq!(PINS.len(), 4);
        for pin in PINS {
            assert_eq!(pin.sha256.len(), 64, "{}", pin.host);
            assert!(
                pin.sha256
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
                "{}",
                pin.host
            );
            assert!(pin.size > 40_000_000, "{}", pin.host);
            let url = pin.url();
            assert!(url.starts_with("https://ziglang.org/download/"), "{url}");
            assert!(
                url.ends_with(&format!("zig-{}-{ZIG_VERSION}.tar.xz", pin.host)),
                "{url}"
            );
        }
    }

    /// The machine the tests run on is one Rustible supports as a controller,
    /// so it must have a pin.
    #[test]
    fn this_host_has_a_pin() {
        let host = host_key().unwrap();
        assert!(PINS.iter().any(|p| p.host == host), "{host}");
    }

    #[test]
    fn cache_root_prefers_xdg_then_home() {
        let x = cache_root_from(Some("/x".into()), Some("/h".into())).unwrap();
        assert_eq!(x, PathBuf::from("/x/rustible"));
        let h = cache_root_from(None, Some("/h".into())).unwrap();
        assert_eq!(h, PathBuf::from("/h/.cache/rustible"));
        let e = cache_root_from(Some("".into()), Some("/h".into())).unwrap();
        assert_eq!(e, PathBuf::from("/h/.cache/rustible"), "empty XDG is unset");
        let err = cache_root_from(None, None).unwrap_err().to_string();
        assert!(err.contains("nowhere to keep zig"), "{err}");
    }

    #[test]
    fn a_wrong_digest_is_refused_naming_both() {
        let ok = format!("{:x}", Sha256::digest(b"hello"));
        verify_sha256(b"hello", &ok).unwrap();
        let err = verify_sha256(b"hello", &"0".repeat(64))
            .unwrap_err()
            .to_string();
        assert!(err.contains("mismatch"), "{err}");
        assert!(err.contains(&ok), "{err}");
        assert!(err.contains("nothing was installed"), "{err}");
    }

    // ---- filesystem ----

    /// A release-shaped tarball, small: one top-level directory holding an
    /// executable `zig`.
    fn fake_release() -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        let body = b"#!/bin/sh\necho fake zig\n";
        let mut header = tar::Header::new_gnu();
        header.set_size(body.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        tar.append_data(&mut header, "zig-x86_64-linux-0.15.2/zig", &body[..])
            .unwrap();
        let tar = tar.into_inner().unwrap();
        let mut xz =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        xz.write_all(&tar).unwrap();
        xz.finish().unwrap()
    }

    /// The bit this module exists to get right: the unpacked `zig` keeps its
    /// mode, and `binary_in` finds it under the release's own directory.
    #[test]
    fn unpack_keeps_zig_executable_and_finds_it() {
        let dir = tempfile::tempdir().unwrap();
        unpack_tar_xz(&fake_release()[..], dir.path()).unwrap();
        let bin = binary_in(dir.path()).expect("zig found");
        assert!(
            bin.ends_with("zig-x86_64-linux-0.15.2/zig"),
            "{}",
            bin.display()
        );
        assert!(is_executable(&bin));
        // Nothing but the release directory was written.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn a_release_without_a_binary_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("zig-x86_64-linux-0.15.2")).unwrap();
        assert_eq!(binary_in(dir.path()), None);
    }

    /// No curl means no download, and the refusal names both curl and the
    /// way round it. The network is never touched: the check comes first.
    #[test]
    fn fetch_refuses_without_curl_before_touching_anything() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("zig").join(ZIG_VERSION);
        let err = fetch_with(&root, &|_| {}, None, "x86_64-linux")
            .unwrap_err()
            .to_string();
        assert!(err.contains("curl is missing"), "{err}");
        assert!(err.contains("RUSTIBLE_ZIG"), "{err}");
        assert!(!root.exists());
    }

    #[test]
    fn fetch_refuses_an_unpinned_host_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let err = fetch_with(
            dir.path(),
            &|_| {},
            Some("/usr/bin/curl".into()),
            "riscv64-linux",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("riscv64-linux"), "{err}");
    }

    /// `export` is the contract with cargo-zigbuild: everything Rustible
    /// chose is exported, and a zig it found on its own is left alone so
    /// PATH keeps winning.
    #[test]
    fn only_rustibles_own_choices_are_exported() {
        let p = PathBuf::from("/z");
        assert_eq!(Located::Env(p.clone()).export(), Some(p.as_path()));
        assert_eq!(Located::Cached(p.clone()).export(), Some(p.as_path()));
        assert_eq!(Located::Fetched(p.clone()).export(), Some(p.as_path()));
        assert_eq!(Located::Found(p).export(), None);
    }
}
