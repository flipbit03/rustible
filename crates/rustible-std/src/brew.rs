//! Homebrew formulae. Ansible's `community.general.homebrew`.
//!
//! Shaped like [`crate::apt`]: one type per desired state, `check` decides
//! from `brew list` and encodes the decision in the diff, `apply` executes
//! exactly that.
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
    /// The formula name as the op was given it. Nothing is resolved: a tap
    /// or an alias is whatever brew makes of it.
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

/// Pure: parse `brew list --formula --versions`, whose lines are a name then
/// one or more versions separated by spaces (`nethack 3.6.7`). Later versions
/// of the same formula are ignored; the first is what `brew` considers
/// linked. A blank or malformed line is skipped rather than failing the step.
pub fn parse_list_versions(stdout: &str) -> Vec<Formula> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let name = parts.next()?;
            Some(Formula {
                name: name.to_string(),
                version: parts.next().unwrap_or_default().to_string(),
            })
        })
        .collect()
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
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_alphanumeric() || "-_.+@/".contains(*c)))
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

/// Every formula brew currently has installed.
fn installed(sys: &System, brew: &str) -> Result<Vec<Formula>> {
    let out = sys
        .cmd(brew)
        .args(["list", "--formula", "--versions"])
        .run()?;
    Ok(parse_list_versions(&out.stdout_str()))
}

