//! Debian/Ubuntu packages via apt. Ansible's `ansible.builtin.apt`, one op
//! per `state` (vision 6.3): [`Present`] (`state=present`), [`Absent`]
//! (`state=absent`, with `purge` and `autoremove`) and [`Latest`]
//! (`state=latest`).
//!
//! Every op refuses on a host whose package manager is not apt and when not
//! running as root. `check` reads with `dpkg-query`, `apt-cache policy` and
//! `stat`. It runs no `apt-get` at all under `--check`, and in a real run
//! only the refresh [`Latest`] asks for with `.update_cache()`, with the
//! `mkdir -p` and `touch` that record it (see below).
//!
//! **Cache refresh.** [`Present`] and [`Latest`] take `.update_cache(max_age)`:
//! `apt-get update` runs when the lists are older than `max_age` or of unknown
//! age; `Duration::ZERO` means always. Their age is that of the newer of
//! `/var/lib/apt/periodic/update-success-stamp` and `/var/lib/apt/lists`
//! (`/var/cache/apt/pkgcache.bin` only when neither can be read), and a
//! refresh that fetched every index touches the stamp, because an
//! `apt-get update` that changes no index leaves the lists as old as they
//! were. *Where* it runs differs, because the two ops need the lists at
//! different moments:
//!
//! - [`Present`] only asks whether a package is installed, which dpkg answers
//!   without the lists. It refreshes in `apply`, right before installing, and
//!   never in `check`.
//! - [`Latest`] compares installed versions against the *candidate* versions
//!   the lists carry, so stale lists give a wrong answer. A real run
//!   refreshes in `check`, before reading the candidates.
//!
//! **Neither refreshes under `--check`**: `apt-get update` contacts the
//! distribution's mirrors and rewrites `/var/lib/apt/lists`, and a dry run
//! touches nothing outside the target (vision 12). For [`Latest`] that means
//! a dry run whose lists are stale cannot know the candidates, and says so:
//! the step reports `would change`, with a diff saying the lists were not
//! refreshed, and has no output. It runs no `apt-cache policy` either, since
//! the answer would come from the stale lists. Lists within `max_age` need no
//! refresh, so the dry run plans from them exactly as the real run will.
//!
//! Ansible's `apt` also skips the refresh under check mode
//! (`ansible/ansible@4da24b8128c8e334f3817f4700f4276c855856db`,
//! `lib/ansible/modules/apt.py` L1460,
//! `if not module.check_mode: cache.update()`), but then plans from the
//! stale lists, so its dry run can call a package current that the real run
//! upgrades. Rustible says it does not know instead.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustible_sdk::prelude::*;

/// One package and the version dpkg has for it. Every report in this module
/// is built out of these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// The binary package name, as the op was given it and as `dpkg-query`
    /// keys on. Nothing is resolved here: a virtual package or an alias is
    /// whatever apt makes of it, and the name is reported unchanged.
    pub name: String,
    /// As dpkg reports it after the step ran. Empty only when dpkg has no
    /// version for the name, which a real run does not produce.
    pub version: String,
}

/// Output of [`Present`]. The two lists together name every package the op
/// was given, so `installed` empty means the step was `ok`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallReport {
    /// Packages this step installed.
    pub installed: Vec<Package>,
    /// Packages that were already there.
    pub already_present: Vec<Package>,
}

/// Output of [`Absent`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoveReport {
    /// Packages this step removed (or purged), with the version they had.
    pub removed: Vec<Package>,
    /// Names that were not installed to begin with.
    pub not_present: Vec<String>,
}

/// Output of [`Latest`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UpgradeReport {
    /// Packages this step upgraded, with the version they had before.
    pub upgraded: Vec<(Package, String)>,
    /// Packages this step installed because they were missing.
    pub installed: Vec<Package>,
    /// Packages that were already at the candidate version.
    pub current: Vec<Package>,
}

/// What `dpkg-query -W -f='${Status}\t${Version}\n'` says about one package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DpkgStatus {
    /// dpkg has never heard of it (exit status 1) or the status is empty.
    Unknown,
    /// `install ok installed`.
    Installed(String),
    /// `deinstall ok config-files`: removed, configuration files remain.
    ConfigFiles(String),
    /// Any other status (half-installed, unpacked, ...), with the raw text.
    Other(String),
}

/// Parse one `${Status}\t${Version}` line.
pub fn parse_dpkg_status(text: &str) -> DpkgStatus {
    let line = text.lines().next().unwrap_or("").trim_end();
    let (status, version) = line.split_once('\t').unwrap_or((line, ""));
    let status = status.trim();
    if status.is_empty() {
        DpkgStatus::Unknown
    } else if status == "install ok installed" {
        DpkgStatus::Installed(version.to_string())
    } else if status == "deinstall ok config-files" {
        DpkgStatus::ConfigFiles(version.to_string())
    } else {
        DpkgStatus::Other(status.to_string())
    }
}

/// The two lines of `apt-cache policy <pkg>` that matter.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Policy {
    /// `None` when the line says `(none)`.
    pub installed: Option<String>,
    /// `None` when the line says `(none)` or the package is unknown.
    pub candidate: Option<String>,
}

/// Parse `apt-cache policy` output. Unknown packages print nothing on stdout,
/// which parses to a `Policy` with no candidate.
pub fn parse_policy(text: &str) -> Policy {
    let field = |key: &str| -> Option<String> {
        text.lines()
            .map(str::trim)
            .find_map(|l| l.strip_prefix(key))
            .map(str::trim)
            .filter(|v| !v.is_empty() && *v != "(none)")
            .map(str::to_string)
    };
    Policy {
        installed: field("Installed:"),
        candidate: field("Candidate:"),
    }
}

/// Parse the output of `stat -c %Y` (seconds since the epoch).
pub fn parse_stat_mtime(text: &str) -> Option<u64> {
    text.trim().parse().ok()
}

/// Query dpkg for one package.
fn dpkg_status(sys: &System, name: &str) -> Result<DpkgStatus> {
    let out = sys
        .cmd("dpkg-query")
        .args(["-W", "-f=${Status}\t${Version}\n", name])
        .allow_failure()
        .run()?;
    if !out.success() {
        return Ok(DpkgStatus::Unknown); // dpkg-query exits 1 for unknown packages
    }
    Ok(parse_dpkg_status(&out.stdout_str()))
}

/// Installed version of one package. `None` if not `install ok installed`.
fn installed_version(sys: &System, name: &str) -> Result<Option<String>> {
    Ok(match dpkg_status(sys, name)? {
        DpkgStatus::Installed(v) => Some(v),
        _ => None,
    })
}

/// Candidate and installed versions from `apt-cache policy`.
fn policy(sys: &System, name: &str) -> Result<Policy> {
    let out = sys.cmd("apt-cache").args(["policy", name]).run()?;
    Ok(parse_policy(&out.stdout_str()))
}

/// The stamp Ubuntu's `update-notifier-common` hook touches after an
/// `apt-get update` that succeeded, and that `ansible.builtin.apt` reads
/// first for the lists' age. [`refresh_lists`] writes it too.
const SUCCESS_STAMP: &str = "/var/lib/apt/periodic/update-success-stamp";

/// [`SUCCESS_STAMP`]'s directory, shipped by the `apt` package but created
/// if missing.
const PERIODIC_DIR: &str = "/var/lib/apt/periodic";

/// What the lists' age is read from, [`SUCCESS_STAMP`] first.
const AGE_SOURCES: [&str; 2] = [SUCCESS_STAMP, "/var/lib/apt/lists"];

/// Read for the lists' age only when none of [`AGE_SOURCES`] can be. Never
/// in the newest-wins: any `apt-get install` rewrites it, and so does an
/// `apt-cache policy` run as root after a dpkg change, so it would make
/// lists weeks old look fresh. `ansible.builtin.apt` never reads it.
const AGE_FALLBACK: &str = "/var/cache/apt/pkgcache.bin";

/// The mtime of `path` from `stat -c %Y` (the SDK's `Stat` carries no
/// mtime), or `None` when it cannot be read.
fn mtime(sys: &System, path: &str) -> Result<Option<u64>> {
    let out = sys.cmd("stat").args(["-c", "%Y", path]).ok()?;
    Ok(out.and_then(|out| parse_stat_mtime(&out.stdout_str())))
}

/// Age of the apt lists: that of the newest of [`AGE_SOURCES`], skipping
/// any that cannot be read, else that of [`AGE_FALLBACK`]. `None` when none
/// can. The newest wins because the lists are as fresh as the last refresh
/// anything recorded: an `apt-get update` that changes no index leaves
/// `/var/lib/apt/lists` where it was, and an old stamp next to lists a
/// newer refresh rewrote says nothing about them.
fn cache_age(sys: &System) -> Result<Option<Duration>> {
    let mut newest = None;
    for path in AGE_SOURCES {
        newest = newest.max(mtime(sys, path)?);
    }
    if newest.is_none() {
        newest = mtime(sys, AGE_FALLBACK)?;
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    Ok(newest.map(|mtime| Duration::from_secs(now.saturating_sub(mtime))))
}

/// Why the apt lists count as stale under the `.update_cache(max_age)` rule.
/// Deciding it reads `stat` and nothing else, so `check` can ask in either
/// mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stale {
    /// `Duration::ZERO`: a refresh on every run.
    Always,
    /// None of [`AGE_SOURCES`] has a readable age.
    AgeUnknown,
    /// Older than the age the op was given.
    Older { age: Duration, max_age: Duration },
}

impl std::fmt::Display for Stale {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Stale::Always => write!(
                f,
                "`.update_cache(Duration::ZERO)` refreshes the package lists on every run"
            ),
            Stale::AgeUnknown => write!(
                f,
                "the age of the package lists cannot be read (from {}, {AGE_FALLBACK})",
                AGE_SOURCES.join(", ")
            ),
            Stale::Older { age, max_age } => write!(
                f,
                "the package lists are {} old, older than the `.update_cache()` age of {}",
                human_duration(*age),
                human_duration(*max_age)
            ),
        }
    }
}

