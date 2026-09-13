//! The fixed core facts. Gathered eagerly at startup from a handful of reads.
//! Anything beyond this is an op.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::backend::{Backend, CmdSpec};

/// The kernel family, taken from `std::env::consts::OS`.
///
/// Compiled in rather than probed, so it names the target the playbook
/// binary was built for. On a real run that is the machine the binary was
/// cross-built for and shipped to, which is the same answer; on a `Fake`
/// system it is the answer for the machine running the tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    /// The only kernel Rustible's own ops target in full.
    Linux,
    /// Darwin. A named variant rather than an [`Os::Other`] string because it
    /// is the one non-Linux system Rustible can build for and ship to, so
    /// operations branch on it or refuse it by name rather than comparing
    /// against a string (`docs/plan/reports/MACOS-TARGET-SPIKE.md`).
    Macos,
    /// Any other `std::env::consts::OS` value, carried verbatim
    /// (`freebsd`, `illumos`).
    Other(String),
}

impl Os {
    /// The kernel's own name for itself, as `std::env::consts::OS` spells it:
    /// `linux`, `macos`, or whatever [`Os::Other`] carries. What a refusal
    /// puts in its message, where `{:?}` would print `Other("freebsd")`.
    pub fn name(&self) -> &str {
        match self {
            Os::Linux => "linux",
            Os::Macos => "macos",
            Os::Other(s) => s,
        }
    }
}

/// Which distribution `/etc/os-release` says this is.
///
/// Matched on that file's `ID` field first, then on `ID_LIKE`, so a
/// derivative lands on the family whose packages and paths it shares. A host
/// with no readable `/etc/os-release` gets `Other("")`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Distro {
    /// `ID=debian`, or any `ID_LIKE` containing `debian` that did not match
    /// a name of its own (Devuan, Raspberry Pi OS).
    Debian,
    /// `ID=ubuntu`. Checked before the `ID_LIKE=debian` fallback, so Ubuntu
    /// never collapses into [`Distro::Debian`].
    Ubuntu,
    /// `ID=alpine`.
    Alpine,
    /// `ID=fedora`. A Fedora derivative that only sets `ID_LIKE=fedora`
    /// lands on [`Distro::Rhel`] instead.
    Fedora,
    /// `ID` of `rhel`, `centos`, `rocky` or `almalinux`, or an `ID_LIKE`
    /// containing `rhel` or `fedora`. One variant for the whole family
    /// because they share `dnf` and the same layout.
    Rhel,
    /// `ID=arch`.
    Arch,
    /// macOS, which has no distribution in the Linux sense: there is one
    /// vendor and one flavour. Set from [`Os::Macos`] rather than from a
    /// file, with [`Facts::distro_version`] carrying `ProductVersion` from
    /// `/System/Library/CoreServices/SystemVersion.plist`.
    Macos,
    /// The `ID` field verbatim when nothing above matched, and the empty
    /// string when `/etc/os-release` is missing or has no `ID`.
    Other(String),
}

/// The CPU architecture, from `std::env::consts::ARCH`. Like [`Os`] this is
/// the binary's own target rather than a probe of the machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arch {
    /// 64-bit x86, including hosts that can still run 32-bit code.
    X86_64,
    /// 64-bit ARM.
    Aarch64,
    /// The `std::env::consts::ARCH` string verbatim (`arm`, `riscv64`).
    Other(String),
}

/// One package manager found on the host.
///
/// Decided by looking for the binary itself, not by inferring from
/// [`Distro`], so a distro that has swapped its manager out is still
/// described correctly. [`Facts::package_managers`] holds **every** one
/// found rather than a single winner, because they genuinely coexist —
/// Homebrew runs on Linux beside apt, macports beside brew on a mac — and a
/// first-match-wins answer would tell `brew::Present` that a host with brew
/// on it has none.
///
/// There is no `Other` variant: "no package manager Rustible knows" is the
/// empty set, which cannot be mistaken for a manager the way `Other("")`
/// could.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Pm {
    /// `/usr/bin/apt-get` exists.
    Apt,
    /// `/usr/bin/dnf` exists.
    Dnf,
    /// `/sbin/apk` exists.
    Apk,
    /// `/usr/bin/pacman` exists.
    Pacman,
    /// `/usr/bin/zypper` exists.
    Zypper,
    /// Homebrew: `/opt/homebrew/bin/brew` (Apple silicon),
    /// `/usr/local/bin/brew` (Intel macs) or
    /// `/home/linuxbrew/.linuxbrew/bin/brew` (Linux). Not implied by
    /// [`Os::Macos`]: a mac without Homebrew installed does not have it, and
    /// a Linux box with it does.
    Brew,
}