/// The names `check` wrote into the diff, which is how `apply` learns what to
/// do without inspecting the host again.
fn planned_names(diff: &Diff) -> Vec<String> {
    match diff {
        Diff::Attrs { changes, .. } => changes.iter().map(|c| c.name.clone()).collect(),
        _ => vec![],
    }
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

impl Op for Present {
    type Output = InstallReport;

    fn check(&self, sys: &System) -> Result<Plan<InstallReport>> {
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
        let have = installed(sys, &brew)?;
        let mut report = InstallReport::default();
        let mut changes = vec![];
        for name in &self.names {
            match have.iter().find(|f| &f.name == name) {
                Some(f) => report.already_present.push(f.clone()),
                None => changes.push(AttrChange {
                    name: name.clone(),
                    from: "absent".into(),
                    to: "installed".into(),
                }),
            }
        }
        if changes.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        // No prediction: brew resolves the version at install time, and an
        // empty one would be a lie dressed as an answer (vision 12).
        Ok(Plan::change(Diff::Attrs {
            subject: "brew formulae".into(),
            changes,
        }))
    }

    fn apply(&self, sys: &System, change: Change<InstallReport>) -> Result<InstallReport> {
        let brew = brew_bin(sys)?;
        // Install what `check` planned, not what brew says now.
        let missing = planned_names(&change.diff);
        ensure!(
            !missing.is_empty(),
            "brew::Present::apply: the plan names no formula to install"
        );
        sys.cmd(&brew)
            .arg("install")
            .args(missing.iter().cloned())
            .run()?;
        // Read the versions back so the report names what actually landed.
        let now = installed(sys, &brew)?;
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

impl Op for Absent {
    type Output = RemoveReport;

    fn check(&self, sys: &System) -> Result<Plan<RemoveReport>> {
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
        let have = installed(sys, &brew)?;
        let mut report = RemoveReport::default();
        let mut changes = vec![];
        for name in &self.names {
            match have.iter().find(|f| &f.name == name) {
                Some(f) => {
                    changes.push(AttrChange {
                        name: name.clone(),
                        from: format!("installed {}", f.version),
                        to: "absent".into(),
                    });
                    report.removed.push(f.clone());
                }
                None => report.already_absent.push(name.clone()),
            }
        }
        if changes.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        // The versions are known now, so the output is honest to predict.
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: "brew formulae".into(),
                changes,
            },
            report,
        ))
    }

    fn apply(&self, sys: &System, change: Change<RemoveReport>) -> Result<RemoveReport> {
        let brew = brew_bin(sys)?;
        let present = planned_names(&change.diff);
        ensure!(
            !present.is_empty(),
            "brew::Absent::apply: the plan names no formula to remove"
        );
        sys.cmd(&brew)
            .arg("uninstall")
            .args(present.iter().cloned())
            .run()?;
        // `check` always predicts here, so a missing prediction is a bug in
        // this module rather than a state of the host.
        match change.predicted {
            Some(report) => Ok(report),
            None => bail!("brew::Absent::apply: check did not carry its prediction"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;
    use rustible_sdk::facts::{Distro, Facts, Os, Pm};

    use super::*;

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

    /// A `Fake` carrying the brew binary, plus whatever `brew list` should say.
    fn mac_sys(list_stdout: &str) -> System {
        let fake = Arc::new(
            Fake::new()
                .with_file("/opt/homebrew/bin/brew", "")
                .with_cmd("/opt/homebrew/bin/brew", None, 0, list_stdout),
        );
        System::fake(fake, Arc::new(Collect::default())).with_facts(mac_facts())
    }

    // ---- pure ----

    #[test]
    fn list_versions_parses_name_and_first_version() {
        let out = parse_list_versions("agg 1.7.0\nnethack 3.6.7\nopenssl@3 3.6.1 3.5.0\n\n");
        assert_eq!(
            out,
            vec![
                Formula {
                    name: "agg".into(),
                    version: "1.7.0".into()
                },
                Formula {
                    name: "nethack".into(),
                    version: "3.6.7".into()
                },
                Formula {
                    name: "openssl@3".into(),
                    version: "3.6.1".into()
                },
            ]
        );
    }

    #[test]
    fn a_formula_with_no_version_column_is_still_a_formula() {
        assert_eq!(
            parse_list_versions("cowsay\n"),
            vec![Formula {
                name: "cowsay".into(),
                version: String::new()
            }]
        );
    }

    #[test]
    fn formula_names_that_are_refused() {
        assert!(validate_formula("nethack").is_ok());
        assert!(validate_formula("openssl@3").is_ok());
        assert!(validate_formula("homebrew/cask/firefox").is_ok());
        assert!(validate_formula("").unwrap_err().contains("empty"));
        assert!(validate_formula("--force").unwrap_err().contains("dash"));
        assert!(validate_formula("a b").unwrap_err().contains("not legal"));
    }

    // ---- Fake ----

    #[test]
    fn satisfied_when_the_formula_is_already_installed() {
        let s = mac_sys("nethack 3.6.7\n");
        let op = Present::new(["nethack"]);
        let Plan::Satisfied(report) = op.check(&s).unwrap() else {
            panic!("expected satisfied")
        };
        assert!(report.installed.is_empty());
        assert_eq!(report.already_present[0].version, "3.6.7");
    }

    /// The plan names only what is missing, and `apply` installs exactly
    /// that — not everything the op was given.
    ///
    /// Note what this test cannot do: `Present` reads its state with a
    /// command, and the `Fake` answers every `brew` invocation with the same
    /// canned stdout, so changed-then-ok is not expressible at this tier
    /// (CLAUDE.md, "the Fake models files well and commands badly"). The
    /// second run is proved against a real mac instead.
    #[test]
    fn change_names_the_missing_formula_and_apply_installs_exactly_that() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/opt/homebrew/bin/brew", "")
                .with_cmd("/opt/homebrew/bin/brew", None, 0, "agg 1.7.0\n"),
        );
        let s = System::fake(fake.clone(), Arc::new(Collect::default())).with_facts(mac_facts());
        let op = Present::new(["ninvaders", "agg"]);

        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        // Only the missing one is in the plan, and the diff is what `apply`
        // reads its work from.
        assert_eq!(planned_names(&c.diff), vec!["ninvaders".to_string()]);
        // No prediction: brew resolves the version at install time.
        assert!(c.predicted.is_none());

        let report = op.apply(&s, c).unwrap();
        let argvs = fake.argvs();
        assert!(
            argvs.contains(&vec![
                "/opt/homebrew/bin/brew".to_string(),
                "install".to_string(),
                "ninvaders".to_string(),
            ]),
            "{argvs:?}"
        );
        // `agg` was already there and is reported as such, never installed.
        assert_eq!(report.already_present[0].name, "agg");
        assert_eq!(report.installed[0].name, "ninvaders");
    }

    #[test]
    fn refuses_as_root_naming_homebrews_own_reason() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/opt/homebrew/bin/brew", "")
                .with_cmd("/opt/homebrew/bin/brew", None, 0, ""),
        );
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
        let fake = Arc::new(
            Fake::new()
                .with_file("/home/linuxbrew/.linuxbrew/bin/brew", "")
                .with_cmd(
                    "/home/linuxbrew/.linuxbrew/bin/brew",
                    None,
                    0,
                    "nethack 3.6.7\n",
                ),
        );
        let facts = Facts {
            os: Os::Linux,
            distro: Distro::Debian,
            package_managers: [Pm::Apt, Pm::Brew].into_iter().collect(),
            is_root: false,
            user: "cadu".into(),
            ..mac_facts()
        };
        let s = System::fake(fake, Arc::new(Collect::default())).with_facts(facts);
        assert!(matches!(
            Present::new(["nethack"]).check(&s).unwrap(),
            Plan::Satisfied(_)
        ));
    }

    #[test]
    fn absent_predicts_the_version_it_is_about_to_remove() {
        let s = mac_sys("nethack 3.6.7\n");
        let Plan::Change(c) = Absent::new(["nethack"]).check(&s).unwrap() else {
            panic!("expected change")
        };
        let predicted = c.predicted.as_ref().expect("Absent always predicts");
        assert_eq!(predicted.removed[0].version, "3.6.7");
        assert_eq!(
            c.diff.render(),
            "brew formulae:\n  nethack: installed 3.6.7 -> absent\n"
        );
    }

    #[test]
    fn absent_is_satisfied_when_nothing_is_installed() {
        let s = mac_sys("agg 1.7.0\n");
        let Plan::Satisfied(report) = Absent::new(["nethack"]).check(&s).unwrap() else {
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
