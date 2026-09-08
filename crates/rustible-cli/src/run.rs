//! `rustible playbook run`: the pipeline of vision doc 5.2, steps 1 to 9.
//!
//! Workspace and inventory; `--describe` (cached); resolve the target hosts
//! and validate every host's vars through `--check-vars`; connect to every
//! host in parallel; probe triple and `$HOME`; one `dist` build for every
//! triple; upload if missing; execute with the host's escalation prefix and
//! drive the protocol; render.
//!
//! While the protocol runs, this side also answers the binary's
//! `FileRequest`s from the workspace root, writes its `FetchChunk`s back
//! under the same root, and turns ctrl-c into a `Cancel` frame to every
//! host with a ten second grace period before the binary is killed
//! (vision doc 5.5).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustible_cli::inventory::{
    Connection, Escalate, HostResults, Inventory, Resolved, Severity, Source, VarError,
    bag_to_json, format_vars_report,
};
use rustible_sdk::HostInfo;
use rustible_sdk::event::Event;
use rustible_sdk::protocol::{Down, PROTOCOL_VERSION, Up};
use rustible_sdk::runtime::{self, HostCheck, HostVars};
use rustible_sdk::secret::Secret;
use rustible_sdk::stream::{WorkspaceFiles, chunks};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::describe::{self, Cargo, Describe};
use crate::render::Renderer;
use crate::transport::{Probe, SshTarget, Transport};
use crate::usage;
use crate::workspace::Workspace;

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Playbook file (`playbooks/cadu/mc.rs`) or name (`cadu/mc`).
    pub playbook: String,
    /// Dry run: report what would change, change nothing.
    #[arg(long)]
    pub check: bool,
    /// -v shows diffs and facts, -vv every command.
    #[arg(short, action = clap::ArgAction::Count)]
    pub verbose: u8,
    /// Playbook vars, `key=value`; JSON-looking values are parsed as JSON.
    /// These win over the inventory.
    #[arg(long = "var")]
    pub vars: Vec<String>,
    /// Comma-separated hosts or groups; only those of the playbook's
    /// `hosts` that match run.
    #[arg(long)]
    pub limit: Option<String>,
    /// Print the raw frames as JSON lines instead of rendering.
    #[arg(long)]
    pub json: bool,
    /// Name of an environment variable holding the escalation password, for
    /// hosts where `sudo -n` is refused. It travels in the `Start` frame,
    /// never on a command line, and is zeroized after use.
    #[arg(long, value_name = "VAR")]
    pub escalate_password_env: Option<String>,
}

/// How long a cancelled binary gets to stop between steps before the
/// orchestrator kills it (vision doc 5.5).
pub const CANCEL_GRACE: Duration = Duration::from_secs(10);

/// Exit codes of `playbook run`.
pub const EXIT_FAILED: u8 = 2;

/// The hosts a run targets: the attribute's `hosts`, narrowed by `--limit`
/// (each entry a host or group name; a name outside the playbook's hosts
/// is an error, as is an empty result).
pub fn select_targets(inv: &Inventory, hosts: &str, limit: Option<&str>) -> Result<Vec<Resolved>> {
    let all = inv
        .select(hosts)
        .map_err(|e| usage(format!("the playbook targets `{hosts}`: {e}")))?;
    let names: Vec<String> = all.iter().map(|h| h.name.clone()).collect();
    let chosen: Vec<String> = match limit {
        None => names.clone(),
        Some(limit) => {
            let mut keep = BTreeSet::new();
            for item in limit.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let picked = inv
                    .select(item)
                    .map_err(|e| usage(format!("--limit {item}: {e}")))?;
                let mut hit = false;
                for h in picked {
                    if names.contains(&h.name) {
                        keep.insert(h.name.clone());
                        hit = true;
                    }
                }
                if !hit {
                    return Err(usage(format!(
                        "--limit {item}: not among the playbook's hosts (`{hosts}` = {})",
                        names.join(", ")
                    )));
                }
            }
            names
                .iter()
                .filter(|n| keep.contains(*n))
                .cloned()
                .collect()
        }
    };
    if chosen.is_empty() {
        return Err(usage(format!("`{hosts}` resolves to no hosts")));
    }
    chosen
        .iter()
        .map(|n| inv.resolve(n).map_err(|e| anyhow::anyhow!("{e}")))
        .collect()
}

