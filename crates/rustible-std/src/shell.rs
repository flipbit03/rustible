//! Running commands as steps. Ansible's `command`/`shell`, with `creates`.

use std::path::PathBuf;

use rustible_sdk::prelude::*;

/// Run a program with arguments. An action: always changes, unless
/// `creates`/`removes` make it skippable.
#[derive(Debug, Clone)]
pub struct Command {
    program: String,
    args: Vec<String>,
    creates: Option<PathBuf>,
    removes: Option<PathBuf>,
    cwd: Option<PathBuf>,
}

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
            creates: None,
            removes: None,
            cwd: None,
        }
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

    /// Skip (report ok) if this path exists.
    pub fn creates(mut self, p: impl Into<PathBuf>) -> Self {
        self.creates = Some(p.into());
        self
    }

    /// Skip (report ok) if this path does not exist.
    pub fn removes(mut self, p: impl Into<PathBuf>) -> Self {
        self.removes = Some(p.into());
        self
    }

    pub fn cwd(mut self, p: impl Into<PathBuf>) -> Self {
        self.cwd = Some(p.into());
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
        if let Some(cwd) = &self.cwd {
            cmd = cmd.cwd(cwd.clone());
        }
        let out = cmd.run()?;
        Ok(CommandOutput {
            status: out.status,
            stdout: out.stdout_str(),
            stderr: out.stderr_str(),
        })
    }

    fn always_changes(&self) -> bool {
        self.creates.is_none() && self.removes.is_none()
    }
}