/// What is running as pid 1, and so which service manager a unit op has to
/// talk to. Read from `/proc/1/comm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Init {
    /// `/proc/1/comm` reads `systemd`.
    Systemd,
    /// macOS: `/sbin/launchd` exists, which is pid 1 there. Probed by path
    /// because macOS has no `/proc`.
    Launchd,
    /// `/proc/1/comm` reads `init` and `/sbin/openrc` exists, which is how
    /// OpenRC hosts (Alpine, Gentoo) present themselves.
    OpenRc,
    /// Whatever `/proc/1/comm` said, trimmed: a container's own supervisor,
    /// a plain `init` with no OpenRC behind it, or the empty string when
    /// `/proc` is not mounted.
    Other(String),
}

/// Everything Rustible knows about a host without being asked.
///
/// Gathered once by [`Facts::gather`] before the first step and handed to
/// every op through [`System::facts`](crate::system::System::facts). Nothing
/// re-gathers them, so a value here describes the machine as it was at the
/// start of the run, not after an earlier step changed it. This set is
/// deliberately small (vision doc: the fixed core facts); anything else a
/// playbook wants to know about a host is an op that goes and looks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    /// The kernel family; see [`Os`] for why this is compiled in rather
    /// than probed.
    pub os: Os,
    /// The distribution family, from `/etc/os-release`. What an op branches
    /// on to pick paths and conventions.
    pub distro: Distro,
    /// The `VERSION_ID` field of `/etc/os-release` verbatim (`12`,
    /// `24.04`), with the quotes stripped, or on macOS `ProductVersion` from
    /// `/System/Library/CoreServices/SystemVersion.plist` (`26.3`). Not a
    /// number: rolling releases print dates or nothing at all, and a host
    /// with no `VERSION_ID` (Arch) or no `/etc/os-release` gets the empty
    /// string.
    pub distro_version: String,
    /// The CPU architecture; see [`Arch`], which like [`Os`] describes the
    /// binary's target.
    pub arch: Arch,
    /// `/proc/sys/kernel/osrelease`, trimmed: the running kernel's release
    /// string, `6.1.0-13-amd64`. Empty when `/proc` cannot be read.
    pub kernel: String,
    /// `/proc/sys/kernel/hostname`, falling back to `/etc/hostname`. The
    /// name the kernel holds, not a resolved FQDN and not the inventory's
    /// name for the host; empty when neither source reads.
    pub hostname: String,
    /// Every package manager found on the box; see [`Pm`]. Empty when there
    /// is none Rustible knows. Ask with [`Facts::has_pm`] rather than
    /// comparing, so a host that gains a second manager does not change the
    /// answer for the first.
    pub package_managers: BTreeSet<Pm>,
    /// What is running as pid 1; see [`Init`].
    pub init: Init,
    /// Logical CPUs, counted as the `processor` lines in `/proc/cpuinfo`,
    /// so hyperthreads count separately and cgroup limits are invisible.
    /// Never zero: an unreadable `/proc/cpuinfo` reports 1.
    pub cpus: u32,
    /// `MemTotal` from `/proc/meminfo`, converted from kibibytes by integer
    /// division. Memory the kernel can hand out, a little under the RAM
    /// physically installed; 0 when `/proc/meminfo` does not read.
    pub memory_mb: u64,
    /// The account this process runs as: `$USER` when set, otherwise the
    /// name `/etc/passwd` gives for `getuid()`, otherwise the numeric uid
    /// written out. [`System::as_user`](crate::system::System::as_user)
    /// compares against this to decide whether a helper process is needed
    /// at all.
    pub user: String,
    /// True when `getuid()` is 0. Ops that need privilege should ask
    /// [`System::is_root`](crate::system::System::is_root) instead, which
    /// also accounts for a handle that switched identity.
    pub is_root: bool,
}

/// Where macOS keeps its product version. A plain file, so it fits
/// [`Facts::gather`]'s read budget.
const SYSTEM_VERSION_PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";

