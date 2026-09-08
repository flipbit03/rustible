//! Debian/Ubuntu packages via apt. Ansible's `ansible.builtin.apt`, one op
//! per `state` (vision 6.3): [`Present`] (`state=present`), [`Absent`]
//! (`state=absent`, with `purge` and `autoremove`) and [`Latest`]
//! (`state=latest`).
//!
//! Every op refuses on a host whose package manager is not apt and when not
//! running as root. `check` reads with `dpkg-query`, `apt-cache policy` and
//! `stat`, and runs no `apt-get` at all unless `.update_cache()` asked for a
//! refresh on [`Latest`] (see below).
//!
//! **Cache refresh.** [`Present`] and [`Latest`] take `.update_cache(max_age)`:
//! `apt-get update` runs when the lists in `/var/lib/apt/lists` (or, failing
//! that, `/var/cache/apt/pkgcache.bin`) are older than `max_age`;
//! `Duration::ZERO` means always. *Where* it runs differs, because the two ops
//! need the lists at different moments:
//!
//! - [`Present`] only asks whether a package is installed, which dpkg answers
//!   without the lists. It refreshes in `apply`, right before installing, and
//!   never in `check`.
//! - [`Latest`] compares installed versions against the *candidate* versions
//!   the lists carry, so stale lists give a wrong answer. It refreshes in
//!   `check`, before reading the candidates, exactly as Ansible's
//!   `apt: state=latest` does.
//!
//! **A `Latest` dry run with `.update_cache()` therefore writes
//! `/var/lib/apt/lists` on the target.** This is the one place in this module
//! where `--check` is not read-only: `apt-get update` is a command, not a
//! mutation through `sys`, so the check-mode guard does not stop it, and
//! without it a dry run against month-old lists reports every package current
//! and is simply wrong. The step logs a warning saying so whenever it happens
//! in check mode. Leave `.update_cache()` off if a dry run must touch nothing;
//! the plan is then computed against whatever the lists already say.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rustible_sdk::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    /// Empty when predicted in check mode and apt has not resolved it yet
    /// (`Present`); `Absent` and `Latest` always know the version.
    pub version: String,
}

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

/// The candidate version apt would install, or `None` when the cache cannot
/// answer: an unknown name, lists that were never fetched, or an `apt-cache`
/// that will not run. Used only to decide whether [`Present`] can predict a
/// version honestly, so a failure here is not an error.
fn candidate_version(sys: &System, name: &str) -> Option<String> {
    match sys
        .cmd("apt-cache")
        .args(["policy", name])
        .allow_failure()
        .run()
    {
        Ok(out) if out.success() => parse_policy(&out.stdout_str()).candidate,
        _ => None,
    }
}

/// The package names an attribute diff plans to act on, in order.
fn planned_names(diff: &Diff) -> Vec<String> {
    match diff {
        Diff::Attrs { changes, .. } => changes.iter().map(|c| c.name.clone()).collect(),
        _ => vec![],
    }
}

/// Candidate and installed versions from `apt-cache policy`.
fn policy(sys: &System, name: &str) -> Result<Policy> {
    let out = sys.cmd("apt-cache").args(["policy", name]).run()?;
    Ok(parse_policy(&out.stdout_str()))
}

/// Age of the apt lists: mtime of `/var/lib/apt/lists`, else of
/// `/var/cache/apt/pkgcache.bin`, via `stat -c %Y` (the SDK's `Stat` carries
/// no mtime). `None` when neither can be read.
fn cache_age(sys: &System) -> Result<Option<Duration>> {
    for path in ["/var/lib/apt/lists", "/var/cache/apt/pkgcache.bin"] {
        let Some(out) = sys.cmd("stat").args(["-c", "%Y", path]).ok()? else {
            continue;
        };
        if let Some(mtime) = parse_stat_mtime(&out.stdout_str()) {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            return Ok(Some(Duration::from_secs(now.saturating_sub(mtime))));
        }
    }
    Ok(None)
}

/// Run `apt-get update` when the lists are older than `max_age` (or their age
/// is unknown). `Duration::ZERO` always updates. Returns whether it ran.
fn update_cache_if_stale(sys: &System, max_age: Duration) -> Result<bool> {
    let stale = max_age.is_zero() || cache_age(sys)?.is_none_or(|age| age > max_age);
    if !stale {
        return Ok(false);
    }
    sys.cmd("apt-get")
        .arg("update")
        .env("DEBIAN_FRONTEND", "noninteractive")
        .run()?;
    Ok(true)
}

