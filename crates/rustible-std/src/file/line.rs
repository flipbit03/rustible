//! `file::Line`: Ansible's `lineinfile` with `state: present`.

use std::path::PathBuf;

use regex::Regex;
use rustible_sdk::prelude::*;

use super::{Insert, TextEdit};

/// Ensure a line is present in a file, replacing the line matched by `matching`
/// if any. Ansible's `lineinfile` with `state: present`.
///
/// ```no_run
/// # use rustible_std::file;
/// let op = file::Line::in_path("/etc/ssh/sshd_config")
///     .matching(r"^#?\s*PasswordAuthentication\b")
///     .set("PasswordAuthentication no");
/// ```
///
/// **Idempotence rests entirely on `matching`.** With a regex, the *first*
/// matching line is the one rewritten, and the step is `ok` once that line
/// reads exactly like the argument to [`LineBuilder::set`]. Later lines that
/// also match are left where they are, so a file with two
/// `PasswordAuthentication` lines keeps the second one and sshd goes on
/// reading it. Without a regex the match is whole-line equality with `set`
/// itself, which means a run whose value differs from the last run's finds
/// nothing to replace and appends a second line; that is Ansible's behaviour
/// too, and it is the usual reason a config file grows a run at a time. Give
/// `matching` a regex that also matches the *old* value.
///
/// [`Insert`] decides where a line that matched nothing goes; the default is
/// [`Insert::Append`]. The file's own line ending is kept (a file containing
/// any CRLF is rewritten with CRLF) and the result always ends with one.
///
/// A symbolic link is refused rather than followed, since the atomic rewrite
/// would replace the link with a regular file, and so is a directory. A
/// missing file is an error unless `.create(true)` — except under `--check`,
/// where an earlier step may create it and the edit is reported as due
/// (vision 12).
#[derive(Debug, Clone)]
pub struct Line {
    path: PathBuf,
    matching: Option<Regex>,
    line: String,
    backup: bool,
    insert: Insert,
    create: bool,
}

/// Output of [`Line`]: where the line ended up, and the backup if one was
/// taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineReport {
    /// 1-based line number where the line now sits: the line `matching`
    /// selected, which with duplicates is not necessarily the first line
    /// equal to the desired text. `0` only if the file lost the line between
    /// the write and the report, which nothing here can cause.
    pub line_no: usize,
    /// The copy taken before the rewrite, set only when `.backup(true)` and
    /// `apply` actually wrote. Always `None` on a satisfied step.
    pub backup_path: Option<PathBuf>,
}

impl Line {
    /// Start a [`Line`] op on this file; [`LineBuilder::set`] supplies the
    /// line and finishes it. Everything else starts off: no `matching`
    /// regex (so the match is whole-line equality with the line itself), no
    /// backup, [`Insert::Append`], and no file creation.
    pub fn in_path(path: impl Into<PathBuf>) -> LineBuilder {
        LineBuilder {
            path: path.into(),
            matching: None,
            backup: false,
            insert: Insert::Append,
            create: false,
        }
    }
}

/// A [`Line`] whose path and options are set but whose content is not;
/// [`LineBuilder::set`] supplies the line and produces the op.
pub struct LineBuilder {
    path: PathBuf,
    matching: Option<Regex>,
    backup: bool,
    insert: Insert,
    create: bool,
}

impl LineBuilder {
    /// Regex selecting the line to replace. Without it, exact match on `set`.
    pub fn matching(mut self, re: &str) -> Self {
        self.matching = Some(Regex::new(re).expect("invalid regex in Line::matching"));
        self
    }

    /// Save the file's previous content next to it
    /// (`<name>.~rustible.<unix-ts>`) before rewriting, and report the path
    /// as [`LineReport::backup_path`]. Off by default. A step that turns out
    /// to be satisfied never writes, and so never makes a backup either.
    pub fn backup(mut self, on: bool) -> Self {
        self.backup = on;
        self
    }

    /// Where to put the line when nothing matched. The default is
    /// [`Insert::Append`]. A line that *does* match is rewritten in place,
    /// so this is not consulted then and cannot be used to move a line.
    pub fn insert(mut self, at: Insert) -> Self {
        self.insert = at;
        self
    }

    /// Create the file if it does not exist.
    pub fn create(mut self, on: bool) -> Self {
        self.create = on;
        self
    }

