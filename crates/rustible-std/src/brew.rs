//! Homebrew formulae. Ansible's `community.general.homebrew`.
//!
//! Shaped like [`crate::apt`]: one type per desired state, `check` decides
//! from what is installed and returns the decision as a typed intent, `apply`
//! executes exactly that.
//!
//! **`check` runs no `brew` at all.** It reads the Cellar through `sys`, as
//! `brew list --formula --versions` does: a formula is a directory
//! `<Cellar>/<name>/`, and its installed versions are the directories in it.
//! Running `brew` itself, even `brew list`, starts Homebrew's Ruby, which may
//! first download a vendored Ruby from `ghcr.io` into the Homebrew
//! installation (`HOMEBREW_LIBRARY`); a dry run contacts nothing outside the
//! target and changes nothing (vision 12). Only `apply` runs `brew install`
//! and `brew uninstall`.
//!
//! Known gaps, both older than the Cellar read: an alias or an old name of a
//! formula never matches its rack, so [`Present`] installs it on every run
//! (#72); and [`Absent`] runs `brew uninstall` without `--force`, which
//! leaves a formula with several versions installed (#71).
//!
//! Two things are different from every other package op here, and both are
//! Homebrew's doing:
//!
//! - **It refuses to run as root**, so these ops require *not* being root,
//!   which is the inverse of [`crate::apt`]. Homebrew's own words:
//!   "Running Homebrew as root is extremely dangerous and no longer
//!   supported. As Homebrew does not drop privileges on installation you
//!   would be giving all build scripts full access to your system." A
//!   playbook with `escalate = true` therefore cannot use these ops
//!   directly; run the playbook unescalated, or reach the owning user with
//!   `ctx.as_user(..)`.
//! - **It is not tied to an operating system.** Homebrew runs on macOS and on
//!   Linux, so these ops ask [`Pm::Brew`] rather than [`Os`]: a mac without
//!   Homebrew does not have it, and a Debian box with `/home/linuxbrew` does.
//!
//! ```no_run
//! use rustible::prelude::*;
//! use rustible_std::brew;
//!
//! # fn f(ctx: &mut Ctx) -> Result<()> {
//! let out = ctx.step("nethack present", brew::Present::new(["nethack"]))?;
//! ctx.log(format!("{} newly installed", out.installed.len()));
//! # Ok(())
//! # }
//! ```

use std::path::{Component, Path, PathBuf};

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

/// Every path Homebrew installs its binary at, most specific first: Apple
/// silicon, Intel macs, then Linux. Probed rather than trusted to `PATH`,
/// because the binary runs under whatever environment `sshd` hands it and
/// that rarely includes `/opt/homebrew/bin`.
const BREW_PATHS: [&str; 3] = [
    "/opt/homebrew/bin/brew",
    "/usr/local/bin/brew",
    "/home/linuxbrew/.linuxbrew/bin/brew",
];

/// One formula and the version Homebrew has for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Formula {
    /// The formula name as the op was given it, which is also the name of
    /// its rack in the Cellar: the two are compared exactly. A tap-qualified
    /// name is refused (see [`validate_formula`]), and an alias or an old
    /// name does not match the rack its formula is installed under, so it
    /// reads as not installed (#72).
    pub name: String,
    /// The installed version. Empty for a formula [`Present`] is about to
    /// install, since brew has not resolved it yet.
    pub version: String,
}

/// Output of [`Present`]. The two lists together name every formula the op
/// was given, so `installed` empty means the step was `ok`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallReport {
    /// Formulae this step installed.
    pub installed: Vec<Formula>,
    /// Formulae that were already there.
    pub already_present: Vec<Formula>,
}

/// Output of [`Absent`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RemoveReport {
    /// Formulae this step uninstalled, with the version they had.
    pub removed: Vec<Formula>,
    /// Names that were not installed to begin with.
    pub already_absent: Vec<String>,
}

/// Pure: the formula one Cellar rack stands for, given the rack's name, the
/// names of the version directories in it, and the version
/// `<prefix>/opt/<name>` points at, if it is a link into one. The rule is
/// `brew list --formula --versions`' (`Formula.racks`): a name starting with
/// `.` is not a formula, and neither is a rack with no version in it. The
/// version is a directory's name exactly, revision suffix and all
/// (`3.6.7_1`). With several installed, it is the one the opt link points
/// at, which is Homebrew's current version (`list.sh`'s `optlinked_version`,
/// the first choice of `resolve_default_keg`); an opt link to a version not
/// in this rack is ignored. Without one, the first in byte order, which is
/// not version order (`10.0` sorts before `9.1`) but is at least stable.
fn rack_formula(name: &str, mut versions: Vec<String>, opt: Option<&str>) -> Option<Formula> {
    if name.starts_with('.') {
        return None;
    }
    versions.sort();
    let version = match opt {
        Some(v) if versions.iter().any(|have| have == v) => v.to_string(),
        _ => versions.into_iter().next()?,
    };
    Some(Formula {
        name: name.to_string(),
        version,
    })
}