/// Refuse early on a non-apt host or without root (vision 6.8).
fn require_apt_root(sys: &System, op: &str) -> Result<()> {
    if sys.facts().package_manager != Pm::Apt {
        bail!(
            "apt::{op} needs apt, but this host uses {:?} ({:?})",
            sys.facts().package_manager,
            sys.facts().distro
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

    pub fn install_recommends(mut self, on: bool) -> Self {
        self.install_recommends = on;
        self
    }
}

impl Op for Present {
    type Output = InstallReport;

    fn check(&self, sys: &System) -> Result<Plan<InstallReport>> {
        require_apt_root(sys, "Present")?;
        let mut report = InstallReport::default();
        let mut changes = vec![];
        // A version we would install is only known when the lists can name a
        // candidate; without one, the output is not predicted at all rather
        // than predicted with an empty version (vision 12).
        let mut versions_known = true;
        for name in &self.names {
            match installed_version(sys, name)? {
                Some(version) => report.already_present.push(Package {
                    name: name.clone(),
                    version,
                }),
                None => {
                    changes.push(AttrChange {
                        name: name.clone(),
                        from: "absent".into(),
                        to: "installed".into(),
                    });
                    let candidate = candidate_version(sys, name);
                    versions_known &= candidate.is_some();
                    report.installed.push(Package {
                        name: name.clone(),
                        version: candidate.unwrap_or_default(),
                    });
                }
            }
        }
        if changes.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        let diff = Diff::Attrs {
            subject: "apt packages".into(),
            changes,
        };
        if versions_known {
            Ok(Plan::change_predicting(diff, report))
        } else {
            Ok(Plan::change(diff))
        }
    }

    fn apply(&self, sys: &System, change: Change<InstallReport>) -> Result<InstallReport> {
        // Install what `check` planned, not what dpkg says now (vision 6.2).
        let missing = planned_names(&change.diff);
        ensure!(
            !missing.is_empty(),
            "apt::Present::apply: the plan names no package to install"
        );
        if let Some(max_age) = self.update_cache {
            let _refreshed = update_cache_if_stale(sys, max_age)?;
        }
        let mut cmd = apt_get(sys).args(["install", "-y"]);
        if !self.install_recommends {
            cmd = cmd.arg("--no-install-recommends");
        }
        cmd.args(missing.iter().cloned()).run()?;

        // Report the versions dpkg actually has now, whatever check predicted.
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

impl Op for Absent {
    type Output = RemoveReport;

    fn check(&self, sys: &System) -> Result<Plan<RemoveReport>> {
        require_apt_root(sys, "Absent")?;
        let verb = if self.purge { "purged" } else { "removed" };
        let mut report = RemoveReport::default();
        let mut changes = vec![];
        for name in &self.names {
            let present = match dpkg_status(sys, name)? {
                DpkgStatus::Installed(v) => Some((v, "installed")),
                DpkgStatus::ConfigFiles(v) if self.purge => Some((v, "config-files")),
                DpkgStatus::ConfigFiles(_) | DpkgStatus::Unknown => None,
                DpkgStatus::Other(status) => bail!(
                    "package `{name}` is in dpkg state `{status}`; \
                     fix it by hand (dpkg --configure -a) before apt::Absent"
                ),
            };
            match present {
                Some((version, from)) => {
                    changes.push(AttrChange {
                        name: name.clone(),
                        from: from.into(),
                        to: verb.into(),
                    });
                    report.removed.push(Package {
                        name: name.clone(),
                        version,
                    });
                }
                None => report.not_present.push(name.clone()),
            }
        }
        if changes.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: "apt packages".into(),
                changes,
            },
            report,
        ))
    }

    fn apply(&self, sys: &System, change: Change<RemoveReport>) -> Result<RemoveReport> {
        let Some(report) = change.predicted else {
            bail!("apt::Absent::apply received a change without its prediction");
        };
        let names: Vec<String> = report.removed.iter().map(|p| p.name.clone()).collect();
        let verb = if self.purge { "purge" } else { "remove" };
        apt_get(sys)
            .args([verb, "-y"])
            .args(names.iter().cloned())
            .run()?;
        if self.autoremove {
            let mut cmd = apt_get(sys).args(["autoremove", "-y"]);
            if self.purge {
                cmd = cmd.arg("--purge");
            }
            cmd.run()?;
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
/// With [`update_cache`](Latest::update_cache), the refresh happens in
/// `check`, before the candidates are read, **including in check mode**: a
/// dry run then writes `/var/lib/apt/lists` on the target and logs a warning
/// saying it did. Without it, the comparison uses whatever the lists already
/// say and a dry run touches nothing. See the module docs.
#[derive(Debug, Clone)]
pub struct Latest {
    names: Vec<String>,
    update_cache: Option<Duration>,
    install_recommends: bool,
}

impl Latest {
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
    /// This runs in check mode too, so **a dry run refreshes the package
    /// lists on the target** and the step logs a warning when it does. That
    /// is the point: candidates read from stale lists would make the dry run
    /// report packages as current when they are not. Leave this off for a
    /// dry run that must not write anything.
    pub fn update_cache(mut self, max_age: Duration) -> Self {
        self.update_cache = Some(max_age);
        self
    }

    /// Install recommended packages along with missing ones (default: no).
    pub fn install_recommends(mut self, on: bool) -> Self {
        self.install_recommends = on;
        self
    }

    /// The `check`-time refresh: `apt-get update` when the caller asked for
    /// it and the lists are stale. Says out loud when a dry run did it.
    fn refresh_cache(&self, sys: &System) -> Result<()> {
        let Some(max_age) = self.update_cache else {
            return Ok(());
        };
        if !update_cache_if_stale(sys, max_age)? {
            return Ok(());
        }
        let msg = "apt::Latest ran `apt-get update`: the lists were older than the \
                   `.update_cache()` age, and the candidate versions come from them";
        if sys.check_mode() {
            sys.warn(format!(
                "{msg}. A dry run does not otherwise change the target, but this \
                 rewrote /var/lib/apt/lists"
            ));
        } else {
            sys.debug(msg);
        }
        Ok(())
    }
}

impl Op for Latest {
    type Output = UpgradeReport;

    fn check(&self, sys: &System) -> Result<Plan<UpgradeReport>> {
        require_apt_root(sys, "Latest")?;
        // Before the candidates are read, not after: `apt-cache policy` can
        // only answer from the lists on disk.
        self.refresh_cache(sys)?;
        let mut report = UpgradeReport::default();
        let mut changes = vec![];
        for name in &self.names {
            let installed = installed_version(sys, name)?;
            let Some(candidate) = policy(sys, name)?.candidate else {
                bail!(
                    "package `{name}` has no candidate version in the apt cache \
                     (unknown name, or the lists need `apt-get update`)"
                );
            };
            let package = Package {
                name: name.clone(),
                version: candidate.clone(),
            };
            match installed {
                None => {
                    changes.push(AttrChange {
                        name: name.clone(),
                        from: "absent".into(),
                        to: candidate,
                    });
                    report.installed.push(package);
                }
                Some(v) if v != candidate => {
                    changes.push(AttrChange {
                        name: name.clone(),
                        from: v.clone(),
                        to: candidate,
                    });
                    report.upgraded.push((package, v));
                }
                Some(_) => report.current.push(package),
            }
        }
        if changes.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: "apt packages".into(),
                changes,
            },
            report,
        ))
    }

    fn apply(&self, sys: &System, change: Change<UpgradeReport>) -> Result<UpgradeReport> {
        let Some(mut report) = change.predicted else {
            bail!("apt::Latest::apply received a change without its prediction");
        };
        // No refresh here: `check` always runs first (this plan came from it)
        // and did it, so the candidates below are already the fresh ones.
        if !report.installed.is_empty() {
            let mut cmd = apt_get(sys).args(["install", "-y"]);
            if !self.install_recommends {
                cmd = cmd.arg("--no-install-recommends");
            }
            cmd.args(report.installed.iter().map(|p| p.name.clone()))
                .run()?;
        }
        if !report.upgraded.is_empty() {
            apt_get(sys)
                .args(["install", "-y", "--only-upgrade"])
                .args(report.upgraded.iter().map(|(p, _)| p.name.clone()))
                .run()?;
        }
        // Report what dpkg actually has now: apt may have landed on something
        // other than the candidate (a hold, a dependency, a pinned version).
        for p in &mut report.installed {
            p.version = installed_version(sys, &p.name)?.unwrap_or(p.version.clone());
        }
        for (p, _) in &mut report.upgraded {
            p.version = installed_version(sys, &p.name)?.unwrap_or(p.version.clone());
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

    fn non_root(fake: &Arc<Fake>) -> System {
        let mut facts = sys(fake).facts().clone();
        facts.is_root = false;
        facts.user = "cadu".into();
        sys(fake).with_facts(facts)
    }

    fn alpine(fake: &Arc<Fake>) -> System {
        let mut facts = sys(fake).facts().clone();
        facts.package_manager = Pm::Apk;
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
        assert_eq!(c.diff.short(), "mc=installed");
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
    fn present_predicts_the_candidate_version_when_the_lists_know_it() {
        let fake = Arc::new(policy_of(
            Fake::new()
                .with_cmd("dpkg-query", None, 1, "")
                .with_cmd("apt-get", None, 0, ""),
            "sl",
            "(none)",
            "5.02-1",
        ));
        let Plan::Change(c) = Present::new(["sl"]).check(&sys(&fake)).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.predicted.unwrap().installed[0].version, "5.02-1");
    }

    #[test]
    fn present_does_not_predict_when_the_lists_have_no_candidate() {
        // A stock image with no package lists: `apt-cache policy` says nothing,
        // so the version is unknown and check mode must not invent one.
        for fake in [
            Arc::new(Fake::new().with_cmd("dpkg-query", None, 1, "").with_cmd(
                "apt-cache",
                None,
                0,
                "",
            )),
            // apt-cache not runnable at all.
            Arc::new(Fake::new().with_cmd("dpkg-query", None, 1, "")),
        ] {
            let Plan::Change(c) = Present::new(["sl"]).check(&sys(&fake)).unwrap() else {
                panic!("expected change")
            };
            assert_eq!(c.diff.short(), "sl=installed");
            assert!(c.predicted.is_none(), "no candidate, no prediction");
        }
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
        assert_eq!(c.diff.short(), "mc=installed");
        // The prediction is absent (no candidate in this fake), so apply must
        // work from the diff alone.
        assert!(c.predicted.is_none());
        let report = op.apply(&s, c).unwrap();
        let argvs = fake.argvs();
        let install = argv_starting(&argvs, &["apt-get", "install"]).unwrap();
        assert!(install.contains(&"mc".into()) && !install.contains(&"zsh".into()));
        assert_eq!(report.installed[0].name, "mc");
        assert_eq!(report.already_present[0].version, "5.9-4");
    }

    #[test]
    fn present_apply_refuses_a_plan_with_no_packages() {
        let fake = Arc::new(Fake::new().with_cmd("apt-get", None, 0, ""));
        let s = sys(&fake);
        let err = Present::new(["mc"])
            .apply(
                &s,
                Change {
                    diff: Diff::summary("nothing"),
                    predicted: None,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("names no package"), "{err}");
        assert!(fake.argvs().is_empty(), "nothing ran");
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
        Fake::new()
            .with_cmd("dpkg-query", None, 1, "")
            .with_cmd("apt-get", None, 0, "")
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
        assert_eq!(
            argv_starting(&argvs, &["stat"]).unwrap(),
            &["stat", "-c", "%Y", "/var/lib/apt/lists"]
        );
        let update = argvs
            .iter()
            .position(|a| a.starts_with(&["apt-get".into(), "update".into()]))
            .expect("apt-get update ran");
        let install = argvs
            .iter()
            .position(|a| a.starts_with(&["apt-get".into(), "install".into()]))
            .unwrap();
        assert!(update < install, "update must precede install: {argvs:?}");
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
    }

    #[test]
    fn update_cache_zero_always_updates_without_stat() {
        let fake = Arc::new(present_missing_mc());
        apply_present(&fake, Present::new(["mc"]).update_cache(Duration::ZERO));
        let argvs = fake.argvs();
        assert!(argv_starting(&argvs, &["stat"]).is_none(), "{argvs:?}");
        assert!(argv_starting(&argvs, &["apt-get", "update"]).is_some());
    }

    #[test]
    fn update_cache_falls_back_to_pkgcache_then_updates_when_unknown() {
        // lists missing, pkgcache.bin fresh: no update.
        let fresh = (now_secs() - 60).to_string();
        let fake = Arc::new(
            present_missing_mc()
                .with_cmd("stat", Some(&["-c", "%Y", "/var/lib/apt/lists"]), 1, "")
                .with_cmd(
                    "stat",
                    Some(&["-c", "%Y", "/var/cache/apt/pkgcache.bin"]),
                    0,
                    &fresh,
                ),
        );
        apply_present(
            &fake,
            Present::new(["mc"]).update_cache(Duration::from_secs(3600)),
        );
        let argvs = fake.argvs();
        assert_eq!(argvs.iter().filter(|a| a[0] == "stat").count(), 2);
        assert!(
            argv_starting(&argvs, &["apt-get", "update"]).is_none(),
            "{argvs:?}"
        );

        // Neither readable: age unknown, update to be safe.
        let fake = Arc::new(present_missing_mc().with_cmd("stat", None, 1, ""));
        apply_present(
            &fake,
            Present::new(["mc"]).update_cache(Duration::from_secs(3600)),
        );
        assert!(argv_starting(&fake.argvs(), &["apt-get", "update"]).is_some());
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
        assert_eq!(c.diff.short(), "apache2=removed");
        let predicted = c.predicted.clone().unwrap();
        assert_eq!(predicted.removed[0].version, "2.4.62-1");
        assert_eq!(predicted.not_present, vec!["sendmail"]);

        let r = op.apply(&s, c).unwrap();
        assert_eq!(r, predicted);
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
        assert_eq!(c.diff.short(), "apache2=purged");
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
            c.diff.render(),
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
    fn absent_apply_without_prediction_is_refused() {
        let fake = Arc::new(Fake::new().with_cmd("apt-get", None, 0, ""));
        let err = Absent::new(["x"])
            .apply(
                &sys(&fake),
                Change {
                    diff: Diff::summary("x"),
                    predicted: None,
                },
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("without its prediction"), "{err}");
        assert!(fake.argvs().is_empty());
    }

    #[test]
    fn absent_check_mode_runs_only_dpkg_query_and_predicts() {
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
        assert!(r.changed && r.predicted && r.is_available());
        assert_eq!(r.removed[0].name, "apache2");
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
            c.diff.render(),
            "apt packages:\n  openssl: 3.0.15-1 -> 3.0.16-1\n"
        );
        let predicted = c.predicted.clone().unwrap();
        assert_eq!(
            predicted.upgraded,
            vec![(
                Package {
                    name: "openssl".into(),
                    version: "3.0.16-1".into()
                },
                "3.0.15-1".to_string()
            )]
        );
        let r = op.apply(&s, c).unwrap();
        // The fake dpkg still answers 3.0.15-1 after "apply": the report says what dpkg says.
        assert_eq!(r.upgraded[0].0.version, "3.0.15-1");
        assert_eq!(r.upgraded[0].1, "3.0.15-1");
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
        assert_eq!(c.diff.short(), "sl=5.02-1");
        assert_eq!(c.predicted.as_ref().unwrap().installed[0].version, "5.02-1");
        op.apply(&s, c).unwrap();
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
        let p = c.predicted.as_ref().unwrap();
        assert_eq!(p.installed.len(), 1);
        assert_eq!(p.upgraded.len(), 1);
        assert_eq!(p.current[0].name, "curl");
        op.apply(&s, c).unwrap();
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
            policy_of(fake, "sl", "(none)", "5.02-1")
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
            .position(|a| a[..2] == ["apt-get", "update"])
            .expect("check ran apt-get update");
        let policy = after_check
            .iter()
            .position(|a| a[0] == "apt-cache")
            .expect("check read the candidate");
        assert!(update < policy, "{after_check:?}");

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
        assert!(r.changed && r.predicted);
        assert_eq!(r.upgraded[0].0.version, "3.0.16-1");
        let argvs = fake.argvs();
        assert!(
            argvs
                .iter()
                .all(|a| a[0] == "dpkg-query" || a[0] == "apt-cache"),
            "{argvs:?}"
        );
    }

    /// With `.update_cache()`, a dry run does refresh the lists, and says so
    /// out loud rather than doing it silently.
    #[test]
    fn latest_check_mode_refreshes_the_cache_and_warns() {
        let fake = dpkg(
            Fake::new(),
            "openssl",
            0,
            "install ok installed\t3.0.15-1\n",
        );
        let fake = Arc::new(
            policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1").with_cmd("apt-get", None, 0, ""),
        );
        let sink = Arc::new(Collect::default());
        let s = System::fake(fake.clone(), sink.clone()).with_check_mode(true);
        let op = Latest::new(["openssl"]).update_cache(Duration::ZERO);
        let Plan::Change(_) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        let argvs = fake.argvs();
        assert!(
            argvs.iter().any(|a| a[..2] == ["apt-get", "update"]),
            "{argvs:?}"
        );
        let warned = sink.events().iter().any(|e| {
            matches!(
                e,
                rustible_sdk::event::Event::Log {
                    level: rustible_sdk::event::Level::Warn,
                    msg
                } if msg.contains("rewrote /var/lib/apt/lists")
            )
        });
        assert!(warned, "{:?}", sink.events());
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
        let fake = Arc::new(
            policy_of(fake, "openssl", "3.0.15-1", "3.0.16-1").with_cmd("apt-get", None, 0, ""),
        );
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