    /// The line that must be present. Finishes the builder.
    pub fn set(self, line: impl Into<String>) -> Line {
        Line {
            path: self.path,
            matching: self.matching,
            line: line.into(),
            backup: self.backup,
            insert: self.insert,
            create: self.create,
        }
    }
}

/// Index of the line the op acts on: the first match of `matching`, or the
/// first line equal to `line` when there is no regex.
///
/// One function so the planner and the reports cannot disagree about which
/// of several identical lines the step is talking about.
fn selected<'a>(
    lines: impl Iterator<Item = &'a str>,
    matching: Option<&Regex>,
    line: &str,
) -> Option<usize> {
    let mut lines = lines;
    lines.position(|l| match matching {
        Some(re) => re.is_match(l),
        None => l == line,
    })
}

/// Pure planning: given the current text, compute the new text and where the
/// line ends up. `None` means already satisfied.
pub fn plan_line(
    text: &str,
    matching: Option<&Regex>,
    line: &str,
    insert: &Insert,
) -> Option<(String, usize)> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let hit = selected(lines.iter().map(String::as_str), matching, line);

    let line_no = match hit {
        Some(i) if lines[i] == line => return None,
        Some(i) => {
            lines[i] = line.to_string();
            i
        }
        None => {
            let at = insert.position(&lines);
            lines.insert(at, line.to_string());
            at
        }
    };

    // Keep the file's line ending; always terminate the last line.
    let eol = super::eol_of(text);
    let mut out = lines.join(eol);
    out.push_str(eol);
    Some((out, line_no + 1))
}