/// Pure: `p` with `.` and `..` resolved by name, as a shell's `cd` does
/// before `pwd`. A symlink's relative target is joined onto the link's
/// directory, and the repository two directories up from it must be a real
/// path rather than one that climbs back out of `bin`.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Pure: where the Cellar may be for the `brew` at `brew`, in the order
/// Homebrew's `bin/brew` and `brew.sh` try them. The prefix is two
/// directories up from the binary; the repository is the prefix too, unless
/// the binary is a symlink, when it is two directories up from its target
/// (`/usr/local/bin/brew -> ../Homebrew/bin/brew` puts it at
/// `/usr/local/Homebrew`). The Cellar is `<repository>/Cellar` when that is
/// a directory, else `<prefix>/Cellar`.
fn cellar_candidates(brew: &Path, link_target: Option<&Path>) -> Vec<PathBuf> {
    let bin = brew.parent().unwrap_or(Path::new("/"));
    let prefix = prefix_of(brew);
    let mut out = vec![];
    if let Some(target) = link_target {
        let target = normalize(&bin.join(target));
        if let Some(repository) = target.parent().and_then(Path::parent)
            && repository != prefix
        {
            out.push(repository.join("Cellar"));
        }
    }
    out.push(prefix.join("Cellar"));
    out
}

/// Pure: validate a formula name before it reaches a command line. Homebrew
/// names are lowercase letters, digits, `-`, `_`, `.`, `+`, `@`, and a tap
/// qualifier may add `/`.
pub fn validate_formula(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("formula name is empty".into());
    }
    if name.starts_with('-') {
        return Err(format!(
            "`{name}` starts with a dash, which brew reads as an option"
        ));
    }
    if name.contains('/') {
        return Err(format!(
            "`{name}` names a tap or a cask; brew::Present and brew::Absent manage formulae by \
             their bare name, because that is all the Cellar keeps them under"
        ));
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || "-_.+@".contains(*c)))
    {
        return Err(format!(
            "`{name}` contains {bad:?}, which is not legal in a formula name"
        ));
    }
    Ok(())
}

/// The absolute path of the `brew` on this host, or a refusal naming where it
/// looked. Probing the binary rather than trusting [`Pm::Brew`] alone is the
/// rule the `user`/`group` ops already follow: the fact is a hint gathered at
/// startup, the binary is the truth now.
fn brew_bin(sys: &System) -> Result<String> {
    for p in BREW_PATHS {
        if sys.exists(p)? {
            return Ok(p.to_string());
        }
    }
    bail!(
        "no `brew` at any of {}; rustible probes these rather than trusting PATH, \
         because the playbook binary runs with whatever environment sshd gave it",
        BREW_PATHS.join(", ")
    )
}

/// Refuse a host these ops cannot serve, before anything runs.
///
/// Not an [`Os`] check: Homebrew is a capability, not a platform. The root
/// check is inverted from every other package op because brew refuses to run
/// as root, and refusing here names the fix instead of surfacing brew's own
/// error from inside `apply`.
fn require_brew_not_root(sys: &System, op: &str) -> Result<String> {
    if !sys.facts().has_pm(&Pm::Brew) {
        bail!(
            "brew::{op} needs Homebrew, which is not installed on this host ({} {:?})",
            sys.facts().os.name(),
            sys.facts().distro
        );
    }
    if sys.is_root() {
        bail!(
            "brew::{op} must not run as root: Homebrew refuses it outright (\"Running \
             Homebrew as root is extremely dangerous and no longer supported\"), because it \
             does not drop privileges and every build script would get the whole machine. \
             Run this playbook without `escalate = true`, or reach the owning user with \
             `ctx.as_user(..)`"
        );
    }
    brew_bin(sys)
}

fn is_symlink(sys: &System, p: &Path) -> Result<bool> {
    Ok(matches!(sys.stat(p)?, Some(s) if s.kind == FileKind::Symlink))
}

/// Whether `p` is a directory, following symlinks, as Ruby's `directory?`
/// decides it: a symlink that cannot be followed (a loop, a target this
/// account may not stat) is not a directory, rather than an error. An error
/// on a path that is not a symlink still fails the step.
fn is_dir(sys: &System, p: &Path) -> Result<bool> {
    match sys.stat_follow(p) {
        Ok(stat) => Ok(matches!(stat, Some(s) if s.kind == FileKind::Dir)),
        Err(_) if is_symlink(sys, p)? => Ok(false),
        Err(e) => Err(e),
    }
}

/// The two directories above `brew`: Homebrew's prefix.
fn prefix_of(brew: &Path) -> &Path {
    brew.parent()
        .and_then(Path::parent)
        .unwrap_or(Path::new("/"))
}

