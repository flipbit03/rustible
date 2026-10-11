//! `file::Symlink`: Ansible's `file` with `state: link`.

use std::path::PathBuf;

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

/// Ensure `link` is a symbolic link pointing at `target`. Ansible's `file`
/// with `state: link`, `src`, `dest`, `force`.
///
/// ```no_run
/// # use rustible_std::file;
/// let op = file::Symlink::at("/etc/nginx/sites-enabled/app")
///     .pointing_to("/etc/nginx/sites-available/app");
/// ```
///
/// A link pointing elsewhere is replaced. A regular file in the way is an
/// error unless `.force(true)`, which replaces it; a directory in the way
/// is always an error (as Ansible refuses too). The target need not exist.
#[derive(Debug, Clone)]
pub struct Symlink {
    link: PathBuf,
    target: PathBuf,
    force: bool,
}

/// A `Symlink` without a target yet; `.pointing_to(target)` finishes it.
#[derive(Debug, Clone)]
pub struct SymlinkBuilder {
    link: PathBuf,
}

/// Output of [`Symlink`]: the desired state echoed back, since a link has
/// nothing else to report once it points where it was told to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymlinkReport {
    /// The link itself, as given to [`Symlink::at`].
    pub link: PathBuf,
    /// What the link now points at, verbatim as given: a relative target
    /// stays relative and is never resolved against the link's directory.
    pub target: PathBuf,
}

impl Symlink {
    /// Start a symlink op at this path; [`SymlinkBuilder::pointing_to`]
    /// supplies the target and finishes it. `force` starts off, so anything
    /// already sitting at `link` that is not a symlink is an error.
    pub fn at(link: impl Into<PathBuf>) -> SymlinkBuilder {
        SymlinkBuilder { link: link.into() }
    }

    /// Replace a regular file sitting at the link path.
    pub fn force(mut self, on: bool) -> Self {
        self.force = on;
        self
    }

    fn report(&self) -> SymlinkReport {
        SymlinkReport {
            link: self.link.clone(),
            target: self.target.clone(),
        }
    }
}

impl SymlinkBuilder {
    /// Where the link points. Finishes the builder.
    pub fn pointing_to(self, target: impl Into<PathBuf>) -> Symlink {
        Symlink {
            link: self.link,
            target: target.into(),
            force: false,
        }
    }
}

/// What [`Symlink`]'s `check` decided, by what it found at the link's path.
/// Its contents are private: only `check` builds one.
#[derive(Debug)]
pub struct SymlinkIntent(Link);

#[derive(Debug)]
enum Link {
    /// Nothing is there: create the link.
    Create {
        /// The link's own path.
        link: PathBuf,
        /// Where it will point.
        target: PathBuf,
    },
    /// A link pointing elsewhere: replace it atomically.
    Retarget {
        /// The link's own path.
        link: PathBuf,
        /// Where it points now.
        from: PathBuf,
        /// Where it will point.
        target: PathBuf,
    },
    /// Something other than a link, and the op has `.force(true)`: replace
    /// it atomically.
    Replace {
        /// The link's own path.
        link: PathBuf,
        /// What is there now.
        kind: FileKind,
        /// Where the link will point.
        target: PathBuf,
    },
}

impl Intent for SymlinkIntent {
    fn diff(&self) -> Diff {
        let (link, changes) = match &self.0 {
            Link::Create { link, target } => (
                link,
                vec![AttrChange::new("target", "-", target.display().to_string())],
            ),
            Link::Retarget { link, from, target } => (
                link,
                vec![AttrChange::new(
                    "target",
                    from.display().to_string(),
                    target.display().to_string(),
                )],
            ),
            Link::Replace { link, kind, target } => (
                link,
                vec![
                    AttrChange::new("kind", format!("{kind:?}").to_lowercase(), "symlink"),
                    AttrChange::new("target", "-", target.display().to_string()),
                ],
            ),
        };
        Diff::attrs(link.display().to_string(), changes)
    }
}

