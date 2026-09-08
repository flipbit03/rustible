//! `file::Copy`: Ansible's `copy` (with `src` or `content`).

use std::path::{Path, PathBuf};

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

use super::{Owner, apply_attrs, plan_attrs};

/// Above this size a content change is reported as a byte-count summary
/// instead of a unified diff, whatever the encoding.
pub const TEXT_DIFF_LIMIT: usize = 64 * 1024;

/// Where the bytes come from.
#[derive(Debug, Clone)]
pub enum CopySource {
    /// Bytes baked into the binary (`include_bytes!`, `include_str!`, or a
    /// string built at run time). Vision 5.6 mechanism 1.
    Bytes(Vec<u8>),
    /// A path read **on the target** at check time (vision 5.1: the playbook
    /// runs there, so this is not the operator's machine). For a file that
    /// only exists on the controller, embed it or stream it with
    /// `ctx.local_file` instead.
    LocalPath(PathBuf),
}

/// Ensure `dest` holds exactly these bytes, optionally with a mode and owner.
/// Ansible's `copy` with `src`/`content`, `mode`, `owner`/`group`, `backup`.
///
/// ```no_run
/// # use rustible_std::file;
/// let op = file::Copy::from_str("net.ipv4.ip_forward = 1\n")
///     .to("/etc/sysctl.d/99-forward.conf")
///     .mode(0o644);
/// ```
///
/// `check` compares bytes and, when given, mode and owner. A content change
/// is shown as a unified diff when both old and new content are UTF-8 and
/// under `TEXT_DIFF_LIMIT`, otherwise as `<n> bytes -> <m> bytes`. Fails
/// if `dest` exists and is not a regular file (vision 6.7: use
/// [`super::Absent`] first to replace a directory or a link).
#[derive(Debug, Clone)]
pub struct Copy {
    source: CopySource,
    dest: PathBuf,
    mode: Option<u32>,
    owner: Option<Owner>,
    backup: bool,
}

/// A `Copy` with a source but no destination yet; `.to(dest)` finishes it.
#[derive(Debug, Clone)]
pub struct CopyBuilder {
    source: CopySource,
}

/// Output of [`struct@Copy`]. `check` predicts all of it except
/// `backup_path`, which cannot exist before `apply` has taken the copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyReport {
    /// Whether the bytes were (or would be) rewritten, as opposed to an
    /// attributes-only change.
    pub content_changed: bool,
    /// The destination, as given to `.to(..)`.
    pub path: PathBuf,
    /// Set only when `.backup(true)` and a previous version was saved.
    pub backup_path: Option<PathBuf>,
    /// Size of the content now at `path`.
    pub bytes: usize,
}

impl Copy {
    /// Content from bytes, typically `include_bytes!("../files/x")`.
    pub fn from_bytes(bytes: impl AsRef<[u8]>) -> CopyBuilder {
        CopyBuilder {
            source: CopySource::Bytes(bytes.as_ref().to_vec()),
        }
    }

    /// Content from a string, typically `include_str!` or a `format!`.
    /// Ansible's `content:`.
    #[allow(
        clippy::should_implement_trait,
        reason = "not a parser; mirrors from_bytes and the vision's naming"
    )]
    pub fn from_str(text: &str) -> CopyBuilder {
        Self::from_bytes(text.as_bytes())
    }

    /// Content read from a file on the target at check time.
    pub fn from_local_path(path: impl Into<PathBuf>) -> CopyBuilder {
        CopyBuilder {
            source: CopySource::LocalPath(path.into()),
        }
    }

    /// Permission bits as an octal literal (`0o644`). Only the low twelve
    /// bits are compared and set. Left unset, the mode is neither checked
    /// nor changed: an existing file keeps its own, and a file this op
    /// creates keeps whatever the write gave it.
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Numeric owner (`chown uid:gid`).
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some(Owner { uid, gid });
        self
    }

    /// Keep a copy of the previous content next to the file before
    /// overwriting it (`<name>.~rustible.<unix-ts>`).
    pub fn backup(mut self, on: bool) -> Self {
        self.backup = on;
        self
    }

    fn source_bytes(&self, sys: &System) -> Result<Vec<u8>> {
        match &self.source {
            CopySource::Bytes(b) => Ok(b.clone()),
            CopySource::LocalPath(p) => sys
                .read(p)
                .with_context(|| format!("reading copy source {}", p.display())),
        }
    }
}

