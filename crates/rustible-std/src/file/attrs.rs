//! `file::Attrs`: Ansible's `file` with `state: file`.

use std::path::PathBuf;

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

use super::{Owner, apply_attrs, plan_attrs};

/// Ensure an existing path has the given mode and owner. Ansible's `file`
/// with `state: file` (or `state: directory` on a directory that must
/// already exist). Never creates anything: a missing path is an error
/// (vision 6.7; use [`super::Copy`] or [`super::Directory`] to create it).
/// With neither `.mode()` nor `.owner()` it is an existence assertion.
///
/// ```no_run
/// # use rustible_std::file;
/// let op = file::Attrs::at("/etc/ssh/sshd_config").mode(0o600).owner(0, 0);
/// ```
///
/// Symbolic links are refused rather than followed, so the report can never
/// show a change that `chmod` would then apply to the link's target.
#[derive(Debug, Clone)]
pub struct Attrs {
    path: PathBuf,
    mode: Option<u32>,
    owner: Option<Owner>,
}

/// Output of [`Attrs`]. It carries only the path: the op sets exactly what
/// it was told to set, so there is nothing to learn from the result that the
/// op does not already say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrsReport {
    /// The path whose attributes were checked, as given to [`Attrs::at`].
    pub path: PathBuf,
}

impl Attrs {
    /// Start an `Attrs` op on a path that must already exist. With neither
    /// `.mode()` nor `.owner()` added, the op asserts that the path exists
    /// and is not a symbolic link, and reports `ok`.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Attrs {
            path: path.into(),
            mode: None,
            owner: None,
        }
    }

    /// Permission bits as an octal literal (`0o600`). Only the low twelve
    /// bits are compared and set, so the file type bits of a value read out
    /// of a `stat` do not matter. Left unset, the mode is neither checked
    /// nor changed.
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Numeric owner (`chown uid:gid`).
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some(Owner { uid, gid });
        self
    }

    fn report(&self) -> AttrsReport {
        AttrsReport {
            path: self.path.clone(),
        }
    }
}

impl Op for Attrs {
    type Output = AttrsReport;