impl Op for Symlink {
    type Output = SymlinkReport;
    type Intent = SymlinkIntent;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable. file::Symlink creates a POSIX symlink through `sys`.
        // The supported set is written out rather than left open, so a new
        // platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!("file::Symlink has no implementation for {}", other.name()),
        }
        let link = self.link.clone();
        let target = self.target.clone();
        let intent = match sys.stat(&self.link)? {
            None => Link::Create { link, target },
            Some(s) if s.kind == FileKind::Symlink => {
                let current = sys.read_link(&self.link)?;
                if current == self.target {
                    return Ok(Plan::Satisfied(self.report()));
                }
                Link::Retarget {
                    link,
                    from: current,
                    target,
                }
            }
            Some(s) if s.kind == FileKind::Dir => bail!(
                "{} is a directory; refusing to replace it with a symlink",
                self.link.display()
            ),
            Some(s) if !self.force => bail!(
                "{} exists and is not a symlink ({:?}); use .force(true) to replace it",
                self.link.display(),
                s.kind
            ),
            Some(s) => Link::Replace {
                link,
                kind: s.kind,
                target,
            },
        };
        Ok(Plan::Change(SymlinkIntent(intent)))
    }

    fn apply(&self, sys: &System, intent: SymlinkIntent) -> Result<SymlinkReport> {
        match intent.0 {
            Link::Create { link, target } => sys.symlink(&target, &link)?,
            Link::Retarget { link, target, .. } | Link::Replace { link, target, .. } => {
                // Replace atomically: a reader never sees the path missing.
                // The error names the link, not only the temporary name
                // beside it that a failed `symlink` reports.
                super::replace_with_symlink(sys, &target, &link).with_context(|| {
                    format!(
                        "replacing {} with a symlink to {}",
                        link.display(),
                        target.display()
                    )
                })?
            }
        }
        Ok(self.report())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::super::testing::{Failing, expect_change, fake_sys};
    use super::*;

    /// The two platform claims every portable op makes, in one place: it
    /// runs on a mac, and it refuses a platform nobody has claimed rather
    /// than assuming. `Symlink` is plain file work through `sys`, so the mac
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
        let op = Symlink::at("/opt/link").pointing_to("/opt/target");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.file("/opt/link").unwrap().kind,
            rustible_sdk::backend::FileKind::Symlink
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));

        let (_, sys) = on(Os::Other("freebsd".into()));
        let err = Symlink::at("/opt/link")
            .pointing_to("/opt/target")
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("file::Symlink has no implementation for freebsd"),
            "{err}"
        );
    }

    #[test]
    fn symlink_is_created_when_missing() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Symlink::at("/etc/link").pointing_to("/etc/real");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().short(), "target=/etc/real");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(
            r,
            SymlinkReport {
                link: "/etc/link".into(),
                target: "/etc/real".into()
            }
        );
        assert_eq!(
            sys.read_link("/etc/link").unwrap(),
            PathBuf::from("/etc/real")
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn symlink_with_wrong_target_is_replaced() {
        let fake = Arc::new(Fake::new().with_symlink("/etc/link", "/old"));
        let sys = fake_sys(&fake);
        let op = Symlink::at("/etc/link").pointing_to("/new");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().render(), "/etc/link:\n  target: /old -> /new\n");
        op.apply(&sys, c).unwrap();
        assert_eq!(sys.read_link("/etc/link").unwrap(), PathBuf::from("/new"));
    }

    #[test]
    fn symlink_over_file_needs_force() {
        let fake = Arc::new(Fake::new().with_file("/etc/link", "i am a file"));
        let sys = fake_sys(&fake);
        let op = Symlink::at("/etc/link").pointing_to("/new");
        let err = op.check(&sys).unwrap_err().to_string();
        assert!(err.contains("use .force(true)"), "{err}");

        let op = op.force(true);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff().short(), "kind=symlink target=/new");
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.file("/etc/link").unwrap().kind, FileKind::Symlink);
        assert_eq!(sys.read_link("/etc/link").unwrap(), PathBuf::from("/new"));
    }

    #[test]
    fn symlink_never_replaces_a_directory() {
        let fake = Arc::new(Fake::new().with_dir("/etc/link"));
        let sys = fake_sys(&fake);
        let op = Symlink::at("/etc/link").pointing_to("/new").force(true);
        let err = op.check(&sys).unwrap_err().to_string();
        assert!(err.contains("is a directory"), "{err}");
    }

    #[test]
    fn symlink_in_check_mode_reports_would_change_and_creates_nothing() {
        let fake = Arc::new(Fake::new());
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = rustible_sdk::testing::ctx(sys);
        let r = ctx
            .step("link", Symlink::at("/l").pointing_to("/t"))
            .unwrap();
        assert!(r.changed && !r.is_available(), "no apply, so no output");
        assert_eq!(r.diff.as_ref().unwrap().short(), "target=/t");
        assert!(fake.file("/l").is_none());
    }

    #[test]
    fn replacing_a_link_is_atomic_and_leaves_no_temp_link() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/etc")
                .with_symlink("/etc/l", "/old")
                .with_dir("/new"),
        );
        let sys = fake_sys(&fake);
        let op = Symlink::at("/etc/l").pointing_to("/new");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(sys.read_link("/etc/l").unwrap(), PathBuf::from("/new"));
        assert_eq!(
            sys.read_dir("/etc").unwrap(),
            vec![PathBuf::from("/etc/l")],
            "only the link is left in its directory"
        );
    }

    /// A link whose name is near `NAME_MAX` (255 bytes) is still repointed:
    /// the temporary link beside it has a short name of its own, not the
    /// link's with a suffix that would push it over.
    #[test]
    fn a_link_with_a_long_name_is_repointed() {
        let link = PathBuf::from("/etc").join("l".repeat(250));
        let fake = Arc::new(Fake::new().with_dir("/etc").with_symlink(&link, "/old"));
        let sys = Failing::sys(&fake, false, false);
        let op = Symlink::at(&link).pointing_to("/new");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(sys.read_link(&link).unwrap(), PathBuf::from("/new"));
        assert_eq!(sys.read_dir("/etc").unwrap(), vec![link]);
    }

    /// A temporary link a killed run left under the old pid-derived name
    /// is not in the way: the name is random, so it is never reused.
    #[test]
    fn a_stale_temp_link_from_a_killed_run_does_not_matter() {
        let stale = format!("/etc/l.rustible-tmp-{}", std::process::id());
        let fake = Arc::new(
            Fake::new()
                .with_symlink("/etc/l", "/old")
                .with_symlink(&stale, "/stale"),
        );
        let sys = fake_sys(&fake);
        let op = Symlink::at("/etc/l").pointing_to("/new");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(sys.read_link("/etc/l").unwrap(), PathBuf::from("/new"));
        assert_eq!(sys.read_link(&stale).unwrap(), PathBuf::from("/stale"));
    }

    /// The entry at the link's path, a link or (forced) a file, is replaced
    /// whole or not at all: a failed `symlink` leaves it untouched, and a
    /// failed `rename` leaves it too and takes the temporary link away, so
    /// nothing named `.rustible-*` is left beside it. The error names the
    /// link.
    #[test]
    fn a_failed_replace_leaves_the_old_entry_and_no_temp_link() {
        for old in ["symlink", "file"] {
            for (symlink, rename, refused) in [(true, false, "symlink"), (false, true, "rename")] {
                let fake = Fake::new().with_dir("/etc");
                let fake = Arc::new(match old {
                    "symlink" => fake.with_symlink("/etc/l", "/old"),
                    _ => fake.with_file("/etc/l", "old contents"),
                });
                let before = fake.file("/etc/l").unwrap();
                let sys = Failing::sys(&fake, symlink, rename);
                let op = Symlink::at("/etc/l").pointing_to("/new").force(true);
                let c = expect_change(&op, &sys);
                let err = op.apply(&sys, c).unwrap_err().chain();
                assert!(
                    err.contains("replacing /etc/l with a symlink to /new: "),
                    "{err}"
                );
                assert!(err.contains(&format!("{refused} ")), "{err}");
                assert!(err.contains("refused (test)"), "{err}");
                let after = fake.file("/etc/l").unwrap();
                assert_eq!(
                    (after.kind, after.bytes),
                    (before.kind, before.bytes),
                    "{old} after a failed {refused}"
                );
                assert_eq!(
                    sys.read_dir("/etc").unwrap(),
                    vec![PathBuf::from("/etc/l")],
                    "no temporary link left beside the {old} after a failed {refused}"
                );
            }
        }
    }
}