/// The Cellar of the `brew` at `brew`: the first of [`cellar_candidates`]
/// that is a directory. `None` when none is, which is a Homebrew with
/// nothing installed yet.
fn cellar(sys: &System, brew: &Path) -> Result<Option<PathBuf>> {
    let target = if is_symlink(sys, brew)? {
        Some(sys.read_link(brew)?)
    } else {
        None
    };
    for candidate in cellar_candidates(brew, target.as_deref()) {
        if is_dir(sys, &candidate)? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

/// The version directory `<prefix>/opt/<name>` resolves to, when it is a
/// symlink to a directory (`list.sh`: `-L` and `-d`), followed hop by hop
/// through `sys` as `realpath` would. `None` otherwise.
fn opt_version(sys: &System, prefix: &Path, name: &str) -> Result<Option<String>> {
    let mut cur = prefix.join("opt").join(name);
    if !is_symlink(sys, &cur)? {
        return Ok(None);
    }
    // Hop by hop, with `..` resolved by name at each step, so the end is a
    // plain path; the bound is the kernel's own, and a chain longer than it
    // (a loop) is not a directory.
    for _ in 0..40 {
        if !is_symlink(sys, &cur)? {
            if !is_dir(sys, &cur)? {
                return Ok(None);
            }
            return Ok(cur.file_name().and_then(|n| n.to_str()).map(str::to_string));
        }
        let target = sys.read_link(&cur)?;
        let dir = cur.parent().unwrap_or(Path::new("/"));
        cur = normalize(&dir.join(target));
    }
    Ok(None)
}

/// Which of `names` brew has installed, read from the Cellar through `sys`
/// as `brew list --formula --versions` reads it, and without running `brew`
/// (see the module docs). Only the named racks are read, not the whole
/// Cellar, which matters when every read is a round trip to a helper under
/// `ctx.as_user`. A rack is a directory in the Cellar, not itself a symlink,
/// whose name is the requested name exactly: the Cellar is listed once and
/// compared by name, because on a case-insensitive volume (APFS by default)
/// `<Cellar>/Python` answers for `python`. Its versions are the directories
/// in it, symlinks to directories included (`Pathname#subdirs`).
fn installed(sys: &System, brew: &str, names: &[String]) -> Result<Vec<Formula>> {
    let brew = Path::new(brew);
    let Some(cellar) = cellar(sys, brew)? else {
        return Ok(vec![]);
    };
    let racks: Vec<String> = sys
        .read_dir(&cellar)?
        .iter()
        .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .collect();
    let mut formulae = vec![];
    for name in names {
        if false && !racks.iter().any(|r| r == name) {
            continue;
        }
        let rack = cellar.join(name);
        if is_symlink(sys, &rack)? || !is_dir(sys, &rack)? {
            continue;
        }
        let mut versions = vec![];
        for entry in sys.read_dir(&rack)? {
            if is_dir(sys, &entry)?
                && let Some(v) = entry.file_name().and_then(|n| n.to_str())
            {
                versions.push(v.to_string());
            }
        }
        // The opt link only decides between several versions; with one,
        // the answer is the same either way and the reads are saved.
        let opt = if versions.len() > 1 {
            opt_version(sys, prefix_of(brew), name)?
        } else {
            None
        };
        formulae.extend(rack_formula(name, versions, opt.as_deref()));
    }
    Ok(formulae)
}

// ---------------------------------------------------------------- Present

/// Ensure formulae are installed. `homebrew: state=present`.
///
/// A formula that is already there is `ok` however old it is; upgrading is a
/// different desired state and not this one.
#[derive(Debug, Clone)]
pub struct Present {
    names: Vec<String>,
}

impl Present {
    /// Ensure every one of `names` is installed, leaving the version to brew.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Present {
            names: names.into_iter().map(Into::into).collect(),
        }
    }
}

/// What [`Present`]'s `check` decided: install these formulae, which brew
/// does not have, with the `brew` it found.
#[derive(Debug)]
pub struct Install {
    brew: String,
    names: Vec<String>,
}

