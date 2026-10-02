//! `file::Directory`: Ansible's `file` with `state: directory`.

use std::path::PathBuf;

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

use super::{AttrPlan, Owner, plan_attrs};

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
    /// Start a `Directory` op on this path. With neither `.mode()` nor
    /// `.owner()` added it only ensures the directory (and its missing
    /// parents) exists, leaving the permissions of one that is already there
    /// alone.
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Directory {
            path: path.into(),
            mode: None,
            owner: None,
        }
    }

    /// Permission bits as an octal literal (`0o750`). Only the low twelve
    /// bits are compared and set, so the file type bits of a value read out
    /// of a `stat` do not matter. Left unset, the mode is neither checked
    /// nor changed, and a directory this op creates keeps whatever `mkdir`
    /// gave it.
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

/// Output of [`Directory`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirReport {
    /// The directory, as given to [`Directory::at`].
    pub path: PathBuf,
    /// True only when this step made the directory. A step that found the
    /// directory already there and merely fixed its mode or owner reports
    /// `false` while still counting as `changed`.
    pub created: bool,
}

/// What [`Directory`]'s `check` decided: whether to create the directory,
/// and the attributes to set on it.
#[derive(Debug)]
pub struct DirectoryIntent {
    path: PathBuf,
    /// Nothing is at the path: `mkdir -p` it.
    create: bool,
    attrs: AttrPlan,
}

impl Intent for DirectoryIntent {
    fn diff(&self) -> Diff {
        let mut changes = vec![];
        if self.create {
            changes.push(AttrChange::new("exists", "no", "yes"));
        }
        changes.extend(self.attrs.changes());
        Diff::attrs(self.path.display().to_string(), changes)
    }
}

impl Op for Directory {
    type Output = DirReport;
    type Intent = DirectoryIntent;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable. file::Directory creates a directory through `sys` and sets mode and owner.
        // The supported set is written out rather than left open, so a new
        // platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!("file::Directory has no implementation for {}", other.name()),
        }
        let stat = sys.stat_follow(&self.path)?;
        if let Some(s) = &stat
            && s.kind != FileKind::Dir
        {
            bail!("{} exists and is not a directory", self.path.display())
        }
        let create = stat.is_none();
        let attrs = plan_attrs(stat.as_ref(), self.mode, self.owner);
        if !create && !attrs.differs() {
            return Ok(Plan::Satisfied(DirReport {
                path: self.path.clone(),
                created: false,
            }));
        }
        Ok(Plan::Change(DirectoryIntent {
            path: self.path.clone(),
            create,
            attrs,
        }))
    }

    fn apply(&self, sys: &System, intent: DirectoryIntent) -> Result<DirReport> {
        if intent.create {
            sys.mkdir_all(&intent.path)?;
        }
        intent.attrs.apply(sys, &intent.path)?;
        Ok(DirReport {
            path: intent.path,
            created: intent.create,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;

    use super::super::testing::{expect_change, fake_sys};
    use super::*;

    /// The two platform claims every portable op makes, in one place: it
    /// runs on a mac, and it refuses a platform nobody has claimed rather
    /// than assuming. `Directory` is plain file work through `sys`, so the mac
    /// half is the same test as on Linux with different facts.
    fn on(os: Os) -> (std::sync::Arc<Fake>, System) {
        let fake = std::sync::Arc::new(Fake::new().with_dir("/opt"));
        let base = fake_sys(&fake);
        let mut facts = base.facts().clone();
        facts.os = os;
        (fake.clone(), base.with_facts(facts))
    }

    #[test]
    fn runs_on_a_mac_and_refuses_an_unclaimed_platform() {
        let (fake, sys) = on(Os::Macos);
        let op = Directory::at("/opt/x");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.file("/opt/x").unwrap().kind,
            rustible_sdk::backend::FileKind::Dir
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));

        let (_, sys) = on(Os::Other("freebsd".into()));
        let err = Directory::at("/opt/x").check(&sys).unwrap_err().chain();
        assert!(
            err.contains("file::Directory has no implementation for freebsd"),
            "{err}"
        );
    }

    #[test]
    fn directory_is_created_with_mode_and_owner() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Directory::at("/srv/app").mode(0o750).owner(33, 33);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().short(), "exists=yes mode=0750 owner=33:33");
        let r = op.apply(&sys, c).unwrap();
        assert!(
            r.created,
            "the intent's `create` is what apply executes"
        );
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
        assert_eq!(c.diff().short(), "owner=1000:1000");
        let r = op.apply(&sys, c).unwrap();
        assert!(!r.created, "the intent does not create, so no mkdir");
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
