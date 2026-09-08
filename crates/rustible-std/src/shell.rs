//! Running commands as steps. Ansible's `ansible.builtin.command` and
//! `ansible.builtin.shell`, one op: [`Command`].
//!
//! | Ansible | Rustible |
//! |---|---|
//! | `command: /usr/bin/make install` | `shell::Command::new("/usr/bin/make").arg("install")` |
//! | `shell: cat a \| grep b` | `shell::Command::sh("cat a | grep b")` |
//! | `args: chdir: /src` | `.cwd("/src")` |
//! | `args: creates: /opt/x/done` | `.creates("/opt/x/done")` |
//! | `args: removes: /opt/x/lock` | `.removes("/opt/x/lock")` |
//! | `args: stdin: "..."` | `.stdin("...")` |
//! | `environment: {K: V}` | `.env("K", "V")` |
//! | `changed_when: "'up to date' not in out.stdout"` | `.changed_when(\|out\| !out.stdout.contains("up to date"))` |
//! | `failed_when` / `ignore_errors` | a `match` or `.ok()` on the step (vision 6.1) |
//!
//! A command is an action (vision 6.4): `check` cannot know what it will do,
//! so it always plans a change and check mode reports `would change` without
//! running anything, as Ansible does. `creates`, `removes` and
//! `changed_when` are the escape hatches that make it state-like: the first
//! two let `check` say `ok` without running, the third lets the step say
//! `ok` after running. A non-zero exit fails the step.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use rustible_sdk::prelude::*;

type Predicate = Arc<dyn Fn(&CommandOutput) -> bool + Send + Sync>;

/// Run a program with arguments. `ansible.builtin.command` (and
/// `ansible.builtin.shell` through [`Command::sh`]).
///
/// ```ignore
/// ctx.step("Build", shell::Command::new("make").arg("install").cwd("/src").creates("/usr/local/bin/x"))?;
/// let sync = ctx.step("Sync repo",
///     shell::Command::new("git").args(["pull", "--ff-only"]).cwd("/srv/app")
///         .changed_when(|out| !out.stdout.contains("Already up to date")))?;
/// ctx.step("Load schema", shell::Command::new("psql").arg("app").stdin(include_str!("schema.sql")))?;
/// ```
///
/// Always changes, unless `creates`/`removes` (skip without running) or
/// `changed_when` (decide after running) say otherwise; `always_changes`
/// reports exactly that, so the orchestrator's "where does this playbook stop
/// being idempotent" mark stays honest.
#[derive(Clone)]
pub struct Command {
    program: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    stdin: Option<Vec<u8>>,
    creates: Option<PathBuf>,
    removes: Option<PathBuf>,
    cwd: Option<PathBuf>,
    changed_when: Option<Predicate>,
}

/// `{:?}` for [`Command`] shows what a value *is* without showing the value:
/// stdin as a byte count, env as its names with each value's length. Both
/// carry secrets in ordinary use (`.stdin(password)`, `.env("PGPASSWORD",
/// ..)`), and a `dbg!` or a formatted error should not put them in a log.
/// `program` and `args` are printed in full: they reach the process table on
/// every host anyway, so hiding them here would buy nothing.
impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let env: BTreeMap<&str, String> = self
            .env
            .iter()
            .map(|(k, v)| (k.as_str(), format!("{} bytes", v.len())))
            .collect();
        f.debug_struct("Command")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("env", &env)
            .field(
                "stdin",
                &self.stdin.as_ref().map(|b| format!("{} bytes", b.len())),
            )
            .field("creates", &self.creates)
            .field("removes", &self.removes)
            .field("cwd", &self.cwd)
            .field(
                "changed_when",
                &self.changed_when.as_ref().map(|_| "<closure>"),
            )
            .finish()
    }
}