/// Pure: a duration as a person reads it, in its largest unit and the one
/// below it, a zero left out: `45s`, `1m 30s`, `1h`, `2h 1m`, `3d 4h`, and
/// `less than a second` below one.
fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs == 0 {
        return "less than a second".to_string();
    }
    let units = [
        (secs / 86_400, "d"),
        (secs % 86_400 / 3_600, "h"),
        (secs % 3_600 / 60, "m"),
        (secs % 60, "s"),
    ];
    units
        .iter()
        .skip_while(|(n, _)| *n == 0)
        .take(2)
        .filter(|(n, _)| *n != 0)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether the lists need a refresh before they can be trusted: `max_age` is
/// `Duration::ZERO`, their age is unknown, or they are older than `max_age`.
/// `None` when they are within it. Runs `stat` at most, never `apt-get`.
fn staleness(sys: &System, max_age: Duration) -> Result<Option<Stale>> {
    if max_age.is_zero() {
        return Ok(Some(Stale::Always));
    }
    Ok(match cache_age(sys)? {
        None => Some(Stale::AgeUnknown),
        Some(age) if age > max_age => Some(Stale::Older { age, max_age }),
        Some(_) => None,
    })
}

/// Pure: the first line of `apt-get update`'s output saying an index was
/// not fetched, or `None` when every one was. apt exits 0 after such a
/// failure (offline, a dead mirror), so the status alone cannot say; apt's
/// own `Post-Invoke-Success` hooks do not run then either. The backend
/// forces `LANG=C`, so the lines are apt's English ones.
fn failed_fetch(output: &str) -> Option<&str> {
    output.lines().map(str::trim).find(|l| {
        l.contains("Failed to fetch") || l.contains("Some index files failed to download")
    })
}

/// `apt-get update`: fetch the package lists from the host's mirrors, then
/// record that it succeeded in [`SUCCESS_STAMP`], whose mtime [`cache_age`]
/// reads. Never under `--check` (vision 12). A failed update writes no
/// stamp, and neither does one that exits 0 having failed to fetch an index
/// ([`failed_fetch`]): that warns instead, and the next run may refresh
/// again.
///
/// The stamp is written with `mkdir -p` and `touch`, not through `sys`,
/// because [`Latest`] refreshes in `check`, where the SDK refuses every
/// write through `sys` even in a real run; a command is outside that guard,
/// as `apt-get update` itself is. A stamp that cannot be written warns and
/// does not fail the step, as Ubuntu's hook ignores a failed `touch`: the
/// lists were refreshed, and the cost is a refresh the next run may repeat.
fn refresh_lists(sys: &System) -> Result<()> {
    let out = apt_get(sys).arg("update").run()?;
    let output = format!("{}\n{}", out.stdout_str(), out.stderr_str());
    if let Some(line) = failed_fetch(&output) {
        sys.warn(format!(
            "`apt-get update` exited 0 but did not fetch every package index (`{line}`), \
             so {SUCCESS_STAMP} was not written and the next run may refresh again; \
             check the host's network, and the repositories in /etc/apt/sources.list and \
             /etc/apt/sources.list.d/ and their signing keys (a `NO_PUBKEY` above)"
        ));
        return Ok(());
    }
    let stamped = sys
        .cmd("mkdir")
        .args(["-p", PERIODIC_DIR])
        .run()
        .and_then(|_| sys.cmd("touch").arg(SUCCESS_STAMP).run());
    if let Err(e) = stamped {
        sys.warn(format!(
            "`apt-get update` succeeded, but recording it in {SUCCESS_STAMP} failed ({}), \
             so the next run may refresh again; check that {PERIODIC_DIR} is a writable directory",
            e.chain()
        ));
    }
    Ok(())
}

/// Run `apt-get update` when [`staleness`] says the lists need it.
fn update_cache_if_stale(sys: &System, max_age: Duration) -> Result<()> {
    if staleness(sys, max_age)?.is_some() {
        refresh_lists(sys)?;
    }
    Ok(())
}

/// The host's package managers, for a refusal message: named if there are
/// any, said plainly if there are none.
fn describe_pms(sys: &System) -> String {
    let pms = &sys.facts().package_managers;
    if pms.is_empty() {
        format!("none that rustible knows ({:?})", sys.facts().distro)
    } else {
        format!("{pms:?} ({:?})", sys.facts().distro)
    }
}

/// Refuse early on a host this op cannot serve: the wrong kernel, no apt, or
/// no root (vision 6.8).
fn require_apt_root(sys: &System, op: &str) -> Result<()> {
    // The OS check is not redundant with the apt check below. `Pm::Apt` means
    // `/usr/bin/apt-get` exists, which is a strong hint and not a promise:
    // this op also drives `dpkg-query` and reads Debian's own layout, and it
    // says so here rather than failing further in on a host that borrowed the
    // binary.
    match sys.facts().os {
        Os::Linux => {}
        ref other => bail!(
            "apt::{op} manages Debian packages and runs on Linux only; this host is {}",
            other.name()
        ),
    }
    if !sys.facts().has_pm(&Pm::Apt) {
        bail!(
            "apt::{op} needs apt, but this host has {}",
            describe_pms(sys)
        );
    }
    if !sys.is_root() {
        bail!(
            "apt::{op} needs root, but this runs as `{}` (use escalate = true or ctx.as_root())",
            sys.facts().user
        );
    }
    Ok(())
}

fn apt_get(sys: &System) -> rustible_sdk::Cmd {
    sys.cmd("apt-get").env("DEBIAN_FRONTEND", "noninteractive")
}

// ---------------------------------------------------------------- Present

/// Ensure packages are installed. `apt: state=present`.
#[derive(Debug, Clone)]
pub struct Present {
    names: Vec<String>,
    update_cache: Option<Duration>,
    install_recommends: bool,
}

impl Present {
    /// Ensure every one of `names` is installed, leaving the version to apt:
    /// a package that is already there is `ok` however old it is (that is
    /// [`Latest`]'s job). Nothing else is on. There is no cache refresh
    /// until [`Present::update_cache`] asks for one, and recommends are off,
    /// so `apt-get install --no-install-recommends` is what runs.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Present {
            names: names.into_iter().map(Into::into).collect(),
            update_cache: None,
            install_recommends: false,
        }
    }

    /// Run `apt-get update` before installing (only when something is
    /// missing) if the apt lists are older than `max_age`. `Duration::ZERO`
    /// always updates. Ansible's `update_cache` with `cache_valid_time`.
    pub fn update_cache(mut self, max_age: Duration) -> Self {
        self.update_cache = Some(max_age);
        self
    }

    /// Pull in each package's `Recommends:` as well. Off by default, which
    /// is what puts `--no-install-recommends` on the `apt-get install` line
    /// and is the opposite of apt's own default. It bears only on packages
    /// this step installs; one that is already there is not revisited.
    pub fn install_recommends(mut self, on: bool) -> Self {
        self.install_recommends = on;
        self
    }
}

/// What [`Present`]'s `check` decided: install these packages, which dpkg
/// does not have, in the order the op named them.
#[derive(Debug)]
pub struct Install {
    names: Vec<String>,
}

impl Intent for Install {
    fn diff(&self) -> Diff {
        Diff::attrs(
            "apt packages",
            self.names
                .iter()
                .map(|name| AttrChange::new(name.as_str(), "absent", "installed"))
                .collect(),
        )
    }
}

impl Op for Present {
    type Output = InstallReport;
    type Intent = Install;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        require_apt_root(sys, "Present")?;
        let mut report = InstallReport::default();
        let mut missing = vec![];
        for name in &self.names {
            match installed_version(sys, name)? {
                Some(version) => report.already_present.push(Package {
                    name: name.clone(),
                    version,
                }),
                None => missing.push(name.clone()),
            }
        }
        if missing.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        Ok(Plan::Change(Install { names: missing }))
    }

    fn apply(&self, sys: &System, intent: Install) -> Result<InstallReport> {
        // Install what `check` planned, not what dpkg says now (vision 6.2).
        let missing = intent.names;
        // `apply` never runs under `--check`, so this refresh never does.
        if let Some(max_age) = self.update_cache {
            update_cache_if_stale(sys, max_age)?;
        }
        let mut cmd = apt_get(sys).args(["install", "-y"]);
        if !self.install_recommends {
            cmd = cmd.arg("--no-install-recommends");
        }
        cmd.args(missing.iter().cloned()).run()?;

        // Report the versions dpkg actually has now.
        let mut report = InstallReport::default();
        for name in &self.names {
            let package = Package {
                name: name.clone(),
                version: installed_version(sys, name)?.unwrap_or_default(),
            };
            if missing.contains(name) {
                report.installed.push(package);
            } else {
                report.already_present.push(package);
            }
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------- Absent

/// Ensure packages are not installed. `apt: state=absent`.
///
/// A package whose status is `deinstall ok config-files` (removed, config
/// files kept) counts as present only with `.purge(true)`, so a plain
/// `Absent` after a plain remove is `ok`, and a purging one is `changed`.
/// `.autoremove(true)` runs `apt-get autoremove -y` after a removal (with
/// `--purge` when purging); it does not run when nothing was removed.
#[derive(Debug, Clone)]
pub struct Absent {
    names: Vec<String>,
    purge: bool,
    autoremove: bool,
}

impl Absent {
    /// Ensure none of `names` is installed. Plain removal: configuration
    /// files stay unless [`Absent::purge`], and no dependencies are swept up
    /// unless [`Absent::autoremove`]. Names dpkg has never heard of are
    /// reported in `not_present` rather than failing the step.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Absent {
            names: names.into_iter().map(Into::into).collect(),
            purge: false,
            autoremove: false,
        }
    }

    /// `apt-get purge` instead of `remove`: configuration files go too.
    pub fn purge(mut self, on: bool) -> Self {
        self.purge = on;
        self
    }

    /// Run `apt-get autoremove -y` after removing.
    pub fn autoremove(mut self, on: bool) -> Self {
        self.autoremove = on;
        self
    }
}

