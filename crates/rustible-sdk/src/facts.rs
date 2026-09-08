//! The fixed core facts. Gathered eagerly at startup from a handful of reads.
//! Anything beyond this is an op.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::backend::Backend;

/// The kernel family, taken from `std::env::consts::OS`.
///
/// Compiled in rather than probed, so it names the target the playbook
/// binary was built for. On a real run that is the machine the binary was
/// cross-built for and shipped to, which is the same answer; on a `Fake`
/// system it is the answer for the machine running the tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    /// The only kernel Rustible's own ops target.
    Linux,
    /// Any other `std::env::consts::OS` value, carried verbatim (`macos`,
    /// `freebsd`).
    Other(String),
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

/// The package manager an op should drive on this host.
///
/// Decided by looking for the binary itself, in the order the variants are
/// listed, not by inferring from [`Distro`]: a host carrying two of them
/// gets the first match, and a distro that has swapped its manager out is
/// still described correctly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// None of the probed paths exist. Gathering never fills the string in,
    /// so it is always empty here; the variant exists so a package op can
    /// refuse a host it has no manager for instead of guessing one.
    Other(String),
}

/// What is running as pid 1, and so which service manager a unit op has to
/// talk to. Read from `/proc/1/comm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Init {
    /// `/proc/1/comm` reads `systemd`.
    Systemd,
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
    /// `24.04`), with the quotes stripped. Not a number: rolling releases
    /// print dates or nothing at all, and a host with no `VERSION_ID` (Arch)
    /// or no `/etc/os-release` gets the empty string.
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
    /// Which package manager is on the box; see [`Pm`] for the probe order.
    pub package_manager: Pm,
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

impl Facts {
    /// Gather from a backend. Real on `Local`; on `Fake` it reads whatever
    /// files the test planted, so distro branches are testable.
    pub fn gather(b: &dyn Backend) -> Facts {
        let read = |p: &str| {
            b.read(Path::new(p))
                .ok()
                .map(|v| String::from_utf8_lossy(&v).into_owned())
        };

        let os_release = read("/etc/os-release").unwrap_or_default();
        let field = |key: &str| -> Option<String> {
            os_release
                .lines()
                .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
                .map(|v| v.trim().trim_matches('"').to_string())
        };
        let id = field("ID").unwrap_or_default();
        let id_like = field("ID_LIKE").unwrap_or_default();
        let distro = match id.as_str() {
            "debian" => Distro::Debian,
            "ubuntu" => Distro::Ubuntu,
            "alpine" => Distro::Alpine,
            "fedora" => Distro::Fedora,
            "rhel" | "centos" | "rocky" | "almalinux" => Distro::Rhel,
            "arch" => Distro::Arch,
            _ if id_like.contains("debian") => Distro::Debian,
            _ if id_like.contains("rhel") || id_like.contains("fedora") => Distro::Rhel,
            _ => Distro::Other(id.clone()),
        };
        let distro_version = field("VERSION_ID").unwrap_or_default();

        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            other => Arch::Other(other.to_string()),
        };
        let os = match std::env::consts::OS {
            "linux" => Os::Linux,
            other => Os::Other(other.to_string()),
        };

        let kernel = read("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
            .to_string();
        let hostname = read("/proc/sys/kernel/hostname")
            .or_else(|| read("/etc/hostname"))
            .unwrap_or_default()
            .trim()
            .to_string();

        let has = |p: &str| b.stat(Path::new(p)).ok().flatten().is_some();
        let package_manager = if has("/usr/bin/apt-get") {
            Pm::Apt
        } else if has("/usr/bin/dnf") {
            Pm::Dnf
        } else if has("/sbin/apk") {
            Pm::Apk
        } else if has("/usr/bin/pacman") {
            Pm::Pacman
        } else if has("/usr/bin/zypper") {
            Pm::Zypper
        } else {
            Pm::Other(String::new())
        };

        let init = match read("/proc/1/comm").unwrap_or_default().trim() {
            "systemd" => Init::Systemd,
            "init" if has("/sbin/openrc") => Init::OpenRc,
            other => Init::Other(other.to_string()),
        };

        let cpus = read("/proc/cpuinfo")
            .map(|s| s.lines().filter(|l| l.starts_with("processor")).count() as u32)
            .filter(|&n| n > 0)
            .unwrap_or(1);

        let memory_mb = read("/proc/meminfo")
            .and_then(|s| {
                s.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|kb| kb.parse::<u64>().ok())
            })
            .map(|kb| kb / 1024)
            .unwrap_or(0);

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
            package_manager,
            init,
            cpus,
            memory_mb,
            user,
            is_root: uid == 0,
        }
    }
}