/// The `Start.vars` object: the host's merged bag, then `--var` on top
/// (vision 10.3 precedence).
pub fn merged_vars(resolved: &Resolved, cli: &serde_json::Map<String, Value>) -> Value {
    let mut obj = match bag_to_json(&resolved.vars) {
        Value::Object(o) => o,
        _ => Default::default(),
    };
    for (k, v) in cli {
        obj.insert(k.clone(), v.clone());
    }
    Value::Object(obj)
}

/// How the binary is launched on the target (vision 5.2 step 8, 11.3):
/// bare, or behind the host's escalation method as `escalate_user`.
pub fn exec_argv(bin: &str, escalate: bool, method: Escalate, escalate_user: &str) -> Vec<String> {
    let mut argv = vec![];
    if escalate {
        match method {
            Escalate::Sudo => argv.extend(["sudo".to_string(), "-n".to_string()]),
            Escalate::Doas => argv.extend(["doas".to_string(), "-n".to_string()]),
            Escalate::None => {}
        }
        if method != Escalate::None && escalate_user != "root" {
            argv.extend(["-u".to_string(), escalate_user.to_string()]);
        }
    }
    argv.push(bin.to_string());
    argv.push("--remote".to_string());
    argv
}

/// `<home>/.cache/rustible/bin/<playbook with / as _>-<sha256>` (vision 5.2
/// step 7; absolute, so no shell expands anything later).
pub fn remote_path(home: &str, playbook: &str, hash: &str) -> String {
    format!(
        "{}/.cache/rustible/bin/{}-{hash}",
        home.trim_end_matches('/'),
        playbook.replace('/', "_")
    )
}

/// The transport target for a resolved host. Parameters the inventory left
/// to the built-in default are not passed, so `~/.ssh/config` applies.
pub fn ssh_target(r: &Resolved) -> Result<SshTarget> {
    let set = |name: &str| {
        r.sources
            .params
            .get(name)
            .is_some_and(|s| *s != Source::BuiltIn)
    };
    Ok(SshTarget {
        addr: r
            .params
            .addr
            .clone()
            .with_context(|| format!("host `{}` has no addr", r.host))?,
        user: set("ssh_user").then(|| r.params.ssh_user.clone()),
        port: set("port").then_some(r.params.port),
        args: r.params.ssh_args.clone(),
    })
}

/// Where rendered output goes: the step view, or raw frames as JSON lines.
pub enum Output {
    Pretty(Renderer<std::io::Stdout>),
    /// One object per line: `{"host", "frame"}` for every frame the binary
    /// sent, `{"host", "error"}`, `{"host", "stderr"}`, `{"host", "exit"}`.
    Json {
        w: std::io::Stdout,
        failed: bool,
    },
}