/// What [`Absent`]'s `check` decided: remove (or purge) these packages, each
/// with what dpkg had for it.
#[derive(Debug)]
pub struct Remove {
    purge: bool,
    packages: Vec<Removal>,
}

/// One package [`Absent`] takes away.
#[derive(Debug)]
struct Removal {
    name: String,
    /// The version dpkg had, which the report names: apt-get takes it.
    version: String,
    /// Only the configuration files were left (`deinstall ok
    /// config-files`), which a purge removes.
    config_files: bool,
}

impl Intent for Remove {
    fn diff(&self) -> Diff {
        let verb = if self.purge { "purged" } else { "removed" };
        Diff::attrs(
            "apt packages",
            self.packages
                .iter()
                .map(|p| {
                    let from = if p.config_files {
                        "config-files"
                    } else {
                        "installed"
                    };
                    AttrChange::new(p.name.as_str(), from, verb)
                })
                .collect(),
        )
    }
}

impl Op for Absent {
    type Output = RemoveReport;
    type Intent = Remove;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        require_apt_root(sys, "Absent")?;
        let mut packages = vec![];
        let mut not_present = vec![];
        for name in &self.names {
            let present = match dpkg_status(sys, name)? {
                DpkgStatus::Installed(v) => Some((v, false)),
                DpkgStatus::ConfigFiles(v) if self.purge => Some((v, true)),
                DpkgStatus::ConfigFiles(_) | DpkgStatus::Unknown => None,
                DpkgStatus::Other(status) => bail!(
                    "package `{name}` is in dpkg state `{status}`; \
                     fix it by hand (dpkg --configure -a) before apt::Absent"
                ),
            };
            match present {
                Some((version, config_files)) => packages.push(Removal {
                    name: name.clone(),
                    version,
                    config_files,
                }),
                None => not_present.push(name.clone()),
            }
        }
        if packages.is_empty() {
            return Ok(Plan::Satisfied(RemoveReport {
                removed: vec![],
                not_present,
            }));
        }
        Ok(Plan::Change(Remove {
            purge: self.purge,
            packages,
        }))
    }

    fn apply(&self, sys: &System, intent: Remove) -> Result<RemoveReport> {
        // Remove what `check` planned, not what dpkg says now (vision 6.2).
        let Remove { purge, packages } = intent;
        let verb = if purge { "purge" } else { "remove" };
        apt_get(sys)
            .args([verb, "-y"])
            .args(packages.iter().map(|p| p.name.clone()))
            .run()?;
        if self.autoremove {
            let mut cmd = apt_get(sys).args(["autoremove", "-y"]);
            if purge {
                cmd = cmd.arg("--purge");
            }
            cmd.run()?;
        }
        // The versions that went are the ones `check` read.
        let mut report = RemoveReport::default();
        for name in &self.names {
            match packages.iter().find(|p| &p.name == name) {
                Some(p) => report.removed.push(Package {
                    name: p.name.clone(),
                    version: p.version.clone(),
                }),
                None => report.not_present.push(name.clone()),
            }
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------- Latest

/// Ensure packages are installed at the candidate version apt knows about.
/// `apt: state=latest`.
///
/// `check` compares the installed version (`dpkg-query`) with the candidate
/// from `apt-cache policy`; a missing package is installed, an outdated one
/// upgraded with `apt-get install --only-upgrade`. A name apt cannot resolve
/// fails the step.
///
/// With [`update_cache`](Latest::update_cache), a real run refreshes the
/// lists in `check`, before the candidates are read. **Under `--check` it
/// does not**: when the lists are stale the step reports `would change`,
/// with a diff saying the candidates are unknown because the lists were not
/// refreshed, and has no output. Lists within the age need no refresh, so a
/// dry run plans from them exactly as the real run will. Without
/// `update_cache` neither mode refreshes. See the module docs.
#[derive(Debug, Clone)]
pub struct Latest {
    names: Vec<String>,
    update_cache: Option<Duration>,
    install_recommends: bool,
}

impl Latest {
    /// Ensure every one of `names` is installed at the candidate version
    /// apt currently knows. No cache refresh until
    /// [`Latest::update_cache`] asks for one, so by default the candidates
    /// come from whatever `/var/lib/apt/lists` already holds, in a real run
    /// and a dry run alike. Recommends are off, as with [`Present`].
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Latest {
            names: names.into_iter().map(Into::into).collect(),
            update_cache: None,
            install_recommends: false,
        }
    }

    /// Run `apt-get update` in `check`, before the candidate versions are
    /// read, if the apt lists are older than `max_age`. `Duration::ZERO`
    /// always updates. Ansible's `update_cache` with `cache_valid_time`.
    ///
    /// **Not under `--check`**: a dry run contacts no mirror (vision 12).
    /// When the lists are stale, it cannot know the candidates, so the step
    /// reports `would change` with a diff saying the lists were not
    /// refreshed, and has no output; it does not plan from the stale lists,
    /// where a package could look current that the real run then upgrades.
    /// When they are within `max_age`, the dry run plans from them as the
    /// real run will.
    pub fn update_cache(mut self, max_age: Duration) -> Self {
        self.update_cache = Some(max_age);
        self
    }

    /// Install recommended packages along with missing ones (default: no).
    pub fn install_recommends(mut self, on: bool) -> Self {
        self.install_recommends = on;
        self
    }

    /// The `check`-time refresh, when the caller asked for one and the lists
    /// are stale. In a real run it runs `apt-get update` and returns `None`.
    /// Under `--check` it runs nothing and returns why the lists would have
    /// been refreshed, for the step to report that it cannot decide.
    fn refresh_cache(&self, sys: &System) -> Result<Option<Stale>> {
        let Some(max_age) = self.update_cache else {
            return Ok(None);
        };
        let Some(stale) = staleness(sys, max_age)? else {
            return Ok(None);
        };
        if sys.check_mode() {
            return Ok(Some(stale));
        }
        refresh_lists(sys)?;
        sys.debug(format!(
            "apt::Latest ran `apt-get update` before reading the candidate versions: {stale}"
        ));
        Ok(None)
    }
}

/// What [`Latest`]'s `check` decided: package by package, in the order the
/// op named them, install a missing one or upgrade an outdated one from the
/// version dpkg has. Under `--check` with stale lists, that it cannot say.
#[derive(Debug)]
pub struct Upgrade(Decision);

#[derive(Debug)]
enum Decision {
    /// What to install and what to upgrade, read from the lists.
    Packages(Vec<(String, Bump)>),
    /// Under `--check` only: the lists are stale and were not refreshed, so
    /// no candidate was read. Nothing here can be applied, and a real run
    /// never plans it: its `check` refreshes instead.
    Unrefreshed { names: Vec<String>, stale: Stale },
}

#[derive(Debug)]
enum Bump {
    /// Not installed: `apt-get install`.
    Install { candidate: String },
    /// Installed at an older version: `apt-get install --only-upgrade`.
    From {
        installed: String,
        candidate: String,
    },
}

impl Intent for Upgrade {
    fn diff(&self) -> Diff {
        match &self.0 {
            Decision::Packages(packages) => Diff::attrs(
                "apt packages",
                packages
                    .iter()
                    .map(|(name, bump)| match bump {
                        Bump::Install { candidate } => {
                            AttrChange::new(name.as_str(), "absent", candidate.as_str())
                        }
                        Bump::From {
                            installed,
                            candidate,
                        } => AttrChange::new(name.as_str(), installed.as_str(), candidate.as_str()),
                    })
                    .collect(),
            ),
            Decision::Unrefreshed { names, stale } => Diff::summary(format!(
                "apt packages {}: candidate versions unknown; {stale}, and the lists are not \
                 refreshed under --check",
                names.join(", ")
            )),
        }
    }
}

impl Op for Latest {
    type Output = UpgradeReport;
    type Intent = Upgrade;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        require_apt_root(sys, "Latest")?;
        // Before the candidates are read, not after: `apt-cache policy` can
        // only answer from the lists on disk. Under `--check` stale lists are
        // not refreshed, and their candidates are not read either: an answer
        // from them could call a package current that the real run upgrades.
        // With no names there is nothing to be unsure about.
        if let Some(stale) = self.refresh_cache(sys)?
            && !self.names.is_empty()
        {
            return Ok(Plan::Change(Upgrade(Decision::Unrefreshed {
                names: self.names.clone(),
                stale,
            })));
        }
        let mut current = vec![];
        let mut packages = vec![];
        for name in &self.names {
            let installed = installed_version(sys, name)?;
            let Some(candidate) = policy(sys, name)?.candidate else {
                bail!(
                    "package `{name}` has no candidate version in the apt cache \
                     (unknown name, or the lists need `apt-get update`)"
                );
            };
            match installed {
                None => packages.push((name.clone(), Bump::Install { candidate })),
                Some(v) if v != candidate => packages.push((
                    name.clone(),
                    Bump::From {
                        installed: v,
                        candidate,
                    },
                )),
                Some(_) => current.push(Package {
                    name: name.clone(),
                    version: candidate,
                }),
            }
        }
        if packages.is_empty() {
            return Ok(Plan::Satisfied(UpgradeReport {
                current,
                ..UpgradeReport::default()
            }));
        }
        Ok(Plan::Change(Upgrade(Decision::Packages(packages))))
    }

    fn apply(&self, sys: &System, intent: Upgrade) -> Result<UpgradeReport> {
        let packages = match intent.0 {
            Decision::Packages(packages) => packages,
            Decision::Unrefreshed { stale, .. } => bail!(
                "apt::Latest cannot apply a plan made under --check, which names no candidate \
                 versions because the package lists were not refreshed ({stale}). A real run \
                 refreshes them in `check` and plans from the result"
            ),
        };
        let mut to_install = vec![];
        let mut to_upgrade = vec![];
        for (name, bump) in packages {
            match bump {
                Bump::Install { .. } => to_install.push(name),
                Bump::From { installed, .. } => to_upgrade.push((name, installed)),
            }
        }
        // No refresh here: `check` always runs first (this plan came from it)
        // and did it, so the candidates apt sees are already the fresh ones.
        if !to_install.is_empty() {
            let mut cmd = apt_get(sys).args(["install", "-y"]);
            if !self.install_recommends {
                cmd = cmd.arg("--no-install-recommends");
            }
            cmd.args(to_install.iter().cloned()).run()?;
        }
        if !to_upgrade.is_empty() {
            apt_get(sys)
                .args(["install", "-y", "--only-upgrade"])
                .args(to_upgrade.iter().map(|(name, _)| name.clone()))
                .run()?;
        }
        // Report what dpkg actually has now: apt may have landed on something
        // other than the candidate (a hold, a dependency, a pinned version).
        let mut report = UpgradeReport::default();
        for name in &self.names {
            let package = Package {
                name: name.clone(),
                version: installed_version(sys, name)?.unwrap_or_default(),
            };
            if to_install.contains(name) {
                report.installed.push(package);
            } else if let Some((_, previous)) = to_upgrade.iter().find(|(n, _)| n == name) {
                report.upgraded.push((package, previous.clone()));
            } else {
                report.current.push(package);
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::*;

    const DPKG_ARGS: [&str; 2] = ["-W", "-f=${Status}\t${Version}\n"];

    fn sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    /// Facts for a mac.
    fn macos(sys: System) -> System {
        let mut facts = sys.facts().clone();
        facts.os = Os::Macos;
        facts.distro = Distro::Macos;
        facts.package_managers = [Pm::Brew].into_iter().collect();
        facts.init = Init::Launchd;
        sys.with_facts(facts)
    }

    /// The OS gate is not the same claim as the apt gate, so it is asserted
    /// separately: a mac with Homebrew has a package manager, just not this
    /// one, and the refusal should say which assumption failed.
    #[test]
    fn apt_refuses_a_mac_on_the_os_not_on_the_manager() {
        let fake = Arc::new(Fake::new());
        let s = macos(sys(&fake));
        let err = Present::new(["mc"]).check(&s).unwrap_err().chain();
        assert!(err.contains("runs on Linux only"), "{err}");
        assert!(err.contains("macos"), "{err}");
    }

    /// A Linux box with brew but no apt still gets the apt refusal, and it
    /// names what the host actually has rather than "Other".
    #[test]
    fn apt_names_the_managers_the_host_does_have() {
        let fake = Arc::new(Fake::new());
        let mut facts = sys(&fake).facts().clone();
        facts.package_managers = [Pm::Brew].into_iter().collect();
        let s = sys(&fake).with_facts(facts);
        let err = Present::new(["mc"]).check(&s).unwrap_err().chain();
        assert!(err.contains("needs apt"), "{err}");
        assert!(err.contains("Brew"), "{err}");
    }

    fn non_root(fake: &Arc<Fake>) -> System {
        let mut facts = sys(fake).facts().clone();
        facts.is_root = false;
        facts.user = "cadu".into();
        sys(fake).with_facts(facts)
    }

    fn alpine(fake: &Arc<Fake>) -> System {
        let mut facts = sys(fake).facts().clone();
        facts.package_managers = [Pm::Apk].into_iter().collect();
        facts.distro = Distro::Alpine;
        sys(fake).with_facts(facts)
    }

    fn check_mode_ctx(fake: &Arc<Fake>) -> Ctx {
        let s = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        Ctx::new(s, rustible_sdk::HostInfo::local())
    }

    /// Can `dpkg-query` for one package.
    fn dpkg(fake: Fake, name: &str, status: i32, line: &str) -> Fake {
        let args = [DPKG_ARGS[0], DPKG_ARGS[1], name];
        fake.with_cmd("dpkg-query", Some(&args), status, line)
    }

    /// Can `apt-cache policy` for one package.
    fn policy_of(fake: Fake, name: &str, installed: &str, candidate: &str) -> Fake {
        let out = format!(
            "{name}:\n  Installed: {installed}\n  Candidate: {candidate}\n  Version table:\n"
        );
        fake.with_cmd("apt-cache", Some(&["policy", name]), 0, &out)
    }

    fn argv_starting<'a>(argvs: &'a [Vec<String>], prefix: &[&str]) -> Option<&'a Vec<String>> {
        argvs
            .iter()
            .find(|a| a.len() >= prefix.len() && a.iter().zip(prefix).all(|(x, y)| x == y))
    }

    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// The two commands a refresh that succeeded records itself with.
    fn stamp_argvs() -> [Vec<String>; 2] {
        [
            ["mkdir", "-p", PERIODIC_DIR].map(String::from).to_vec(),
            ["touch", SUCCESS_STAMP].map(String::from).to_vec(),
        ]
    }

    /// Can [`stamp_argvs`], both succeeding.
    fn stamp_tools(fake: Fake) -> Fake {
        fake.with_cmd("mkdir", Some(&["-p", PERIODIC_DIR]), 0, "")
            .with_cmd("touch", Some(&[SUCCESS_STAMP]), 0, "")
    }

    /// Every command that ran to write the stamp. The `Fake` runs no
    /// command for real, so this, and not the file, is what a test reads.
    fn stamp_writes(fake: &Fake) -> Vec<Vec<String>> {
        fake.argvs()
            .into_iter()
            .filter(|a| a[0] == "mkdir" || a[0] == "touch")
            .collect()
    }

    /// The commands from `apt-get update` to the end, asserting it ran.
    fn from_update(fake: &Fake) -> Vec<Vec<String>> {
        let argvs = fake.argvs();
        let at = argvs
            .iter()
            .position(|a| a.starts_with(&["apt-get".into(), "update".into()]))
            .unwrap_or_else(|| panic!("apt-get update ran: {argvs:?}"));
        argvs[at..].to_vec()
    }

    // ---- pure parsers ----

    #[test]
    fn parse_dpkg_status_variants() {
        assert_eq!(
            parse_dpkg_status("install ok installed\t3:4.8.30-1\n"),
            DpkgStatus::Installed("3:4.8.30-1".into())
        );
        assert_eq!(
            parse_dpkg_status("deinstall ok config-files\t1.0\n"),
            DpkgStatus::ConfigFiles("1.0".into())
        );
        assert_eq!(parse_dpkg_status(""), DpkgStatus::Unknown);
        assert_eq!(parse_dpkg_status("\n"), DpkgStatus::Unknown);
        assert_eq!(
            parse_dpkg_status("install ok half-configured\t2.0\n"),
            DpkgStatus::Other("install ok half-configured".into())
        );
        // No tab at all: status only, empty version.
        assert_eq!(
            parse_dpkg_status("install ok installed"),
            DpkgStatus::Installed(String::new())
        );
    }

    #[test]
    fn parse_policy_installed_and_candidate() {
        let text = "openssl:\n  Installed: 3.0.15-1~deb12u1\n  Candidate: 3.0.16-1~deb12u1\n  Version table:\n     3.0.16-1~deb12u1 500\n";
        assert_eq!(
            parse_policy(text),
            Policy {
                installed: Some("3.0.15-1~deb12u1".into()),
                candidate: Some("3.0.16-1~deb12u1".into()),
            }
        );
    }

    #[test]
    fn parse_policy_none_means_not_installed() {
        let text = "sl:\n  Installed: (none)\n  Candidate: 5.02-1\n";
        assert_eq!(
            parse_policy(text),
            Policy {
                installed: None,
                candidate: Some("5.02-1".into()),
            }
        );
    }

    #[test]
    fn parse_policy_unknown_package_has_no_candidate() {
        assert_eq!(parse_policy(""), Policy::default());
        let text = "foo:\n  Installed: (none)\n  Candidate: (none)\n  Version table:\n";
        assert_eq!(parse_policy(text), Policy::default());
    }

    #[test]
    fn human_duration_shows_the_two_largest_units() {
        let d = Duration::from_secs;
        assert_eq!(human_duration(d(0)), "less than a second");
        assert_eq!(
            human_duration(Duration::from_millis(999)),
            "less than a second"
        );
        assert_eq!(human_duration(d(45)), "45s");
        assert_eq!(human_duration(d(90)), "1m 30s");
        assert_eq!(human_duration(d(3_600)), "1h");
        assert_eq!(human_duration(d(7_261)), "2h 1m");
        assert_eq!(human_duration(d(86_400 + 60)), "1d");
        assert_eq!(human_duration(d(3 * 86_400 + 4 * 3_600 + 5)), "3d 4h");
    }

    #[test]
    fn parse_stat_mtime_reads_seconds() {
        assert_eq!(parse_stat_mtime("1725800000\n"), Some(1725800000));
        assert_eq!(parse_stat_mtime(""), None);
        assert_eq!(parse_stat_mtime("nope"), None);
    }

    // ---- Present (pre-existing behaviour) ----

    #[test]
    fn satisfied_when_installed() {
        let fake = Arc::new(Fake::new().with_cmd(
            "dpkg-query",
            Some(&["-W", "-f=${Status}\t${Version}\n", "mc"]),
            0,
            "install ok installed\t3:4.8.30-1\n",
        ));
        let plan = Present::new(["mc"]).check(&sys(&fake)).unwrap();
        let Plan::Satisfied(r) = plan else {
            panic!("expected satisfied")
        };
        assert_eq!(r.already_present[0].version, "3:4.8.30-1");
        assert!(r.installed.is_empty());
    }

    #[test]
    fn change_when_missing_and_apply_runs_apt_get() {
        let fake = Arc::new(
            Fake::new()
                .with_cmd("dpkg-query", None, 1, "")
                .with_cmd("apt-get", None, 0, ""),
        );
        let s = sys(&fake);
        let op = Present::new(["mc"]);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff().short(), "mc=installed");
        op.apply(&s, c).unwrap();
        let argvs = fake.argvs();
        assert!(
            argvs.iter().any(
                |a| a.starts_with(&["apt-get".into(), "install".into(), "-y".into()])
                    && a.contains(&"mc".into())
            ),
            "{argvs:?}"
        );
        // No update_cache: neither stat nor apt-get update ran.
        assert!(argv_starting(&argvs, &["stat"]).is_none());
        assert!(argv_starting(&argvs, &["apt-get", "update"]).is_none());
    }

    #[test]
    fn present_check_asks_dpkg_only_and_never_apt_cache() {
        // `check` used to run `apt-cache policy` per missing package to
        // predict a version; without predictions the only probe is dpkg's,
        // and a stock image with no lists gets the same answer.
        let fake = Arc::new(Fake::new().with_cmd("dpkg-query", None, 1, ""));
        let Plan::Change(c) = Present::new(["sl"]).check(&sys(&fake)).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff().short(), "sl=installed");
        let argvs = fake.argvs();
        assert!(argvs.iter().all(|a| a[0] == "dpkg-query"), "{argvs:?}");
    }

    #[test]
    fn present_in_check_mode_would_change_with_no_output() {
        let fake = Arc::new(Fake::new().with_cmd("dpkg-query", None, 1, ""));
        let mut ctx = check_mode_ctx(&fake);
        let r = ctx.step("sl", Present::new(["sl"])).unwrap();
        assert!(r.changed && !r.is_available());
        let err = r.output().unwrap_err().to_string();
        assert!(err.contains("would have changed"), "{err}");
        assert!(fake.argvs().iter().all(|a| a[0] == "dpkg-query"));
    }

    /// `Present` refreshes only in `apply`, which a dry run never reaches,
    /// so `.update_cache(..)` adds no `apt-get update` and no stamp to one.
    #[test]
    fn present_in_check_mode_neither_refreshes_nor_writes_the_stamp() {
        let fake = Arc::new(present_missing_mc());
        let mut ctx = check_mode_ctx(&fake);
        let r = ctx
            .step("sl", Present::new(["sl"]).update_cache(Duration::ZERO))
            .unwrap();
        assert!(r.changed && !r.is_available());
        assert!(fake.argvs().iter().all(|a| a[0] == "dpkg-query"));
        assert!(stamp_writes(&fake).is_empty());
    }

    #[test]
    fn present_apply_installs_what_the_plan_named_and_rereads_versions() {
        let fake = Arc::new(
            dpkg(Fake::new().with_cmd("apt-get", None, 0, ""), "mc", 1, "").with_cmd(
                "dpkg-query",
                Some(&[DPKG_ARGS[0], DPKG_ARGS[1], "zsh"]),
                0,
                "install ok installed\t5.9-4\n",
            ),
        );
        let s = sys(&fake);
        let op = Present::new(["mc", "zsh"]);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        // Only the missing one is planned, and only it is installed.
        assert_eq!(c.diff().short(), "mc=installed");
        // Apply works from the intent alone.
        let report = op.apply(&s, c).unwrap();
        let argvs = fake.argvs();
        let install = argv_starting(&argvs, &["apt-get", "install"]).unwrap();
        assert!(install.contains(&"mc".into()) && !install.contains(&"zsh".into()));
        assert_eq!(report.installed[0].name, "mc");
        assert_eq!(report.already_present[0].version, "5.9-4");
    }

    #[test]
    fn refuses_on_non_apt_distro() {
        let fake = Arc::new(Fake::new());
        let err = Present::new(["mc"])
            .check(&alpine(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs apt"), "{err}");
    }

    #[test]
    fn deconfigured_package_counts_as_missing() {
        // dpkg knows it but it is not "install ok installed".
        let fake = Arc::new(Fake::new().with_cmd(
            "dpkg-query",
            None,
            0,
            "deinstall ok config-files\t1.0\n",
        ));
        let plan = Present::new(["mc"]).check(&sys(&fake)).unwrap();
        assert!(plan.is_change());
    }

    // ---- Present: update_cache(Duration) ----

    fn present_missing_mc() -> Fake {
        stamp_tools(
            Fake::new()
                .with_cmd("dpkg-query", None, 1, "")
                .with_cmd("apt-get", None, 0, ""),
        )
    }

    fn apply_present(fake: &Arc<Fake>, op: Present) {
        let s = sys(fake);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        op.apply(&s, c).unwrap();
    }

    #[test]
    fn update_cache_runs_apt_get_update_when_lists_are_stale() {
        let stale = (now_secs() - 7200).to_string();
        let fake = Arc::new(present_missing_mc().with_cmd("stat", None, 0, &stale));
        apply_present(
            &fake,
            Present::new(["mc"]).update_cache(Duration::from_secs(3600)),
        );
        let argvs = fake.argvs();
        let stats: Vec<_> = argvs.iter().filter(|a| a[0] == "stat").collect();
        let expected: Vec<_> = AGE_SOURCES
            .iter()
            .map(|p| ["stat", "-c", "%Y", p].map(String::from).to_vec())
            .collect();
        assert_eq!(stats, expected.iter().collect::<Vec<_>>());
        // The update, then the stamp recording it, then the install.
        let after = from_update(&fake);
        assert_eq!(after[1..3], stamp_argvs(), "{argvs:?}");
        assert_eq!(after[3][..2], ["apt-get", "install"], "{argvs:?}");
    }

    #[test]
    fn update_cache_skips_update_when_lists_are_fresh() {
        let fresh = (now_secs() - 60).to_string();
        let fake = Arc::new(present_missing_mc().with_cmd("stat", None, 0, &fresh));
        apply_present(
            &fake,
            Present::new(["mc"]).update_cache(Duration::from_secs(3600)),
        );
        let argvs = fake.argvs();
        assert!(
            argv_starting(&argvs, &["apt-get", "update"]).is_none(),
            "{argvs:?}"
        );
        assert!(argv_starting(&argvs, &["apt-get", "install"]).is_some());
        assert!(stamp_writes(&fake).is_empty(), "no refresh, no stamp");
    }

    #[test]
    fn update_cache_zero_always_updates_without_stat() {
        let fake = Arc::new(present_missing_mc());
        apply_present(&fake, Present::new(["mc"]).update_cache(Duration::ZERO));
        let argvs = fake.argvs();
        assert!(argv_starting(&argvs, &["stat"]).is_none(), "{argvs:?}");
        assert!(argv_starting(&argvs, &["apt-get", "update"]).is_some());
    }

    /// Plants `stat -c %Y` for each of [`AGE_SOURCES`] and then
    /// [`AGE_FALLBACK`], in order: an mtime that many seconds ago, or `None`
    /// for one `stat` cannot read.
    fn ages(fake: Fake, ago: [Option<u64>; 3]) -> Fake {
        let paths = [AGE_SOURCES[0], AGE_SOURCES[1], AGE_FALLBACK];
        paths.iter().zip(ago).fold(fake, |fake, (path, ago)| {
            let args = ["-c", "%Y", path];
            match ago {
                Some(ago) => fake.with_cmd("stat", Some(&args), 0, &(now_secs() - ago).to_string()),
                None => fake.with_cmd("stat", Some(&args), 1, ""),
            }
        })
    }

    /// The lists' age is the newer of the stamp's and the lists', whichever
    /// is readable; `pkgcache.bin` answers only when neither is, and the age
    /// is unknown only when nothing can be read.
    #[test]
    fn cache_age_is_the_newest_readable_source() {
        const DAY: u64 = 86_400;
        let hour = Duration::from_secs(3_600);
        let staleness_of = |ago| staleness(&sys(&Arc::new(ages(Fake::new(), ago))), hour).unwrap();

        // A refresh that changed no index: the stamp is fresh, the lists are
        // as old as the last refresh that did.
        assert_eq!(staleness_of([Some(60), Some(2 * DAY), Some(2 * DAY)]), None);
        // An old stamp next to fresh lists cannot make them look stale.
        assert_eq!(staleness_of([Some(2 * DAY), Some(60), None]), None);
        // An install rewrote pkgcache.bin a minute ago without refreshing:
        // it does not make two-day-old lists look fresh.
        let Some(Stale::Older { age, .. }) = staleness_of([None, Some(2 * DAY), Some(60)]) else {
            panic!("pkgcache.bin outweighed the lists")
        };
        assert!(age.as_secs().abs_diff(2 * DAY) <= 2, "{age:?}");
        // All old: the age is the newer of the two, to the second.
        let Some(Stale::Older { age, .. }) = staleness_of([Some(3 * DAY), Some(2 * DAY), Some(60)])
        else {
            panic!("expected stale")
        };
        assert!(age.as_secs().abs_diff(2 * DAY) <= 2, "{age:?}");
        // Neither readable: pkgcache.bin is the fallback.
        assert_eq!(staleness_of([None, None, Some(60)]), None);
        let Some(Stale::Older { .. }) = staleness_of([None, None, Some(2 * DAY)]) else {
            panic!("expected stale from the fallback")
        };
        // Nothing readable.
        assert_eq!(staleness_of([None, None, None]), Some(Stale::AgeUnknown));
    }

    /// `pkgcache.bin` is not even asked about while a source answers.
    #[test]
    fn cache_age_stats_the_fallback_only_when_no_source_answers() {
        let fake = Arc::new(ages(Fake::new(), [None, Some(60), Some(60)]));
        cache_age(&sys(&fake)).unwrap();
        let argvs = fake.argvs();
        assert!(
            argvs.iter().all(|a| !a.contains(&AGE_FALLBACK.to_string())),
            "{argvs:?}"
        );
    }

    /// The same, end to end: the stamp a refresh wrote keeps `Present` from
    /// refreshing again although the lists directory is two days old, and
    /// with nothing readable it refreshes to be safe.
    #[test]
    fn update_cache_trusts_a_fresh_stamp_over_old_lists() {
        let fake = Arc::new(ages(present_missing_mc(), [Some(60), Some(172_800), None]));
        apply_present(
            &fake,
            Present::new(["mc"]).update_cache(Duration::from_secs(3600)),
        );
        let argvs = fake.argvs();
        assert!(
            argv_starting(&argvs, &["apt-get", "update"]).is_none(),
            "{argvs:?}"
        );

        let fake = Arc::new(ages(present_missing_mc(), [None, None, None]));
        apply_present(
            &fake,
            Present::new(["mc"]).update_cache(Duration::from_secs(3600)),
        );
        assert!(argv_starting(&fake.argvs(), &["apt-get", "update"]).is_some());
    }

    /// The stamp says an `apt-get update` succeeded, so a failed one writes
    /// none, and `apply` stops there without installing.
    #[test]
    fn a_failed_refresh_writes_no_stamp() {
        let fake = Arc::new(stamp_tools(
            Fake::new()
                .with_cmd("dpkg-query", None, 1, "")
                .with_cmd("apt-get", Some(&["update"]), 100, "")
                .with_cmd("apt-get", None, 0, ""),
        ));
        let s = sys(&fake);
        let op = Present::new(["mc"]).update_cache(Duration::ZERO);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        let err = op.apply(&s, c).unwrap_err().chain();
        assert!(err.contains("apt-get update"), "{err}");
        // Nothing follows the failed update: no stamp, no install.
        assert_eq!(from_update(&fake).len(), 1, "{:?}", fake.argvs());

        // `Latest` refreshes in `check`, and fails there the same way. The
        // failing update is canned first, because the first match answers.
        let fake = stamp_tools(Fake::new().with_cmd("apt-get", Some(&["update"]), 100, ""));
        let fake = dpkg(fake, "openssl", 0, "install ok installed\t3.0.15-1\n");
        let fake = Arc::new(policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1"));
        let err = Latest::new(["openssl"])
            .update_cache(Duration::ZERO)
            .check(&sys(&fake))
            .unwrap_err()
            .chain();
        assert!(err.contains("apt-get update"), "{err}");
        assert_eq!(from_update(&fake).len(), 1, "{:?}", fake.argvs());
    }

    /// Through a real step, not `check` called by hand: `Latest` refreshes in
    /// `check`, where the SDK refuses a write through `sys`, so the stamp
    /// must be written in a way a check-phase refresh may use. Calling
    /// `check` directly would not enter that phase and could not fail.
    #[test]
    fn latest_stamps_a_check_time_refresh_in_a_real_step() {
        let fake = Arc::new(openssl_outdated_curl_missing());
        let mut ctx = Ctx::new(sys(&fake), rustible_sdk::HostInfo::local());
        let r = ctx
            .step("up", Latest::new(["openssl"]).update_cache(Duration::ZERO))
            .unwrap();
        assert!(r.changed);
        assert_eq!(
            from_update(&fake)[1..3],
            stamp_argvs(),
            "{:?}",
            fake.argvs()
        );
    }

    /// A refresh records itself right after the update, with `mkdir -p`
    /// for the directory and `touch` for the stamp, and nothing in between.
    #[test]
    fn a_refresh_runs_update_then_mkdir_then_touch() {
        let fake = Arc::new(present_missing_mc());
        apply_present(&fake, Present::new(["mc"]).update_cache(Duration::ZERO));
        let after = from_update(&fake);
        assert_eq!(after[1..3], stamp_argvs(), "{after:?}");
    }

    /// A stamp that cannot be written warns, saying the next run may
    /// refresh again, and the step goes on: the lists were refreshed.
    #[test]
    fn a_stamp_that_cannot_be_written_warns_and_installs() {
        let fake = Arc::new(
            Fake::new()
                .with_cmd("dpkg-query", None, 1, "")
                .with_cmd("apt-get", None, 0, "")
                .with_cmd("mkdir", Some(&["-p", PERIODIC_DIR]), 0, "")
                .with_cmd("touch", Some(&[SUCCESS_STAMP]), 1, ""),
        );
        let sink = Arc::new(Collect::default());
        let s = System::fake(fake.clone(), sink.clone());
        let op = Present::new(["mc"]).update_cache(Duration::ZERO);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        op.apply(&s, c).unwrap();
        let warned = warnings(&sink);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].contains(SUCCESS_STAMP)
                && warned[0].contains("may refresh again")
                && warned[0].contains("touch"),
            "{warned:?}"
        );
        let after = from_update(&fake);
        assert_eq!(after[1..3], stamp_argvs(), "{after:?}");
        assert_eq!(after[3][..2], ["apt-get", "install"], "{after:?}");
    }

    /// What `apt-get update` prints, under `LANG=C`, when it cannot reach a
    /// mirror and still exits 0. Real apt prints the `W:` lines on stderr,
    /// and the `Fake` cannot plant stderr, so this exercises the stdout half
    /// only; `it_apt_latest.rs`'s `an_update_that_fetched_nothing_writes_no_stamp`
    /// (T2) is what pins the stderr path.
    const OFFLINE_UPDATE: &str = "\
Ign:1 http://127.0.0.1:9/debian bookworm InRelease
Err:1 http://127.0.0.1:9/debian bookworm InRelease
  Could not connect to 127.0.0.1:9 (127.0.0.1). - connect (111: Connection refused)
Reading package lists...
W: Failed to fetch http://127.0.0.1:9/debian/dists/bookworm/InRelease  Could not connect to 127.0.0.1:9 (127.0.0.1). - connect (111: Connection refused)
W: Some index files failed to download. They have been ignored, or old ones used instead.
";

    #[test]
    fn failed_fetch_finds_either_line() {
        assert_eq!(
            failed_fetch(OFFLINE_UPDATE),
            Some(
                "W: Failed to fetch http://127.0.0.1:9/debian/dists/bookworm/InRelease  \
                 Could not connect to 127.0.0.1:9 (127.0.0.1). - connect (111: Connection refused)"
            )
        );
        assert!(
            failed_fetch(
                "W: Some index files failed to download. They have been ignored, or old ones used instead.\n"
            )
            .is_some()
        );
        let clean =
            "Hit:1 http://deb.debian.org/debian bookworm InRelease\nReading package lists...\n";
        assert_eq!(failed_fetch(clean), None);
        assert_eq!(failed_fetch(""), None);
    }

    /// An update that exited 0 without fetching every index is not a
    /// refresh to record: no stamp, and a warning that names the line. The
    /// step goes on, for `Present` in `apply` and `Latest` in `check`.
    #[test]
    fn a_partial_refresh_warns_and_writes_no_stamp() {
        let fake = Arc::new(stamp_tools(
            Fake::new()
                .with_cmd("dpkg-query", None, 1, "")
                .with_cmd("apt-get", Some(&["update"]), 0, OFFLINE_UPDATE)
                .with_cmd("apt-get", None, 0, ""),
        ));
        let sink = Arc::new(Collect::default());
        let s = System::fake(fake.clone(), sink.clone());
        let op = Present::new(["mc"]).update_cache(Duration::ZERO);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        op.apply(&s, c).unwrap();
        assert!(stamp_writes(&fake).is_empty(), "{:?}", fake.argvs());
        let warned = warnings(&sink);
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].contains("W: Failed to fetch")
                && warned[0].contains("was not written")
                && warned[0].contains("may refresh again")
                && warned[0].contains("/etc/apt/sources.list.d/")
                && warned[0].contains("NO_PUBKEY"),
            "{warned:?}"
        );

        let fake =
            stamp_tools(Fake::new().with_cmd("apt-get", Some(&["update"]), 0, OFFLINE_UPDATE));
        let fake = dpkg(fake, "openssl", 0, "install ok installed\t3.0.15-1\n");
        let fake = Arc::new(
            policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1").with_cmd("apt-get", None, 0, ""),
        );
        let sink = Arc::new(Collect::default());
        let mut ctx = Ctx::new(
            System::fake(fake.clone(), sink.clone()),
            rustible_sdk::HostInfo::local(),
        );
        let r = ctx
            .step("up", Latest::new(["openssl"]).update_cache(Duration::ZERO))
            .unwrap();
        assert!(r.changed);
        assert!(stamp_writes(&fake).is_empty(), "{:?}", fake.argvs());
        assert_eq!(warnings(&sink).len(), 1, "{:?}", sink.events());
    }

    #[test]
    fn present_refuses_without_root() {
        let fake = Arc::new(Fake::new());
        let err = Present::new(["mc"])
            .check(&non_root(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("needs root") && err.contains("cadu"), "{err}");
        assert!(fake.argvs().is_empty());
    }

    // ---- Absent ----

    #[test]
    fn absent_satisfied_when_not_installed() {
        let fake = Arc::new(dpkg(Fake::new(), "apache2", 1, ""));
        let Plan::Satisfied(r) = Absent::new(["apache2"]).check(&sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.not_present, vec!["apache2"]);
        assert!(r.removed.is_empty());
    }

    #[test]
    fn absent_change_and_apply_runs_apt_get_remove() {
        let fake = Arc::new(
            dpkg(
                dpkg(
                    Fake::new(),
                    "apache2",
                    0,
                    "install ok installed\t2.4.62-1\n",
                ),
                "sendmail",
                1,
                "",
            )
            .with_cmd("apt-get", None, 0, ""),
        );
        let s = sys(&fake);
        let op = Absent::new(["apache2", "sendmail"]);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff().short(), "apache2=removed");

        // The version reported is the one `check` read from dpkg, carried in the intent.
        let r = op.apply(&s, c).unwrap();
        assert_eq!(r.removed[0].name, "apache2");
        assert_eq!(r.removed[0].version, "2.4.62-1");
        assert_eq!(r.not_present, vec!["sendmail"]);
        let argvs = fake.argvs();
        assert_eq!(
            argv_starting(&argvs, &["apt-get"]).unwrap(),
            &["apt-get", "remove", "-y", "apache2"]
        );
        assert!(argv_starting(&argvs, &["apt-get", "autoremove"]).is_none());
    }

    #[test]
    fn absent_purge_uses_apt_get_purge_and_autoremove_purge() {
        let fake = Arc::new(
            dpkg(
                Fake::new(),
                "apache2",
                0,
                "install ok installed\t2.4.62-1\n",
            )
            .with_cmd("apt-get", None, 0, ""),
        );
        let s = sys(&fake);
        let op = Absent::new(["apache2"]).purge(true).autoremove(true);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff().short(), "apache2=purged");
        op.apply(&s, c).unwrap();
        let apt: Vec<_> = fake
            .argvs()
            .into_iter()
            .filter(|a| a[0] == "apt-get")
            .collect();
        assert_eq!(apt[0], ["apt-get", "purge", "-y", "apache2"]);
        assert_eq!(apt[1], ["apt-get", "autoremove", "-y", "--purge"]);
        assert_eq!(apt.len(), 2);
    }

    #[test]
    fn absent_autoremove_without_purge() {
        let fake = Arc::new(
            dpkg(
                Fake::new(),
                "apache2",
                0,
                "install ok installed\t2.4.62-1\n",
            )
            .with_cmd("apt-get", None, 0, ""),
        );
        let s = sys(&fake);
        let op = Absent::new(["apache2"]).autoremove(true);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!()
        };
        op.apply(&s, c).unwrap();
        let apt: Vec<_> = fake
            .argvs()
            .into_iter()
            .filter(|a| a[0] == "apt-get")
            .collect();
        assert_eq!(apt[1], ["apt-get", "autoremove", "-y"]);
    }

    #[test]
    fn absent_config_files_count_as_present_only_when_purging() {
        let line = "deinstall ok config-files\t2.4.62-1\n";
        let fake = Arc::new(dpkg(Fake::new(), "apache2", 0, line));
        assert!(matches!(
            Absent::new(["apache2"]).check(&sys(&fake)).unwrap(),
            Plan::Satisfied(_)
        ));
        let Plan::Change(c) = Absent::new(["apache2"])
            .purge(true)
            .check(&sys(&fake))
            .unwrap()
        else {
            panic!("purge must see config-files as present")
        };
        assert_eq!(
            c.diff().render(),
            "apt packages:\n  apache2: config-files -> purged\n"
        );
    }

    #[test]
    fn absent_refuses_half_configured_package() {
        let fake = Arc::new(dpkg(
            Fake::new(),
            "apache2",
            0,
            "install ok half-configured\t2.4.62-1\n",
        ));
        let err = Absent::new(["apache2"])
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("half-configured"), "{err}");
    }

    #[test]
    fn absent_refuses_wrong_pm_and_non_root() {
        let fake = Arc::new(Fake::new());
        let err = Absent::new(["x"])
            .check(&alpine(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("apt::Absent needs apt"), "{err}");
        let err = Absent::new(["x"])
            .check(&non_root(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("apt::Absent needs root"), "{err}");
        assert!(fake.argvs().is_empty(), "refusal must not run commands");
    }

    #[test]
    fn absent_check_mode_runs_only_dpkg_query_and_has_no_output() {
        let fake = Arc::new(
            dpkg(
                Fake::new(),
                "apache2",
                0,
                "install ok installed\t2.4.62-1\n",
            )
            .with_cmd("apt-get", None, 0, ""),
        );
        let mut ctx = check_mode_ctx(&fake);
        let r = ctx
            .step("rm", Absent::new(["apache2"]).autoremove(true))
            .unwrap();
        assert!(r.changed && !r.is_available());
        let argvs = fake.argvs();
        assert!(argvs.iter().all(|a| a[0] == "dpkg-query"), "{argvs:?}");
    }

    // ---- Latest ----

    #[test]
    fn latest_satisfied_when_at_candidate() {
        let fake = dpkg(
            Fake::new(),
            "openssl",
            0,
            "install ok installed\t3.0.16-1\n",
        );
        let fake = Arc::new(policy_of(fake, "openssl", "3.0.16-1", "3.0.16-1"));
        let Plan::Satisfied(r) = Latest::new(["openssl"]).check(&sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.current[0].version, "3.0.16-1");
        assert!(r.upgraded.is_empty() && r.installed.is_empty());
    }

    #[test]
    fn latest_upgrades_with_only_upgrade() {
        let fake = dpkg(
            Fake::new(),
            "openssl",
            0,
            "install ok installed\t3.0.15-1\n",
        );
        let fake = Arc::new(
            policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1").with_cmd("apt-get", None, 0, ""),
        );
        let s = sys(&fake);
        let op = Latest::new(["openssl"]);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(
            c.diff().render(),
            "apt packages:\n  openssl: 3.0.15-1 -> 3.0.16-1\n"
        );
        let r = op.apply(&s, c).unwrap();
        // The version left behind comes from the intent; the one reported is
        // what dpkg says now, and the fake still answers 3.0.15-1.
        assert_eq!(r.upgraded[0].0.name, "openssl");
        assert_eq!(r.upgraded[0].0.version, "3.0.15-1");
        assert_eq!(r.upgraded[0].1, "3.0.15-1");
        assert!(r.installed.is_empty() && r.current.is_empty());
        let apt: Vec<_> = fake
            .argvs()
            .into_iter()
            .filter(|a| a[0] == "apt-get")
            .collect();
        assert_eq!(
            apt,
            vec![["apt-get", "install", "-y", "--only-upgrade", "openssl"]]
        );
    }

    #[test]
    fn latest_installs_missing_packages() {
        let fake = dpkg(Fake::new(), "sl", 1, "");
        let fake =
            Arc::new(policy_of(fake, "sl", "(none)", "5.02-1").with_cmd("apt-get", None, 0, ""));
        let s = sys(&fake);
        let op = Latest::new(["sl"]);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff().short(), "sl=5.02-1");
        let r = op.apply(&s, c).unwrap();
        // Installed per the intent; the fake dpkg never learns of it, so the
        // version read back is empty rather than the candidate.
        assert_eq!(r.installed[0].name, "sl");
        assert_eq!(r.installed[0].version, "");
        assert!(r.upgraded.is_empty() && r.current.is_empty());
        let apt: Vec<_> = fake
            .argvs()
            .into_iter()
            .filter(|a| a[0] == "apt-get")
            .collect();
        assert_eq!(
            apt,
            vec![["apt-get", "install", "-y", "--no-install-recommends", "sl"]]
        );
    }

    #[test]
    fn latest_mixed_install_upgrade_current() {
        let fake = dpkg(Fake::new(), "sl", 1, "");
        let fake = dpkg(fake, "openssl", 0, "install ok installed\t3.0.15-1\n");
        let fake = dpkg(fake, "curl", 0, "install ok installed\t8.0-1\n");
        let fake = policy_of(fake, "sl", "(none)", "5.02-1");
        let fake = policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1");
        let fake = policy_of(fake, "curl", "8.0-1", "8.0-1");
        let fake = Arc::new(fake.with_cmd("apt-get", None, 0, ""));
        let s = sys(&fake);
        let op = Latest::new(["sl", "openssl", "curl"]).install_recommends(true);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        let r = op.apply(&s, c).unwrap();
        assert_eq!(r.installed.len(), 1);
        assert_eq!(r.installed[0].name, "sl");
        assert_eq!(r.upgraded.len(), 1);
        assert_eq!(r.upgraded[0].0.name, "openssl");
        assert_eq!(r.upgraded[0].1, "3.0.15-1");
        assert_eq!(r.current[0].name, "curl");
        assert_eq!(r.current[0].version, "8.0-1");
        let apt: Vec<_> = fake
            .argvs()
            .into_iter()
            .filter(|a| a[0] == "apt-get")
            .collect();
        assert_eq!(apt[0], ["apt-get", "install", "-y", "sl"]);
        assert_eq!(
            apt[1],
            ["apt-get", "install", "-y", "--only-upgrade", "openssl"]
        );
        assert_eq!(apt.len(), 2);
    }

    #[test]
    fn latest_refuses_unknown_package() {
        let fake = dpkg(Fake::new(), "nope", 1, "");
        let fake = Arc::new(fake.with_cmd("apt-cache", Some(&["policy", "nope"]), 0, ""));
        let err = Latest::new(["nope"])
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no candidate version"), "{err}");
    }

    #[test]
    fn latest_refuses_wrong_pm_and_non_root() {
        let fake = Arc::new(Fake::new());
        let err = Latest::new(["x"])
            .check(&alpine(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("apt::Latest needs apt"), "{err}");
        let err = Latest::new(["x"])
            .check(&non_root(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("apt::Latest needs root"), "{err}");
        assert!(fake.argvs().is_empty());
    }

    /// The refresh happens in `check`, before `apt-cache policy` is asked for
    /// a candidate, and `apply` does not repeat it.
    #[test]
    fn latest_update_cache_runs_in_check_before_reading_candidates() {
        let fake = dpkg(Fake::new(), "sl", 1, "");
        let fake = Arc::new(
            stamp_tools(policy_of(fake, "sl", "(none)", "5.02-1"))
                .with_cmd("stat", None, 0, "0")
                .with_cmd("apt-get", None, 0, ""),
        );
        let s = sys(&fake);
        let op = Latest::new(["sl"]).update_cache(Duration::from_secs(3600));
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!()
        };
        let after_check = fake.argvs();
        let update = after_check
            .iter()
            .position(|a| a.starts_with(&["apt-get".into(), "update".into()]))
            .expect("check ran apt-get update");
        let policy = after_check
            .iter()
            .position(|a| a[0] == "apt-cache")
            .expect("check read the candidate");
        assert!(update < policy, "{after_check:?}");
        assert_eq!(
            after_check[update + 1..update + 3],
            stamp_argvs(),
            "a real run's check records the refresh: {after_check:?}"
        );

        op.apply(&s, c).unwrap();
        let apt: Vec<_> = fake
            .argvs()
            .into_iter()
            .filter(|a| a[0] == "apt-get")
            .collect();
        assert_eq!(apt.len(), 2, "apply does not update again: {apt:?}");
        assert_eq!(apt[1][..3], ["apt-get", "install", "-y"]);
    }

    /// Fresh lists: no `apt-get` at all, in check or apply.
    #[test]
    fn latest_update_cache_skips_a_fresh_cache() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let fake = dpkg(
            Fake::new(),
            "openssl",
            0,
            "install ok installed\t3.0.16-1\n",
        );
        let fake = Arc::new(policy_of(fake, "openssl", "3.0.16-1", "3.0.16-1").with_cmd(
            "stat",
            None,
            0,
            &now.to_string(),
        ));
        let op = Latest::new(["openssl"]).update_cache(Duration::from_secs(3600));
        let Plan::Satisfied(_) = op.check(&sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        let argvs = fake.argvs();
        assert!(argvs.iter().all(|a| a[0] != "apt-get"), "{argvs:?}");
    }

    /// Without `.update_cache()`, `check` stays read-only in check mode.
    #[test]
    fn latest_check_mode_runs_only_read_only_commands_without_update_cache() {
        let fake = dpkg(
            Fake::new(),
            "openssl",
            0,
            "install ok installed\t3.0.15-1\n",
        );
        let fake = Arc::new(
            policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1").with_cmd("apt-get", None, 0, ""),
        );
        let mut ctx = check_mode_ctx(&fake);
        let r = ctx.step("up", Latest::new(["openssl"])).unwrap();
        assert!(r.changed && !r.is_available());
        assert_eq!(
            r.diff.as_ref().unwrap().render(),
            "apt packages:\n  openssl: 3.0.15-1 -> 3.0.16-1\n"
        );
        let argvs = fake.argvs();
        assert!(
            argvs
                .iter()
                .all(|a| a[0] == "dpkg-query" || a[0] == "apt-cache"),
            "{argvs:?}"
        );
    }

    fn warnings(sink: &Collect) -> Vec<String> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                rustible_sdk::event::Event::Log {
                    level: rustible_sdk::event::Level::Warn,
                    msg,
                } => Some(msg),
                _ => None,
            })
            .collect()
    }

    /// Every tool an outdated `openssl` and a missing `curl` would need, so
    /// that what the op leaves unasked is its own choice and not the Fake's.
    fn openssl_outdated_curl_missing() -> Fake {
        let fake = dpkg(
            Fake::new(),
            "openssl",
            0,
            "install ok installed\t3.0.15-1\n",
        );
        let fake = dpkg(fake, "curl", 1, "");
        let fake = policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1");
        stamp_tools(policy_of(fake, "curl", "(none)", "7.88.1-10").with_cmd("apt-get", None, 0, ""))
    }

    /// Under `--check`, stale lists are not refreshed (vision 12: a dry run
    /// contacts no mirror), and the candidates are not read from them
    /// either. The step cannot decide, says why, and has no output.
    #[test]
    fn latest_check_mode_does_not_refresh_stale_lists_and_cannot_decide() {
        let fake = Arc::new(openssl_outdated_curl_missing());
        let sink = Arc::new(Collect::default());
        let s = System::fake(fake.clone(), sink.clone()).with_check_mode(true);
        let mut ctx = Ctx::new(s, rustible_sdk::HostInfo::local());
        let r = ctx
            .step(
                "up",
                Latest::new(["openssl", "curl"]).update_cache(Duration::ZERO),
            )
            .unwrap();
        assert!(r.changed && !r.is_available());
        assert_eq!(
            r.diff.as_ref().unwrap().render(),
            "apt packages openssl, curl: candidate versions unknown; \
             `.update_cache(Duration::ZERO)` refreshes the package lists on every run, \
             and the lists are not refreshed under --check"
        );
        let argvs = fake.argvs();
        assert!(
            argvs
                .iter()
                .all(|a| a[0] != "apt-get" && a[0] != "apt-cache"),
            "no refresh, and no candidate read from the stale lists: {argvs:?}"
        );
        assert!(stamp_writes(&fake).is_empty(), "a dry run writes no stamp");
        // Nothing was rewritten, so there is nothing to warn about.
        assert!(warnings(&sink).is_empty(), "{:?}", sink.events());
    }

    /// The other two ways the lists are stale, by age and of unknown age,
    /// each named in the diff, and neither refreshed under `--check`.
    #[test]
    fn latest_check_mode_names_why_the_lists_are_stale() {
        let old = (now_secs() - 7_260).to_string();
        let fake = Arc::new(openssl_outdated_curl_missing().with_cmd("stat", None, 0, &old));
        let s = sys(&fake).with_check_mode(true);
        let op = Latest::new(["openssl"]).update_cache(Duration::from_secs(3_600));
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        let rendered = c.diff().render();
        assert!(
            rendered.starts_with("apt packages openssl: candidate versions unknown; "),
            "{rendered}"
        );
        // Shown to the minute, so the second or two the test takes to get
        // here does not move it.
        assert!(
            rendered.contains(
                "the package lists are 2h 1m old, older than the \
                 `.update_cache()` age of 1h, and the lists are not refreshed under --check"
            ),
            "{rendered}"
        );
        let argvs = fake.argvs();
        assert!(argvs.iter().all(|a| a[0] == "stat"), "{argvs:?}");

        let fake = Arc::new(openssl_outdated_curl_missing().with_cmd("stat", None, 1, ""));
        let s = sys(&fake).with_check_mode(true);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert!(
            c.diff()
                .render()
                .contains("the age of the package lists cannot be read"),
            "{}",
            c.diff().render()
        );
        let argvs = fake.argvs();
        assert!(argvs.iter().all(|a| a[0] == "stat"), "{argvs:?}");
    }

    /// Lists within the max age need no refresh in a real run either, so the
    /// dry run plans from them, and its `ok` is the real run's `ok`.
    #[test]
    fn latest_check_mode_plans_normally_from_fresh_lists() {
        let fresh = (now_secs() - 60).to_string();
        let fake = Arc::new(
            openssl_outdated_curl_missing()
                .with_cmd("stat", None, 0, &fresh)
                .with_cmd(
                    "dpkg-query",
                    Some(&[DPKG_ARGS[0], DPKG_ARGS[1], "zlib1g"]),
                    0,
                    "install ok installed\t1:1.2.13\n",
                )
                .with_cmd(
                    "apt-cache",
                    Some(&["policy", "zlib1g"]),
                    0,
                    "zlib1g:\n  Installed: 1:1.2.13\n  Candidate: 1:1.2.13\n",
                ),
        );
        let s = sys(&fake).with_check_mode(true);
        let hour = Duration::from_secs(3_600);

        // Up to date: satisfied, with the version as output.
        let Plan::Satisfied(r) = Latest::new(["zlib1g"])
            .update_cache(hour)
            .check(&s)
            .unwrap()
        else {
            panic!("expected satisfied")
        };
        assert_eq!(r.current[0].version, "1:1.2.13");

        // Outdated and missing: the usual attribute diff, candidates and all.
        let Plan::Change(c) = Latest::new(["openssl", "curl"])
            .update_cache(hour)
            .check(&s)
            .unwrap()
        else {
            panic!("expected change")
        };
        assert_eq!(
            c.diff().render(),
            "apt packages:\n  openssl: 3.0.15-1 -> 3.0.16-1\n  curl: absent -> 7.88.1-10\n"
        );

        let argvs = fake.argvs();
        assert!(
            argv_starting(&argvs, &["apt-cache", "policy"]).is_some(),
            "{argvs:?}"
        );
        assert!(argvs.iter().all(|a| a[0] != "apt-get"), "{argvs:?}");
    }

    /// The plan a dry run makes from stale lists names no candidate, so it
    /// cannot be executed. `Ctx::step` never hands it to `apply` (it is only
    /// made under `--check`); called directly, `apply` refuses and runs
    /// nothing.
    #[test]
    fn latest_unrefreshed_plan_cannot_be_applied() {
        let fake = Arc::new(openssl_outdated_curl_missing());
        let op = Latest::new(["openssl"]).update_cache(Duration::ZERO);
        let Plan::Change(c) = op.check(&sys(&fake).with_check_mode(true)).unwrap() else {
            panic!("expected change")
        };
        let err = op.apply(&sys(&fake), c).unwrap_err().chain();
        assert!(
            err.contains("cannot apply a plan made under --check"),
            "{err}"
        );
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    /// The refusals about the machine stand under `--check` too, and come
    /// before the stale-lists answer: a dry run on a box that is not root,
    /// or has no apt, is refused rather than told the candidates are
    /// unknown, and runs nothing.
    #[test]
    fn latest_refusals_stand_under_check_before_the_stale_lists_answer() {
        let op = Latest::new(["x"]).update_cache(Duration::ZERO);

        let fake = Arc::new(Fake::new().with_cmd("apt-get", None, 0, ""));
        let err = op
            .check(&non_root(&fake).with_check_mode(true))
            .unwrap_err()
            .chain();
        assert!(err.contains("apt::Latest needs root"), "{err}");
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());

        let fake = Arc::new(Fake::new().with_cmd("apt-get", None, 0, ""));
        let err = op
            .check(&alpine(&fake).with_check_mode(true))
            .unwrap_err()
            .chain();
        assert!(err.contains("apt::Latest needs apt"), "{err}");
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    /// No names, nothing to be unsure about: `ok` in both modes, and the dry
    /// run still refreshes nothing.
    #[test]
    fn latest_with_no_names_is_satisfied_under_check_with_stale_lists() {
        let fake = Arc::new(Fake::new().with_cmd("apt-get", None, 0, ""));
        let op = Latest::new(Vec::<String>::new()).update_cache(Duration::ZERO);
        let s = sys(&fake).with_check_mode(true);
        assert!(matches!(op.check(&s).unwrap(), Plan::Satisfied(_)));
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    /// Outside check mode the same refresh is unremarkable: debug, not warn.
    #[test]
    fn latest_cache_refresh_outside_check_mode_does_not_warn() {
        let fake = dpkg(
            Fake::new(),
            "openssl",
            0,
            "install ok installed\t3.0.15-1\n",
        );
        let fake = Arc::new(stamp_tools(
            policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1").with_cmd("apt-get", None, 0, ""),
        ));
        let sink = Arc::new(Collect::default());
        let s = System::fake(fake.clone(), sink.clone());
        Latest::new(["openssl"])
            .update_cache(Duration::ZERO)
            .check(&s)
            .unwrap();
        let warned = sink.events().iter().any(|e| {
            matches!(
                e,
                rustible_sdk::event::Event::Log {
                    level: rustible_sdk::event::Level::Warn,
                    ..
                }
            )
        });
        assert!(!warned, "{:?}", sink.events());
    }
}
