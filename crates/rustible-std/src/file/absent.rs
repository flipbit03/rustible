//! `file::Absent`: Ansible's `file` with `state: absent`.

use std::path::PathBuf;

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

/// Ensure nothing exists at `path`: removes a file, a symbolic link (not
/// what it points at), or a directory. Ansible's `file` with
/// `state: absent`. Unlike Ansible, a non-empty directory is refused
/// unless `.recursive(true)`, so a typo cannot take a tree with it.
///
/// ```no_run
/// # use rustible_std::file;
/// let op = file::Absent::at("/etc/nginx/sites-enabled/default");
/// let tree = file::Absent::at("/var/cache/old").recursive(true);
/// ```
#[derive(Debug, Clone)]
pub struct Absent {
    path: PathBuf,
    recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsentReport {
    pub path: PathBuf,
    /// False when there was nothing to remove.
    pub removed: bool,
}

impl Absent {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Absent {
            path: path.into(),
            recursive: false,
        }
    }

    /// Allow removing a directory that still has entries (`rm -r`).
    pub fn recursive(mut self, on: bool) -> Self {
        self.recursive = on;
        self
    }

    fn report(&self, removed: bool) -> AbsentReport {
        AbsentReport {
            path: self.path.clone(),
            removed,
        }
    }
}

impl Op for Absent {
    type Output = AbsentReport;

    fn check(&self, sys: &System) -> Result<Plan<AbsentReport>> {
        let Some(stat) = sys.stat(&self.path)? else {
            return Ok(Plan::Satisfied(self.report(false)));
        };
        let mut changes = vec![AttrChange {
            name: "exists".into(),
            from: format!("yes ({})", format!("{:?}", stat.kind).to_lowercase()),
            to: "no".into(),
        }];
        if stat.kind == FileKind::Dir {
            let entries = sys.read_dir(&self.path)?.len();
            if entries > 0 && !self.recursive {
                bail!(
                    "{} is a directory with {entries} entries; use .recursive(true) to remove it",
                    self.path.display()
                );
            }
            if entries > 0 {
                changes.push(AttrChange {
                    name: "entries".into(),
                    from: entries.to_string(),
                    to: "0".into(),
                });
            }
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.path.display().to_string(),
                changes,
            },
            self.report(true),
        ))
    }

    fn apply(&self, sys: &System, change: Change<AbsentReport>) -> Result<AbsentReport> {
        sys.remove(&self.path)?;
        Ok(change.predicted.unwrap_or_else(|| self.report(true)))
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
    fn absent_is_satisfied_when_missing() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let Plan::Satisfied(r) = Absent::at("/nope").check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert!(!r.removed);
    }

    #[test]
    fn absent_removes_a_file_and_a_symlink() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/f", "x")
                .with_symlink("/etc/l", "/etc/f"),
        );
        let sys = fake_sys(&fake);

        let op = Absent::at("/etc/l");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.render(), "/etc/l:\n  exists: yes (symlink) -> no\n");
        assert!(op.apply(&sys, c).unwrap().removed);
        assert!(fake.file("/etc/l").is_none());
        assert!(
            fake.file("/etc/f").is_some(),
            "removing a link keeps its target"
        );

        let op = Absent::at("/etc/f");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "exists=no");
        assert!(op.apply(&sys, c).unwrap().removed);
        assert!(fake.file("/etc/f").is_none());
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn absent_removes_an_empty_directory_without_recursive() {
        let fake = Arc::new(Fake::new().with_dir("/empty"));
        let sys = fake_sys(&fake);
        let op = Absent::at("/empty");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "exists=no");
        op.apply(&sys, c).unwrap();
        assert!(fake.file("/empty").is_none());
    }

    #[test]
    fn absent_refuses_non_empty_directory_unless_recursive() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/d")
                .with_file("/d/a", "")
                .with_file("/d/b", ""),
        );
        let sys = fake_sys(&fake);
        let err = Absent::at("/d").check(&sys).unwrap_err().to_string();
        assert!(err.contains("2 entries; use .recursive(true)"), "{err}");

        let op = Absent::at("/d").recursive(true);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "exists=no entries=0");
        op.apply(&sys, c).unwrap();
        assert!(fake.file("/d").is_none() && fake.file("/d/a").is_none());
    }

    #[test]
    fn absent_in_check_mode_predicts_and_removes_nothing() {
        let fake = Arc::new(Fake::new().with_file("/f", "x"));
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx.step("rm", Absent::at("/f")).unwrap();
        assert!(r.changed && r.predicted && r.removed);
        assert!(fake.file("/f").is_some());
    }
}
