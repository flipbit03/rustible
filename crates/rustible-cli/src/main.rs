//! Spike orchestrator: build a playbook per target triple, ship it, run it
//! over the framed protocol, render the events. Local and SSH transports.

mod transport;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use rustible_sdk::HostInfo;
use rustible_sdk::event::{Event, EventSink, Pretty};
use rustible_sdk::protocol::{Down, Up};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use transport::Transport;

#[derive(Parser, Debug)]
#[command(name = "rustible", about = "Rustible orchestrator (spike)")]
struct Cli {
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
    let cli = Cli::parse();
    let t_start = Instant::now();
    let mut vars_map = serde_json::Map::new();
    for kv in &cli.vars {
        let Some((k, v)) = kv.split_once('=') else {
            bail!("--var needs key=value, got `{kv}`");
        };
        let value = serde_json::from_str::<serde_json::Value>(v)
            .unwrap_or(serde_json::Value::String(v.to_string()));
        vars_map.insert(k.to_string(), value);
    }
    let vars_json = serde_json::Value::Object(vars_map);

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
        let bin = cli.bin.clone();
        let playbook = cli.playbook.clone();
        let vars_json = vars_json.clone();
        let (escalate, check, verbosity) = (cli.escalate, cli.check, cli.verbose);
        runs.push(tokio::spawn(async move {
            let (bytes, hash) = &artifacts[&triple];
            let remote_path = format!(".cache/rustible/bin/{bin}-{}-{hash}", playbook.replace('/', "_"));

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
                    connection: if name == "local" { "local".into() } else { "ssh".into() },
                    ..HostInfo::local()
                },
                vars: vars_json,
                check_mode: check,
                verbosity,
            };
            write_frame(&mut proc.stdin, &start).await?;

            let sink: Arc<dyn EventSink> = Arc::new(Pretty::new(std::io::stdout(), name.clone(), verbosity));
            let mut t_hello = None;
            let mut summary = None;
            loop {
                let Some(up) = read_frame::<_, Up>(&mut proc.stdout).await? else {
                    break;
                };
                match up {
                    Up::Hello { protocol, playbook: announced } => {
                        t_hello = Some(t_exec.elapsed());
                        if protocol != rustible_sdk::protocol::PROTOCOL_VERSION {
                            bail!(
                                "protocol mismatch: orchestrator speaks {}, binary speaks {protocol}; rebuild the workspace against this rustible",
                                rustible_sdk::protocol::PROTOCOL_VERSION
                            );
                        }
                        if announced != playbook {
                            bail!("asked for playbook `{playbook}`, binary answered with `{announced}`");
                        }
                        eprintln!("[{name}]  hello: protocol {protocol}, playbook {announced}, {:.2?} after exec", t_exec.elapsed());
                    }
                    Up::Event(ev) => {
                        if let Event::Finished(s) = &ev {
                            summary = Some(s.clone());
                        }
                        sink.emit(ev);
                    }
                }
            }
            let exit = proc.wait().await?;
            let stderr = proc.stderr_text().await;
            if !stderr.trim().is_empty() {
                for line in stderr.lines() {
                    eprintln!("[{name}]  stderr: {line}");
                }
            }
            eprintln!(
                "[{name}]  exit {exit}, hello after {:?}, total run {:.2?}",
                t_hello.map(|d| format!("{d:.2?}")).unwrap_or_else(|| "never".into()),
                t_exec.elapsed()
            );
            anyhow::Ok((name, exit, summary))
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