impl Op for Line {
    type Output = LineReport;
    type Intent = TextEdit;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable. file::Line edits file text through `sys`.
        // The supported set is written out rather than left open, so a new
        // platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!("file::Line has no implementation for {}", other.name()),
        }
        let Some(text) = super::read_text_or_empty(sys, &self.path, self.create)? else {
            return Ok(Plan::Change(TextEdit::await_file(
                "file::Line",
                self.path.clone(),
            )));
        };

        match plan_line(&text, self.matching.as_ref(), &self.line, &self.insert) {
            None => {
                let line_no = selected(text.lines(), self.matching.as_ref(), &self.line)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                Ok(Plan::Satisfied(LineReport {
                    line_no,
                    backup_path: None,
                }))
            }
            Some((after, line_no)) => Ok(Plan::Change(TextEdit::rewrite(
                self.path.clone(),
                text,
                after,
                line_no,
            ))),
        }
    }

    fn apply(&self, sys: &System, intent: TextEdit) -> Result<LineReport> {
        // Writes the text `check` planned, which is the text the diff
        // showed; the file is not read and merged again here.
        let (line_no, backup_path) = intent.write(sys, self.backup)?;
        Ok(LineReport {
            line_no,
            backup_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::super::testing::fake_sys;
    use super::*;

    /// The two platform claims every portable op makes, in one place: it
    /// runs on a mac, and it refuses a platform nobody has claimed rather
    /// than assuming. `Line` is plain file work through `sys`, so the mac
    /// half is the same test as on Linux with different facts.
    fn on(os: Os) -> (std::sync::Arc<Fake>, System) {
        let fake = std::sync::Arc::new(Fake::new().with_file("/etc/x", "a=1\n"));
        let base = fake_sys(&fake);
        let mut facts = base.facts().clone();
        facts.os = os;
        (fake.clone(), base.with_facts(facts))
    }

    #[test]
    fn runs_on_a_mac_and_refuses_an_unclaimed_platform() {
        let (fake, sys) = on(Os::Macos);
        let op = Line::in_path("/etc/x").matching("^b=").set("b=2");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/etc/x").unwrap(), "a=1\nb=2\n");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));

        let (_, sys) = on(Os::Other("freebsd".into()));
        let err = Line::in_path("/etc/x")
            .matching("^b=")
            .set("b=2")
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("file::Line has no implementation for freebsd"),
            "{err}"
        );
    }

    #[test]
    fn plan_replaces_commented_line() {
        let re = Regex::new(r"^#?PasswordAuthentication").unwrap();
        let text = "#PasswordAuthentication yes\nPort 22\n";
        let (out, no) = plan_line(
            text,
            Some(&re),
            "PasswordAuthentication no",
            &Insert::Append,
        )
        .unwrap();
        assert_eq!(out, "PasswordAuthentication no\nPort 22\n");
        assert_eq!(no, 1);
    }

    #[test]
    fn plan_is_satisfied_when_line_present() {
        let re = Regex::new(r"^PasswordAuthentication").unwrap();
        let text = "PasswordAuthentication no\n";
        assert!(
            plan_line(
                text,
                Some(&re),
                "PasswordAuthentication no",
                &Insert::Append
            )
            .is_none()
        );
    }

    #[test]
    fn plan_appends_when_absent() {
        let (out, no) = plan_line("Port 22\n", None, "X y", &Insert::Append).unwrap();
        assert_eq!(out, "Port 22\nX y\n");
        assert_eq!(no, 2);
    }

    #[test]
    fn plan_inserts_after_regex() {
        let after = Regex::new(r"^Port").unwrap();
        let (out, no) = plan_line("A\nPort 22\nB\n", None, "X", &Insert::After(after)).unwrap();
        assert_eq!(out, "A\nPort 22\nX\nB\n");
        assert_eq!(no, 3);
    }

    #[test]
    fn plan_handles_missing_trailing_newline() {
        let (out, _) = plan_line("A\nB", None, "C", &Insert::Append).unwrap();
        assert_eq!(out, "A\nB\nC\n");
    }

    #[test]
    fn line_op_check_then_apply_on_fake() {
        let fake = Arc::new(Fake::new().with_file(
            "/etc/ssh/sshd_config",
            "#PasswordAuthentication yes\nPort 22\n",
        ));
        let sys = fake_sys(&fake);
        let op = Line::in_path("/etc/ssh/sshd_config")
            .matching(r"^#?PasswordAuthentication")
            .set("PasswordAuthentication no");

        let plan = op.check(&sys).unwrap();
        let Plan::Change(change) = plan else {
            panic!("expected change")
        };
        assert_eq!(change.diff().short(), "+1 -1 lines");

        let report = op.apply(&sys, change).unwrap();
        assert_eq!(report.line_no, 1);
        assert_eq!(
            fake.content("/etc/ssh/sshd_config").unwrap(),
            "PasswordAuthentication no\nPort 22\n"
        );

        // Second check is satisfied.
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// With duplicates, the satisfied report has to name the line
    /// `matching` selected, not the first line that happens to equal the
    /// desired one. Only the report is at stake; the file is untouched
    /// either way.
    #[test]
    fn a_satisfied_step_reports_the_line_matching_selected() {
        let fake = Arc::new(Fake::new().with_file(
            "/etc/ssh/sshd_config",
            "Port 22\nPasswordAuthentication no\nPort 22\n",
        ));
        let sys = fake_sys(&fake);
        // `matching` selects line 1; the desired text is already there, so
        // this is satisfied at line 1 and not at line 3.
        let op = Line::in_path("/etc/ssh/sshd_config")
            .matching(r"^Port\b")
            .set("Port 22");
        let Plan::Satisfied(report) = op.check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(report.line_no, 1);
    }

    /// The report and the write come from one value: the diff a dry run
    /// shows is rendered from the same `after` text `apply` writes. Pure: an
    /// intent built by hand, no file and no `System`.
    #[test]
    fn the_diff_is_rendered_from_the_text_apply_writes() {
        let intent = TextEdit::rewrite("/etc/x".into(), "a=1\n".into(), "a=1\nb=2\n".into(), 2);
        assert_eq!(
            intent.diff().render(),
            "--- /etc/x (before)\n+++ /etc/x (after)\n@@ -1 +1,2 @@\n a=1\n+b=2\n"
        );
    }

    /// `apply` executes the text `check` planned. The file changes between
    /// the two, and what lands is the planned text, not a merge of the line
    /// into the new content: the race is accepted, as in Ansible, and what
    /// was shown is what ran.
    #[test]
    fn apply_writes_what_check_planned_even_if_the_file_moved_on() {
        let fake = Arc::new(Fake::new().with_file("/etc/x", "a=1\n"));
        let sys = fake_sys(&fake);
        let op = Line::in_path("/etc/x").matching("^b=").set("b=2");
        let intent = op.check(&sys).unwrap();
        let Plan::Change(intent) = intent else {
            panic!("expected change")
        };

        rustible_sdk::backend::Backend::write(&*fake, std::path::Path::new("/etc/x"), b"c=3\n")
            .unwrap();
        let report = op.apply(&sys, intent).unwrap();
        assert_eq!(fake.content("/etc/x").unwrap(), "a=1\nb=2\n");
        assert_eq!(report.line_no, 2, "where the planned text puts the line");
    }

    /// The old `apply` planned again and, finding the file already
    /// satisfied, wrote nothing while the step still reported `changed`. It
    /// now writes what it planned, so a `changed` step always wrote.
    #[test]
    fn apply_never_reports_changed_having_written_nothing() {
        let fake = Arc::new(Fake::new().with_file("/etc/x", "a=1\n"));
        let sys = fake_sys(&fake);
        let op = Line::in_path("/etc/x").backup(true).set("b=2");
        let Plan::Change(intent) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        // Someone else adds the line first.
        rustible_sdk::backend::Backend::write(
            &*fake,
            std::path::Path::new("/etc/x"),
            b"a=1\nb=2\n",
        )
        .unwrap();
        let report = op.apply(&sys, intent).unwrap();
        assert!(
            report.backup_path.is_some(),
            "the planned text was written, after a backup"
        );
        assert_eq!(fake.content("/etc/x").unwrap(), "a=1\nb=2\n");
    }

    /// A real run refuses a missing file. A dry run does not (vision 12): an
    /// earlier step may create it (a package shipping its config, a
    /// `file::Copy`), so the edit is reported as due, without a text diff
    /// since there is no text yet.
    #[test]
    fn line_op_fails_on_missing_file_without_create_except_under_check() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Line::in_path("/nope").set("x");
        let err = op.check(&sys).unwrap_err().chain();
        assert!(
            err.contains("/nope does not exist (use .create(true) to create it)"),
            "{err}"
        );

        let dry = fake_sys(&fake).with_check_mode(true);
        let Plan::Change(c) = op.check(&dry).unwrap() else {
            panic!("expected would change")
        };
        assert_eq!(
            c.diff().render(),
            "/nope: does not exist yet; file::Line would edit it once an earlier step \
             creates it (or use .create(true) to create it here)"
        );
        assert!(fake.file("/nope").is_none());
    }

    #[test]
    fn line_op_creates_when_asked() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Line::in_path("/new").create(true).set("hello");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!()
        };
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/new").unwrap(), "hello\n");
    }

    #[test]
    fn mutation_during_check_is_refused() {
        // An op that (wrongly) writes in check(). The guard must catch it.
        struct Bad;
        impl Op for Bad {
            type Output = ();
            type Intent = std::convert::Infallible;
            fn check(&self, sys: &System) -> Result<Plan<Self>> {
                sys.write_atomic("/x", b"oops")?;
                Ok(Plan::Satisfied(()))
            }
            fn apply(&self, _: &System, intent: Self::Intent) -> Result<()> {
                match intent {}
            }
        }
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let sink = Arc::new(Collect::default());
        let sys = System::fake(fake.clone(), sink).with_facts(sys.facts().clone());
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let err = ctx.step("bad", Bad).unwrap_err().chain();
        assert!(err.contains("during check()"), "{err}");
        assert!(fake.content("/x").is_none());
    }

    /// Vision 12, the rule itself: a step that would change has no output
    /// in check mode. `.changed` and `.diff` stay readable, `is_available()`
    /// says so, and reading the output is a clear error naming the step.
    #[test]
    fn a_would_change_step_has_no_output_in_check_mode() {
        let fake = Arc::new(Fake::new().with_file("/f", "a\n"));
        let sink = Arc::new(Collect::default());
        let sys = System::fake(fake.clone(), sink).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());

        let r = ctx.step("line", Line::in_path("/f").set("b")).unwrap();
        assert!(r.changed && !r.is_available());
        assert_eq!(r.diff.as_ref().unwrap().short(), "+1 -0 lines");
        let err = r.output().unwrap_err().to_string();
        assert!(
            err.contains("would have changed") && err.contains("unavailable in check mode"),
            "{err}"
        );
        assert_eq!(
            fake.content("/f").unwrap(),
            "a\n",
            "check mode must not write"
        );

        // A satisfied step is unaffected: its output is what the machine has.
        let r = ctx.step("line", Line::in_path("/f").set("a")).unwrap();
        assert!(!r.changed && r.is_available());
        assert_eq!(r.line_no, 1);
    }
}