impl Output {
    fn json(&mut self, host: &str, key: &str, value: Value) {
        if let Output::Json { w, .. } = self {
            let line = serde_json::json!({ "host": host, key: value });
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }

    fn frame(&mut self, host: &str, up: &Up) {
        match self {
            Output::Pretty(r) => {
                if let Up::Event(ev) = up {
                    r.event(host, ev);
                }
            }
            Output::Json { failed, .. } => {
                if let Up::Event(Event::Finished(s)) = up
                    && s.failed > 0
                {
                    *failed = true;
                }
                self.json(
                    host,
                    "frame",
                    serde_json::to_value(up).unwrap_or(Value::Null),
                );
            }
        }
    }

    fn note(&mut self, host: &str, msg: &str) {
        if let Output::Pretty(r) = self {
            r.note(host, msg);
        }
    }

    /// A `ctx.fetch` that landed. `--json` gets the path and size rather
    /// than the `FetchChunk` frames themselves: a fetched file can be
    /// hundreds of megabytes and its bytes are already on disk.
    fn fetched(&mut self, host: &str, dest: &str, path: &Path, bytes: u64) {
        match self {
            Output::Pretty(r) => r.note(
                host,
                &format!("fetched `{dest}` ({bytes} bytes) to {}", path.display()),
            ),
            Output::Json { .. } => self.json(
                host,
                "fetched",
                serde_json::json!({ "dest": dest, "path": path, "bytes": bytes }),
            ),
        }
    }

    fn failed(&mut self, host: &str, msg: &str) {
        match self {
            Output::Pretty(r) => r.failed(host, msg),
            Output::Json { failed, .. } => {
                *failed = true;
                self.json(host, "error", Value::String(msg.into()));
            }
        }
    }

    fn stderr(&mut self, host: &str, text: &str) {
        match self {
            Output::Pretty(r) => r.stderr(host, text),
            Output::Json { .. } => self.json(host, "stderr", Value::String(text.into())),
        }
    }

    fn exited(&mut self, host: &str, code: i32) {
        match self {
            Output::Pretty(r) => r.exited(host, code),
            Output::Json { failed, .. } => {
                *failed |= code != 0;
                self.json(host, "exit", Value::from(code));
            }
        }
    }

    /// Close the run; whether any host failed.
    fn finish(&mut self) -> bool {
        match self {
            Output::Pretty(r) => r.finish(),
            Output::Json { failed, .. } => *failed,
        }
    }
}

type Shared = Arc<Mutex<Output>>;

/// Parse `--var k=v` flags into one object.
pub fn cli_vars(flags: &[String]) -> Result<serde_json::Map<String, Value>> {
    let mut map = serde_json::Map::new();
    for kv in flags {
        let (k, v) =
            rustible_sdk::vars::parse_var(kv).map_err(|e| usage(format!("--var: {e:#}")))?;
        map.insert(k, v);
    }
    Ok(map)
}

/// `--check-vars` output split into what the vision 10.3 report takes and
/// the warnings (undeclared vars) as `(host, message)`.
pub fn split_checks(checks: Vec<HostCheck>) -> (HostResults, Vec<(String, String)>) {
    let mut results: HostResults = vec![];
    let mut warnings = vec![];
    for c in checks {
        let mut errs = vec![];
        for p in c.problems {
            match p.severity {
                runtime::Severity::Error => errs.push(VarError {
                    var: p.var,
                    severity: Severity::Error,
                    message: p.message,
                }),
                runtime::Severity::Warning => warnings.push((c.host.clone(), p.message)),
            }
        }
        results.push((c.host, errs));
    }
    (results, warnings)
}

/// Validate every target's vars through the playbook binary before
/// anything is built for a target (vision 5.2 step 3). `Ok(None)` is
/// clean; `Ok(Some(report))` is the vision 10.3 message. Warnings are not
/// printed here: the binary repeats them at `Start`, rendered with the run.
pub async fn precheck(
    ws: &Workspace,
    inv: &Inventory,
    cargo: &Cargo,
    d: &Describe,
    targets: &[Resolved],
    cli: &serde_json::Map<String, Value>,
) -> Result<Option<String>> {
    if d.vars_schema.is_null() {
        return Ok(None);
    }
    let bin = describe::check_binary(cargo, d).await?;
    let input: Vec<HostVars> = targets
        .iter()
        .map(|r| HostVars {
            host: r.host.clone(),
            vars: merged_vars(r, cli),
        })
        .collect();
    let (results, _warnings) = split_checks(describe::check_vars(&bin, &d.name, &input).await?);
    Ok(format_vars_report(
        &d.hosts,
        inv.groups.contains_key(&d.hosts),
        &ws.playbook_file(&d.name),
        &ws.config.inventory.display().to_string(),
        &results,
    ))
}

pub async fn run(ws: &Workspace, inv: &Inventory, args: RunArgs) -> Result<u8> {
    let t_start = Instant::now();
    let name = ws
        .playbook_name(&args.playbook)
        .map_err(|e| usage(format!("{e:#}")))?;
    let cli = cli_vars(&args.vars)?;
    let cargo = Cargo::load(&ws.manifest()).await?;

    // 2. Metadata from the compiled playbook.
    let d = describe::describe_playbook(ws, &cargo, &name).await?;

    // 3. Hosts, then vars for every one of them, before anything else.
    let targets = select_targets(inv, &d.hosts, args.limit.as_deref())?;
    if let Some(report) = precheck(ws, inv, &cargo, &d, &targets, &cli).await? {
        eprint!("{report}");
        return Ok(1);
    }

    let host_names: Vec<String> = targets.iter().map(|r| r.host.clone()).collect();
    let out: Shared = Arc::new(Mutex::new(if args.json {
        Output::Json {
            w: std::io::stdout(),
            failed: false,
        }
    } else {
        Output::Pretty(Renderer::new(std::io::stdout(), &host_names, args.verbose))
    }));

    let escalate_password = match &args.escalate_password_env {
        Some(var) => Some(Secret::from(
            std::env::var(var).with_context(|| format!("reading ${var}"))?,
        )),
        None => None,
    };
    let files = Arc::new(
        WorkspaceFiles::new(&ws.root)
            .with_context(|| format!("serving files from {}", ws.root.display()))?,
    );

    // Ctrl-c: every host's frame loop watches this and sends `Cancel`. The
    // signal handler only flips the flag, so no step is interrupted
    // mid-apply (vision 5.5).
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!(
                "\nctrl-c: cancelling; each host gets {CANCEL_GRACE:?} to stop between steps"
            );
            let _ = cancel_tx.send(true);
        }
    });

    // 4, 5. Connect and probe every host in parallel.
    let mut connects = vec![];
    for r in targets {
        let out = out.clone();
        connects.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let connected: Result<(Transport, Probe)> = async {
                let tr = match r.params.connection {
                    Connection::Local => Transport::Local,
                    Connection::Ssh => Transport::ssh(&ssh_target(&r)?).await?,
                };
                let probe = tr.probe().await?;
                Ok((tr, probe))
            }
            .await;
            match connected {
                Ok((tr, probe)) => {
                    out.lock().unwrap().note(
                        &r.host,
                        &format!(
                            "connected: {} home {} in {:.2?}",
                            probe.triple,
                            probe.home,
                            t0.elapsed()
                        ),
                    );
                    Some((r, tr, probe))
                }
                Err(e) => {
                    out.lock().unwrap().failed(&r.host, &format!("{e:#}"));
                    None
                }
            }
        }));
    }
    let mut hosts = vec![];
    for c in connects {
        if let Some(h) = c.await? {
            hosts.push(h);
        }
    }

    if !hosts.is_empty() {
        // 6. One build for every triple.
        let triples: Vec<String> = hosts
            .iter()
            .map(|h| h.2.triple.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let t0 = Instant::now();
        // Not `?`: the hosts are connected, so a build failure has to be
        // reported against them and their control masters closed, or the run
        // ends with no summary and an ssh master left behind for its
        // ControlPersist window.
        let built = build_artifacts(&cargo, &name, &triples).await;
        let artifacts = match built {
            Ok(a) => a,
            Err(e) => {
                for (r, tr, _) in hosts {
                    out.lock().unwrap().failed(&r.host, &format!("{e:#}"));
                    tr.close().await;
                }
                let failed = out.lock().unwrap().finish();
                debug_assert!(failed);
                return Ok(EXIT_FAILED);
            }
        };
        if args.verbose >= 1 {
            eprintln!(
                "built {} for {} in {:.2?}",
                name,
                triples.join(", "),
                t0.elapsed()
            );
        }

        // 7, 8, 9. Per host: upload if missing, execute, stream frames.
        let plan = Arc::new(Plan {
            name: name.clone(),
            escalate: d.escalate,
            check: args.check,
            verbosity: args.verbose,
            files,
            escalate_password,
        });
        let mut runs = vec![];
        for (r, tr, probe) in hosts {
            let artifact = artifacts[&probe.triple].clone();
            let (out, plan) = (out.clone(), plan.clone());
            let vars = merged_vars(&r, &cli);
            let cancel_rx = cancel_rx.clone();
            runs.push(tokio::spawn(async move {
                if let Err(e) =
                    drive(&plan, &r, &tr, &probe, &artifact, vars, &out, cancel_rx).await
                {
                    out.lock().unwrap().failed(&r.host, &format!("{e:#}"));
                }
                tr.close().await;
            }));
        }
        for r in runs {
            r.await?;
        }
    }

    let failed = out.lock().unwrap().finish();
    if args.verbose >= 1 {
        eprintln!("total {:.2?}", t_start.elapsed());
    }
    Ok(if failed { EXIT_FAILED } else { 0 })
}

