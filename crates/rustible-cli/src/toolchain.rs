//! The C toolchain a playbook build needs, and the pre-flight that asks for it
//! by name.
//!
//! Rustible's TLS provider is `ring`, which compiles a little C, so building a
//! playbook binary needs a C compiler on the operator's machine (vision 5.3;
//! the target host still needs nothing). Left alone, a machine without one
//! fails deep inside a build script:
//!
//! ```text
//! error occurred in cc-rs: failed to find tool "aarch64-linux-musl-gcc"
//! ```
//!
//! which names a program nobody should install and never mentions clang. This
//! module exists so that never reaches a user: it finds the compiler, sets the
//! environment for the cargo invocation, and when there is nothing usable says
//! so in terms that name clang and the install command.
//!
//! ## Which compiler serves which target
//!
//! Measured in `docs/plan/reports/C-TOOLCHAIN-SPIKE.md`, not guessed:
//!
//! - **clang serves every musl target.** For targets that are not x86_64,
//!   ring's build script passes `-nostdlibinc` when the compiler is clang-like
//!   and supplies its own fallbacks, so no libc headers are needed at all.
//! - **The host's own `cc` serves the musl target of the host's own
//!   architecture**, and only that one, because the libc headers it falls back
//!   to are then at least the right architecture. This is why an ordinary
//!   x86_64 Linux box with nothing but gcc can still build for itself.
//! - **`x86_64-unknown-linux-musl` under clang needs real libc headers**:
//!   ring's `check.h` includes `<assert.h>` unguarded and clang's `immintrin.h`
//!   pulls `<stdlib.h>`. Rustible carries musl's own headers ([`MUSL_VERSION`],
//!   MIT) and unpacks them into the workspace cache, so the operator installs
//!   nothing beyond clang.
//!
//! An operator who wants a different compiler sets `CC_<triple>` themselves;
//! anything already in the environment is left exactly as it is.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

include!(concat!(env!("OUT_DIR"), "/musl_headers.rs"));

/// The musl release the vendored headers come from. MIT licensed; see
/// `crates/rustible-cli/vendor/musl-headers/`.
pub const MUSL_VERSION: &str = "1.2.5";

/// How to install clang, per platform. One line, ready to paste.
#[cfg(target_os = "macos")]
const INSTALL_CLANG: &str = "xcode-select --install   (the macOS command line tools include clang)";

/// How to install clang, per platform. One line, ready to paste.
#[cfg(not(target_os = "macos"))]
const INSTALL_CLANG: &str = "sudo apt install clang   (or: dnf install clang, \
     pacman -S clang, apk add clang)";

/// What compilers this machine has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compilers {
    /// `clang` on `PATH`, if it is there. Serves every target.
    pub clang: Option<PathBuf>,
    /// The host's own C compiler (`CC`, else `cc`, else `gcc`). Serves the
    /// host's own architecture only.
    pub host_cc: Option<PathBuf>,
}

impl Compilers {
    /// Look on `PATH`.
    pub fn probe() -> Compilers {
        Compilers {
            clang: which("clang"),
            host_cc: std::env::var_os("CC")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| which("cc"))
                .or_else(|| which("gcc")),
        }
    }

    /// Nothing at all to compile C with.
    pub fn is_empty(&self) -> bool {
        self.clang.is_none() && self.host_cc.is_none()
    }

    /// The warning `rustible init` prints, or `None` when the machine is
    /// fully equipped. Creating a workspace compiles nothing, so this is
    /// never a failure; it is said now because the first build is when it
    /// would otherwise be discovered.
    pub fn init_warning(&self) -> Option<String> {
        if self.clang.is_some() {
            return None;
        }
        let head = "warning: no `clang` on PATH. Rustible's TLS provider (ring) compiles C, \
             so building a playbook binary needs a C compiler.";
        Some(match usable_host_cc(self) {
            Some(cc) => format!(
                "{head}\n         {} can serve {}, this machine's own architecture, so a \
                 playbook for hosts like this one will build. Any other architecture needs \
                 clang:\n             {INSTALL_CLANG}",
                cc.display(),
                host_musl_triple(),
            ),
            None => format!(
                "{head}\n         No playbook will build until one is \
                 installed:\n             {INSTALL_CLANG}"
            ),
        })
    }
}