    fn check(&self, sys: &System) -> Result<Plan<AttrsReport>> {
        // Portable. file::Attrs sets POSIX mode and owner through `sys`.
        // The supported set is written out rather than left open, so a new
        // platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!("file::Attrs has no implementation for {}", other.name()),
        }
        let Some(stat) = sys.stat(&self.path)? else {
            // Under --check an earlier step may create the path (vision 12):
            // report the attributes it would get. A real run refuses.
            if sys.check_mode() {
                let changes = plan_attrs(None, self.mode, self.owner);
                if changes.is_empty() {
                    return Ok(Plan::Satisfied(self.report()));
                }
                return Ok(Plan::change(Diff::Attrs {
                    subject: self.path.display().to_string(),
                    changes,
                }));
            }
            bail!(
                "{} does not exist; file::Attrs only sets attributes (create it with file::Copy or file::Directory)",
                self.path.display()
            );
        };
        if stat.kind == FileKind::Symlink {
            bail!(
                "{} is a symbolic link; file::Attrs does not follow links (point it at the target instead)",
                self.path.display()
            );
        }
        let changes = plan_attrs(Some(&stat), self.mode, self.owner);
        if changes.is_empty() {
            return Ok(Plan::Satisfied(self.report()));
        }
        Ok(Plan::change(Diff::Attrs {
            subject: self.path.display().to_string(),
            changes,
        }))
    }

    fn apply(&self, sys: &System, _: Change) -> Result<AttrsReport> {
        apply_attrs(sys, &self.path, self.mode, self.owner)?;
        Ok(self.report())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::super::testing::{expect_change, fake_sys};
    use super::*;

    /// The two platform claims every portable op makes, in one place: it
    /// runs on a mac, and it refuses a platform nobody has claimed rather
    /// than assuming. `Attrs` is plain file work through `sys`, so the mac
    /// half is the same test as on Linux with different facts.
    fn on(os: Os) -> (std::sync::Arc<Fake>, System) {
        let fake = std::sync::Arc::new(Fake::new().with_file_mode("/etc/x", "a\n", 0o644));
        let base = fake_sys(&fake);
        let mut facts = base.facts().clone();
        facts.os = os;
        (fake.clone(), base.with_facts(facts))
    }

    #[test]
    fn runs_on_a_mac_and_refuses_an_unclaimed_platform() {
        let (fake, sys) = on(Os::Macos);
        let op = Attrs::at("/etc/x").mode(0o600);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.file("/etc/x").unwrap().mode, 0o600);
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));

        let (_, sys) = on(Os::Other("freebsd".into()));
        let err = Attrs::at("/etc/x")
            .mode(0o600)
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("file::Attrs has no implementation for freebsd"),
            "{err}"
        );
    }

    #[test]
    fn attrs_is_satisfied_when_matching_or_nothing_asked() {
        let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o600));
        let sys = fake_sys(&fake);
        assert!(matches!(
            Attrs::at("/f").mode(0o600).owner(0, 0).check(&sys).unwrap(),
            Plan::Satisfied(_)
        ));
        // Existence assertion only.
        assert!(matches!(
            Attrs::at("/f").check(&sys).unwrap(),
            Plan::Satisfied(_)
        ));
    }

    #[test]
    fn attrs_changes_mode_and_owner_then_is_satisfied() {
        let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o644));
        let sys = fake_sys(&fake);
        let op = Attrs::at("/f").mode(0o600).owner(1000, 1000);
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.diff.render(),
            "/f:\n  mode: 0644 -> 0600\n  owner: 0:0 -> 1000:1000\n"
        );
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.path, PathBuf::from("/f"));
        let f = fake.file("/f").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o600, 1000, 1000));
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn attrs_only_touches_what_was_asked() {
        let fake = Arc::new(Fake::new().with_dir("/d"));
        let sys = fake_sys(&fake);
        let op = Attrs::at("/d").owner(5, 6);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "owner=5:6");
        op.apply(&sys, c).unwrap();
        let f = fake.file("/d").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o755, 5, 6));
    }

    #[test]
    fn attrs_fails_on_missing_path_and_on_symlink() {
        let fake = Arc::new(Fake::new().with_symlink("/l", "/t"));
        let sys = fake_sys(&fake);
        let err = Attrs::at("/missing")
            .mode(0o600)
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
        let err = Attrs::at("/l")
            .mode(0o600)
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("symbolic link"), "{err}");
    }

    #[test]
    fn attrs_in_check_mode_reports_would_change_and_changes_nothing() {
        let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o644));
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx.step("attrs", Attrs::at("/f").mode(0o600)).unwrap();
        assert!(r.changed && !r.is_available(), "no apply, so no output");
        assert_eq!(r.diff.as_ref().unwrap().short(), "mode=0600");
        assert_eq!(fake.file("/f").unwrap().mode, 0o644);
    }

    /// Vision 12: a path an earlier step in the run could create is not a
    /// refusal in a dry run; the op reports the attributes it would set. A
    /// real run still refuses, because it is about to act.
    #[test]
    fn attrs_on_a_missing_path_defers_the_refusal_to_a_real_run() {
        let fake = Arc::new(Fake::new());
        let dry = fake_sys(&fake).with_check_mode(true);
        let op = Attrs::at("/missing").mode(0o644).owner(1, 2);
        let c = expect_change(&op, &dry);
        assert_eq!(
            c.diff.render(),
            "/missing:\n  mode: - -> 0644\n  owner: - -> 1:2\n"
        );
        // Nothing asked beyond existence: nothing to report either.
        assert!(matches!(
            Attrs::at("/missing").check(&dry).unwrap(),
            Plan::Satisfied(_)
        ));

        let real = fake_sys(&fake);
        let err = op.check(&real).unwrap_err().chain();
        assert!(
            err.contains("/missing does not exist; file::Attrs only sets attributes"),
            "{err}"
        );
    }
}