/// Every path Homebrew installs its binary at: Apple silicon, Intel macs, and
/// Linux. Probed in that order, though only whether *any* matches is used.
const BREW_PATHS: [&str; 3] = [
    "/opt/homebrew/bin/brew",
    "/usr/local/bin/brew",
    "/home/linuxbrew/.linuxbrew/bin/brew",
];

/// The one `sysctl` invocation macOS needs, and the order its answers come
/// back in. `sysctl -n` takes several keys and prints one value per line.
const DARWIN_SYSCTL_KEYS: [&str; 4] = ["hw.ncpu", "hw.memsize", "kern.osrelease", "kern.hostname"];

/// The four facts macOS has no file for. Gathered together because one
/// `sysctl` call answers all four, so the cost is one process and not four.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DarwinSysctl {
    kernel: String,
    hostname: String,
    cpus: u32,
    memory_mb: u64,
}

/// Pull the value of `key` out of a plist's XML. Deliberately not a plist
/// parser: the file is `<key>K</key><string>V</string>` pairs, and pulling
/// one string out of it does not justify a dependency (vision 5.3). Returns
/// `None` when the key is absent or is not followed by a `<string>`.
fn plist_string(xml: &str, key: &str) -> Option<String> {
    let after = xml.split_once(&format!("<key>{key}</key>"))?.1;
    let open = after.find("<string>")? + "<string>".len();
    let close = after[open..].find("</string>")? + open;
    Some(after[open..close].trim().to_string())
}

/// Parse `sysctl -n`'s output for [`DARWIN_SYSCTL_KEYS`]: one value per line,
/// in the order asked. A short or unparseable answer falls back to the same
/// values a Linux host with no `/proc` would report, because `gather` cannot
/// fail.
fn parse_darwin_sysctl(stdout: &str) -> DarwinSysctl {
    let mut lines = stdout.lines().map(str::trim);
    let cpus = lines.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    let memory_mb = lines
        .next()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|bytes| bytes / (1024 * 1024))
        .unwrap_or(0);
    let kernel = lines.next().unwrap_or_default().to_string();
    let hostname = lines.next().unwrap_or_default().to_string();
    DarwinSysctl {
        kernel,
        hostname,
        cpus,
        memory_mb,
    }
}

/// Ask a macOS host for the four facts no file carries. `None` when `sysctl`
/// could not be run or exited non-zero, which leaves every field on its
/// Linux-shaped default rather than inventing one.
fn darwin_sysctl(b: &dyn Backend) -> Option<DarwinSysctl> {
    let out = b
        .spawn(&CmdSpec {
            program: "/usr/sbin/sysctl".into(),
            args: std::iter::once("-n".to_string())
                .chain(DARWIN_SYSCTL_KEYS.iter().map(|k| k.to_string()))
                .collect(),
            env: Default::default(),
            cwd: None,
            stdin: None,
            prefix: Vec::new(),
        })
        .ok()?;
    out.success()
        .then(|| parse_darwin_sysctl(&out.stdout_str()))
}

/// Which [`Distro`] an `/etc/os-release` text describes. Pure, so the whole
/// table is testable from strings.
fn distro_of(os_release: &str) -> Distro {
    let id = os_release_field(os_release, "ID");
    let id_like = os_release_field(os_release, "ID_LIKE");
    match id.as_str() {
        "debian" => Distro::Debian,
        "ubuntu" => Distro::Ubuntu,
        "alpine" => Distro::Alpine,
        "fedora" => Distro::Fedora,
        "rhel" | "centos" | "rocky" | "almalinux" => Distro::Rhel,
        "arch" => Distro::Arch,
        _ if id_like.contains("debian") => Distro::Debian,
        _ if id_like.contains("rhel") || id_like.contains("fedora") => Distro::Rhel,
        _ => Distro::Other(id),
    }
}

/// One `KEY=value` field of `/etc/os-release`, quotes stripped, empty when
/// absent.
fn os_release_field(os_release: &str, key: &str) -> String {
    os_release
        .lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .map(|v| v.trim().trim_matches('"').to_string())
        .unwrap_or_default()
}

impl Facts {
    /// Whether `pm` is on this host. The question every package op asks:
    /// `apt::Present` wants to know that apt is here, not which manager won
    /// a ranking it never asked for.
    pub fn has_pm(&self, pm: &Pm) -> bool {
        self.package_managers.contains(pm)
    }

