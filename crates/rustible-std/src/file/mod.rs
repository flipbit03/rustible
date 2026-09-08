//! File operations: the Rustible side of Ansible's `copy`, `file`,
//! `lineinfile` and `blockinfile` modules. One type per desired state
//! (vision 6.3):
//!
//! | Ansible | Rustible |
//! |---|---|
//! | `copy` (with `src` or `content`) | [`Copy`] |
//! | `file: state=directory` | [`Directory`] |
//! | `file: state=link` | [`Symlink`] |
//! | `file: state=absent` | [`Absent`] |
//! | `file: state=file` (attributes only) | [`Attrs`] |
//! | `lineinfile: state=present` | [`Line`] |
//! | `blockinfile` | [`Block`] |
//!
//! All I/O goes through [`System`] (vision 7). Every op predicts its output
//! so chained steps keep working in check mode (vision 12).
//!
//! TODO(m6-harness): once the Docker harness (PR #5) is on main, add
//! `tests/it_file_copy.rs` and `tests/it_file_block.rs` doing changed-then-ok
//! on `debian:12` and `ubuntu:24.04`, in the shape of `tests/it_file_line.rs`
//! from that branch. Not written here so the branch does not carry a test
//! that cannot run.

use std::path::Path;

use regex::Regex;
use rustible_sdk::backend::Stat;
use rustible_sdk::prelude::*;

mod absent;
mod attrs;
mod block;
mod copy;
mod directory;
mod line;
mod symlink;

pub use absent::{Absent, AbsentReport};
pub use attrs::{Attrs, AttrsReport};
pub use block::{Block, BlockBuilder, BlockReport, plan_block};
pub use copy::{Copy, CopyReport, CopySource};
pub use directory::{DirReport, Directory};
pub use line::{Line, LineBuilder, LineReport, plan_line};
pub use symlink::{Symlink, SymlinkBuilder, SymlinkReport};

/// Where to put a line (or block) that is not present yet.
#[derive(Debug, Clone)]
pub enum Insert {
    Append,
    Prepend,
    After(Regex),
    Before(Regex),
}

impl Insert {
    /// Index at which to insert into `lines` when the thing is absent.
    /// `After` takes the last match (Ansible's `insertafter`), `Before` the
    /// first (`insertbefore`); no match falls back to the end of the file.
    pub(crate) fn position(&self, lines: &[String]) -> usize {
        match self {
            Insert::Append => lines.len(),
            Insert::Prepend => 0,
            Insert::After(re) => lines
                .iter()
                .rposition(|l| re.is_match(l))
                .map(|i| i + 1)
                .unwrap_or(lines.len()),
            Insert::Before(re) => lines
                .iter()
                .position(|l| re.is_match(l))
                .unwrap_or(lines.len()),
        }
    }
}

/// Numeric owner, as `chown uid:gid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Owner {
    pub uid: u32,
    pub gid: u32,
}

impl Owner {
    fn label(&self) -> String {
        format!("{}:{}", self.uid, self.gid)
    }

    fn of(s: &Stat) -> Owner {
        Owner {
            uid: s.uid,
            gid: s.gid,
        }
    }
}

/// Pure planning of the attribute part shared by every op that takes
/// `.mode()` and `.owner()`: which of the wanted attributes differ from
/// `current` (`None` when the path does not exist yet, rendered as `-`).
/// Mode comparison ignores the file type bits (`& 0o7777`).
pub fn plan_attrs(
    current: Option<&Stat>,
    mode: Option<u32>,
    owner: Option<Owner>,
) -> Vec<AttrChange> {
    let mut changes = vec![];
    if let Some(want) = mode {
        let want = want & 0o7777;
        match current {
            Some(s) if s.mode & 0o7777 == want => {}
            Some(s) => changes.push(AttrChange {
                name: "mode".into(),
                from: format!("{:04o}", s.mode & 0o7777),
                to: format!("{want:04o}"),
            }),
            None => changes.push(AttrChange {
                name: "mode".into(),
                from: "-".into(),
                to: format!("{want:04o}"),
            }),
        }
    }
    if let Some(want) = owner {
        match current {
            Some(s) if Owner::of(s) == want => {}
            Some(s) => changes.push(AttrChange {
                name: "owner".into(),
                from: Owner::of(s).label(),
                to: want.label(),
            }),
            None => changes.push(AttrChange {
                name: "owner".into(),
                from: "-".into(),
                to: want.label(),
            }),
        }
    }
    changes
}

/// Apply the attributes an op was given. Unconditional: `chmod`/`chown`
/// are idempotent and `check` already decided a change is due.
pub(crate) fn apply_attrs(
    sys: &System,
    path: &Path,
    mode: Option<u32>,
    owner: Option<Owner>,
) -> Result<()> {
    if let Some(mode) = mode {
        sys.set_mode(path, mode & 0o7777)?;
    }
    if let Some(o) = owner {
        sys.set_owner(path, o.uid, o.gid)?;
    }
    Ok(())
}

