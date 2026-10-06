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
        let attrs = plan_attrs(stat.as_ref(), self.mode, self.owner).on_a_directory();
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

    use rustible_sdk::backend::{AttrCall, Backend, Fake};

    use super::super::testing::{chown_refused_sys, expect_change, fake_sys};
    use super::*;

    /// `/srv/shared` at `mode`, owned by root, and the `attr_calls` offset
    /// past the planting.
    fn shared_dir(mode: u32) -> (Arc<Fake>, usize) {
        let fake = Arc::new(Fake::new().with_dir("/srv/shared"));
        fake.set_mode(std::path::Path::new("/srv/shared"), mode)
            .unwrap();
        let planted = fake.attr_calls().len();
        (fake, planted)
    }

    /// A setgid directory gets its full mode first and its owner after it
    /// (issue #79). `chown(2)` keeps setgid on a directory, so clearing it
    /// first would only open a moment where files created in it get the
    /// creator's group, and a refused `chown` would leave the bit off. A
    /// directory the op creates is treated the same.
    #[test]
    fn a_setgid_directory_gets_its_full_mode_before_its_owner() {
        let calls = |path: &str| {
            [
                AttrCall::Chmod {
                    path: path.into(),
                    mode: 0o2775,
                },
                AttrCall::Chown {
                    path: path.into(),
                    uid: 5,
                    gid: 6,
                },
            ]
        };
        let (fake, planted) = shared_dir(0o2775);
        let sys = fake_sys(&fake);
        let op = Directory::at("/srv/shared").mode(0o2775).owner(5, 6);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.attr_calls()[planted..], calls("/srv/shared"));
        let d = fake.file("/srv/shared").unwrap();
        assert_eq!((d.mode, d.uid, d.gid), (0o2775, 5, 6));

        let (fake, planted) = shared_dir(0o755);
        let sys = fake_sys(&fake);
        let op = Directory::at("/srv/shared/new").mode(0o2775).owner(5, 6);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.attr_calls()[planted..], calls("/srv/shared/new"));
    }

    /// With the `chown` refused, a setgid directory keeps the bit: the step
    /// fails with the mode already whole, rather than at 0775.
    #[test]
    fn a_refused_chown_leaves_a_setgid_directory_setgid() {
        let (fake, _) = shared_dir(0o2775);
        let sys = chown_refused_sys(&fake);
        let op = Directory::at("/srv/shared").mode(0o2775).owner(5, 6);
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("Operation not permitted"), "{err}");
        let d = fake.file("/srv/shared").unwrap();
        assert_eq!((d.mode, d.uid, d.gid), (0o2775, 0, 0));
    }

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
        assert!(r.created, "the intent's `create` is what apply executes");
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
