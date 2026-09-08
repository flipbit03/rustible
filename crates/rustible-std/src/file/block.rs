//! `file::Block`: Ansible's `blockinfile`.

use std::path::PathBuf;

use rustible_sdk::prelude::*;

use super::Insert;

/// Ansible's default is `# {mark} ANSIBLE MANAGED BLOCK`.
pub const DEFAULT_MARKER: &str = "# {mark} MANAGED BY RUSTIBLE";

/// Ensure a file holds exactly one copy of a text block between two marker
/// lines. Ansible's `blockinfile`.
///
/// ```no_run
/// # use rustible_std::file;
/// let op = file::Block::in_path("/etc/hosts")
///     .marker("# {mark} rustible: lab hosts")
///     .backup(true)
///     .set("10.0.0.1 lab1\n10.0.0.2 lab2\n");
/// ```
///
/// `{mark}` in the marker is replaced by `BEGIN` and `END`. A block that is
/// already there is replaced in place; a missing one is inserted where
/// `.insert(..)` says (append by default). Setting an empty block removes
/// the managed block, as in Ansible's `state: absent`.
#[derive(Debug, Clone)]
pub struct Block {
    path: PathBuf,
    marker: String,
    block: String,
    insert: Insert,
    backup: bool,
    create: bool,
}

pub struct BlockBuilder {
    path: PathBuf,
    marker: String,
    insert: Insert,
    backup: bool,
    create: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockReport {
    pub path: PathBuf,
    /// 1-based line number of the BEGIN marker; 0 when there is no block
    /// (an empty block that was, or is now, absent).
    pub line_no: usize,
    pub backup_path: Option<PathBuf>,
}

impl Block {
    pub fn in_path(path: impl Into<PathBuf>) -> BlockBuilder {
        BlockBuilder {
            path: path.into(),
            marker: DEFAULT_MARKER.into(),
            insert: Insert::Append,
            backup: false,
            create: false,
        }
    }

    fn markers(&self) -> (String, String) {
        (
            self.marker.replace("{mark}", "BEGIN"),
            self.marker.replace("{mark}", "END"),
        )
    }
}

impl BlockBuilder {
    /// Marker line template; must contain `{mark}`.
    pub fn marker(mut self, marker: impl Into<String>) -> Self {
        self.marker = marker.into();
        self
    }

    pub fn insert(mut self, at: Insert) -> Self {
        self.insert = at;
        self
    }

    pub fn backup(mut self, on: bool) -> Self {
        self.backup = on;
        self
    }

    /// Create the file if it does not exist.
    pub fn create(mut self, on: bool) -> Self {
        self.create = on;
        self
    }

    /// The block content (with or without a trailing newline). Empty removes
    /// the managed block. Finishes the builder.
    pub fn set(self, block: impl Into<String>) -> Block {
        Block {
            path: self.path,
            marker: self.marker,
            block: block.into(),
            insert: self.insert,
            backup: self.backup,
            create: self.create,
        }
    }
}

/// The first BEGIN marker and the first END marker after it, as 0-based
/// line indexes. A BEGIN without a matching END counts as no block.
fn find_block(lines: &[String], begin: &str, end: &str) -> Option<(usize, usize)> {
    let b = lines.iter().position(|l| l == begin)?;
    let e = lines[b + 1..].iter().position(|l| l == end)? + b + 1;
    Some((b, e))
}

/// Pure planning: given the current text, compute the new text and the
/// 1-based line of the BEGIN marker (or of where the block was, after a
/// removal). `None` means already satisfied. The result always ends with a
/// newline unless it is empty.
pub fn plan_block(
    text: &str,
    begin: &str,
    end: &str,
    block: &str,
    insert: &Insert,
) -> Option<(String, usize)> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let want: Vec<String> = block.lines().map(str::to_string).collect();

    let at = match (find_block(&lines, begin, end), want.is_empty()) {
        (None, true) => return None,
        (Some((b, e)), true) => {
            lines.drain(b..=e);
            b
        }
        (Some((b, e)), false) => {
            if lines[b + 1..e] == want[..] {
                return None;
            }
            lines.splice(b + 1..e, want);
            b
        }
        (None, false) => {
            let at = insert.position(&lines);
            let mut body = Vec::with_capacity(want.len() + 2);
            body.push(begin.to_string());
            body.extend(want);
            body.push(end.to_string());
            lines.splice(at..at, body);
            at
        }
    };

    let eol = super::eol_of(text);
    let mut out = lines.join(eol);
    if !lines.is_empty() {
        out.push_str(eol);
    }
    Some((out, at + 1))
}

impl Op for Block {
    type Output = BlockReport;

