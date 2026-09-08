//! File operations.

use std::path::{Path, PathBuf};

use regex::Regex;
use rustible_sdk::prelude::*;

/// Where to put a line that is not present yet.
#[derive(Debug, Clone)]
pub enum Insert {
    Append,
    Prepend,
    After(Regex),
    Before(Regex),
}

/// Ensure a line is present in a file, replacing the line matched by `matching`
/// if any. Ansible's `lineinfile` with `state: present`.
#[derive(Debug, Clone)]
pub struct Line {
    path: PathBuf,
    matching: Option<Regex>,
    line: String,
    backup: bool,
    insert: Insert,
    create: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineReport {
    /// 1-based line number where the line now sits.
    pub line_no: usize,
    pub backup_path: Option<PathBuf>,
}

impl Line {
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

    pub fn backup(mut self, on: bool) -> Self {
        self.backup = on;
        self
    }

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

/// Pure planning: given the current text, compute the new text and where the
/// line ends up. `None` means already satisfied.
pub fn plan_line(
    text: &str,
    matching: Option<&Regex>,
    line: &str,
    insert: &Insert,
) -> Option<(String, usize)> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();

    let hit = lines.iter().position(|l| match matching {
        Some(re) => re.is_match(l),
        None => l == line,
    });

    let line_no = match hit {
        Some(i) if lines[i] == line => return None,
        Some(i) => {
            lines[i] = line.to_string();
            i
        }
        None => {
            let at = match insert {
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
            };
            lines.insert(at, line.to_string());
            at
        }
    };

    // Always terminate with a newline; we normalize files that lacked one.
    let mut out = lines.join("\n");
    out.push('\n');
    Some((out, line_no + 1))
}

impl Op for Line {
    type Output = LineReport;

    fn check(&self, sys: &System) -> Result<Plan<LineReport>> {
        let text = match sys.exists(&self.path)? {
            true => sys.read_to_string(&self.path)?,
            false if self.create => String::new(),
            false => bail!(
                "{} does not exist (use .create(true) to create it)",
                self.path.display()
            ),
        };

        match plan_line(&text, self.matching.as_ref(), &self.line, &self.insert) {
            None => {
                let line_no = text
                    .lines()
                    .position(|l| l == self.line)
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

    fn apply(&self, sys: &System, change: Change<LineReport>) -> Result<LineReport> {
        let Diff::Text { after, .. } = &change.diff else {
            bail!("Line::apply received a non-text diff");
        };
        let backup_path = if self.backup && sys.exists(&self.path)? {
            Some(sys.backup(&self.path)?)
        } else {
            None
        };
        sys.write_atomic(&self.path, after.as_bytes())?;
        let line_no = change.predicted.map(|p| p.line_no).unwrap_or(0);
        Ok(LineReport {
            line_no,
            backup_path,
        })
    }
}

/// Ensure a directory exists with the given mode. Minimal for the spike.
#[derive(Debug, Clone)]
pub struct Directory {
    path: PathBuf,
    mode: Option<u32>,
}

impl Directory {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Directory {
            path: path.into(),
            mode: None,
        }
    }

    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirReport {
    pub path: PathBuf,
    pub created: bool,
}

impl Op for Directory {
    type Output = DirReport;

    fn check(&self, sys: &System) -> Result<Plan<DirReport>> {
        let mut changes = vec![];
        let stat = sys.stat(&self.path)?;
        let created = match &stat {
            None => {
                changes.push(AttrChange {
                    name: "exists".into(),
                    from: "no".into(),
                    to: "yes".into(),
                });
                true
            }
            Some(s) if s.kind != rustible_sdk::backend::FileKind::Dir => {
                bail!("{} exists and is not a directory", self.path.display())
            }
            Some(_) => false,
        };
        if let (Some(want), Some(s)) = (self.mode, &stat)
            && s.mode != want
        {
            changes.push(AttrChange {
                name: "mode".into(),
                from: format!("{:04o}", s.mode),
                to: format!("{want:04o}"),
            });
        }
        if let (Some(want), None) = (self.mode, &stat) {
            changes.push(AttrChange {
                name: "mode".into(),
                from: "-".into(),
                to: format!("{want:04o}"),
            });
        }
        if changes.is_empty() {
            return Ok(Plan::Satisfied(DirReport {
                path: self.path.clone(),
                created: false,
            }));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.path.display().to_string(),
                changes,
            },
            DirReport {
                path: self.path.clone(),
                created,
            },
        ))
    }

    fn apply(&self, sys: &System, change: Change<DirReport>) -> Result<DirReport> {
        let created = change.predicted.map(|p| p.created).unwrap_or(false);
        if created {
            sys.mkdir_all(&self.path)?;
        }
        if let Some(mode) = self.mode {
            sys.set_mode(&self.path, mode)?;
        }
        Ok(DirReport {
            path: self.path.clone(),
            created,
        })
    }
}

/// Helper for ops that take a path.
pub fn path_str(p: &Path) -> String {
    p.display().to_string()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

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

    fn fake_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
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
