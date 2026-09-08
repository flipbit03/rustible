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

/// Output of [`Symlink`]: the desired state echoed back, which is why
/// `check` can predict it in full and `apply` returns the prediction
/// unchanged.
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

impl Op for Symlink {
    type Output = SymlinkReport;

    fn check(&self, sys: &System) -> Result<Plan<SymlinkReport>> {
        let target = self.target.display().to_string();
        let mut changes = vec![];
        match sys.stat(&self.link)? {
            None => changes.push(AttrChange {
                name: "target".into(),
                from: "-".into(),
                to: target,
            }),
            Some(s) if s.kind == FileKind::Symlink => {
                let current = sys.read_link(&self.link)?;
                if current == self.target {
                    return Ok(Plan::Satisfied(self.report()));
                }
                changes.push(AttrChange {
                    name: "target".into(),
                    from: current.display().to_string(),
                    to: target,
                });
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
            Some(s) => {
                changes.push(AttrChange {
                    name: "kind".into(),
                    from: format!("{:?}", s.kind).to_lowercase(),
                    to: "symlink".into(),
                });
                changes.push(AttrChange {
                    name: "target".into(),
                    from: "-".into(),
                    to: target,
                });
            }
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.link.display().to_string(),
                changes,
            },
            self.report(),
        ))
    }

    fn apply(&self, sys: &System, change: Change<SymlinkReport>) -> Result<SymlinkReport> {
        if sys.exists(&self.link)? {
            // Replace atomically: a reader never sees the path missing.
            let mut tmp = self.link.clone().into_os_string();
            tmp.push(format!(".rustible-tmp-{}", std::process::id()));
            let tmp = std::path::PathBuf::from(tmp);
            sys.symlink(&self.target, &tmp)?;
            sys.rename(&tmp, &self.link)?;
        } else {
            sys.symlink(&self.target, &self.link)?;
        }
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
    fn symlink_is_created_when_missing() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Symlink::at("/etc/link").pointing_to("/etc/real");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "target=/etc/real");
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
        assert_eq!(c.diff.render(), "/etc/link:\n  target: /old -> /new\n");
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
        assert_eq!(c.diff.short(), "kind=symlink target=/new");
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
    fn symlink_in_check_mode_predicts_and_creates_nothing() {
        let fake = Arc::new(Fake::new());
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step("link", Symlink::at("/l").pointing_to("/t"))
            .unwrap();
        assert!(r.changed && r.predicted);
        assert_eq!(r.target, PathBuf::from("/t"));
        assert!(fake.file("/l").is_none());
    }

    #[test]
    fn replacing_a_link_is_atomic_and_leaves_no_temp_link() {
        let fake = Arc::new(Fake::new().with_symlink("/etc/l", "/old").with_dir("/new"));
        let sys = fake_sys(&fake);
        let op = Symlink::at("/etc/l").pointing_to("/new");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(
            sys.read_link("/etc/l").unwrap(),
            std::path::PathBuf::from("/new")
        );
        assert!(
            fake.file(format!("/etc/l.rustible-tmp-{}", std::process::id()))
                .is_none()
        );
    }
}