/// The line terminator a text uses, so a rewrite keeps CRLF files CRLF.
pub(crate) fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Read a text file for editing. Refuses a symlink: an atomic rewrite would
/// replace the link with a regular file and leave its target stale, which is
/// never what an edit meant. Missing is empty text when `create`, else an
/// error naming the `.create(true)` option.
pub(crate) fn read_text_or_empty(sys: &System, path: &Path, create: bool) -> Result<String> {
    use rustible_sdk::backend::FileKind;
    match sys.stat(path)? {
        Some(s) if s.kind == FileKind::Symlink => bail!(
            "{} is a symlink; edit its target instead (an atomic rewrite would replace the link)",
            path.display()
        ),
        Some(s) if s.kind == FileKind::Dir => bail!("{} is a directory", path.display()),
        Some(_) => sys.read_to_string(path),
        None if create => Ok(String::new()),
        None => bail!(
            "{} does not exist (use .create(true) to create it)",
            path.display()
        ),
    }
}

/// Back up (when asked, and the file exists) then write atomically. Returns
/// the backup path.
pub(crate) fn write_with_backup(
    sys: &System,
    path: &Path,
    backup: bool,
    bytes: &[u8],
) -> Result<Option<std::path::PathBuf>> {
    let backup_path = if backup && sys.exists(path)? {
        Some(sys.backup(path)?)
    } else {
        None
    };
    sys.write_atomic(path, bytes)?;
    Ok(backup_path)
}

#[cfg(test)]
pub(crate) mod testing {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;
    use rustible_sdk::prelude::*;

    pub fn fake_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    /// Run `check`, insist on a change, return it.
    pub fn expect_change<O: Op>(op: &O, sys: &System) -> Change<O::Output> {
        match op.check(sys).unwrap() {
            Plan::Change(c) => c,
            Plan::Satisfied(_) => panic!("expected a change, op was satisfied"),
        }
    }
}

#[cfg(test)]
mod tests {
    use rustible_sdk::backend::FileKind;

    use super::*;

    fn stat(mode: u32, uid: u32, gid: u32) -> Stat {
        Stat {
            mode,
            uid,
            gid,
            size: 0,
            kind: FileKind::File,
        }
    }

    #[test]
    fn plan_attrs_nothing_wanted_or_all_equal_is_empty() {
        let s = stat(0o644, 1000, 1000);
        assert!(plan_attrs(Some(&s), None, None).is_empty());
        assert!(plan_attrs(None, None, None).is_empty());
        assert!(
            plan_attrs(
                Some(&s),
                Some(0o644),
                Some(Owner {
                    uid: 1000,
                    gid: 1000
                })
            )
            .is_empty()
        );
    }

    #[test]
    fn plan_attrs_reports_mode_and_owner_changes() {
        let s = stat(0o644, 0, 0);
        let ch = plan_attrs(Some(&s), Some(0o600), Some(Owner { uid: 33, gid: 33 }));
        let rendered: Vec<String> = ch
            .iter()
            .map(|c| format!("{}:{}->{}", c.name, c.from, c.to))
            .collect();
        assert_eq!(rendered, vec!["mode:0644->0600", "owner:0:0->33:33"]);
    }

    #[test]
    fn plan_attrs_ignores_file_type_bits() {
        // A stat that (wrongly) carries S_IFREG must still compare equal.
        let s = stat(0o100644, 0, 0);
        assert!(plan_attrs(Some(&s), Some(0o644), None).is_empty());
        assert!(plan_attrs(Some(&s), Some(0o100644), None).is_empty());
        let ch = plan_attrs(Some(&s), Some(0o100600), None);
        assert_eq!(ch[0].from, "0644");
        assert_eq!(ch[0].to, "0600");
    }

    #[test]
    fn plan_attrs_on_missing_path_renders_dash() {
        let ch = plan_attrs(None, Some(0o750), Some(Owner { uid: 1, gid: 2 }));
        assert_eq!(ch.len(), 2);
        assert_eq!((ch[0].from.as_str(), ch[0].to.as_str()), ("-", "0750"));
        assert_eq!((ch[1].from.as_str(), ch[1].to.as_str()), ("-", "1:2"));
    }

    #[test]
    fn insert_positions() {
        let lines: Vec<String> = ["a", "Port 1", "b", "Port 2", "c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let re = Regex::new("^Port").unwrap();
        assert_eq!(Insert::Append.position(&lines), 5);
        assert_eq!(Insert::Prepend.position(&lines), 0);
        assert_eq!(Insert::After(re.clone()).position(&lines), 4);
        assert_eq!(Insert::Before(re).position(&lines), 1);
        let none = Regex::new("^zzz").unwrap();
        assert_eq!(Insert::After(none.clone()).position(&lines), 5);
        assert_eq!(Insert::Before(none).position(&lines), 5);
    }
}
