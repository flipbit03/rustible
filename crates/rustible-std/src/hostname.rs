//! The machine's hostname. Ansible's `ansible.builtin.hostname`.
//!
//! [`Is`] makes both the static hostname (`/etc/hostname`) and the kernel's
//! (`/proc/sys/kernel/hostname`) equal the given name. On systemd hosts it
//! goes through `hostnamectl set-hostname`, which writes both; elsewhere it
//! writes `/etc/hostname` and runs `hostname <name>`.

use rustible_sdk::prelude::*;

const ETC_HOSTNAME: &str = "/etc/hostname";
const KERNEL_HOSTNAME: &str = "/proc/sys/kernel/hostname";

/// Output of [`Is`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostnameReport {
    /// The kernel hostname before this step (equal to `current` when nothing changed).
    pub previous: String,
    /// The hostname now.
    pub current: String,
}

/// Ensure the hostname is `name`. `hostname: name=...`.
///
/// Refuses names that are not valid hostnames: one or more RFC 1123 labels
/// (letters, digits, hyphens; no leading or trailing hyphen; at most 63
/// characters each) joined by dots, at most 64 characters in all, which is
/// the kernel's `HOST_NAME_MAX` and what `hostnamectl` accepts. Needs root.
#[derive(Debug, Clone)]
pub struct Is {
    name: String,
}

impl Is {
    /// Ensure the machine is called `name`. The name is checked against
    /// [`validate_hostname`] at `check`, not here, so building the op never
    /// fails however malformed the argument is.
    pub fn new(name: impl Into<String>) -> Self {
        Is { name: name.into() }
    }
}

/// Why a string is not a hostname. Pure, so it is testable with strings.
pub fn validate_hostname(name: &str) -> std::result::Result<(), String> {
    if name.is_empty() {
        return Err("hostname is empty".into());
    }
    // The kernel's HOST_NAME_MAX is 64, and systemd refuses anything longer,
    // so a DNS-legal 253-character name is not a legal hostname.
    if name.len() > 64 {
        return Err(format!(
            "hostname is {} characters, the limit is 64 (the kernel's HOST_NAME_MAX)",
            name.len()
        ));
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err(format!(
                "`{name}` has an empty label (leading, trailing or doubled dot)"
            ));
        }
        if label.len() > 63 {
            return Err(format!(
                "label `{label}` is {} characters, the limit is 63",
                label.len()
            ));
        }
        if let Some(bad) = label
            .chars()
            .find(|c| !(c.is_ascii_alphanumeric() || *c == '-'))
        {
            return Err(format!(
                "`{name}` contains `{bad}`; only letters, digits and hyphens are allowed"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!("label `{label}` starts or ends with a hyphen"));
        }
    }
    Ok(())
}

/// First line of a file through `sys`, trimmed; `None` when it does not exist.
fn read_name(sys: &System, path: &str) -> Result<Option<String>> {
    if !sys.exists(path)? {
        return Ok(None);
    }
    let text = sys.read_to_string(path)?;
    Ok(Some(text.lines().next().unwrap_or("").trim().to_string()))
}

impl Op for Is {
    type Output = HostnameReport;