impl Intent for Install {
    fn diff(&self) -> Diff {
        Diff::attrs(
            "brew formulae",
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
        let brew = require_brew_not_root(sys, "Present")?;
        ensure!(
            !self.names.is_empty(),
            "brew::Present was given no formula to install"
        );
        for name in &self.names {
            if let Err(why) = validate_formula(name) {
                bail!("brew::Present: {why}");
            }
        }
        let have = installed(sys, &brew, &self.names)?;
        let mut report = InstallReport::default();
        let mut missing = vec![];
        for name in &self.names {
            match have.iter().find(|f| &f.name == name) {
                Some(f) => report.already_present.push(f.clone()),
                None => missing.push(name.clone()),
            }
        }
        if missing.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        Ok(Plan::Change(Install {
            brew,
            names: missing,
        }))
    }

    fn apply(&self, sys: &System, intent: Install) -> Result<InstallReport> {
        // Install what `check` planned, not what brew says now.
        let Install {
            brew,
            names: missing,
        } = intent;
        sys.cmd(&brew)
            .arg("install")
            .args(missing.iter().cloned())
            .run()?;
        // Read the versions back so the report names what actually landed.
        let now = installed(sys, &brew, &self.names)?;
        let mut report = InstallReport::default();
        for name in &self.names {
            let f = now
                .iter()
                .find(|f| &f.name == name)
                .cloned()
                .unwrap_or_else(|| Formula {
                    name: name.clone(),
                    version: String::new(),
                });
            if missing.contains(name) {
                report.installed.push(f);
            } else {
                report.already_present.push(f);
            }
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------- Absent

/// Ensure formulae are not installed. `homebrew: state=absent`.
#[derive(Debug, Clone)]
pub struct Absent {
    names: Vec<String>,
}

impl Absent {
    /// Ensure none of `names` is installed. A name that is not there is `ok`.
    pub fn new<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Absent {
            names: names.into_iter().map(Into::into).collect(),
        }
    }
}

/// What [`Absent`]'s `check` decided: uninstall these formulae, each with
/// the version the Cellar showed, using the `brew` it found.
#[derive(Debug)]
pub struct Uninstall {
    brew: String,
    formulae: Vec<Formula>,
}

impl Intent for Uninstall {
    fn diff(&self) -> Diff {
        Diff::attrs(
            "brew formulae",
            self.formulae
                .iter()
                .map(|f| {
                    AttrChange::new(
                        f.name.as_str(),
                        format!("installed {}", f.version),
                        "absent",
                    )
                })
                .collect(),
        )
    }
}

impl Op for Absent {
    type Output = RemoveReport;
    type Intent = Uninstall;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        let brew = require_brew_not_root(sys, "Absent")?;
        ensure!(
            !self.names.is_empty(),
            "brew::Absent was given no formula to remove"
        );
        for name in &self.names {
            if let Err(why) = validate_formula(name) {
                bail!("brew::Absent: {why}");
            }
        }
        let have = installed(sys, &brew, &self.names)?;
        let mut report = RemoveReport::default();
        for name in &self.names {
            match have.iter().find(|f| &f.name == name) {
                Some(f) => report.removed.push(f.clone()),
                None => report.already_absent.push(name.clone()),
            }
        }
        if report.removed.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        Ok(Plan::Change(Uninstall {
            brew,
            formulae: report.removed,
        }))
    }

    fn apply(&self, sys: &System, intent: Uninstall) -> Result<RemoveReport> {
        let Uninstall { brew, formulae } = intent;
        sys.cmd(&brew)
            .arg("uninstall")
            .args(formulae.iter().map(|f| f.name.clone()))
            .run()?;
        // What went, with the versions `check` read before the uninstall
        // took them.
        let already_absent = self
            .names
            .iter()
            .filter(|name| !formulae.iter().any(|f| &f.name == *name))
            .cloned()
            .collect();
        Ok(RemoveReport {
            removed: formulae,
            already_absent,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::{Backend, Fake};
    use rustible_sdk::event::Collect;
    use rustible_sdk::facts::{Distro, Facts, Os, Pm};

    use super::*;

    const BREW: &str = "/opt/homebrew/bin/brew";
    const CELLAR: &str = "/opt/homebrew/Cellar";

    /// A mac with Homebrew, running as the login user. Homebrew's ops are the
    /// only ones here that need `is_root: false`.
    fn mac_facts() -> Facts {
        Facts {
            os: Os::Macos,
            distro: Distro::Macos,
            distro_version: "26.3".into(),
            arch: rustible_sdk::facts::Arch::Aarch64,
            kernel: "25.3.0".into(),
            hostname: "fake-mac".into(),
            package_managers: [Pm::Brew].into_iter().collect(),
            init: rustible_sdk::facts::Init::Launchd,
            cpus: 12,
            memory_mb: 49152,
            user: "cadu".into(),
            is_root: false,
        }
    }

    /// Plant `<cellar>/<name>/<version>/` for each formula and version, with
    /// the directories above them.
    fn plant(fake: &Fake, cellar: &str, formulae: &[(&str, &[&str])]) {
        fake.mkdir_all(Path::new(cellar)).unwrap();
        for (name, versions) in formulae {
            let rack = Path::new(cellar).join(name);
            fake.mkdir_all(&rack).unwrap();
            for v in *versions {
                fake.mkdir_all(&rack.join(v)).unwrap();
            }
        }
    }

    /// An Apple-silicon Homebrew with these formulae in its Cellar. `brew`
    /// answers anything with success, for `apply`; `check` must not ask it.
    fn mac_fake(formulae: &[(&str, &[&str])]) -> Arc<Fake> {
        let fake = Fake::new().with_file(BREW, "").with_cmd(BREW, None, 0, "");
        plant(&fake, CELLAR, formulae);
        Arc::new(fake)
    }

    fn mac_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default())).with_facts(mac_facts())
    }

    // ---- pure ----

    #[test]
    fn a_rack_is_its_name_and_its_version_directory_verbatim() {
        assert_eq!(
            rack_formula("nethack", vec!["3.6.7".into()], None),
            Some(Formula {
                name: "nethack".into(),
                version: "3.6.7".into()
            })
        );
        // The revision suffix is part of the version, as `brew list` prints it.
        assert_eq!(
            rack_formula("openssl@3", vec!["3.6.1_1".into()], None)
                .unwrap()
                .version,
            "3.6.1_1"
        );
    }

    /// Several versions: the one the opt link points at is Homebrew's
    /// current one, whatever byte order says (`10.0` sorts before `9.1`).
    /// Without an opt link, or with one pointing at a version this rack does
    /// not have, the first in byte order, whatever order they came in.
    #[test]
    fn several_versions_report_the_opt_linked_one_else_the_first_in_byte_order() {
        let versions = || vec!["9.1".to_string(), "10.0".to_string()];
        let version = |opt| rack_formula("x", versions(), opt).unwrap().version;
        assert_eq!(version(Some("9.1")), "9.1");
        assert_eq!(version(None), "10.0");
        assert_eq!(version(Some("11.0")), "10.0");
    }

    /// The two racks `brew list` does not list.
    #[test]
    fn a_hidden_rack_or_one_with_no_version_is_not_a_formula() {
        assert_eq!(rack_formula(".DS_Store", vec!["x".into()], None), None);
        assert_eq!(rack_formula("cowsay", vec![], None), None);
    }

    #[test]
    fn the_cellar_is_found_where_homebrew_looks_for_it() {
        // Apple silicon: brew is not a link, the repository is the prefix.
        assert_eq!(
            cellar_candidates(Path::new(BREW), None),
            vec![PathBuf::from(CELLAR)]
        );
        // Intel: `/usr/local/bin/brew -> ../Homebrew/bin/brew`, so the
        // repository's Cellar is tried first and the prefix's after it.
        assert_eq!(
            cellar_candidates(
                Path::new("/usr/local/bin/brew"),
                Some(Path::new("../Homebrew/bin/brew"))
            ),
            vec![
                PathBuf::from("/usr/local/Homebrew/Cellar"),
                PathBuf::from("/usr/local/Cellar"),
            ]
        );
        // An absolute target is taken as it is.
        assert_eq!(
            cellar_candidates(
                Path::new("/home/linuxbrew/.linuxbrew/bin/brew"),
                Some(Path::new("/home/linuxbrew/.linuxbrew/Homebrew/bin/brew"))
            )[1],
            PathBuf::from("/home/linuxbrew/.linuxbrew/Cellar")
        );
    }

    #[test]
    fn formula_names_that_are_refused() {
        assert!(validate_formula("nethack").is_ok());
        assert!(validate_formula("openssl@3").is_ok());
        let err = validate_formula("homebrew/cask/firefox").unwrap_err();
        assert!(err.contains("tap or a cask"), "{err}");
        assert!(err.contains("by their bare name"), "{err}");
        assert!(validate_formula("").unwrap_err().contains("empty"));
        assert!(validate_formula("--force").unwrap_err().contains("dash"));
        assert!(validate_formula("a b").unwrap_err().contains("not legal"));
    }

    // ---- Fake ----

    /// `check` reads the Cellar and runs no `brew` at all: even `brew list`
    /// starts Homebrew's Ruby, which may fetch one first (vision 12).
    #[test]
    fn satisfied_from_the_cellar_without_running_brew() {
        let fake = mac_fake(&[("nethack", &["3.6.7"])]);
        let op = Present::new(["nethack"]);
        let Plan::Satisfied(report) = op.check(&mac_sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert!(report.installed.is_empty());
        assert_eq!(report.already_present[0].version, "3.6.7");
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    /// What the Cellar holds that is not a formula: a symlinked rack, a
    /// stray file, a rack with nothing in it, a hidden directory, and a
    /// file where a version would be.
    #[test]
    fn only_real_racks_with_a_version_directory_are_installed() {
        let fake = mac_fake(&[
            ("agg", &["1.7.0"]),
            ("empty", &[]),
            (".hidden", &["1.0"]),
            ("onlyfile", &[]),
        ]);
        fake.mkdir_all(Path::new("/elsewhere/linked/1.0")).unwrap();
        fake.symlink(
            Path::new("/elsewhere/linked"),
            Path::new("/opt/homebrew/Cellar/linked"),
        )
        .unwrap();
        fake.write(Path::new("/opt/homebrew/Cellar/stray"), b"")
            .unwrap();
        fake.write(Path::new("/opt/homebrew/Cellar/onlyfile/1.0"), b"")
            .unwrap();
        let names: Vec<String> = ["agg", "empty", ".hidden", "onlyfile", "linked", "stray"]
            .map(String::from)
            .into();
        let installed = installed(&mac_sys(&fake), BREW, &names).unwrap();
        assert_eq!(
            installed,
            vec![Formula {
                name: "agg".into(),
                version: "1.7.0".into()
            }]
        );
    }

    /// Only the requested racks are read: a formula that is installed but
    /// not asked about is not in the answer, and its rack is never listed.
    #[test]
    fn only_the_named_racks_are_read() {
        let fake = mac_fake(&[("agg", &["1.7.0"]), ("nethack", &["3.6.7"])]);
        let names = vec!["nethack".to_string()];
        let installed = installed(&mac_sys(&fake), BREW, &names).unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].name, "nethack");
    }

    /// A version directory may be a symlink to a directory: Homebrew's
    /// `Pathname#subdirs` follows links (`children.select(&:directory?)`).
    #[test]
    fn a_symlinked_version_directory_counts() {
        let fake = mac_fake(&[("agg", &[])]);
        fake.mkdir_all(Path::new("/elsewhere/agg-1.7.0")).unwrap();
        fake.symlink(
            Path::new("/elsewhere/agg-1.7.0"),
            Path::new("/opt/homebrew/Cellar/agg/1.7.0"),
        )
        .unwrap();
        let installed = installed(&mac_sys(&fake), BREW, &["agg".to_string()]).unwrap();
        assert_eq!(installed[0].version, "1.7.0");
    }

    /// A symlink that loops, as a rack or as a version, is "not a
    /// directory", as Ruby's `directory?` says, and does not fail `check`.
    #[test]
    fn a_looping_symlink_is_not_a_directory_and_not_an_error() {
        let fake = mac_fake(&[("agg", &["1.7.0"])]);
        for (a, b) in [
            ("/opt/homebrew/Cellar/loop", "/opt/homebrew/Cellar/loop2"),
            ("/opt/homebrew/Cellar/loop2", "/opt/homebrew/Cellar/loop"),
            (
                "/opt/homebrew/Cellar/agg/2.0",
                "/opt/homebrew/Cellar/agg/2.1",
            ),
            (
                "/opt/homebrew/Cellar/agg/2.1",
                "/opt/homebrew/Cellar/agg/2.0",
            ),
        ] {
            fake.symlink(Path::new(b), Path::new(a)).unwrap();
        }
        let names = vec!["agg".to_string(), "loop".to_string()];
        let installed = installed(&mac_sys(&fake), BREW, &names).unwrap();
        assert_eq!(installed.len(), 1);
        assert_eq!(installed[0].version, "1.7.0");
    }

    /// With several versions installed, the one `<prefix>/opt/<name>`
    /// points at, through a relative link as Homebrew writes it.
    #[test]
    fn the_opt_link_names_the_current_version() {
        let fake = mac_fake(&[("python@3", &["3.9.1", "3.10.0"])]);
        fake.mkdir_all(Path::new("/opt/homebrew/opt")).unwrap();
        fake.symlink(
            Path::new("../Cellar/python@3/3.9.1"),
            Path::new("/opt/homebrew/opt/python@3"),
        )
        .unwrap();
        let names = vec!["python@3".to_string()];
        let found = installed(&mac_sys(&fake), BREW, &names).unwrap();
        assert_eq!(found[0].version, "3.9.1");

        // No opt link: the first in byte order.
        let fake = mac_fake(&[("python@3", &["3.9.1", "3.10.0"])]);
        let found = installed(&mac_sys(&fake), BREW, &names).unwrap();
        assert_eq!(found[0].version, "3.10.0");
    }

    /// Intel: `/usr/local/bin/brew -> ../Homebrew/bin/brew`. When the
    /// repository has a Cellar of its own it wins over the prefix's
    /// (`brew.sh` L41-46), so the link must be read, and both candidates
    /// tried in order.
    #[test]
    fn the_repository_cellar_wins_over_the_prefix_cellar() {
        let fake = Fake::new()
            .with_dir("/usr/local/bin")
            .with_symlink("/usr/local/bin/brew", "../Homebrew/bin/brew");
        plant(
            &fake,
            "/usr/local/Homebrew/Cellar",
            &[("nethack", &["3.6.7"])],
        );
        plant(&fake, "/usr/local/Cellar", &[("nethack", &["3.6.6"])]);
        let names = vec!["nethack".to_string()];
        let installed =
            installed(&mac_sys(&Arc::new(fake)), "/usr/local/bin/brew", &names).unwrap();
        assert_eq!(installed[0].version, "3.6.7");
    }

    /// A regular file named `Cellar` is not a Cellar: nothing is installed,
    /// and nothing fails.
    #[test]
    fn a_file_named_cellar_is_not_a_cellar() {
        let fake = Arc::new(
            Fake::new()
                .with_file(BREW, "")
                .with_file(CELLAR, "not a directory"),
        );
        let names = vec!["nethack".to_string()];
        assert!(installed(&mac_sys(&fake), BREW, &names).unwrap().is_empty());
    }

    /// The same two rules against the real filesystem, through the `Local`
    /// backend, because the `Fake` cannot show them: a symlink loop is a
    /// real `ELOOP` from `stat`, which must read as "not a directory"; and
    /// on a case-insensitive volume (the macOS runner's APFS)
    /// `<Cellar>/Python` answers for `python`, which must not count as
    /// installed. On a case-sensitive Linux volume the second half holds
    /// trivially.
    #[test]
    fn on_a_real_filesystem_loops_are_not_directories_and_names_match_exactly() {
        let root = std::env::temp_dir().join(format!("rustible-brew-{}", std::process::id()));
        let s = System::local(false, Arc::new(Collect::default()));
        let _ = s.remove_all(&root);
        let brew = root.join("bin/brew");
        s.mkdir_all(root.join("bin")).unwrap();
        s.write_atomic(&brew, b"").unwrap();
        s.mkdir_all(root.join("Cellar/Python/3.12.0")).unwrap();
        s.mkdir_all(root.join("Cellar/agg/1.7.0")).unwrap();
        s.symlink(root.join("Cellar/agg/x"), root.join("Cellar/agg/y"))
            .unwrap();
        s.symlink(root.join("Cellar/agg/y"), root.join("Cellar/agg/x"))
            .unwrap();
        s.symlink(root.join("Cellar/loop2"), root.join("Cellar/loop"))
            .unwrap();
        s.symlink(root.join("Cellar/loop"), root.join("Cellar/loop2"))
            .unwrap();

        // The precondition, checked rather than assumed: does `python` reach
        // the `Python` rack on this volume? Only where it does can this test
        // catch a dropped exact-name compare.
        let case_insensitive = s.exists(root.join("Cellar/python")).unwrap();
        let names = ["agg", "loop", "python"].map(String::from).to_vec();
        let found = installed(&s, brew.to_str().unwrap(), &names);
        let _ = s.remove_all(&root);
        if cfg!(target_os = "macos") {
            assert!(
                case_insensitive,
                "the macOS runner's temp volume is expected to be case-insensitive (APFS \
                 default); without that, this test cannot catch a dropped exact-name compare"
            );
        }
        eprintln!(
            "temp volume is case-{}: the exact-name half of this test is {}",
            if case_insensitive {
                "insensitive"
            } else {
                "sensitive"
            },
            if case_insensitive {
                "live"
            } else {
                "trivially true"
            }
        );
        let found = found.unwrap();
        assert!(
            !found.iter().any(|f| f.name == "python"),
            "`python` was reported installed from the `Python` rack (case-insensitive \
             volume: {case_insensitive}); the rack's name must match the request exactly: \
             {found:?}"
        );
        assert_eq!(
            found,
            vec![Formula {
                name: "agg".into(),
                version: "1.7.0".into()
            }]
        );
    }

    /// No Cellar at all: a Homebrew that has installed nothing yet.
    #[test]
    fn no_cellar_means_nothing_is_installed() {
        let fake = Arc::new(Fake::new().with_file(BREW, ""));
        let s = mac_sys(&fake);
        assert!(Present::new(["nethack"]).check(&s).unwrap().is_change());
        assert!(matches!(
            Absent::new(["nethack"]).check(&s).unwrap(),
            Plan::Satisfied(_)
        ));
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    /// The plan names only what is missing, `apply` installs exactly that,
    /// and once the formula's directory is in the Cellar the op is
    /// satisfied: changed-then-ok, which reading state from files makes
    /// expressible against the `Fake` (CLAUDE.md).
    #[test]
    fn change_names_the_missing_formula_and_apply_installs_exactly_that() {
        let fake = mac_fake(&[("agg", &["1.7.0"])]);
        let s = mac_sys(&fake);
        let op = Present::new(["ninvaders", "agg"]);

        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(
            c.diff().render(),
            "brew formulae:\n  ninvaders: absent -> installed\n"
        );
        assert!(fake.argvs().is_empty(), "check ran {:?}", fake.argvs());

        op.apply(&s, c).unwrap();
        assert_eq!(fake.argvs(), vec![vec![BREW, "install", "ninvaders"]]);

        // What `brew install` would have left behind.
        plant(&fake, CELLAR, &[("ninvaders", &["0.1.1_1"])]);
        let Plan::Satisfied(r) = op.check(&s).unwrap() else {
            panic!("expected satisfied once installed")
        };
        assert_eq!(r.already_present.len(), 2);
        assert_eq!(r.already_present[0].version, "0.1.1_1");
    }

    /// `apply` reports the version that landed, read back from the Cellar.
    #[test]
    fn apply_reports_the_installed_version_from_the_cellar() {
        let fake = mac_fake(&[("agg", &["1.7.0"])]);
        // Planted before `apply`, so the read-back after `brew install`
        // finds it: the Fake's `brew` writes nothing.
        let s = mac_sys(&fake);
        let op = Present::new(["ninvaders"]);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        plant(&fake, CELLAR, &[("ninvaders", &["0.1.1_1"])]);
        let report = op.apply(&s, c).unwrap();
        assert_eq!(report.installed[0].name, "ninvaders");
        assert_eq!(report.installed[0].version, "0.1.1_1");
    }

    /// Under `--check` a missing formula is `would change` and nothing runs.
    #[test]
    fn check_mode_would_change_and_runs_nothing() {
        let fake = mac_fake(&[]);
        let s = mac_sys(&fake).with_check_mode(true);
        let mut ctx = Ctx::new(s, rustible_sdk::HostInfo::local());
        let r = ctx.step("nethack", Present::new(["nethack"])).unwrap();
        assert!(r.changed && !r.is_available());
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    /// Intel: `/usr/local/bin/brew` links into `/usr/local/Homebrew`, and the
    /// Cellar is the prefix's, because the repository has none.
    #[test]
    fn an_intel_homebrew_reads_the_prefix_cellar() {
        let fake = Fake::new()
            .with_dir("/usr/local/bin")
            .with_symlink("/usr/local/bin/brew", "../Homebrew/bin/brew");
        plant(&fake, "/usr/local/Cellar", &[("nethack", &["3.6.7"])]);
        let fake = Arc::new(fake);
        let Plan::Satisfied(r) = Present::new(["nethack"]).check(&mac_sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.already_present[0].version, "3.6.7");
    }

    #[test]
    fn refuses_as_root_naming_homebrews_own_reason() {
        let fake = mac_fake(&[]);
        let mut facts = mac_facts();
        facts.is_root = true;
        facts.user = "root".into();
        let s = System::fake(fake, Arc::new(Collect::default())).with_facts(facts);
        let err = Present::new(["nethack"]).check(&s).unwrap_err().chain();
        assert!(err.contains("must not run as root"), "{err}");
        assert!(err.contains("extremely dangerous"), "{err}");
        assert!(err.contains("as_user"), "{err}");
    }

    /// Homebrew is a capability, not a platform: the refusal is about the
    /// manager being absent and says nothing about the OS being wrong.
    #[test]
    fn refuses_a_host_without_homebrew() {
        let fake = Arc::new(Fake::new());
        let mut facts = mac_facts();
        facts.package_managers = Default::default();
        let s = System::fake(fake, Arc::new(Collect::default())).with_facts(facts);
        let err = Present::new(["nethack"]).check(&s).unwrap_err().chain();
        assert!(err.contains("needs Homebrew"), "{err}");
    }

    /// Linuxbrew: the same ops on a Debian box with `/home/linuxbrew`, which
    /// is why these gate on `Pm::Brew` and not on `Os::Macos`.
    #[test]
    fn linuxbrew_is_served_too() {
        let fake = Fake::new()
            .with_dir("/home/linuxbrew/.linuxbrew/bin")
            .with_symlink(
                "/home/linuxbrew/.linuxbrew/bin/brew",
                "../Homebrew/bin/brew",
            );
        plant(
            &fake,
            "/home/linuxbrew/.linuxbrew/Cellar",
            &[("nethack", &["3.6.7"])],
        );
        let facts = Facts {
            os: Os::Linux,
            distro: Distro::Debian,
            package_managers: [Pm::Apt, Pm::Brew].into_iter().collect(),
            is_root: false,
            user: "cadu".into(),
            ..mac_facts()
        };
        let s = System::fake(Arc::new(fake), Arc::new(Collect::default())).with_facts(facts);
        assert!(matches!(
            Present::new(["nethack"]).check(&s).unwrap(),
            Plan::Satisfied(_)
        ));
    }

    #[test]
    fn absent_diff_names_the_version_and_apply_reports_what_went() {
        let fake = mac_fake(&[("nethack", &["3.6.7"])]);
        let s = mac_sys(&fake);
        let op = Absent::new(["nethack", "agg"]);
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(
            c.diff().render(),
            "brew formulae:\n  nethack: installed 3.6.7 -> absent\n"
        );
        assert!(fake.argvs().is_empty(), "check ran {:?}", fake.argvs());
        // The version is the one `check` read from the Cellar, carried in the
        // intent; a name that was never there is reported as already absent.
        let r = op.apply(&s, c).unwrap();
        assert_eq!(r.removed[0].name, "nethack");
        assert_eq!(r.removed[0].version, "3.6.7");
        assert_eq!(r.already_absent, vec!["agg".to_string()]);
        // Only what `check` planned is uninstalled, never the absent `agg`.
        assert_eq!(fake.argvs(), vec![vec![BREW, "uninstall", "nethack"]]);
    }

    #[test]
    fn absent_is_satisfied_when_nothing_is_installed() {
        let fake = mac_fake(&[("agg", &["1.7.0"])]);
        let Plan::Satisfied(report) = Absent::new(["nethack"]).check(&mac_sys(&fake)).unwrap()
        else {
            panic!("expected satisfied")
        };
        assert_eq!(report.already_absent, vec!["nethack".to_string()]);
    }

    /// `brew` is found by probing, not through `PATH`, so a host without the
    /// binary is refused by a message that names where it looked.
    #[test]
    fn refuses_when_the_binary_is_not_at_any_known_path() {
        let fake = Arc::new(Fake::new());
        let s = System::fake(fake, Arc::new(Collect::default())).with_facts(mac_facts());
        let err = Present::new(["nethack"]).check(&s).unwrap_err().chain();
        assert!(err.contains("/opt/homebrew/bin/brew"), "{err}");
        assert!(err.contains("/home/linuxbrew"), "{err}");
    }
}