/// One cargo build for every triple in play, then the bytes and the hash of
/// each artifact.
async fn build_artifacts(
    cargo: &Cargo,
    name: &str,
    triples: &[String],
) -> Result<BTreeMap<String, Arc<(Vec<u8>, String)>>> {
    cargo.build(Some(name), triples).await?;
    let mut artifacts = BTreeMap::new();
    for t in triples {
        let p = cargo.dist_bin(t);
        let bytes = std::fs::read(&p).with_context(|| format!("reading {}", p.display()))?;
        let hash = describe::hex(&Sha256::digest(&bytes));
        artifacts.insert(t.clone(), Arc::new((bytes, hash)));
    }
    Ok(artifacts)
}

/// What every host of one run shares.
struct Plan {
    name: String,
    escalate: bool,
    check: bool,
    verbosity: u8,
    /// Serves `FileRequest` and receives `FetchChunk`, rooted at the
    /// workspace so a playbook cannot read outside it.
    files: Arc<WorkspaceFiles>,
    escalate_password: Option<Secret>,
}

/// Steps 7 to 9 for one host: upload if missing, execute, drive the
/// protocol until EOF, hand every frame to the output.
#[allow(clippy::too_many_arguments)]
async fn drive(
    plan: &Plan,
    r: &Resolved,
    tr: &Transport,
    probe: &Probe,
    artifact: &(Vec<u8>, String),
    vars: Value,
    out: &Shared,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let host = r.host.as_str();
    let name = plan.name.as_str();
    let (bytes, hash) = artifact;
    let path = remote_path(&probe.home, name, hash);

    let t0 = Instant::now();
    let cached = tr.exists(&path).await?;
    if !cached {
        tr.upload(bytes, &path).await?;
    }
    out.lock().unwrap().note(
        host,
        &format!(
            "binary {} ({} bytes) in {:.2?}",
            if cached { "already cached" } else { "uploaded" },
            bytes.len(),
            t0.elapsed()
        ),
    );

    if plan.escalate && r.params.escalate == Escalate::None {
        out.lock().unwrap().note(
            host,
            "playbook says escalate, host has escalate=\"none\": running unescalated",
        );
    }
    let argv = exec_argv(
        &path,
        plan.escalate,
        r.params.escalate,
        &r.params.escalate_user,
    );
    let mut proc = tr.spawn(&argv).await?;
    // From here on the child owns the failure story: `sudo -n` refusing, a
    // binary that dies at once, a desynced stream. Returning early would drop
    // its stderr and leave the user with "Broken pipe", so every error below
    // is caught and told with what the child said.
    match drive_frames(
        tr, &mut proc, plan, r, probe, vars, out, host, name, cancel_rx,
    )
    .await
    {
        Ok(()) => {}
        Err(e) => {
            let stderr = proc.stderr_text().await;
            let stderr = stderr.trim();
            return Err(if stderr.is_empty() {
                e
            } else {
                e.context(format!("the playbook binary said: {stderr}"))
            });
        }
    }
    let exit = proc.wait().await?;
    let stderr = proc.stderr_text().await;
    if !stderr.trim().is_empty() {
        out.lock().unwrap().stderr(host, stderr.trim_end());
    }
    out.lock().unwrap().exited(host, exit);
    Ok(())
}