/// The environment a `cargo build` needs, and the check that it can be met.
///
/// `triples` is what is about to be built for; empty means a host-native
/// build, which needs a compiler but no cross configuration. `cache_dir` is
/// the workspace's cache, where the vendored musl headers are unpacked on
/// first use.
///
/// Returns the variables to set. An entry is produced only for a variable the
/// environment does not already define, so an operator's own `CC_<triple>`
/// always wins.
pub fn env_for_build(
    compilers: &Compilers,
    triples: &[String],
    cache_dir: &Path,
) -> Result<BTreeMap<String, OsString>> {
    env_for_build_with(compilers, triples, cache_dir, &|name| {
        std::env::var_os(name).filter(|v| !v.is_empty())
    })
}

/// [`env_for_build`], with the surrounding environment as a parameter so the
/// tests can vary it. Setting a real variable would be a data race against
/// every other test in the binary.
fn env_for_build_with(
    compilers: &Compilers,
    triples: &[String],
    cache_dir: &Path,
    existing: &dyn Fn(&str) -> Option<OsString>,
) -> Result<BTreeMap<String, OsString>> {
    let mut env = BTreeMap::new();
    let mut unservable: Vec<&str> = Vec::new();

    if triples.is_empty() && compilers.is_empty() {
        bail!(no_compiler_at_all());
    }

    for triple in triples {
        if !triple.ends_with("-linux-musl") {
            // Not a target this module knows how to configure. cargo and
            // cc-rs are left to their own devices rather than guessing.
            continue;
        }
        let cc_var = format!("CC_{}", triple.replace('-', "_"));
        if existing(&cc_var).is_some() {
            continue;
        }
        match choose(compilers, triple) {
            Some(Chosen::Clang(path)) => {
                env.insert(cc_var, path.clone().into_os_string());
                // Only x86_64 needs libc headers; every other musl target
                // takes ring's `-nostdlibinc` path under clang.
                if triple.starts_with("x86_64-") {
                    let flags_var = format!("CFLAGS_{}", triple.replace('-', "_"));
                    if existing(&flags_var).is_none() {
                        let sysroot = unpack_x86_64_musl_headers(cache_dir)?;
                        let mut flag = OsString::from("--sysroot=");
                        flag.push(sysroot);
                        env.insert(flags_var, flag);
                    }
                }
            }
            Some(Chosen::HostCc(path)) => {
                env.insert(cc_var, path.clone().into_os_string());
            }
            None => unservable.push(triple),
        }
    }

    if !unservable.is_empty() {
        bail!(needs_clang(&unservable, compilers));
    }
    Ok(env)
}

/// The compiler that will be used for one musl triple.
enum Chosen<'a> {
    Clang(&'a PathBuf),
    HostCc(&'a PathBuf),
}

fn choose<'a>(compilers: &'a Compilers, triple: &str) -> Option<Chosen<'a>> {
    if let Some(clang) = &compilers.clang {
        return Some(Chosen::Clang(clang));
    }
    match &compilers.host_cc {
        Some(cc) if host_cc_serves(triple) => Some(Chosen::HostCc(cc)),
        _ => None,
    }
}

/// Whether the host's own compiler can build for `triple` with no sysroot.
///
/// Only the host's own architecture, and only on Linux: the fallback works
/// because the compiler's default headers are then both the right architecture
/// and the right operating system. A mac's SDK headers are Darwin's and would
/// not serve a musl Linux target, so this is not merely an architecture
/// comparison. Nothing is lost there, since macOS always has clang.
fn host_cc_serves(triple: &str) -> bool {
    cfg!(target_os = "linux") && triple == host_musl_triple()
}

/// The host compiler, but only where it can actually serve the host's own
/// musl target. Both messages below have to agree with [`choose`], or they
/// would offer a compiler the build then refuses.
fn usable_host_cc(compilers: &Compilers) -> Option<&PathBuf> {
    compilers
        .host_cc
        .as_ref()
        .filter(|_| host_cc_serves(&host_musl_triple()))
}

