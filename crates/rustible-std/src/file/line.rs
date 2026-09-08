//! `file::Line`: Ansible's `lineinfile` with `state: present`.

use std::path::PathBuf;

use regex::Regex;
use rustible_sdk::prelude::*;

use super::Insert;

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
/// missing file is an error unless `.create(true)`. `check` predicts its
/// [`LineReport`], so a check-mode run can read `line_no` off a step that
/// only *would* change.
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
    /// `apply` actually wrote. Always `None` on a satisfied step and on the
    /// prediction `check` returns, because no copy exists until the write.
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

    fn check(&self, sys: &System) -> Result<Plan<LineReport>> {
        let text = super::read_text_or_empty(sys, &self.path, self.create)?;

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
            Some((new_text, line_no)) => Ok(Plan::change_predicting(
                Diff::text(&self.path, text, new_text),
                LineReport {
                    line_no,
                    backup_path: None,
                },
            )),
        }
    }

    fn apply(&self, sys: &System, _change: Change<LineReport>) -> Result<LineReport> {
        // Re-plan from the current text (cheap, pure) rather than trusting a
        // copy carried in the diff: the file may have moved on since check.
        let text = super::read_text_or_empty(sys, &self.path, self.create)?;
        let Some((after, line_no)) =
            plan_line(&text, self.matching.as_ref(), &self.line, &self.insert)
        else {
            // The file satisfied the op between `check` and here, so nothing
            // is written. Where the line sits is read from the file as it is
            // now: `check`'s prediction described the text before whatever
            // changed it, and with duplicates it can name a different line.
            return Ok(LineReport {
                line_no: selected(text.lines(), self.matching.as_ref(), &self.line)
                    .map(|i| i + 1)
                    .unwrap_or(0),
                backup_path: None,
            });
        };
        let backup_path = super::write_with_backup(sys, &self.path, self.backup, after.as_bytes())?;
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
        assert_eq!(change.diff.short(), "+1 -1 lines");
        assert_eq!(change.predicted.as_ref().unwrap().line_no, 1);

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

        // And apply, reached when the file changed under a stale plan,
        // agrees rather than falling back to the prediction.
        let change = Change {
            diff: Diff::summary("stale"),
            predicted: Some(LineReport {
                line_no: 99,
                backup_path: None,
            }),
        };
        assert_eq!(op.apply(&sys, change).unwrap().line_no, 1);
    }

    #[test]
    fn line_op_fails_on_missing_file_without_create() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let op = Line::in_path("/nope").set("x");
        let err = op.check(&sys).unwrap_err().to_string();
        assert!(err.contains("does not exist"), "{err}");
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
            fn check(&self, sys: &System) -> Result<Plan<()>> {
                sys.write_atomic("/x", b"oops")?;
                Ok(Plan::Satisfied(()))
            }
            fn apply(&self, _: &System, _: Change<()>) -> Result<()> {
                Ok(())
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

    #[test]
    fn check_mode_output_unavailable_unless_predicted() {
        let fake = Arc::new(Fake::new().with_file("/f", "a\n"));
        let sink = Arc::new(Collect::default());
        let sys = System::fake(fake.clone(), sink).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());

        // Line predicts, so its output is available in check mode.
        let r = ctx.step("line", Line::in_path("/f").set("b")).unwrap();
        assert!(r.changed && r.predicted && r.is_available());
        assert_eq!(r.line_no, 2);
        assert_eq!(
            fake.content("/f").unwrap(),
            "a\n",
            "check mode must not write"
        );

        // An op that does not predict: output unavailable.
        struct NoPredict;
        impl Op for NoPredict {
            type Output = u32;
            fn check(&self, _: &System) -> Result<Plan<u32>> {
                Ok(Plan::change(Diff::summary("would do it")))
            }
            fn apply(&self, _: &System, _: Change<u32>) -> Result<u32> {
                Ok(7)
            }
        }
        let r = ctx.step("np", NoPredict).unwrap();
        assert!(r.changed && !r.is_available());
        let err = r.output().unwrap_err().to_string();
        assert!(err.contains("unavailable in check mode"), "{err}");
    }
}