/// `Start` down, every `Up` frame to the renderer, until the binary closes
/// its stdout.
#[allow(clippy::too_many_arguments)]
async fn drive_frames(
    tr: &Transport,
    proc: &mut crate::transport::Proc,
    plan: &Plan,
    r: &Resolved,
    probe: &Probe,
    vars: Value,
    out: &Shared,
    host: &str,
    name: &str,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    let _ = probe;
    let start = Down::Start {
        run_id: format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ),
        playbook: name.to_string(),
        host: HostInfo {
            name: host.to_string(),
            groups: r.groups.clone(),
            escalate_user: r.params.escalate_user.clone(),
            escalate_method: r.params.escalate.as_str().to_string(),
            connection: r.params.connection.as_str().to_string(),
        },
        vars,
        check_mode: plan.check,
        verbosity: plan.verbosity,
        escalate_password: plan.escalate_password.clone(),
    };
    write_frame(&mut proc.stdin, &start).await?;

    // The frames are read by their own task: `read_exact` is not
    // cancel-safe, so it cannot sit directly in the `select!` below.
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
                let Some(up) = frame else { break };
                handle_frame(plan, proc, out, host, name, up?).await?;
            }
            changed = cancel_rx.changed(), if deadline.is_none() => {
                if changed.is_err() || !*cancel_rx.borrow() {
                    continue;
                }
                eprintln!("[{host}] sending Cancel");
                // A binary that already exited has closed its stdin; that is
                // fine, the read side reports the exit.
                let _ = write_frame(&mut proc.stdin, &Down::Cancel).await;
                deadline = Some(tokio::time::Instant::now() + CANCEL_GRACE);
            }
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(tokio::time::Instant::now)),
                if deadline.is_some() =>
            {
                eprintln!(
                    "[{host}] cancelled: the running step did not finish within {CANCEL_GRACE:?}, killing the binary"
                );
                tr.kill(proc).await?;
                break;
            }
        }
    }
    Ok(())
}