    fn check(&self, sys: &System) -> Result<Plan<BlockReport>> {
        if !self.marker.contains("{mark}") {
            bail!("Block marker {:?} does not contain {{mark}}", self.marker);
        }
        let (begin, end) = self.markers();
        // A body line equal to a marker would make every run find a shorter
        // block and grow the file forever.
        if let Some(l) = self.block.lines().find(|l| *l == begin || *l == end) {
            bail!(
                "block body contains a line equal to a marker ({l:?}); change the marker with .marker(..)"
            );
        }
        let text = super::read_text_or_empty(sys, &self.path, self.create)?;
        match plan_block(&text, &begin, &end, &self.block, &self.insert) {
            None => {
                let lines: Vec<String> = text.lines().map(str::to_string).collect();
                let line_no = find_block(&lines, &begin, &end)
                    .map(|(b, _)| b + 1)
                    .unwrap_or(0);
                Ok(Plan::Satisfied(BlockReport {
                    path: self.path.clone(),
                    line_no,
                    backup_path: None,
                }))
            }
            Some((new_text, line_no)) => Ok(Plan::change_predicting(
                Diff::text(&self.path, text, new_text),
                BlockReport {
                    path: self.path.clone(),
                    line_no: if self.block.is_empty() { 0 } else { line_no },
                    backup_path: None,
                },
            )),
        }
    }

    fn apply(&self, sys: &System, change: Change<BlockReport>) -> Result<BlockReport> {
        // Re-plan from the current text rather than trusting the diff copy.
        let text = super::read_text_or_empty(sys, &self.path, self.create)?;
        let (begin, end) = self.markers();
        let Some((after, line_no)) = plan_block(&text, &begin, &end, &self.block, &self.insert)
        else {
            return Ok(BlockReport {
                path: self.path.clone(),
                line_no: change.predicted.map(|p| p.line_no).unwrap_or(0),
                backup_path: None,
            });
        };
        let backup_path = super::write_with_backup(sys, &self.path, self.backup, after.as_bytes())?;
        Ok(BlockReport {
            path: self.path.clone(),
            line_no: if self.block.is_empty() { 0 } else { line_no },
            backup_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use regex::Regex;
    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::super::testing::{expect_change, fake_sys};
    use super::*;

    const B: &str = "# BEGIN X";
    const E: &str = "# END X";

    #[test]
    fn plan_block_appends_when_absent() {
        let (out, no) = plan_block("a\nb\n", B, E, "one\ntwo\n", &Insert::Append).unwrap();
        assert_eq!(out, "a\nb\n# BEGIN X\none\ntwo\n# END X\n");
        assert_eq!(no, 3);
    }

    #[test]
    fn plan_block_replaces_existing_in_place() {
        let text = "a\n# BEGIN X\nold\n# END X\nz\n";
        let (out, no) = plan_block(text, B, E, "new1\nnew2", &Insert::Append).unwrap();
        assert_eq!(out, "a\n# BEGIN X\nnew1\nnew2\n# END X\nz\n");
        assert_eq!(no, 2);
    }

    #[test]
    fn plan_block_identical_is_satisfied() {
        let text = "a\n# BEGIN X\none\ntwo\n# END X\n";
        assert!(plan_block(text, B, E, "one\ntwo\n", &Insert::Append).is_none());
        // Trailing newline on the block does not matter.
        assert!(plan_block(text, B, E, "one\ntwo", &Insert::Append).is_none());
    }

    #[test]
    fn plan_block_empty_removes_managed_block() {
        let text = "a\n# BEGIN X\none\n# END X\nz\n";
        let (out, no) = plan_block(text, B, E, "", &Insert::Append).unwrap();
        assert_eq!(out, "a\nz\n");
        assert_eq!(no, 2);
        // Nothing to remove: satisfied.
        assert!(plan_block("a\nz\n", B, E, "", &Insert::Append).is_none());
        // Removing the only content leaves an empty file, not a blank line.
        let (out, _) = plan_block("# BEGIN X\none\n# END X\n", B, E, "", &Insert::Append).unwrap();
        assert_eq!(out, "");
    }

    #[test]
    fn plan_block_trailing_newline_edge_cases() {
        // File without a trailing newline gets one before the block.
        let (out, _) = plan_block("a\nb", B, E, "x", &Insert::Append).unwrap();
        assert_eq!(out, "a\nb\n# BEGIN X\nx\n# END X\n");
        // Empty file (create): just the block.
        let (out, no) = plan_block("", B, E, "x\n", &Insert::Append).unwrap();
        assert_eq!(out, "# BEGIN X\nx\n# END X\n");
        assert_eq!(no, 1);
        // Markers as the last lines with no final newline are still found.
        let text = "a\n# BEGIN X\nold\n# END X";
        let (out, _) = plan_block(text, B, E, "new", &Insert::Append).unwrap();
        assert_eq!(out, "a\n# BEGIN X\nnew\n# END X\n");
    }

    #[test]
    fn plan_block_inserts_after_and_before_regex() {
        let after = Regex::new("^\\[section\\]").unwrap();
        let (out, no) = plan_block("[section]\nk=v\n", B, E, "x", &Insert::After(after)).unwrap();
        assert_eq!(out, "[section]\n# BEGIN X\nx\n# END X\nk=v\n");
        assert_eq!(no, 2);
        let before = Regex::new("^k=").unwrap();
        let (out, _) = plan_block("[section]\nk=v\n", B, E, "x", &Insert::Before(before)).unwrap();
        assert_eq!(out, "[section]\n# BEGIN X\nx\n# END X\nk=v\n");
        let (out, no) = plan_block("a\n", B, E, "x", &Insert::Prepend).unwrap();
        assert_eq!(out, "# BEGIN X\nx\n# END X\na\n");
        assert_eq!(no, 1);
    }

    #[test]
    fn plan_block_orphan_begin_marker_counts_as_absent() {
        let text = "# BEGIN X\nstray\n";
        let (out, no) = plan_block(text, B, E, "x", &Insert::Append).unwrap();
        assert_eq!(out, "# BEGIN X\nstray\n# BEGIN X\nx\n# END X\n");
        assert_eq!(no, 3);
        // And the second run converges: first BEGIN, first END after it.
        assert!(plan_block(&out, B, E, "stray\n# BEGIN X\nx", &Insert::Append).is_none());
    }

    #[test]
    fn block_op_check_apply_then_satisfied() {
        let fake = Arc::new(Fake::new().with_file("/etc/hosts", "127.0.0.1 localhost\n"));
        let sys = fake_sys(&fake);
        let op = Block::in_path("/etc/hosts")
            .marker("# {mark} lab hosts")
            .backup(true)
            .set("10.0.0.1 lab1\n10.0.0.2 lab2\n");
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "+4 -0 lines");
        assert_eq!(c.predicted.as_ref().unwrap().line_no, 2);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.line_no, 2);
        assert!(r.backup_path.is_some());
        assert_eq!(
            fake.content("/etc/hosts").unwrap(),
            "127.0.0.1 localhost\n# BEGIN lab hosts\n10.0.0.1 lab1\n10.0.0.2 lab2\n# END lab hosts\n"
        );
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.line_no, 2);

        // Replace in place, then remove with an empty block.
        let op2 = Block::in_path("/etc/hosts")
            .marker("# {mark} lab hosts")
            .set("10.0.0.9 lab9\n");
        let c = expect_change(&op2, &sys);
        op2.apply(&sys, c).unwrap();
        assert_eq!(
            fake.content("/etc/hosts").unwrap(),
            "127.0.0.1 localhost\n# BEGIN lab hosts\n10.0.0.9 lab9\n# END lab hosts\n"
        );
        let op3 = Block::in_path("/etc/hosts")
            .marker("# {mark} lab hosts")
            .set("");
        let c = expect_change(&op3, &sys);
        let r = op3.apply(&sys, c).unwrap();
        assert_eq!(r.line_no, 0);
        assert_eq!(fake.content("/etc/hosts").unwrap(), "127.0.0.1 localhost\n");
        assert!(matches!(op3.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn block_op_default_marker_and_create() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let err = Block::in_path("/new")
            .set("x")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("use .create(true)"), "{err}");

        let op = Block::in_path("/new").create(true).set("x");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.content("/new").unwrap(),
            "# BEGIN MANAGED BY RUSTIBLE\nx\n# END MANAGED BY RUSTIBLE\n"
        );
    }

