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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrsReport {
    pub path: PathBuf,
}

impl Attrs {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Attrs {
            path: path.into(),
            mode: None,
            owner: None,
        }
    }

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
        let Some(stat) = sys.stat(&self.path)? else {
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
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.path.display().to_string(),
                changes,
            },
            self.report(),
        ))
    }

    fn apply(&self, sys: &System, change: Change<AttrsReport>) -> Result<AttrsReport> {
        apply_attrs(sys, &self.path, self.mode, self.owner)?;
        Ok(change.predicted.unwrap_or_else(|| self.report()))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::super::testing::{expect_change, fake_sys};
    use super::*;

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
        assert_eq!(c.predicted.as_ref().unwrap().path, PathBuf::from("/f"));
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
    fn attrs_in_check_mode_predicts_and_changes_nothing() {
        let fake = Arc::new(Fake::new().with_file_mode("/f", "", 0o644));
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx.step("attrs", Attrs::at("/f").mode(0o600)).unwrap();
        assert!(r.changed && r.predicted);
        assert_eq!(r.path, PathBuf::from("/f"));
        assert_eq!(fake.file("/f").unwrap().mode, 0o644);
    }
}