    fn check(&self, sys: &System) -> Result<Plan<HostnameReport>> {
        if let Err(why) = validate_hostname(&self.name) {
            bail!("hostname::Is: {why}");
        }
        if !sys.is_root() {
            bail!(
                "hostname::Is needs root, but this runs as `{}` (use escalate = true or ctx.as_root())",
                sys.facts().user
            );
        }
        let in_file = read_name(sys, ETC_HOSTNAME)?;
        // Read the live value rather than `facts.hostname` so a second check
        // in the same run sees what apply did.
        let kernel =
            read_name(sys, KERNEL_HOSTNAME)?.unwrap_or_else(|| sys.facts().hostname.clone());

        let mut changes = vec![];
        if in_file.as_deref() != Some(self.name.as_str()) {
            changes.push(AttrChange {
                name: ETC_HOSTNAME.into(),
                from: in_file.unwrap_or_else(|| "absent".into()),
                to: self.name.clone(),
            });
        }
        if kernel != self.name {
            changes.push(AttrChange {
                name: "kernel".into(),
                from: kernel.clone(),
                to: self.name.clone(),
            });
        }
        let report = HostnameReport {
            previous: kernel,
            current: self.name.clone(),
        };
        if changes.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: "hostname".into(),
                changes,
            },
            report,
        ))
    }

    fn apply(&self, sys: &System, change: Change<HostnameReport>) -> Result<HostnameReport> {
        let Some(report) = change.predicted else {
            bail!("hostname::Is::apply received a change without its prediction");
        };
        if sys.facts().init == Init::Systemd {
            sys.cmd("hostnamectl")
                .args(["set-hostname", &self.name])
                .run()?;
        } else {
            sys.write_atomic(ETC_HOSTNAME, format!("{}\n", self.name).as_bytes())?;
            sys.cmd("hostname").arg(&self.name).run()?;
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

    fn openrc(fake: &Arc<Fake>) -> System {
        let mut facts = sys(fake).facts().clone();
        facts.init = Init::OpenRc;
        facts.distro = Distro::Alpine;
        sys(fake).with_facts(facts)
    }

    fn box_named(file: &str, kernel: &str) -> Fake {
        Fake::new()
            .with_file(ETC_HOSTNAME, format!("{file}\n"))
            .with_file(KERNEL_HOSTNAME, format!("{kernel}\n"))
    }

    // ---- pure ----

    #[test]
    fn valid_hostnames() {
        for n in [
            "HOME-GAMES",
            "ourserver",
            "a",
            "host01.example.com",
            "x-1-y",
            "123",
        ] {
            assert_eq!(validate_hostname(n), Ok(()), "{n}");
        }
        assert_eq!(validate_hostname(&"a".repeat(63)), Ok(()));
    }

    #[test]
    fn invalid_hostnames() {
        let bad = |n: &str, needle: &str| {
            let err = validate_hostname(n).unwrap_err();
            assert!(err.contains(needle), "{n}: {err}");
        };
        bad("", "empty");
        bad("home_games", "`_`");
        bad("home games", "` `");
        bad("-games", "hyphen");
        bad("games-", "hyphen");
        bad("a..b", "empty label");
        bad(".a", "empty label");
        bad(&"a".repeat(64), "limit is 63");
        let long = ["a".repeat(63).as_str(); 5].join(".");
        bad(&long, "limit is 64");
        // 63 legal labels, 65 characters in all: DNS says yes, the kernel no.
        bad(&["a".repeat(32).as_str(); 2].join("."), "HOST_NAME_MAX");
    }

    // ---- Fake ----

    #[test]
    fn satisfied_when_file_and_kernel_match() {
        let fake = Arc::new(box_named("HOME-GAMES", "HOME-GAMES"));
        let Plan::Satisfied(r) = Is::new("HOME-GAMES").check(&sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.previous, "HOME-GAMES");
        assert_eq!(r.current, "HOME-GAMES");
        assert!(fake.argvs().is_empty());
    }

    #[test]
    fn change_when_only_the_file_differs() {
        let fake = Arc::new(box_named("old", "HOME-GAMES"));
        let Plan::Change(c) = Is::new("HOME-GAMES").check(&sys(&fake)).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(
            c.diff.render(),
            "hostname:\n  /etc/hostname: old -> HOME-GAMES\n"
        );
        assert_eq!(c.predicted.unwrap().previous, "HOME-GAMES");
    }

    #[test]
    fn change_when_only_the_kernel_differs() {
        let fake = Arc::new(box_named("HOME-GAMES", "old"));
        let Plan::Change(c) = Is::new("HOME-GAMES").check(&sys(&fake)).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "kernel=HOME-GAMES");
        assert_eq!(c.predicted.unwrap().previous, "old");
    }

    #[test]
    fn missing_etc_hostname_reads_as_absent() {
        let fake = Arc::new(Fake::new().with_file(KERNEL_HOSTNAME, "old\n"));
        let Plan::Change(c) = Is::new("new").check(&sys(&fake)).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(
            c.diff.render(),
            "hostname:\n  /etc/hostname: absent -> new\n  kernel: old -> new\n"
        );
    }

    #[test]
    fn kernel_falls_back_to_facts_when_proc_is_unreadable() {
        let fake = Arc::new(Fake::new().with_file(ETC_HOSTNAME, "fake\n"));
        // System::fake's facts say hostname = "fake".
        assert!(matches!(
            Is::new("fake").check(&sys(&fake)).unwrap(),
            Plan::Satisfied(_)
        ));
    }

    #[test]
    fn apply_on_systemd_uses_hostnamectl() {
        let fake = Arc::new(box_named("old", "old").with_cmd("hostnamectl", None, 0, ""));
        let s = sys(&fake);
        let op = Is::new("HOME-GAMES");
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!()
        };
        let r = op.apply(&s, c).unwrap();
        assert_eq!(
            r,
            HostnameReport {
                previous: "old".into(),
                current: "HOME-GAMES".into()
            }
        );
        assert_eq!(
            fake.argvs(),
            vec![vec!["hostnamectl", "set-hostname", "HOME-GAMES"]]
        );
        // hostnamectl owns /etc/hostname; the op does not write it itself.
        assert_eq!(fake.content(ETC_HOSTNAME).unwrap(), "old\n");
    }

    #[test]
    fn apply_without_systemd_writes_file_and_runs_hostname() {
        let fake = Arc::new(box_named("old", "old").with_cmd("hostname", None, 0, ""));
        let s = openrc(&fake);
        let op = Is::new("HOME-GAMES");
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!()
        };
        op.apply(&s, c).unwrap();
        assert_eq!(fake.content(ETC_HOSTNAME).unwrap(), "HOME-GAMES\n");
        assert_eq!(fake.argvs(), vec![vec!["hostname", "HOME-GAMES"]]);
    }

    #[test]
    fn apply_creates_etc_hostname_when_missing() {
        let fake = Arc::new(
            Fake::new()
                .with_file(KERNEL_HOSTNAME, "old\n")
                .with_cmd("hostname", None, 0, ""),
        );
        let s = openrc(&fake);
        let op = Is::new("new");
        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!()
        };
        op.apply(&s, c).unwrap();
        assert_eq!(fake.content(ETC_HOSTNAME).unwrap(), "new\n");
    }

    #[test]
    fn refuses_invalid_name_before_touching_anything() {
        let fake = Arc::new(box_named("old", "old"));
        let err = Is::new("home games")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.starts_with("hostname::Is:"), "{err}");
        assert!(err.contains("` `"), "{err}");
    }

    #[test]
    fn refuses_without_root() {
        let fake = Arc::new(box_named("old", "old"));
        let mut facts = sys(&fake).facts().clone();
        facts.is_root = false;
        facts.user = "cadu".into();
        let s = sys(&fake).with_facts(facts);
        let err = Is::new("new").check(&s).unwrap_err().to_string();
        assert!(err.contains("needs root") && err.contains("cadu"), "{err}");
    }

    #[test]
    fn apply_without_prediction_is_refused() {
        let fake = Arc::new(Fake::new().with_cmd("hostnamectl", None, 0, ""));
        let err = Is::new("x")
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
    fn check_mode_predicts_and_runs_nothing() {
        let fake = Arc::new(box_named("old", "old").with_cmd("hostnamectl", None, 0, ""));
        let s = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(s, rustible_sdk::HostInfo::local());
        let r = ctx.step("name", Is::new("HOME-GAMES")).unwrap();
        assert!(r.changed && r.predicted && r.is_available());
        assert_eq!(r.previous, "old");
        assert_eq!(r.current, "HOME-GAMES");
        assert!(fake.argvs().is_empty(), "check must run no commands");
        assert_eq!(fake.content(ETC_HOSTNAME).unwrap(), "old\n");
    }

    #[test]
    fn step_is_ok_on_second_run_after_apply() {
        // Without systemd the fake box really changes: the op rewrites the
        // file, and we simulate the kernel side, so a second step is `ok`.
        let fake = Arc::new(box_named("old", "old").with_cmd("hostname", None, 0, ""));
        let mut ctx = Ctx::new(openrc(&fake), rustible_sdk::HostInfo::local());
        let r = ctx.step("name", Is::new("new")).unwrap();
        assert!(r.changed);
        rustible_sdk::backend::Backend::write(
            &*fake,
            std::path::Path::new(KERNEL_HOSTNAME),
            b"new\n",
        )
        .unwrap();
        let r = ctx.step("name", Is::new("new")).unwrap();
        assert!(!r.changed);
    }
}
