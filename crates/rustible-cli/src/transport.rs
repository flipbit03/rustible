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
    /// The executable `kill` hunts for on a remote host, absolute. `None`
    /// for a process that is never a kill target (the upload shell).
    ///
    /// It is passed in rather than recovered from `argv`, because `argv`
    /// does not reliably contain it in a recoverable position: behind
    /// `sudo -n -u <someone-not-root>` the binary is the fifth word, not
    /// the first non-flag one. See `run::exec_argv`'s test.
    binary: Option<String>,
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
    /// shell. `binary` is the executable `kill` should hunt on the target,
    /// `None` when this process is never cancelled (the upload shell).
    pub async fn spawn(&self, argv: &[String], binary: Option<&str>) -> Result<Proc> {
        let (prog, rest) = argv.split_first().context("empty argv")?;
        let binary = binary.map(str::to_string);
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
                    binary,
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
                    binary,
                })
            }
        }
    }

    /// Stop a process that ignored `Cancel` for too long. Locally the whole
    /// process group goes (the binary and any helper it spawned); over SSH
    /// every process running the binary's path is killed, since the mux
    /// channel cannot signal the remote process.
    pub async fn kill(&self, proc: &mut Proc) -> Result<()> {
        match (&mut proc.waiter, self) {
            (Waiter::Local(child), _) => {
                if let Some(pid) = child.id() {
                    // Negative pid: the process group we created at spawn.
                    let _ = tokio::process::Command::new("kill")
                        .args(["-KILL", "--", &format!("-{pid}")])
                        .status()
                        .await;
                }
                child.start_kill().ok();
                Ok(())
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
                let Some(bin) = &proc.binary else {
                    bail!("no kill target for this process");
                };
                self.sh(&kill_script(bin)).await?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// The remote snippet that kills `bin` and its children. Only processes
/// actually running `bin` are signalled: `/proc/<pid>/exe` is the kernel's
/// answer to "what is this process running", where a command line is just
/// text that anything, this script included, can carry.
fn kill_script(bin: &str) -> String {
    let q = shell_quote(bin);
    format!(
        "for p in $(pgrep -f {q}); do \
           [ \"$(readlink /proc/$p/exe 2>/dev/null)\" = {q} ] || continue; \
           pkill -KILL -P \"$p\"; kill -KILL \"$p\"; \
         done 2>/dev/null; true"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kill_script_only_signals_processes_running_the_binary() {
        let script = kill_script("/home/cadu/.cache/rustible/bin/cadu_slow-ab12");
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
        let nasty = kill_script("/tmp/x'; rm -rf /; '");
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

    #[tokio::test]
    async fn local_probe_and_exists() {
        let t = Transport::Local;
        let p = t.probe().await.unwrap();
        assert!(p.home.starts_with('/'));
        assert!(p.triple.ends_with("-unknown-linux-musl"));
        assert!(t.exists("/bin/sh").await.unwrap());
        assert!(!t.exists("/definitely/not/here").await.unwrap());
    }
}
