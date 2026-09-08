//! `file::Directory`: Ansible's `file` with `state: directory`.

use std::path::PathBuf;

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

use super::{Owner, apply_attrs, plan_attrs};

/// Ensure a directory exists, with the given mode and owner. Ansible's
/// `file` with `state: directory`. Creates missing parents (`mkdir -p`).
/// Fails if the path exists and is not a directory (vision 6.7: it does
/// not remove things in the way).
#[derive(Debug, Clone)]
pub struct Directory {
    path: PathBuf,
    mode: Option<u32>,
    owner: Option<Owner>,
}

impl Directory {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Directory {
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirReport {
    pub path: PathBuf,
    pub created: bool,
}

impl Op for Directory {
    type Output = DirReport;

    fn check(&self, sys: &System) -> Result<Plan<DirReport>> {
        let mut changes = vec![];
        let stat = sys.stat_follow(&self.path)?;
        let created = match &stat {
            None => {
                changes.push(AttrChange {
                    name: "exists".into(),
                    from: "no".into(),
                    to: "yes".into(),
                });
                true
            }
            Some(s) if s.kind != FileKind::Dir => {
                bail!("{} exists and is not a directory", self.path.display())
            }
            Some(_) => false,
        };
        changes.extend(plan_attrs(stat.as_ref(), self.mode, self.owner));
        if changes.is_empty() {
            return Ok(Plan::Satisfied(DirReport {
                path: self.path.clone(),
                created: false,
            }));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.path.display().to_string(),
                changes,
            },
            DirReport {
                path: self.path.clone(),
                created,
            },
        ))
    }

    fn apply(&self, sys: &System, change: Change<DirReport>) -> Result<DirReport> {
        let created = change.predicted.map(|p| p.created).unwrap_or(false);
        if created {
            sys.mkdir_all(&self.path)?;
        }
        apply_attrs(sys, &self.path, self.mode, self.owner)?;
        Ok(DirReport {
            path: self.path.clone(),
            created,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;

    use super::super::testing::{expect_change, fake_sys};
    use super::*;

    #[test]
    fn directory_is_created_with_mode_and_owner() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Directory::at("/srv/app").mode(0o750).owner(33, 33);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "exists=yes mode=0750 owner=33:33");
        assert!(c.predicted.as_ref().unwrap().created);
        let r = op.apply(&sys, c).unwrap();
        assert!(r.created);
        let f = fake.file("/srv/app").unwrap();
        assert_eq!(
            (f.kind, f.mode, f.uid, f.gid),
            (FileKind::Dir, 0o750, 33, 33)
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn directory_owner_change_on_existing_dir() {
        let fake = Arc::new(Fake::new().with_dir("/srv/app"));
        let sys = fake_sys(&fake);
        let op = Directory::at("/srv/app").owner(1000, 1000);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "owner=1000:1000");
        assert!(!c.predicted.as_ref().unwrap().created);
        let r = op.apply(&sys, c).unwrap();
        assert!(!r.created);
        let f = fake.file("/srv/app").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o755, 1000, 1000));
    }

    #[test]
    fn directory_fails_on_file_in_the_way() {
        let fake = Arc::new(Fake::new().with_file("/srv/app", ""));
        let sys = fake_sys(&fake);
        let err = Directory::at("/srv/app")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn a_symlink_to_a_directory_counts_as_the_directory() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/run/lock")
                .with_symlink("/var/lock", "/run/lock"),
        );
        let sys = fake_sys(&fake);
        assert!(matches!(
            Directory::at("/var/lock").check(&sys).unwrap(),
            Plan::Satisfied(_)
        ));
    }
}