/// `<arch>-unknown-linux-musl` for the architecture this `rustible` is running
/// on. The one musl target the host's own compiler can serve.
pub fn host_musl_triple() -> String {
    format!("{}-unknown-linux-musl", std::env::consts::ARCH)
}

/// The message when a build needs clang and there is none.
fn needs_clang(triples: &[&str], compilers: &Compilers) -> String {
    let list = triples.join(", ");
    let have = match usable_host_cc(compilers) {
        Some(cc) => format!("{} serves {} alone", cc.display(), host_musl_triple()),
        None => "this machine has no other usable compiler".to_string(),
    };
    format!(
        "no `clang` on PATH, and this playbook has to be built for {list}. \
         Rustible's TLS provider (ring) compiles a little C, and building for any \
         architecture but this machine's own needs clang: {have}. One package covers every \
         target, Rustible supplies the musl headers and compiler flags itself, and the \
         target hosts still need nothing.\n\
         Install it and run this again:  {INSTALL_CLANG}"
    )
}

/// The message when even a host-native build has nothing to compile C with.
fn no_compiler_at_all() -> String {
    format!(
        "no C compiler on PATH. Rustible's TLS provider (ring) compiles a little C, so \
         building a playbook binary needs one; `clang` is the one to install because it also \
         covers every cross-compilation target:\n    {INSTALL_CLANG}\n\n\
         Nothing is needed on the target hosts, only here."
    )
}

/// Attach the clang hint to a failed build when the machine has no clang, so
/// a `cc-rs` failure that escaped the pre-flight is never read bare.
///
/// The pre-flight refuses the builds it knows cannot work, so anything that
/// still fails inside `cc-rs` is a compiler that exists but did not do the
/// job. Naming clang is only useful when there is none, so the note is added
/// only then, and it says "if" rather than claiming to know the cause.
pub fn build_failure_hint(compilers: &Compilers) -> Option<String> {
    if compilers.clang.is_some() {
        return None;
    }
    Some(format!(
        "note: if the output above mentions `cc-rs`, a missing `*-linux-musl-gcc`, or a \
         header that was not found, the cause is that this machine has no clang and \
         Rustible's TLS provider (ring) compiles C. Install it with:\n    {INSTALL_CLANG}"
    ))
}

/// Write the vendored musl headers into `<cache_dir>/musl-headers/<version>-x86_64`
/// and return that directory, which is a sysroot: it holds `include/`.
///
/// Written once. A `.complete` stamp is written last and checked first, so an
/// unpack interrupted half way is redone rather than used.
pub fn unpack_x86_64_musl_headers(cache_dir: &Path) -> Result<PathBuf> {
    let root = cache_dir
        .join("musl-headers")
        .join(format!("{MUSL_VERSION}-x86_64"));
    let stamp = root.join(".complete");
    if stamp.is_file() {
        return Ok(root);
    }
    let include = root.join("include");
    for (rel, bytes) in X86_64_MUSL_HEADERS {
        let path = include.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    }
    std::fs::write(&stamp, format!("musl {MUSL_VERSION}\n"))
        .with_context(|| format!("writing {}", stamp.display()))?;
    Ok(root)
}