/// Output of [`Command`]. When `creates`/`removes` made the step `ok`
/// without running, `status` is 0 and both streams are empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Command {
    pub fn new(program: impl Into<String>) -> Self {
        Command {
            program: program.into(),
            args: vec![],
            env: BTreeMap::new(),
            stdin: None,
            creates: None,
            removes: None,
            cwd: None,
            changed_when: None,
        }
    }

    /// Run `script` through `/bin/sh -c`: pipes, globs and redirections
    /// work. `ansible.builtin.shell`.
    pub fn sh(script: impl Into<String>) -> Self {
        Command::new("/bin/sh").args(["-c".to_string(), script.into()])
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Set an environment variable for the process. `LANG` and `LC_ALL` are
    /// `C` unless set here (vision 7.3).
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Bytes (or a `&str`) fed to the process on standard input. Without it
    /// the process reads from `/dev/null`.
    pub fn stdin(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(bytes.into());
        self
    }

    /// Skip (report `ok`) if this path exists.
    pub fn creates(mut self, p: impl Into<PathBuf>) -> Self {
        self.creates = Some(p.into());
        self
    }

    /// Skip (report `ok`) if this path does not exist.
    pub fn removes(mut self, p: impl Into<PathBuf>) -> Self {
        self.removes = Some(p.into());
        self
    }

    pub fn cwd(mut self, p: impl Into<PathBuf>) -> Self {
        self.cwd = Some(p.into());
        self
    }

    /// Decide from the output whether the step counts as `changed`. The
    /// command still runs (there is no other way to know); `false` reports
    /// the step `ok`. In check mode the step reports `would change` either
    /// way, because the command does not run there.
    pub fn changed_when(
        mut self,
        predicate: impl Fn(&CommandOutput) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.changed_when = Some(Arc::new(predicate));
        self
    }

    fn argv_str(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Op for Command {
    type Output = CommandOutput;

    fn check(&self, sys: &System) -> Result<Plan<CommandOutput>> {
        let satisfied = CommandOutput {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        };
        if let Some(p) = &self.creates
            && sys.exists(p)?
        {
            return Ok(Plan::Satisfied(satisfied));
        }
        if let Some(p) = &self.removes
            && !sys.exists(p)?
        {
            return Ok(Plan::Satisfied(satisfied));
        }
        Ok(Plan::change(Diff::summary(format!(
            "$ {}",
            self.argv_str()
        ))))
    }

    fn apply(&self, sys: &System, _: Change<CommandOutput>) -> Result<CommandOutput> {
        let mut cmd = sys.cmd(&self.program).args(self.args.iter().cloned());
        for (k, v) in &self.env {
            cmd = cmd.env(k, v);
        }
        if let Some(cwd) = &self.cwd {
            cmd = cmd.cwd(cwd.clone());
        }
        if let Some(input) = &self.stdin {
            cmd = cmd.stdin(input.clone());
        }
        let out = cmd.run()?;
        Ok(CommandOutput {
            status: out.status,
            stdout: out.stdout_str(),
            stderr: out.stderr_str(),
        })
    }

    fn always_changes(&self) -> bool {
        self.creates.is_none() && self.removes.is_none() && self.changed_when.is_none()
    }

    fn changed_by_apply(&self, output: &CommandOutput) -> bool {
        match &self.changed_when {
            Some(pred) => pred(output),
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::{Collect, Event, Status};

    use super::*;

    fn fake_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    /// A `Ctx` over the fake plus the sink it reports to, so tests can
    /// assert the status a step finished with.
    fn ctx_with(fake: &Arc<Fake>, check_mode: bool) -> (Ctx, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        let sys = System::fake(fake.clone(), sink.clone()).with_check_mode(check_mode);
        (Ctx::new(sys, rustible_sdk::HostInfo::local()), sink)
    }

    fn finished_statuses(sink: &Collect) -> Vec<Status> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::StepFinished { status, .. } => Some(status),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sh_runs_through_bin_sh_dash_c() {
        let op = Command::sh("echo a | tr a b");
        assert_eq!(op.program, "/bin/sh");
        assert_eq!(op.args, vec!["-c", "echo a | tr a b"]);
        assert_eq!(op.argv_str(), "/bin/sh -c echo a | tr a b");
    }

    #[test]
    fn check_plans_the_command_line_and_apply_runs_it_with_env_cwd_and_stdin() {
        let fake = Arc::new(Fake::new().with_cmd("psql", None, 0, "CREATE TABLE\n"));
        let sys = fake_sys(&fake);
        let op = Command::new("psql")
            .args(["-v", "ON_ERROR_STOP=1", "app"])
            .env("PGPASSWORD", "s3cret")
            .cwd("/srv")
            .stdin("create table t (id int);\n");
        assert!(op.always_changes());
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("a command always plans a change");
        };
        assert_eq!(c.diff.short(), "$ psql -v ON_ERROR_STOP=1 app");
        assert!(fake.commands().is_empty(), "check runs nothing");

        let out = op.apply(&sys, c).unwrap();
        assert_eq!(out.status, 0);
        assert_eq!(out.stdout, "CREATE TABLE\n");
        let ran = fake.commands();
        assert_eq!(ran.len(), 1);
        assert_eq!(ran[0].argv(), vec!["psql", "-v", "ON_ERROR_STOP=1", "app"]);
        assert_eq!(ran[0].env.get("PGPASSWORD").unwrap(), "s3cret");
        assert_eq!(ran[0].cwd.as_deref(), Some(std::path::Path::new("/srv")));
        assert_eq!(
            ran[0].stdin.as_deref(),
            Some(b"create table t (id int);\n".as_slice())
        );
    }

    #[test]
    fn stdin_is_absent_unless_given() {
        let fake = Arc::new(Fake::new().with_cmd("true", None, 0, ""));
        let (mut ctx, _) = ctx_with(&fake, false);
        ctx.step("run", Command::new("true")).unwrap();
        assert_eq!(fake.commands()[0].stdin, None);
        assert!(fake.commands()[0].env.is_empty());
    }

    #[test]
    fn creates_and_removes_make_check_satisfied_without_running() {
        let fake = Arc::new(Fake::new().with_file("/opt/done", ""));
        let sys = fake_sys(&fake);
        let op = Command::new("make").creates("/opt/done");
        assert!(!op.always_changes());
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        let op = Command::new("make").creates("/opt/missing");
        assert!(op.check(&sys).unwrap().is_change());

        let op = Command::new("rm").arg("/opt/lock").removes("/opt/lock");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        let op = Command::new("rm").arg("/opt/done").removes("/opt/done");
        assert!(op.check(&sys).unwrap().is_change());
        assert!(fake.commands().is_empty());
    }

    #[test]
    fn changed_when_false_reports_ok_after_running() {
        let fake = Arc::new(Fake::new().with_cmd("git", None, 0, "Already up to date.\n"));
        let (mut ctx, sink) = ctx_with(&fake, false);
        let op = Command::new("git")
            .args(["pull", "--ff-only"])
            .changed_when(|out| !out.stdout.contains("Already up to date"));
        assert!(!op.always_changes());
        let r = ctx.step("pull", op).unwrap();
        assert!(!r.changed, "the predicate said no");
        assert!(r.diff.is_some(), "the command line is kept for -v");
        assert_eq!(r.stdout, "Already up to date.\n");
        assert_eq!(fake.argvs(), vec![vec!["git", "pull", "--ff-only"]]);
        assert_eq!(finished_statuses(&sink), vec![Status::Ok]);

        let fake = Arc::new(Fake::new().with_cmd("git", None, 0, "Updating 1..2\n"));
        let (mut ctx, sink) = ctx_with(&fake, false);
        let r = ctx
            .step(
                "pull",
                Command::new("git")
                    .arg("pull")
                    .changed_when(|out| !out.stdout.contains("Already up to date")),
            )
            .unwrap();
        assert!(r.changed);
        assert_eq!(finished_statuses(&sink), vec![Status::Changed]);
    }

    #[test]
    fn check_mode_runs_nothing_and_reports_would_change_even_with_changed_when() {
        let fake = Arc::new(Fake::new());
        let (mut ctx, sink) = ctx_with(&fake, true);
        let r = ctx
            .step(
                "pull",
                Command::new("git").arg("pull").changed_when(|_| false),
            )
            .unwrap();
        assert!(r.changed && !r.is_available());
        assert!(fake.commands().is_empty());
        assert_eq!(finished_statuses(&sink), vec![Status::WouldChange]);
        // `creates` still says ok in check mode: nothing to run.
        let fake = Arc::new(Fake::new().with_file("/opt/done", ""));
        let (mut ctx, _) = ctx_with(&fake, true);
        let r = ctx
            .step("build", Command::new("make").creates("/opt/done"))
            .unwrap();
        assert!(!r.changed && r.is_available());
    }

    #[test]
    fn non_zero_exit_fails_the_step_with_stderr() {
        let fake = Arc::new(Fake::new().with_cmd("false", None, 3, ""));
        let (mut ctx, sink) = ctx_with(&fake, false);
        let err = ctx
            .step("fail", Command::new("false").changed_when(|_| false))
            .unwrap_err()
            .chain();
        assert!(err.contains('3'), "{err}");
        assert_eq!(finished_statuses(&sink), vec![Status::Failed]);
    }

    #[test]
    fn debug_hides_stdin_and_env_values_and_names_the_closure() {
        let op = Command::new("psql")
            .arg("app")
            .stdin("secret")
            .env("PGPASSWORD", "s3cret")
            .env("LANG", "C")
            .changed_when(|_| true);
        let d = format!("{op:?}");
        assert!(d.contains("stdin: Some(\"6 bytes\")"), "{d}");
        assert!(d.contains("changed_when: Some(\"<closure>\")"), "{d}");
        // Env names stay, so `{:?}` still says what the command was given.
        assert!(d.contains("\"PGPASSWORD\": \"6 bytes\""), "{d}");
        assert!(d.contains("\"LANG\": \"1 bytes\""), "{d}");
        // Neither a stdin secret nor an env secret appears anywhere.
        assert!(!d.contains("secret"), "{d}");
        assert!(!d.contains("s3cret"), "{d}");
        // What is not a secret is still printed in full.
        assert!(d.contains("program: \"psql\""), "{d}");
        assert!(d.contains("\"app\""), "{d}");
        let _ = op.clone();
    }
}