/// One `Up` frame: the `Hello` check, an event to render, a file to serve,
/// or a chunk of a fetched file to write.
async fn handle_frame(
    plan: &Plan,
    proc: &mut crate::transport::Proc,
    out: &Shared,
    host: &str,
    name: &str,
    up: Up,
) -> Result<()> {
    match &up {
        Up::Hello { protocol, playbook } => {
            if *protocol != PROTOCOL_VERSION {
                bail!(
                    "protocol mismatch: rustible speaks {PROTOCOL_VERSION}, the binary speaks {protocol}; \
                     rebuild the workspace against this rustible"
                );
            }
            if playbook != name {
                bail!("asked for playbook `{name}`, the binary answered with `{playbook}`");
            }
        }
        Up::FileRequest { req, path } => {
            let (req, path) = (*req, path.clone());
            let t0 = Instant::now();
            match plan.files.open(&path) {
                Err(reason) => {
                    out.lock()
                        .unwrap()
                        .note(host, &format!("denied file request `{path}`: {reason}"));
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
                    out.lock().unwrap().note(
                        host,
                        &format!("sent `{path}` ({total} bytes) in {:.2?}", t0.elapsed()),
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
            let written = plan
                .files
                .write_chunk(dest, *offset, bytes)
                .map_err(|reason| anyhow::anyhow!("fetch to `{dest}` refused: {reason}"))?;
            if *last {
                out.lock()
                    .unwrap()
                    .fetched(host, dest, &written, offset + bytes.len() as u64);
            }
            // Not `frame`: the chunk's bytes would be re-encoded into the
            // JSON stream after being written to disk.
            return Ok(());
        }
        Up::Event(_) => {}
    }
    out.lock().unwrap().frame(host, &up);
    Ok(())
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

/// Same ceiling the SDK's framing uses. A playbook that writes to stdout
/// desyncs the stream, and four bytes of prose read as a huge length; a cap
/// turns that into a protocol error instead of an allocation.
const MAX_FRAME: usize = 64 * 1024 * 1024;

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
    if len > MAX_FRAME {
        bail!(
            "protocol error: the binary announced a {len}-byte frame (limit {MAX_FRAME}); \
             a playbook that prints to stdout desyncs the stream, use ctx.log instead"
        );
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(serde_json::from_slice(&body)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Usage;

    #[tokio::test]
    async fn read_frame_refuses_an_absurd_length() {
        // Four bytes of prose from a stray println! read as a length.
        let mut stream: &[u8] = b"hello, world";
        let err = read_frame::<_, Up>(&mut stream)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("protocol error"), "{err}");
        assert!(err.contains("ctx.log"), "{err}");
        // A clean EOF is still not an error.
        let mut empty: &[u8] = b"";
        assert!(read_frame::<_, Up>(&mut empty).await.unwrap().is_none());
    }

    const INV: &str = r#"
defaults ssh_user="cadu"
vars { fruit "banana" }
group "lab" {
    vars { package "mc" }
    host "local" connection="local"
    host "arm" addr="cadu-cogram-vm-arm" port=2222 escalate="doas" escalate_user="admin"
}
host "solo" addr="10.0.0.9"
"#;

    fn inv() -> Inventory {
        Inventory::parse(INV, "hosts.kdl").unwrap()
    }

    #[test]
    fn escalation_argv() {
        let bin = "/home/cadu/.cache/rustible/bin/cadu_mc-abc";
        assert_eq!(
            exec_argv(bin, false, Escalate::Sudo, "root"),
            [bin, "--remote"]
        );
        assert_eq!(
            exec_argv(bin, true, Escalate::Sudo, "root"),
            ["sudo", "-n", bin, "--remote"]
        );
        assert_eq!(
            exec_argv(bin, true, Escalate::Doas, "root"),
            ["doas", "-n", bin, "--remote"]
        );
        assert_eq!(
            exec_argv(bin, true, Escalate::Sudo, "admin"),
            ["sudo", "-n", "-u", "admin", bin, "--remote"]
        );
        assert_eq!(
            exec_argv(bin, true, Escalate::None, "admin"),
            [bin, "--remote"]
        );
    }

    #[test]
    fn remote_path_is_absolute_and_flat() {
        assert_eq!(
            remote_path("/home/cadu", "cadu/mc", "abc"),
            "/home/cadu/.cache/rustible/bin/cadu_mc-abc"
        );
        assert_eq!(
            remote_path("/root/", "hello", "1"),
            "/root/.cache/rustible/bin/hello-1"
        );
    }

    #[test]
    fn limit_narrows_the_playbooks_hosts() {
        let inv = inv();
        let names = |v: Vec<Resolved>| v.into_iter().map(|r| r.host).collect::<Vec<_>>();
        assert_eq!(
            names(select_targets(&inv, "lab", None).unwrap()),
            ["local", "arm"]
        );
        assert_eq!(
            names(select_targets(&inv, "lab", Some("arm")).unwrap()),
            ["arm"]
        );
        assert_eq!(
            names(select_targets(&inv, "lab", Some("lab, local")).unwrap()),
            ["local", "arm"]
        );
        let e = select_targets(&inv, "lab", Some("solo")).unwrap_err();
        assert!(e.downcast_ref::<Usage>().is_some());
        assert!(
            e.to_string().contains("not among the playbook's hosts"),
            "{e}"
        );
        let e = select_targets(&inv, "nowhere", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("targets `nowhere`"), "{e}");
    }

    #[test]
    fn ssh_target_passes_only_explicit_parameters() {
        let inv = inv();
        let arm = ssh_target(&inv.resolve("arm").unwrap()).unwrap();
        assert_eq!(
            arm,
            SshTarget {
                addr: "cadu-cogram-vm-arm".into(),
                user: Some("cadu".into()),
                port: Some(2222),
                args: vec![],
            }
        );
        let solo = ssh_target(&inv.resolve("solo").unwrap()).unwrap();
        assert_eq!(solo.port, None, "built-in port stays with ssh");
        assert_eq!(solo.user.as_deref(), Some("cadu"), "from defaults");
    }

    #[test]
    fn cli_vars_win_over_the_inventory() {
        let inv = inv();
        let r = inv.resolve("local").unwrap();
        let cli = cli_vars(&["package=htop".into(), "port=22".into()]).unwrap();
        let v = merged_vars(&r, &cli);
        assert_eq!(v["package"], "htop");
        assert_eq!(v["fruit"], "banana");
        assert_eq!(v["port"], 22);
        assert!(cli_vars(&["novalue".into()]).is_err());
    }
}
