//! How the orchestrator reaches a host: run a process locally, or over an SSH
//! ControlMaster session. The playbook binary never knows which.
//!
//! Ctrl-c on the orchestrator must not kill what it spawned: the binary is
//! told to stop with a `Cancel` frame and gets ten seconds (vision doc 5.5).
//! Local children therefore start in their own process group, and SSH uses
//! the native mux client so no per-command `ssh` process sits in the
//! terminal's foreground group; the master itself is daemonized by ssh.

use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use openssh::{KnownHosts, Session};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub enum Transport {
    Local,
    Ssh(Arc<Session>),
}

/// A running remote process with piped stdio.
pub struct Proc {
    pub stdin: Pin<Box<dyn AsyncWrite + Send>>,
    pub stdout: Pin<Box<dyn AsyncRead + Send>>,
    stderr: Pin<Box<dyn AsyncRead + Send>>,
    waiter: Waiter,
    /// What `kill` matches on a remote host.
    argv0: String,
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

impl Transport {
    pub async fn connect(spec: &str) -> Result<Transport> {
        if spec == "local" {
            return Ok(Transport::Local);
        }
        let session = Session::connect_mux(spec, KnownHosts::Accept)
            .await
            .with_context(|| format!("ssh to {spec}"))?;
        Ok(Transport::Ssh(Arc::new(session)))
    }

    /// Run a shell snippet, return stdout. The bootstrap probe and cache
    /// checks use this; everything else is the static binary.
    async fn sh(&self, script: &str) -> Result<(i32, String)> {
        match self {
            Transport::Local => {
                let out = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(script)
                    .process_group(0)
                    .output()
                    .await?;
                Ok((
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                ))
            }
            Transport::Ssh(s) => {
                let out = s.command("sh").arg("-c").arg(script).output().await?;
                Ok((
                    out.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&out.stdout).into_owned(),
                ))
            }
        }
    }

    pub async fn probe_triple(&self) -> Result<String> {
        let (_, out) = self.sh("uname -sm").await?;
        let out = out.trim();
        Ok(match out {
            "Linux x86_64" => "x86_64-unknown-linux-musl".into(),
            "Linux aarch64" => "aarch64-unknown-linux-musl".into(),
            other => bail!("unsupported target: {other:?}"),
        })
    }

    pub async fn exists(&self, rel_home_path: &str) -> Result<bool> {
        let (code, _) = self
            .sh(&format!("test -x \"$HOME/{rel_home_path}\""))
            .await?;
        Ok(code == 0)
    }

    /// Stream bytes to `$HOME/<path>` through the process's stdin, then
    /// atomically move into place and mark executable.
    pub async fn upload(&self, bytes: &[u8], rel_home_path: &str) -> Result<()> {
        let script = format!(
            "set -e; p=\"$HOME/{rel_home_path}\"; mkdir -p \"$(dirname \"$p\")\"; \
             cat > \"$p.tmp\"; chmod 755 \"$p.tmp\"; mv \"$p.tmp\" \"$p\""
        );
        let mut proc = self.spawn(&["sh".into(), "-c".into(), script]).await?;
        proc.stdin.write_all(bytes).await?;
        proc.stdin.shutdown().await?;
        // Close our handle so the remote sees EOF, then wait.
        proc.stdin = Box::pin(tokio::io::sink());
        let code = proc.wait().await?;
        if code != 0 {
            bail!(
                "upload failed with exit {code}: {}",
                proc.stderr_text().await
            );
        }
        Ok(())
    }

    /// Spawn argv with piped stdio. argv[0] may contain `$HOME`, which the
    /// remote shell expands; that is why it goes through `sh -c`.
    pub async fn spawn(&self, argv: &[String]) -> Result<Proc> {
        let script = argv.join(" ");
        let argv0 = argv
            .iter()
            .find(|a| !matches!(a.as_str(), "sudo" | "-n"))
            .cloned()
            .unwrap_or_default();
        match self {
            Transport::Local => {
                let mut child = tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(&script)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .process_group(0)
                    .spawn()
                    .with_context(|| format!("spawning {script}"))?;
                Ok(Proc {
                    stdin: Box::pin(child.stdin.take().unwrap()),
                    stdout: Box::pin(child.stdout.take().unwrap()),
                    stderr: Box::pin(child.stderr.take().unwrap()),
                    waiter: Waiter::Local(child),
                    argv0,
                })
            }
            Transport::Ssh(s) => {
                let mut child = s
                    .clone()
                    .arc_command("sh")
                    .arg("-c")
                    .arg(&script)
                    .stdin(openssh::Stdio::piped())
                    .stdout(openssh::Stdio::piped())
                    .stderr(openssh::Stdio::piped())
                    .spawn()
                    .await
                    .with_context(|| format!("spawning {script} over ssh"))?;
                Ok(Proc {
                    stdin: Box::pin(child.stdin().take().unwrap()),
                    stdout: Box::pin(child.stdout().take().unwrap()),
                    stderr: Box::pin(child.stderr().take().unwrap()),
                    waiter: Waiter::Ssh(Some(child)),
                    argv0,
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
            (Waiter::Ssh(_), Transport::Ssh(_)) => {
                let path = proc.argv0.replace("$HOME/", "");
                let (_, _) = self
                    .sh(&format!("pkill -KILL -f \"$HOME/{path}\" || true"))
                    .await?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
}