    /// Gather from a backend. Real on `Local`; on `Fake` it reads whatever
    /// files the test planted, so distro branches are testable.
    pub fn gather(b: &dyn Backend) -> Facts {
        let read = |p: &str| {
            b.read(Path::new(p))
                .ok()
                .map(|v| String::from_utf8_lossy(&v).into_owned())
        };

        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            other => Arch::Other(other.to_string()),
        };
        let os = match std::env::consts::OS {
            "linux" => Os::Linux,
            "macos" => Os::Macos,
            other => Os::Other(other.to_string()),
        };

        // `/etc/os-release` is the Linux answer and macOS has no equivalent,
        // so the two are separate sources rather than one with a fallback.
        let (distro, distro_version) = if matches!(os, Os::Macos) {
            (
                Distro::Macos,
                read(SYSTEM_VERSION_PLIST)
                    .and_then(|t| plist_string(&t, "ProductVersion"))
                    .unwrap_or_default(),
            )
        } else {
            let os_release = read("/etc/os-release").unwrap_or_default();
            (
                distro_of(&os_release),
                os_release_field(&os_release, "VERSION_ID"),
            )
        };

        // The four fields with no file behind them on macOS. One `sysctl`
        // spawn answers all of them; Linux keeps its reads and pays nothing.
        let darwin = matches!(os, Os::Macos).then(|| darwin_sysctl(b)).flatten();

        let kernel = match &darwin {
            Some(d) => d.kernel.clone(),
            None => read("/proc/sys/kernel/osrelease")
                .unwrap_or_default()
                .trim()
                .to_string(),
        };
        let hostname = match &darwin {
            Some(d) => d.hostname.clone(),
            None => read("/proc/sys/kernel/hostname")
                .or_else(|| read("/etc/hostname"))
                .unwrap_or_default()
                .trim()
                .to_string(),
        };

        let has = |p: &str| b.stat(Path::new(p)).ok().flatten().is_some();
        // Every one found, not the first: see `Pm`.
        let mut package_managers = BTreeSet::new();
        for (path, pm) in [
            ("/usr/bin/apt-get", Pm::Apt),
            ("/usr/bin/dnf", Pm::Dnf),
            ("/sbin/apk", Pm::Apk),
            ("/usr/bin/pacman", Pm::Pacman),
            ("/usr/bin/zypper", Pm::Zypper),
        ] {
            if has(path) {
                package_managers.insert(pm);
            }
        }
        if BREW_PATHS.iter().any(|p| has(p)) {
            package_managers.insert(Pm::Brew);
        }

        // launchd is pid 1 on macOS, but there is no `/proc/1/comm` to say
        // so, so it is a path probe guarded by the OS rather than a read.
        let init = if matches!(os, Os::Macos) && has("/sbin/launchd") {
            Init::Launchd
        } else {
            match read("/proc/1/comm").unwrap_or_default().trim() {
                "systemd" => Init::Systemd,
                "init" if has("/sbin/openrc") => Init::OpenRc,
                other => Init::Other(other.to_string()),
            }
        };

