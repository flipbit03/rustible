//! The fixed core facts. Gathered eagerly at startup from a handful of reads.
//! Anything beyond this is an op.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::backend::Backend;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Os {
    Linux,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Distro {
    Debian,
    Ubuntu,
    Alpine,
    Fedora,
    Rhel,
    Arch,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Arch {
    X86_64,
    Aarch64,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pm {
    Apt,
    Dnf,
    Apk,
    Pacman,
    Zypper,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Init {
    Systemd,
    OpenRc,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    pub os: Os,
    pub distro: Distro,
    pub distro_version: String,
    pub arch: Arch,
    pub kernel: String,
    pub hostname: String,
    pub package_manager: Pm,
    pub init: Init,
    pub cpus: u32,
    pub memory_mb: u64,
    pub user: String,
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