impl CopyBuilder {
    /// The destination path. Finishes the builder.
    pub fn to(self, dest: impl Into<PathBuf>) -> Copy {
        Copy {
            source: self.source,
            dest: dest.into(),
            mode: None,
            owner: None,
            backup: false,
        }
    }
}

/// The diff for a content change: unified text when both sides are small
/// UTF-8, a byte-count summary otherwise.
pub fn content_diff(path: &Path, old: Option<&[u8]>, new: &[u8]) -> Diff {
    let old = old.unwrap_or_default();
    if old.len() <= TEXT_DIFF_LIMIT
        && new.len() <= TEXT_DIFF_LIMIT
        && let (Ok(before), Ok(after)) = (std::str::from_utf8(old), std::str::from_utf8(new))
    {
        return Diff::text(path, before, after);
    }
    Diff::summary(format!(
        "{}: {} bytes -> {} bytes",
        path.display(),
        old.len(),
        new.len()
    ))
}

impl Op for Copy {
    type Output = CopyReport;

    fn check(&self, sys: &System) -> Result<Plan<CopyReport>> {
        let new = self.source_bytes(sys)?;
        let stat = sys.stat(&self.dest)?;
        let old = match &stat {
            None => None,
            Some(s) if s.kind == FileKind::File => Some(sys.read(&self.dest)?),
            Some(s) => bail!(
                "{} exists and is not a regular file ({:?}); remove it first with file::Absent",
                self.dest.display(),
                s.kind
            ),
        };
        let content_changed = old.as_deref() != Some(new.as_slice());
        let attrs = plan_attrs(stat.as_ref(), self.mode, self.owner);
        let report = CopyReport {
            content_changed,
            path: self.dest.clone(),
            backup_path: None,
            bytes: new.len(),
        };
        if !content_changed && attrs.is_empty() {
            return Ok(Plan::Satisfied(report));
        }
        let diff = if content_changed {
            content_diff(&self.dest, old.as_deref(), &new)
        } else {
            Diff::Attrs {
                subject: self.dest.display().to_string(),
                changes: attrs,
            }
        };
        Ok(Plan::change_predicting(diff, report))
    }