/// The first executable named `name` on `PATH`.
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests run against an environment that defines nothing, so a
    /// `CC_*` exported in the shell running `cargo test` cannot change them.
    fn env_for_build(
        compilers: &Compilers,
        triples: &[String],
        cache_dir: &Path,
    ) -> Result<BTreeMap<String, OsString>> {
        env_for_build_with(compilers, triples, cache_dir, &|_| None)
    }

    fn compilers(clang: Option<&str>, cc: Option<&str>) -> Compilers {
        Compilers {
            clang: clang.map(PathBuf::from),
            host_cc: cc.map(PathBuf::from),
        }
    }

    fn other_musl_triple() -> String {
        if host_musl_triple().starts_with("x86_64") {
            "aarch64-unknown-linux-musl".into()
        } else {
            "x86_64-unknown-linux-musl".into()
        }
    }

    /// The headers land as a sysroot: `<dir>/include/<header>`, which is what
    /// `--sysroot` looks for. `stdlib.h` and `assert.h` are the two ring
    /// actually needs on x86_64, so they are named rather than counted.
    #[test]
    fn headers_unpack_into_a_sysroot_and_are_written_once() {
        let t = tempfile::tempdir().unwrap();
        let root = unpack_x86_64_musl_headers(t.path()).unwrap();
        assert!(root.join("include/stdlib.h").is_file());
        assert!(root.join("include/assert.h").is_file());
        assert!(root.join("include/bits/alltypes.h").is_file());
        assert!(
            std::fs::read_to_string(root.join("include/stdlib.h"))
                .unwrap()
                .contains("malloc")
        );
        // Second call is a no-op: overwrite one file and check it survives.
        std::fs::write(root.join("include/stdlib.h"), "sentinel").unwrap();
        let again = unpack_x86_64_musl_headers(t.path()).unwrap();
        assert_eq!(again, root);
        assert_eq!(
            std::fs::read_to_string(root.join("include/stdlib.h")).unwrap(),
            "sentinel"
        );
    }

    /// An unpack that died before the stamp is redone, not trusted.
    #[test]
    fn an_incomplete_unpack_is_redone() {
        let t = tempfile::tempdir().unwrap();
        let root = unpack_x86_64_musl_headers(t.path()).unwrap();
        std::fs::remove_file(root.join(".complete")).unwrap();
        std::fs::write(root.join("include/stdlib.h"), "sentinel").unwrap();
        unpack_x86_64_musl_headers(t.path()).unwrap();
        assert!(
            std::fs::read_to_string(root.join("include/stdlib.h"))
                .unwrap()
                .contains("malloc")
        );
    }

    /// With clang, every target is served, and only x86_64 gets a sysroot.
    #[test]
    fn clang_serves_every_target_and_only_x86_64_gets_headers() {
        let t = tempfile::tempdir().unwrap();
        let c = compilers(Some("/usr/bin/clang"), None);
        let env = env_for_build(
            &c,
            &[
                "x86_64-unknown-linux-musl".into(),
                "aarch64-unknown-linux-musl".into(),
            ],
            t.path(),
        )
        .unwrap();
        assert_eq!(
            env["CC_x86_64_unknown_linux_musl"],
            OsString::from("/usr/bin/clang")
        );
        assert_eq!(
            env["CC_aarch64_unknown_linux_musl"],
            OsString::from("/usr/bin/clang")
        );
        let flags = env["CFLAGS_x86_64_unknown_linux_musl"]
            .to_string_lossy()
            .into_owned();
        assert!(flags.starts_with("--sysroot="), "{flags}");
        assert!(
            flags.ends_with(&format!("{MUSL_VERSION}-x86_64")),
            "{flags}"
        );
        assert!(!env.contains_key("CFLAGS_aarch64_unknown_linux_musl"));
    }

    /// Without clang the host's own compiler still serves the host's own
    /// architecture, with no sysroot: its headers are already the right ones.
    /// Refusing this machine would refuse one that works. Linux only, because
    /// that is the only place the fallback is offered.
    #[cfg(target_os = "linux")]
    #[test]
    fn host_cc_serves_the_host_architecture_alone() {
        let t = tempfile::tempdir().unwrap();
        let c = compilers(None, Some("/usr/bin/gcc"));
        let env = env_for_build(&c, &[host_musl_triple()], t.path()).unwrap();
        let var = format!("CC_{}", host_musl_triple().replace('-', "_"));
        assert_eq!(env[&var], OsString::from("/usr/bin/gcc"));
        assert_eq!(env.len(), 1, "no sysroot for the host's own compiler");
    }

    /// The error names clang, the targets and the install command, and does
    /// not name `*-linux-musl-gcc`, which is what cc-rs would have said.
    #[test]
    fn a_cross_target_without_clang_names_clang_and_the_targets() {
        let t = tempfile::tempdir().unwrap();
        let c = compilers(None, Some("/usr/bin/gcc"));
        let other = other_musl_triple();
        let e = env_for_build(&c, std::slice::from_ref(&other), t.path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("clang"), "{e}");
        assert!(e.contains(&other), "{e}");
        assert!(
            e.contains("install clang") || e.contains("xcode-select"),
            "{e}"
        );
        assert!(e.contains("/usr/bin/gcc"), "{e}");
        assert!(!e.contains("linux-musl-gcc"), "{e}");
    }

    /// A machine with nothing fails even a host-native build, and says so
    /// before cc-rs does.
    #[test]
    fn nothing_at_all_fails_the_host_build_too() {
        let t = tempfile::tempdir().unwrap();
        let c = compilers(None, None);
        let e = env_for_build(&c, &[], t.path()).unwrap_err().to_string();
        assert!(e.contains("no C compiler on PATH"), "{e}");
        assert!(e.contains("clang"), "{e}");
        let e = env_for_build(&c, &[host_musl_triple()], t.path())
            .unwrap_err()
            .to_string();
        assert!(e.contains("no other usable compiler"), "{e}");
    }

    /// A host-native build needs no cross configuration, only a compiler.
    #[test]
    fn a_host_build_sets_nothing() {
        let t = tempfile::tempdir().unwrap();
        let c = compilers(None, Some("/usr/bin/cc"));
        assert!(env_for_build(&c, &[], t.path()).unwrap().is_empty());
    }

    /// A triple that is not musl is left entirely alone: cc-rs's own defaults
    /// are right for a glibc or Darwin target, and guessing would break them.
    #[test]
    fn non_musl_triples_are_not_configured() {
        let t = tempfile::tempdir().unwrap();
        let c = compilers(None, None);
        let env = env_for_build(&c, &["x86_64-unknown-linux-gnu".into()], t.path()).unwrap();
        assert!(env.is_empty());
    }

    /// `rustible init` warns only when clang is missing, and says something
    /// different depending on whether anything else is there.
    #[test]
    fn init_warns_only_without_clang() {
        assert_eq!(
            compilers(Some("/usr/bin/clang"), Some("/usr/bin/cc")).init_warning(),
            None
        );
        let w = compilers(None, Some("/usr/bin/gcc"))
            .init_warning()
            .unwrap();
        assert!(w.contains("clang"), "{w}");
        assert!(w.contains(&host_musl_triple()), "{w}");
        assert!(w.contains("will build"), "{w}");
        let w = compilers(None, None).init_warning().unwrap();
        assert!(w.contains("No playbook will build"), "{w}");
    }

    /// The hint is attached to a build failure only where clang would be the
    /// answer; with clang installed a failed build is about something else.
    #[test]
    fn the_build_hint_appears_only_without_clang() {
        assert_eq!(
            build_failure_hint(&compilers(Some("/usr/bin/clang"), None)),
            None
        );
        let h = build_failure_hint(&compilers(None, Some("/usr/bin/cc"))).unwrap();
        assert!(h.contains("cc-rs") && h.contains("clang"), "{h}");
    }

    /// Whatever the operator put in the environment is what runs: neither the
    /// compiler nor the sysroot is overridden.
    #[test]
    fn an_explicit_cc_for_a_triple_is_never_overridden() {
        let t = tempfile::tempdir().unwrap();
        let env = env_for_build_with(
            &compilers(Some("/usr/bin/clang"), None),
            &["x86_64-unknown-linux-musl".into()],
            t.path(),
            &|name| (name == "CC_x86_64_unknown_linux_musl").then(|| OsString::from("/opt/my/cc")),
        )
        .unwrap();
        assert!(env.is_empty());

        // The sysroot alone can be overridden, keeping Rustible's compiler.
        let env = env_for_build_with(
            &compilers(Some("/usr/bin/clang"), None),
            &["x86_64-unknown-linux-musl".into()],
            t.path(),
            &|name| {
                (name == "CFLAGS_x86_64_unknown_linux_musl")
                    .then(|| OsString::from("--sysroot=/opt/musl"))
            },
        )
        .unwrap();
        assert_eq!(
            env.keys().collect::<Vec<_>>(),
            ["CC_x86_64_unknown_linux_musl"]
        );
    }

    /// `which` finds a real executable and ignores a non-executable file of
    /// the same name.
    #[test]
    fn which_wants_an_executable() {
        assert!(which("sh").is_some() || which("cargo").is_some());
        assert_eq!(which("definitely-not-a-program-nobody-has"), None);
    }
}