    #[test]
    fn block_op_rejects_marker_without_mark() {
        let fake = Arc::new(Fake::new().with_file("/f", ""));
        let sys = fake_sys(&fake);
        let err = Block::in_path("/f")
            .marker("# no placeholder")
            .set("x")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("{mark}"), "{err}");
    }

    #[test]
    fn block_in_check_mode_predicts_and_writes_nothing() {
        let fake = Arc::new(Fake::new().with_file("/f", "a\n"));
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step(
                "block",
                Block::in_path("/f").insert(Insert::Prepend).set("x"),
            )
            .unwrap();
        assert!(r.changed && r.predicted);
        assert_eq!(r.line_no, 1);
        assert_eq!(fake.content("/f").unwrap(), "a\n");
    }

    #[test]
    fn body_line_equal_to_a_marker_is_refused() {
        let fake = Arc::new(Fake::new().with_file("/f", "a\n"));
        let sys = fake_sys(&fake);
        let e = Block::in_path("/f")
            .marker("# {mark} X")
            .set("foo\n# END X\nbar\n")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(e.contains("equal to a marker"), "{e}");
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        let (out, _) = plan_block("a\r\nb\r\n", "# BEGIN", "# END", "x", &Insert::Append).unwrap();
        assert_eq!(out, "a\r\nb\r\n# BEGIN\r\nx\r\n# END\r\n");
    }

    #[test]
    fn symlinked_file_is_refused_not_replaced() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/real", "a\n")
                .with_symlink("/etc/resolv.conf", "/real"),
        );
        let sys = fake_sys(&fake);
        let e = Block::in_path("/etc/resolv.conf")
            .set("x")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(e.contains("is a symlink"), "{e}");
        assert_eq!(fake.content("/real").unwrap(), "a\n");
    }
}
