//! The `rustible` command. `run` is the spike orchestrator (build a playbook
//! per target triple, ship it, run it over the framed protocol, render the
//! events, serve `FileRequest`s from the workspace, write `FetchChunk`s under
//! it, and turn ctrl-c into a `Cancel` frame with a ten second grace period);
//! `init` and `playbook create` scaffold workspaces and playbooks; `inventory
//! show` and `inventory check` are the M2 subcommands over
//! `rustible_cli::inventory`.

mod create;
mod init;
mod transport;
mod workspace;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rustible_cli::inventory::{Inventory, render_show};
use rustible_sdk::HostInfo;
use rustible_sdk::event::{Event, EventSink, Pretty};
use rustible_sdk::protocol::{Down, Up};
use rustible_sdk::secret::Secret;
use rustible_sdk::stream::{WorkspaceFiles, chunks};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use transport::{Proc, Transport};

/// How long a cancelled binary gets to stop between steps before it is killed.
const CANCEL_GRACE: Duration = Duration::from_secs(10);

#[derive(Parser, Debug)]
#[command(
    name = "rustible",
    about = "Configuration management as real code",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Build a playbook and run it on hosts (spike orchestrator; `playbook
    /// run` replaces it in M3).
    Run(RunArgs),
    /// Create a Rustible workspace in a directory.
    Init(init::InitArgs),
    /// Playbook commands.
    Playbook {
        #[command(subcommand)]
        cmd: PlaybookCmd,
    },
    /// Inspect and validate `hosts.kdl`.
    Inventory {
        #[command(subcommand)]
        cmd: InventoryCmd,
    },
}

#[derive(Subcommand, Debug)]
enum PlaybookCmd {
    /// Scaffold a playbook file.
    Create(create::CreateArgs),
}

#[derive(Subcommand, Debug)]
enum InventoryCmd {
    /// Resolved parameters and vars of one host, with the source of each.
    Show {
        /// Host name as written in the inventory.
        host: String,
        /// Inventory file.
        #[arg(long, default_value = "hosts.kdl")]
        file: PathBuf,
    },
    /// Load the inventory and report every error (exit 1 on any).
    Check {
        /// Inventory file.
        #[arg(long, default_value = "hosts.kdl")]
        file: PathBuf,
    },
}

#[derive(clap::Args, Debug)]
struct RunArgs {
    /// Rustible workspace directory (a Cargo package generated like
    /// `examples/workspace`).
    #[arg(long, default_value = "examples/workspace")]
    workspace: PathBuf,
    /// The workspace's bin name (its package name).
    #[arg(long, default_value = "workspace")]
    bin: String,
    /// Playbook name inside the workspace, e.g. `cadu/mc`.
    #[arg(long)]
    playbook: String,
    /// Hosts: `local` or `user@addr`. Repeatable.
    #[arg(long = "host", required = true)]
    hosts: Vec<String>,
    /// Run the binary under sudo on the target. This is Ansible's `become`;
    /// named `escalate` because `become` is a reserved keyword in Rust and
    /// `r#become` everywhere is ugly.
    #[arg(long)]
    escalate: bool,
    /// Name of an environment variable holding the sudo password for
    /// per-step escalation (`ctx.as_root()`) on hosts where `sudo -n` is
    /// refused. Sent in the `Start` frame, never on a command line.
    #[arg(long, value_name = "VAR")]
    escalate_password_env: Option<String>,
    #[arg(long)]
    check: bool,
    #[arg(short, action = clap::ArgAction::Count)]
    verbose: u8,
    /// Playbook vars, `key=value`; JSON-looking values are parsed as JSON.
    #[arg(long = "var")]
    vars: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Run(args) => run(args).await,
        Cmd::Init(args) => init::run(args),
        Cmd::Playbook {
            cmd: PlaybookCmd::Create(args),
        } => create::run(args),
        Cmd::Inventory { cmd } => inventory(cmd),
    }
}