        let cpus = match &darwin {
            Some(d) => d.cpus,
            None => read("/proc/cpuinfo")
                .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as u32)
                .filter(|&n| n > 0)
                .unwrap_or(1),
        };

        let memory_mb = match &darwin {
            Some(d) => d.memory_mb,
            None => read("/proc/meminfo")
                .and_then(|s| {
                    s.lines()
                        .find(|l| l.starts_with("MemTotal:"))
                        .and_then(|l| l.split_whitespace().nth(1))
                        .and_then(|kb| kb.parse::<u64>().ok())
                })
                .map(|kb| kb / 1024)
                .unwrap_or(0),
        };

        let uid = rustix::process::getuid().as_raw();
        let user = std::env::var("USER").ok().unwrap_or_else(|| {
            read("/etc/passwd")
                .and_then(|p| {
                    p.lines().find_map(|l| {
                        let mut it = l.split(':');
                        let name = it.next()?;
                        let _pw = it.next()?;
                        let id: u32 = it.next()?.parse().ok()?;
                        (id == uid).then(|| name.to_string())
                    })
                })
                .unwrap_or_else(|| uid.to_string())
        });

        Facts {
            os,
            distro,
            distro_version,
            arch,
            kernel,
            hostname,
            package_managers,
            init,
            cpus,
            memory_mb,
            user,
            is_root: uid == 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure ----

    /// The real file from a macOS 26.3 machine, trimmed to the keys that
    /// matter. `ProductVersion` and `ProductUserVisibleVersion` both exist
    /// and the first must win, which is why this is not a "find any string"
    /// search.
    const PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>ProductBuildVersion</key>
	<string>25D125</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductUserVisibleVersion</key>
	<string>26.3</string>
	<key>ProductVersion</key>
	<string>26.3</string>
</dict>
</plist>"#;

    #[test]
    fn plist_finds_the_key_it_was_asked_for() {
        assert_eq!(plist_string(PLIST, "ProductVersion").unwrap(), "26.3");
        assert_eq!(plist_string(PLIST, "ProductName").unwrap(), "macOS");
        assert_eq!(
            plist_string(PLIST, "ProductBuildVersion").unwrap(),
            "25D125"
        );
        assert_eq!(plist_string(PLIST, "NotThere"), None);
    }

    #[test]
    fn plist_of_rubbish_is_none_not_a_panic() {
        assert_eq!(plist_string("", "ProductVersion"), None);
        assert_eq!(
            plist_string("<key>ProductVersion</key>", "ProductVersion"),
            None
        );
    }

    /// Verbatim from `sysctl -n hw.ncpu hw.memsize kern.osrelease
    /// kern.hostname` on a real mac.
    #[test]
    fn darwin_sysctl_output_maps_to_the_four_fields() {
        let d = parse_darwin_sysctl("12\n51539607552\n25.3.0\nCADUMAC\n");
        assert_eq!(d.cpus, 12);
        assert_eq!(d.memory_mb, 49152);
        assert_eq!(d.kernel, "25.3.0");
        assert_eq!(d.hostname, "CADUMAC");
    }

    /// A short or unparseable answer falls back rather than inventing, and
    /// never panics: `gather` returns `Facts`, not `Result`.
    #[test]
    fn a_truncated_sysctl_answer_falls_back_field_by_field() {
        let d = parse_darwin_sysctl("12\n");
        assert_eq!(d.cpus, 12);
        assert_eq!(d.memory_mb, 0);
        assert_eq!(d.kernel, "");
        assert_eq!(d.hostname, "");
        assert_eq!(parse_darwin_sysctl("").cpus, 1);
        assert_eq!(parse_darwin_sysctl("not-a-number\n").cpus, 1);
    }

    #[test]
    fn os_names_itself_as_the_kernel_spells_it() {
        assert_eq!(Os::Linux.name(), "linux");
        assert_eq!(Os::Macos.name(), "macos");
        assert_eq!(Os::Other("freebsd".into()).name(), "freebsd");
    }

    #[test]
    fn distro_table_is_unchanged_by_the_macos_variant() {
        assert_eq!(distro_of("ID=debian\nVERSION_ID=\"12\"\n"), Distro::Debian);
        assert_eq!(distro_of("ID=ubuntu\n"), Distro::Ubuntu);
        assert_eq!(distro_of("ID=raspbian\nID_LIKE=debian\n"), Distro::Debian);
        assert_eq!(distro_of("ID=rocky\n"), Distro::Rhel);
        assert_eq!(distro_of(""), Distro::Other(String::new()));
        assert_eq!(
            os_release_field("ID=debian\nVERSION_ID=\"12\"\n", "VERSION_ID"),
            "12"
        );
        assert_eq!(os_release_field("", "VERSION_ID"), "");
    }

    // ---- Fake ----

    /// The package-manager fact is a set because they coexist. A Debian box
    /// with Homebrew has both, and first-match-wins would have told
    /// `brew::Present` that this host has no brew.
    #[test]
    fn every_package_manager_present_is_reported_not_the_first() {
        use crate::backend::Fake;
        let fake = Fake::new()
            .with_file("/usr/bin/apt-get", "")
            .with_file("/home/linuxbrew/.linuxbrew/bin/brew", "");
        let f = Facts::gather(&fake);
        assert!(f.has_pm(&Pm::Apt));
        assert!(f.has_pm(&Pm::Brew));
        assert!(!f.has_pm(&Pm::Dnf));
    }

    /// No manager found is the empty set, which cannot be mistaken for one.
    #[test]
    fn no_known_manager_is_an_empty_set() {
        use crate::backend::Fake;
        let f = Facts::gather(&Fake::new());
        assert!(f.package_managers.is_empty());
        assert!(!f.has_pm(&Pm::Apt));
    }
}
