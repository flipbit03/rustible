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

/// Output of [`Absent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsentReport {
    /// The path that is now absent, as given to [`Absent::at`].
    pub path: PathBuf,
    /// False when there was nothing to remove.
    pub removed: bool,
}

impl Absent {
    /// Start an `Absent` op on this path. Not recursive: a directory that
    /// still has entries is refused at `check` until `.recursive(true)`.
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

/// What [`Absent`]'s `check` decided: remove what it found at the path, as
/// a single entry or as a tree.
#[derive(Debug)]
pub struct AbsentIntent {
    path: PathBuf,
    /// What `check` found there.
    kind: FileKind,
    /// Entries `check` counted in a directory; 0 for anything else.
    entries: usize,
    /// Remove a directory and everything in it (`rm -r`), which only a
    /// `.recursive(true)` op on a directory decides. Otherwise one entry is
    /// removed, and the kernel refuses a directory populated since `check`.
    tree: bool,
}

impl Intent for AbsentIntent {
    fn diff(&self) -> Diff {
        let mut changes = vec![AttrChange::new(
            "exists",
            format!("yes ({})", format!("{:?}", self.kind).to_lowercase()),
            "no",
        )];
        if self.entries > 0 {
            changes.push(AttrChange::new("entries", self.entries.to_string(), "0"));
        }
        Diff::attrs(self.path.display().to_string(), changes)
    }
}

impl Op for Absent {
    type Output = AbsentReport;
    type Intent = AbsentIntent;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable. file::Absent removes a path through `sys`.
        // The supported set is written out rather than left open, so a new
        // platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!("file::Absent has no implementation for {}", other.name()),
        }
        let Some(stat) = sys.stat(&self.path)? else {
            return Ok(Plan::Satisfied(self.report(false)));
        };
        let mut entries = 0;
        if stat.kind == FileKind::Dir {
            entries = sys.read_dir(&self.path)?.len();
            if entries > 0 && !self.recursive {
                bail!(
                    "{} is a directory with {entries} entries; use .recursive(true) to remove it",
                    self.path.display()
                );
            }
        }
        Ok(Plan::Change(AbsentIntent {
            path: self.path.clone(),
            kind: stat.kind,
            entries,
            tree: self.recursive && stat.kind == FileKind::Dir,
        }))
    }

    fn apply(&self, sys: &System, intent: AbsentIntent) -> Result<AbsentReport> {
        // The kernel is the floor: without `.recursive(true)` a populated
        // directory fails here too, not only at check.
        if intent.tree {
            sys.remove_all(&intent.path)?;
        } else {
            sys.remove(&intent.path)?;
        }
        Ok(self.report(true))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::{Backend, Fake};
    use rustible_sdk::event::Collect;

    use super::super::testing::{expect_change, fake_sys};
    use super::*;

    /// The two platform claims every portable op makes, in one place: it
    /// runs on a mac, and it refuses a platform nobody has claimed rather
    /// than assuming. `Absent` is plain file work through `sys`, so the mac
    /// half is the same test as on Linux with different facts.
    fn on(os: Os) -> (std::sync::Arc<Fake>, System) {
        let fake = std::sync::Arc::new(Fake::new().with_file("/etc/x", "a\n"));
        let base = fake_sys(&fake);
        let mut facts = base.facts().clone();
        facts.os = os;
        (fake.clone(), base.with_facts(facts))
    }

    #[test]
    fn runs_on_a_mac_and_refuses_an_unclaimed_platform() {
        let (fake, sys) = on(Os::Macos);
        let op = Absent::at("/etc/x");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        op.apply(&sys, c).unwrap();
        assert!(fake.content("/etc/x").is_none());
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));

        let (_, sys) = on(Os::Other("freebsd".into()));
        let err = Absent::at("/etc/x").check(&sys).unwrap_err().chain();
        assert!(
            err.contains("file::Absent has no implementation for freebsd"),
            "{err}"
        );
    }

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
        assert_eq!(
            c.diff().render(),
            "/etc/l:\n  exists: yes (symlink) -> no\n"
        );
        assert!(op.apply(&sys, c).unwrap().removed);
        assert!(fake.file("/etc/l").is_none());
        assert!(
            fake.file("/etc/f").is_some(),
            "removing a link keeps its target"
        );

        let op = Absent::at("/etc/f");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().short(), "exists=no");
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
        assert_eq!(c.diff().short(), "exists=no");
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
        assert_eq!(c.diff().short(), "exists=no entries=0");
        op.apply(&sys, c).unwrap();
        assert!(fake.file("/d").is_none() && fake.file("/d/a").is_none());
    }

    #[test]
    fn absent_in_check_mode_reports_would_change_and_removes_nothing() {
        let fake = Arc::new(Fake::new().with_file("/f", "x"));
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx.step("rm", Absent::at("/f")).unwrap();
        assert!(r.changed && !r.is_available(), "no apply, so no output");
        assert_eq!(r.diff.as_ref().unwrap().short(), "exists=no");
        assert!(fake.file("/f").is_some());
    }

    /// `.recursive(true)` is permission to take a tree, and the intent uses
    /// it only for what `check` found: a file at `check` is removed as one
    /// entry. A populated directory that appeared there since is refused by
    /// the kernel rather than taken with `rm -r`.
    #[test]
    fn recursive_apply_takes_no_tree_check_did_not_see() {
        let fake = Arc::new(Fake::new().with_file("/d", "x"));
        let sys = fake_sys(&fake);
        let op = Absent::at("/d").recursive(true);
        let c = expect_change(&op, &sys);
        Backend::remove(&*fake, std::path::Path::new("/d")).unwrap();
        Backend::mkdir_all(&*fake, std::path::Path::new("/d")).unwrap();
        Backend::write(&*fake, std::path::Path::new("/d/late"), b"x").unwrap();
        assert!(op.apply(&sys, c).is_err(), "no tree was planned");
        assert!(fake.file("/d/late").is_some());
    }

    #[test]
    fn non_recursive_apply_refuses_a_directory_populated_after_check() {
        let fake = Arc::new(Fake::new().with_dir("/d"));
        let sys = fake_sys(&fake);
        let op = Absent::at("/d");
        let c = expect_change(&op, &sys);
        // Something lands in the directory between check and apply.
        Backend::write(&*fake, std::path::Path::new("/d/late"), b"x").unwrap();
        assert!(op.apply(&sys, c).is_err(), "remove must not take the tree");
        assert!(fake.file("/d/late").is_some());
        // Recursive removal is explicit.
        let op = Absent::at("/d").recursive(true);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert!(fake.file("/d").is_none() && fake.file("/d/late").is_none());
    }
}