/// `inventory show` and `inventory check`. Load errors go to stderr one per
/// line as `file:line:col: error: message`; any error exits 1.
fn inventory(cmd: InventoryCmd) -> Result<()> {
    match cmd {
        InventoryCmd::Show { host, file } => {
            let inv = load_or_exit(&file);
            match inv.resolve(&host) {
                Ok(resolved) => print!("{}", render_show(&resolved)),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        InventoryCmd::Check { file } => {
            let inv = load_or_exit(&file);
            println!(
                "{}: ok ({} hosts, {} groups)",
                file.display(),
                inv.hosts.len(),
                inv.groups.len()
            );
        }
    }
    Ok(())
}

fn load_or_exit(file: &std::path::Path) -> Inventory {
    match Inventory::load(file) {
        Ok(inv) => inv,
        Err(errs) => {
            for e in errs.iter() {
                eprintln!("{e}");
            }
            let n = errs.len();
            eprintln!(
                "{}: {n} error{}",
                file.display(),
                if n == 1 { "" } else { "s" }
            );
            std::process::exit(1);
        }
    }
}

async fn run(cli: RunArgs) -> Result<()> {
    let t_start = Instant::now();
    let mut vars_map = serde_json::Map::new();
    for kv in &cli.vars {
        let (k, v) = rustible_sdk::vars::parse_var(kv).map_err(|e| anyhow::anyhow!("{e:#}"))?;
        vars_map.insert(k, v);
    }
    let vars_json = serde_json::Value::Object(vars_map);
    let escalate_password = match &cli.escalate_password_env {
        Some(var) => Some(Secret::from(
            std::env::var(var).with_context(|| format!("reading ${var}"))?,
        )),
        None => None,
    };
    let files = Arc::new(
        WorkspaceFiles::new(&cli.workspace)
            .with_context(|| format!("workspace {}", cli.workspace.display()))?,
    );

    // Ctrl-c: every host run watches this and sends `Cancel`.
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!(
                "\nctrl-c: cancelling; each host gets {CANCEL_GRACE:?} to stop between steps"
            );
            let _ = cancel_tx.send(true);
        }
    });

    // 1. Connect to every host in parallel and probe its triple.
    let mut connects = vec![];
    for h in &cli.hosts {
        let h = h.clone();
        connects.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let tr = Transport::connect(&h).await?;
            let t_conn = t0.elapsed();
            let t0 = Instant::now();
            let triple = tr.probe_triple().await?;
            let t_probe = t0.elapsed();
            anyhow::Ok((h, tr, triple, t_conn, t_probe))
        }));
    }
    let mut hosts = vec![];
    for c in connects {
        let (name, tr, triple, t_conn, t_probe) = c.await??;
        eprintln!("[{name}]  connected in {t_conn:.2?}, probed {triple} in {t_probe:.2?}");
        hosts.push((name, tr, triple));
    }

    // 2. One cargo invocation builds every needed triple (cargo locks the
    //    target dir, so parallel invocations would serialize anyway).
    let triples: Vec<String> = {
        let mut v: Vec<String> = hosts.iter().map(|h| h.2.clone()).collect();
        v.sort();
        v.dedup();
        v
    };
    let t0 = Instant::now();
    let manifest = cli.workspace.join("Cargo.toml");
    let mut cmd = tokio::process::Command::new("cargo");
    cmd.args([
        "build",
        "--profile",
        "dist",
        "--features",
        "selected",
        "--manifest-path",
    ])
    .arg(&manifest)
    .env("RUSTIBLE_PLAYBOOK", &cli.playbook);
    for t in &triples {
        cmd.args(["--target", t]);
    }
    let status = cmd.status().await.context("running cargo")?;
    if !status.success() {
        bail!("cargo build failed");
    }
    eprintln!("built {} in {:.2?}", triples.join(", "), t0.elapsed());

    let mut artifacts: BTreeMap<String, (Vec<u8>, String)> = BTreeMap::new();
    for t in &triples {
        let p = cli
            .workspace
            .join("target")
            .join(t)
            .join("dist")
            .join(&cli.bin);
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        let hash = hex(&Sha256::digest(&bytes));
        eprintln!("{t}: {} bytes, sha256 {}", bytes.len(), &hash[..16]);
        artifacts.insert(t.clone(), (bytes, hash));
    }
    let artifacts = Arc::new(artifacts);

    // 3. Per host: upload if missing, execute, drive the protocol, render.
    let mut runs = vec![];
    for (name, tr, triple) in hosts {
        let artifacts = artifacts.clone();
        let files = files.clone();
        let bin = cli.bin.clone();
        let playbook = cli.playbook.clone();
        let vars_json = vars_json.clone();
        let escalate_password = escalate_password.clone();
        let (escalate, check, verbosity) = (cli.escalate, cli.check, cli.verbose);
        let cancel_rx = cancel_rx.clone();
        runs.push(tokio::spawn(async move {
            let (bytes, hash) = &artifacts[&triple];
            let remote_path = format!(
                ".cache/rustible/bin/{bin}-{}-{hash}",
                playbook.replace('/', "_")
            );

            let t0 = Instant::now();
            let cached = tr.exists(&remote_path).await?;
            if !cached {
                tr.upload(bytes, &remote_path).await?;
            }
            let t_upload = t0.elapsed();
            eprintln!(
                "[{name}]  binary {} in {t_upload:.2?}",
                if cached { "already cached" } else { "uploaded" }
            );

            let mut argv = vec![];
            if escalate {
                argv.extend(["sudo".to_string(), "-n".to_string()]);
            }
            argv.push(format!("$HOME/{remote_path}"));
            argv.push("--remote".to_string());

            let t_exec = Instant::now();
            let mut proc = tr.spawn(&argv).await?;

            let start = Down::Start {
                run_id: format!("{:x}", t_exec.elapsed().as_nanos()),
                playbook: playbook.clone(),
                host: HostInfo {
                    name: name.clone(),
                    connection: if name == "local" {
                        "local".into()
                    } else {
                        "ssh".into()
                    },
                    ..HostInfo::local()
                },
                vars: vars_json,
                check_mode: check,
                verbosity,
                escalate_password,
            };
            write_frame(&mut proc.stdin, &start).await?;

            let sink: Arc<dyn EventSink> =
                Arc::new(Pretty::new(std::io::stdout(), name.clone(), verbosity));
            let mut driver = Driver {
                name: name.clone(),
                playbook: playbook.clone(),
                files,
                sink,
                t_exec,
                t_hello: None,
                summary: None,
            };
            let outcome = driver.drive(&tr, &mut proc, cancel_rx).await;
            let exit = proc.wait().await?;
            let stderr = proc.stderr_text().await;
            if !stderr.trim().is_empty() {
                for line in stderr.lines() {
                    eprintln!("[{name}]  stderr: {line}");
                }
            }
            eprintln!(
                "[{name}]  exit {exit}, hello after {}, total run {:.2?}",
                driver
                    .t_hello
                    .map(|d| format!("{d:.2?}"))
                    .unwrap_or_else(|| "never".into()),
                t_exec.elapsed()
            );
            outcome?;
            anyhow::Ok((name, exit, driver.summary))
        }));
    }

    let mut failed = false;
    for r in runs {
        match r.await? {
            Ok((name, exit, summary)) => {
                if exit != 0 || summary.as_ref().is_none_or(|s| s.failed > 0) {
                    failed = true;
                    eprintln!("[{name}]  FAILED");
                }
            }
            Err(e) => {
                failed = true;
                eprintln!("error: {e:#}");
            }
        }
    }
    eprintln!("\ntotal {:.2?}", t_start.elapsed());
    if failed {
        std::process::exit(2)
    } else {
        Ok(())
    }
}

