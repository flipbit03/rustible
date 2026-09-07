//! Debian/Ubuntu packages via apt. Ansible's `apt` module.

use rustible_sdk::prelude::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    /// Empty when predicted in check mode (apt has not resolved it yet).
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InstallReport {
    /// Packages this step installed.
    pub installed: Vec<Package>,
    /// Packages that were already there.
    pub already_present: Vec<Package>,
}

/// Ensure packages are installed. `apt: state=present`.
#[derive(Debug, Clone)]
pub struct Present {
    names: Vec<String>,
    update_cache: bool,
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
            update_cache: false,
            install_recommends: false,
        }
    }

    /// Run `apt-get update` before installing anything (only if something is missing).
    pub fn update_cache(mut self, on: bool) -> Self {
        self.update_cache = on;
        self
    }

    pub fn install_recommends(mut self, on: bool) -> Self {
        self.install_recommends = on;
        self
    }
}

/// Query dpkg for one package. `None` if not installed.
fn installed_version(sys: &System, name: &str) -> Result<Option<String>> {
    let out = sys
        .cmd("dpkg-query")
        .args(["-W", "-f=${Status}\t${Version}\n", name])
        .allow_failure()
        .run()?;
    if !out.success() {
        return Ok(None); // dpkg-query exits 1 for unknown packages
    }
    let text = out.stdout_str();
    let (status, version) = text
        .trim_end()
        .split_once('\t')
        .unwrap_or((text.trim_end(), ""));
    Ok(status
        .contains("install ok installed")
        .then(|| version.to_string()))
}

impl Op for Present {
    type Output = InstallReport;

    fn check(&self, sys: &System) -> Result<Plan<InstallReport>> {
        if sys.facts().package_manager != Pm::Apt {
            bail!(
                "apt::Present needs apt, but this host uses {:?} ({:?})",
                sys.facts().package_manager,
                sys.facts().distro
            );
        }
        let mut report = InstallReport::default();
        let mut changes = vec![];
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
                    report.installed.push(Package {
                        name: name.clone(),
                        version: String::new(),
                    });
                }
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

    fn apply(&self, sys: &System, change: Change<InstallReport>) -> Result<InstallReport> {
        let mut report = change.predicted.unwrap_or_default();
        let missing: Vec<String> = report.installed.iter().map(|p| p.name.clone()).collect();

        if self.update_cache {
            sys.cmd("apt-get")
                .arg("update")
                .env("DEBIAN_FRONTEND", "noninteractive")
                .run()?;
        }
        let mut cmd = sys
            .cmd("apt-get")
            .args(["install", "-y"])
            .env("DEBIAN_FRONTEND", "noninteractive");
        if !self.install_recommends {
            cmd = cmd.arg("--no-install-recommends");
        }
        cmd.args(missing.iter().cloned()).run()?;

        for p in &mut report.installed {
            p.version = installed_version(sys, &p.name)?.unwrap_or_default();
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

    fn sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

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
    }

    #[test]
    fn refuses_on_non_apt_distro() {
        let fake = Arc::new(Fake::new());
        let mut facts = sys(&fake).facts().clone();
        facts.package_manager = Pm::Apk;
        facts.distro = Distro::Alpine;
        let s = sys(&fake).with_facts(facts);
        let err = Present::new(["mc"]).check(&s).unwrap_err().to_string();
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
}