    fn apply(&self, sys: &System, change: Change<CopyReport>) -> Result<CopyReport> {
        // Branch on the plan, not on the diff's presentation: recompute the
        // comparison when no prediction is at hand.
        let bytes = self.source_bytes(sys)?;
        let rewrite = match &change.predicted {
            Some(p) => p.content_changed,
            None => match sys.stat(&self.dest)? {
                Some(s) if s.kind == FileKind::File => sys.read(&self.dest)? != bytes,
                _ => true,
            },
        };
        let backup_path = if rewrite {
            super::write_with_backup(sys, &self.dest, self.backup, &bytes)?
        } else {
            None
        };
        apply_attrs(sys, &self.dest, self.mode, self.owner)?;
        Ok(CopyReport {
            content_changed: rewrite,
            path: self.dest.clone(),
            backup_path,
            bytes: bytes.len(),
        })
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
    fn copy_is_satisfied_when_bytes_and_attrs_match() {
        let fake = Arc::new(Fake::new().with_file_mode("/etc/x.conf", "a=1\n", 0o600));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("a=1\n")
            .to("/etc/x.conf")
            .mode(0o600)
            .owner(0, 0);
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(
            r,
            CopyReport {
                content_changed: false,
                path: "/etc/x.conf".into(),
                backup_path: None,
                bytes: 4
            }
        );
    }

    #[test]
    fn copy_text_change_has_unified_diff_and_predicts() {
        let fake = Arc::new(Fake::new().with_file("/etc/x.conf", "a=1\nb=2\n"));
        let sys = fake_sys(&fake);
        let op = Copy::from_bytes(b"a=1\nb=3\n").to("/etc/x.conf");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "+1 -1 lines");
        assert!(
            c.diff.render().contains("-b=2\n+b=3"),
            "{}",
            c.diff.render()
        );
        assert_eq!(c.predicted.as_ref().unwrap().bytes, 8);

        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.bytes, 8);
        assert_eq!(r.backup_path, None);
        assert_eq!(fake.content("/etc/x.conf").unwrap(), "a=1\nb=3\n");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn copy_creates_missing_file_with_mode() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Copy::from_str("hello\n").to("/new").mode(0o640);
        let c = expect_change(&op, &sys);
        // Creation is a text diff against an empty "before".
        assert_eq!(c.diff.short(), "+1 -0 lines");
        op.apply(&sys, c).unwrap();
        let f = fake.file("/new").unwrap();
        assert_eq!(
            (f.mode, String::from_utf8(f.bytes).unwrap()),
            (0o640, "hello\n".into())
        );
    }

    #[test]
    fn copy_binary_content_produces_summary_diff() {
        let fake = Arc::new(Fake::new().with_file("/bin/blob", [0u8, 159, 146, 150]));
        let sys = fake_sys(&fake);
        let op = Copy::from_bytes([0u8, 159, 146, 150, 1, 2]).to("/bin/blob");
        let c = expect_change(&op, &sys);
        assert!(matches!(&c.diff, Diff::Summary(s) if s == "/bin/blob: 4 bytes -> 6 bytes"));
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.file("/bin/blob").unwrap().bytes.len(), 6);
    }

    #[test]
    fn copy_large_text_falls_back_to_summary() {
        let big = "x".repeat(TEXT_DIFF_LIMIT + 1);
        let d = content_diff(Path::new("/f"), Some(b"small"), big.as_bytes());
        assert!(matches!(d, Diff::Summary(_)));
        let d = content_diff(Path::new("/f"), None, b"small");
        assert!(matches!(d, Diff::Text { .. }));
    }

    #[test]
    fn copy_rewrite_preserves_existing_mode_when_none_given() {
        let fake = Arc::new(Fake::new().with_file_mode("/etc/shadowish", "old\n", 0o600));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("new\n").to("/etc/shadowish");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        let f = fake.file("/etc/shadowish").unwrap();
        assert_eq!(
            f.mode, 0o600,
            "write_atomic keeps the mode of an existing file"
        );
        assert_eq!(String::from_utf8(f.bytes).unwrap(), "new\n");
    }

    #[test]
    fn copy_attrs_only_change_does_not_rewrite() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/etc")
                .with_file_mode("/etc/x", "same\n", 0o644),
        );
        let sys = fake_sys(&fake);
        let op = Copy::from_str("same\n")
            .to("/etc/x")
            .mode(0o600)
            .owner(7, 8)
            .backup(true);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "mode=0600 owner=7:8");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.backup_path, None, "no content change, so no backup");
        let f = fake.file("/etc/x").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o600, 7, 8));
        // No backup file appeared next to it.
        assert_eq!(sys.read_dir("/etc").unwrap(), vec![PathBuf::from("/etc/x")]);
    }

    #[test]
    fn copy_backup_is_taken_before_rewrite() {
        let fake = Arc::new(Fake::new().with_file("/etc/x", "old\n"));
        let sys = fake_sys(&fake);
        let op = Copy::from_str("new\n").to("/etc/x").backup(true);
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.predicted.as_ref().unwrap().backup_path,
            None,
            "path unknown until apply"
        );
        let r = op.apply(&sys, c).unwrap();
        let bp = r.backup_path.expect("backup path");
        assert!(
            bp.to_string_lossy().starts_with("/etc/x.~rustible."),
            "{}",
            bp.display()
        );
        assert_eq!(fake.content(&bp).unwrap(), "old\n");
        assert_eq!(fake.content("/etc/x").unwrap(), "new\n");
    }

    #[test]
    fn copy_from_local_path_reads_source_through_sys() {
        let fake = Arc::new(Fake::new().with_file("/opt/src.conf", "from source\n"));
        let sys = fake_sys(&fake);
        let op = Copy::from_local_path("/opt/src.conf").to("/etc/dst.conf");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/etc/dst.conf").unwrap(), "from source\n");

        let err = Copy::from_local_path("/opt/missing")
            .to("/etc/dst.conf")
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(err.contains("reading copy source /opt/missing"), "{err}");
    }

    #[test]
    fn copy_fails_when_dest_is_a_directory_or_link() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/etc/d")
                .with_symlink("/etc/l", "/etc/real"),
        );
        let sys = fake_sys(&fake);
        let err = Copy::from_str("x")
            .to("/etc/d")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file (Dir)"), "{err}");
        let err = Copy::from_str("x")
            .to("/etc/l")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a regular file (Symlink)"), "{err}");
    }

    #[test]
    fn copy_in_check_mode_predicts_and_writes_nothing() {
        let fake = Arc::new(Fake::new().with_file("/f", "a\n"));
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step("copy", Copy::from_str("bb\n").to("/f").mode(0o600))
            .unwrap();
        assert!(r.changed && r.predicted);
        assert_eq!(r.bytes, 3);
        let f = fake.file("/f").unwrap();
        assert_eq!((f.mode, f.bytes.as_slice()), (0o644, b"a\n".as_slice()));
    }
}