/// One host's protocol loop: renders events, answers file requests, writes
/// fetched chunks, and handles cancellation.
struct Driver {
    name: String,
    playbook: String,
    files: Arc<WorkspaceFiles>,
    sink: Arc<dyn EventSink>,
    t_exec: Instant,
    t_hello: Option<Duration>,
    summary: Option<rustible_sdk::event::Summary>,
}

impl Driver {
    async fn drive(
        &mut self,
        tr: &Transport,
        proc: &mut Proc,
        mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        // Frames are read by their own task: `read_exact` is not cancel-safe,
        // so it cannot sit directly in the `select!`.
        let mut stdout = std::mem::replace(&mut proc.stdout, Box::pin(tokio::io::empty()));
        let (tx, mut frames) = tokio::sync::mpsc::channel::<Result<Up>>(64);
        tokio::spawn(async move {
            loop {
                match read_frame::<_, Up>(&mut stdout).await {
                    Ok(Some(up)) => {
                        if tx.send(Ok(up)).await.is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            tokio::select! {
                frame = frames.recv() => {
                    let Some(frame) = frame else { break };
                    self.handle(proc, frame?).await?;
                }
                changed = cancel_rx.changed(), if deadline.is_none() => {
                    if changed.is_err() || !*cancel_rx.borrow() {
                        continue;
                    }
                    eprintln!("[{}]  sending Cancel", self.name);
                    // A binary that already exited has closed its stdin; that
                    // is fine, the read side reports the exit.
                    let _ = write_frame(&mut proc.stdin, &Down::Cancel).await;
                    deadline = Some(tokio::time::Instant::now() + CANCEL_GRACE);
                }
                _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)), if deadline.is_some() => {
                    eprintln!("[{}]  cancelled: the running step did not finish within {CANCEL_GRACE:?}, killing the binary", self.name);
                    tr.kill(proc).await?;
                    break;
                }
            }
        }
        Ok(())
    }

    async fn handle(&mut self, proc: &mut Proc, up: Up) -> Result<()> {
        match up {
            Up::Hello {
                protocol,
                playbook: announced,
            } => {
                self.t_hello = Some(self.t_exec.elapsed());
                if protocol != rustible_sdk::protocol::PROTOCOL_VERSION {
                    bail!(
                        "protocol mismatch: orchestrator speaks {}, binary speaks {protocol}; rebuild the workspace against this rustible",
                        rustible_sdk::protocol::PROTOCOL_VERSION
                    );
                }
                if announced != self.playbook {
                    bail!(
                        "asked for playbook `{}`, binary answered with `{announced}`",
                        self.playbook
                    );
                }
                eprintln!(
                    "[{}]  hello: protocol {protocol}, playbook {announced}, {:.2?} after exec",
                    self.name,
                    self.t_exec.elapsed()
                );
            }
            Up::Event(ev) => {
                if let Event::Finished(s) = &ev {
                    self.summary = Some(s.clone());
                }
                self.sink.emit(ev);
            }
            Up::FileRequest { req, path } => {
                let t0 = Instant::now();
                match self.files.open(&path) {
                    Err(reason) => {
                        eprintln!("[{}]  denied file request `{path}`: {reason}", self.name);
                        write_frame(&mut proc.stdin, &Down::FileDenied { req, reason }).await?;
                    }
                    Ok(file) => {
                        let mut total = 0u64;
                        for chunk in chunks(file) {
                            let chunk = chunk.with_context(|| format!("reading `{path}`"))?;
                            total += chunk.bytes.len() as u64;
                            let last = chunk.last;
                            write_frame(
                                &mut proc.stdin,
                                &Down::FileChunk {
                                    req,
                                    offset: chunk.offset,
                                    bytes: chunk.bytes,
                                    last,
                                },
                            )
                            .await?;
                        }
                        eprintln!(
                            "[{}]  sent `{path}` ({total} bytes) in {:.2?}",
                            self.name,
                            t0.elapsed()
                        );
                    }
                }
            }
            Up::FetchChunk {
                dest,
                offset,
                bytes,
                last,
                ..
            } => {
                let written = self
                    .files
                    .write_chunk(&dest, offset, &bytes)
                    .map_err(|reason| anyhow::anyhow!("fetch to `{dest}` refused: {reason}"))?;
                if last {
                    eprintln!(
                        "[{}]  fetched `{dest}` ({} bytes) to {}",
                        self.name,
                        offset + bytes.len() as u64,
                        written.display()
                    );
                }
            }
        }
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn write_frame<W: tokio::io::AsyncWrite + Unpin, T: serde::Serialize>(
    w: &mut W,
    msg: &T,
) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

async fn read_frame<R: tokio::io::AsyncRead + Unpin, T: serde::de::DeserializeOwned>(
    r: &mut R,
) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}
