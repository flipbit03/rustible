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
use rustible_sdk::event::Event;
use rustible_sdk::launch::{self, Answer, Launch, Mode, Next, Place, Spawn};
use rustible_sdk::protocol::{Down, MAX_FRAME, PROTOCOL_VERSION, Up};
use rustible_sdk::runtime::{self, HostCheck, HostVars};
use rustible_sdk::secret::Secret;
use rustible_sdk::stream::{WorkspaceFiles, chunks, run_dir_name};
use rustible_sdk::{HostInfo, InventoryLogin, LoginOverride};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::describe::{self, Cargo, Describe};
use crate::render::Renderer;
use crate::transport::{KillTarget, Probe, SshTarget, Transport};
use crate::usage;
use crate::workspace::Workspace;

#[derive(clap::Args, Debug)]
pub struct RunArgs {
    /// Playbook file (`playbooks/demo/mc.rs`) or name (`demo/mc`).
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
///
/// The binary's own position in the result is not fixed: it is first when
/// nothing escalates, third behind `sudo -n`, and fifth behind `sudo -n -u
/// <someone-not-root>`. Anything that needs the path (`Transport::kill`)
/// takes it as `remote_path` gave it, never by scanning this.
pub fn exec_argv(bin: &str, escalate: bool, method: Escalate, escalate_user: &str) -> Vec<String> {
    let mut argv = escalate_prefix(escalate, method, escalate_user);
    argv.push(bin.to_string());
    argv.push("--remote".to_string());
    argv
}

/// The escalation words the binary is launched behind, empty when it runs
/// as the login user. Whatever the binary creates on the target belongs to
/// that identity, so removing it later takes the same prefix.
pub fn escalate_prefix(escalate: bool, method: Escalate, escalate_user: &str) -> Vec<String> {
    let mut argv = vec![];
    if escalate {
        match method {
            Escalate::Sudo => argv.extend(["sudo".to_string(), "-n".to_string()]),
            Escalate::Doas => argv.extend(["doas".to_string(), "-n".to_string()]),
            Escalate::None => return argv,
        }
        if escalate_user != "root" {
            argv.extend(["-u".to_string(), escalate_user.to_string()]);
        }
    }
    argv
}

/// A run id, unique per host run: the orchestrator sends it in `Start` and
/// it names the run's temp directory on the target.
fn new_run_id() -> String {
    format!(
        "{:x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// `<home>/.cache/rustible/bin/<playbook with / as _>-<sha256>` (vision 5.2
/// step 7; absolute, so no shell expands anything later).
pub fn remote_path(home: &str, playbook: &str, hash: &str) -> String {
    format!(
        "{}/.cache/rustible/bin/{}",
        home.trim_end_matches('/'),
        binary_name(playbook, hash)
    )
}

/// The binary's file name in any account's cache: `<playbook with / as
/// _>-<sha256>`. A streamed copy is found by it, so it must change with the
/// contents.
pub fn binary_name(playbook: &str, hash: &str) -> String {
    format!("{}-{hash}", playbook.replace('/', "_"))
}

/// Whether the binary is streamed to `escalate_user` rather than run from
/// the login user's cache: when the playbook escalates to an account that is
/// neither root, which reads anything, nor the login user, whose cache it
/// is (vision 11.3, #62).
///
/// A login with no name (`id -un` failed: a uid without a passwd entry) is
/// not streamed to anyone; the launch is the one it always was.
pub fn streams_launch(escalate: bool, method: Escalate, escalate_user: &str, login: &str) -> bool {
    escalate
        && method != Escalate::None
        && !login.is_empty()
        && escalate_user != "root"
        && escalate_user != login
}

/// The transport target for a resolved host. Parameters the inventory left
/// to the built-in default are not passed, so `~/.ssh/config` applies. A
/// playbook's `ssh_user` replaces the host's, from whatever level, and is
/// always passed.
pub fn ssh_target(r: &Resolved, playbook_ssh_user: Option<&str>) -> Result<SshTarget> {
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
        user: match playbook_ssh_user {
            Some(u) => Some(u.to_string()),
            None => set("ssh_user").then(|| r.params.ssh_user.clone()),
        },
        port: set("port").then_some(r.params.port),
        args: r.params.ssh_args.clone(),
    })
}

/// What a playbook's `ssh_user` replaced on this host: the inventory's
/// value and the level that set it. Every message that has to say where the
/// login user came from renders it ([`LoginOverride::note`]), and the binary
/// gets it in `Start` for its own escalation failures. `None` when the
/// playbook leaves the login to the inventory.
pub fn login_override(r: &Resolved, playbook_ssh_user: Option<&str>) -> Option<LoginOverride> {
    let ssh_user = playbook_ssh_user?;
    // A built-in `ssh_user` is never passed (`ssh_target`), so what would
    // have decided is ssh's own default, not the value `inventory show`
    // prints for it.
    let inventory = match r.sources.params.get("ssh_user") {
        None | Some(Source::BuiltIn) => None,
        Some(source) => Some(InventoryLogin {
            ssh_user: r.params.ssh_user.clone(),
            source: source.to_string(),
        }),
    };
    Some(LoginOverride {
        ssh_user: ssh_user.to_string(),
        inventory,
    })
}

/// How a host is reached: as a local child, or over ssh to this target,
/// which carries the playbook's `ssh_user` when it sets one.
#[derive(Debug, PartialEq, Eq)]
pub enum Reach {
    Local,
    Ssh(SshTarget),
}

/// [`Reach`] for one host of a run.
pub fn reach(r: &Resolved, playbook_ssh_user: Option<&str>) -> Result<Reach> {
    Ok(match r.params.connection {
        Connection::Local => Reach::Local,
        Connection::Ssh => Reach::Ssh(ssh_target(r, playbook_ssh_user)?),
    })
}

/// The `HostInfo` the `Start` frame carries: the inventory's view of the
/// host, and where the login came from when the playbook chose it.
pub fn host_info(r: &Resolved, login_override: Option<LoginOverride>) -> HostInfo {
    HostInfo {
        name: r.host.clone(),
        groups: r.groups.clone(),
        escalate_user: r.params.escalate_user.clone(),
        escalate_method: r.params.escalate.as_str().to_string(),
        connection: r.params.connection.as_str().to_string(),
        login_override: login_override.map(Box::new),
    }
}

/// A run's hosts: [`select_targets`], then the refusal of a playbook
/// `ssh_user` on any of them reached without ssh. After `--limit`, so a
/// limit past the local hosts lets the run go ahead.
pub fn run_targets(inv: &Inventory, d: &Describe, limit: Option<&str>) -> Result<Vec<Resolved>> {
    let targets = select_targets(inv, &d.hosts, limit)?;
    if let Some(refusal) = local_login_refusal(d, &targets) {
        return Err(usage(format!(
            "{refusal}. Remove the attribute, or --limit the run to hosts reached over ssh"
        )));
    }
    Ok(targets)
}

/// The refusal for a playbook whose `ssh_user` reaches a host with
/// `connection="local"`, where there is no ssh login to change. `None` when
/// the playbook sets no `ssh_user` or every host is reached over ssh. The
/// caller adds what to do, which differs between `run` and
/// `inventory check`.
pub fn local_login_refusal(d: &Describe, targets: &[Resolved]) -> Option<String> {
    let ssh_user = d.ssh_user.as_deref()?;
    let local: Vec<String> = targets
        .iter()
        .filter(|r| r.params.connection == Connection::Local)
        .map(|r| {
            let from = r
                .sources
                .params
                .get("connection")
                .unwrap_or(&Source::BuiltIn);
            format!("`{}` (connection=\"local\", from {from})", r.host)
        })
        .collect();
    if local.is_empty() {
        return None;
    }
    Some(format!(
        "playbook `{}` sets `ssh_user = {ssh_user:?}`, but {} reached without ssh: {}; \
         there is no ssh login to change",
        d.name,
        if local.len() == 1 {
            "this host is"
        } else {
            "these hosts are"
        },
        local.join(", ")
    ))
}

/// The line for a binary that `sudo -n`/`doas -n` refused to launch, when
/// the playbook's `ssh_user` chose the account that escalated: that account
/// is the likely reason, and nothing else in the output names it. Refused
/// means the binary never said `Hello` and the last thing on stderr is the
/// escalation tool's own complaint (`sudo: ...`), so an exec failure, a bad
/// `Start` or a crash keeps its own story. The tool reporting that it could
/// not execute the binary is such an exec failure too: sudo's `unable to
/// execute <path>: ...` and `<path>: command not found`, and doas's
/// `<path>: ...`, the binary's path always being absolute. `None` in every
/// other case, which keep the output they had; the stderr is always shown as
/// well.
pub fn launch_escalation_failure(
    hello: bool,
    prefix: &[String],
    escalate_user: &str,
    stderr: &str,
    login: Option<&LoginOverride>,
) -> Option<String> {
    let login = login?;
    let method = prefix.first()?;
    if hello {
        return None;
    }
    let last = stderr.lines().map(str::trim).rfind(|l| !l.is_empty())?;
    let complaint = last.strip_prefix(&format!("{method}:"))?.trim_start();
    if complaint.starts_with("unable to execute") || complaint.starts_with('/') {
        return None;
    }
    Some(format!(
        "escalating to `{escalate_user}` with `{method} -n` failed before the playbook \
         started; {}",
        login.note()
    ))
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
                *failed |= reports_a_failed_host(up);
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

/// Whether a frame says its host failed: a summary counting a `failed`
/// step. `recovered` is a failure the playbook caught and never fails a
/// host, so it is not read here; the frame carries it to `--json` as is.
fn reports_a_failed_host(up: &Up) -> bool {
    matches!(up, Up::Event(Event::Finished(s)) if s.failed > 0)
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
    let cargo = Cargo::load(ws).await?;

    // 2. Metadata from the compiled playbook.
    let d = describe::describe_playbook(ws, &cargo, &name).await?;

    // 3. Hosts, then vars for every one of them, before anything else.
    let targets = run_targets(inv, &d, args.limit.as_deref())?;
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
        // In a loop, and not just the first press: tokio's handler stays
        // installed for the life of the process, so once this task ended
        // every later ctrl-c was swallowed and the terminal could no longer
        // stop the run at all. Local children are in their own process
        // group precisely so the signal does not reach them, which makes
        // this task the only thing standing between the user and a run they
        // cannot abort.
        let mut presses = 0u32;
        while tokio::signal::ctrl_c().await.is_ok() {
            presses += 1;
            if presses == 1 {
                eprintln!(
                    "\nctrl-c: cancelling; each host gets {CANCEL_GRACE:?} to stop between \
                     steps. Press ctrl-c again to quit at once."
                );
                let _ = cancel_tx.send(true);
            } else {
                eprintln!(
                    "\nctrl-c again: quitting now. Processes already started on the targets \
                     are left running, and an ssh master may persist for its ControlPersist \
                     window."
                );
                std::process::exit(EXIT_FAILED as i32);
            }
        }
    });

    // Between phases: connect, build and upload are not steps, so `Cancel`
    // means "do not start the next phase" rather than anything on a target.
    macro_rules! bail_if_cancelled {
        ($hosts:expr) => {
            if *cancel_rx.borrow() {
                // The branch returns, so taking the hosts here is a move on
                // a path that never reaches their later use.
                for (r, tr, _) in $hosts {
                    out.lock()
                        .unwrap()
                        .failed(&r.host, "cancelled before the playbook started");
                    tr.close().await;
                }
                out.lock().unwrap().finish();
                return Ok(EXIT_FAILED);
            }
        };
    }

    // 4, 5. Connect and probe every host in parallel.
    let mut connects = vec![];
    for r in targets {
        let out = out.clone();
        let ssh_user = d.ssh_user.clone();
        connects.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let login = login_override(&r, ssh_user.as_deref());
            let connected: Result<(Transport, Probe)> = async {
                let tr = match reach(&r, ssh_user.as_deref())? {
                    Reach::Local => Transport::Local,
                    Reach::Ssh(target) => Transport::ssh(&target)
                        .await
                        // An ssh refusal for an account the inventory never
                        // names is the case where saying where it came from
                        // matters most.
                        .map_err(|e| match &login {
                            Some(o) => anyhow::anyhow!(
                                "{}; {}",
                                format!("{e:#}").trim_end().trim_end_matches('.'),
                                o.note()
                            ),
                            None => e,
                        })?,
                };
                let probe = tr.probe().await?;
                Ok((tr, probe))
            }
            .await;
            match connected {
                Ok((tr, probe)) => {
                    let mut out = out.lock().unwrap();
                    out.note(
                        &r.host,
                        &format!(
                            "connected: {} home {} in {:.2?}",
                            probe.triple,
                            probe.home,
                            t0.elapsed()
                        ),
                    );
                    if let Some(o) = &login {
                        out.note(&r.host, &o.note());
                    }
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

    bail_if_cancelled!(hosts);

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
        // `cargo` runs in this process group, so a ctrl-c during the build
        // reached it too and it may have died; either way the run stops here
        // rather than uploading and starting playbooks nobody is waiting for.
        bail_if_cancelled!(hosts);

        // 7, 8, 9. Per host: upload if missing, execute, stream frames.
        let plan = Arc::new(Plan {
            name: name.clone(),
            escalate: d.escalate,
            ssh_user: d.ssh_user.clone(),
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
    /// The playbook's `ssh_user`, which replaced every host's own.
    ssh_user: Option<String>,
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
    let login = login_override(r, plan.ssh_user.as_deref());
    let run_id = new_run_id();

    if plan.escalate && r.params.escalate == Escalate::None {
        out.lock().unwrap().note(
            host,
            "playbook says escalate, host has escalate=\"none\": running unescalated",
        );
    }
    let t0 = Instant::now();
    let (mut proc, prefix) = if streams_launch(
        plan.escalate,
        r.params.escalate,
        &r.params.escalate_user,
        &probe.user,
    ) {
        // The login user's cache is no use to this account, so nothing is
        // uploaded there: the bytes go to the account from memory.
        let user = &r.params.escalate_user;
        let streamed = launch_streamed(
            tr,
            r,
            &binary_name(name, hash),
            bytes,
            &run_id,
            login.as_ref(),
        )
        .await?;
        let how = match streamed.installed {
            None => format!("already cached for `{user}`"),
            Some(Place::Home) => format!("streamed to `{user}`"),
            Some(Place::Temp) => format!("streamed to a private temp directory for `{user}`"),
        };
        out.lock().unwrap().note(
            host,
            &format!(
                "binary {how} ({} bytes) in {:.2?}",
                bytes.len(),
                t0.elapsed()
            ),
        );
        (streamed.proc, streamed.prefix)
    } else {
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
        let argv = exec_argv(
            &path,
            plan.escalate,
            r.params.escalate,
            &r.params.escalate_user,
        );
        let prefix = escalate_prefix(plan.escalate, r.params.escalate, &r.params.escalate_user);
        let proc = tr
            .spawn(
                &argv,
                Some(KillTarget {
                    binary: path.clone(),
                    by_name: false,
                    run_dir: run_dir_name(&run_id),
                    escalate: prefix.clone(),
                    ephemeral: None,
                }),
            )
            .await?;
        (proc, prefix)
    };
    // From here on the child owns the failure story: `sudo -n` refusing, a
    // binary that dies at once, a desynced stream. Returning early would drop
    // its stderr and leave the user with "Broken pipe", so every error below
    // is caught and told with what the child said.
    let mut hello = false;
    let killed = match drive_frames(
        tr,
        &mut proc,
        plan,
        r,
        login.clone(),
        vars,
        out,
        host,
        name,
        cancel_rx,
        &run_id,
        &mut hello,
    )
    .await
    {
        Ok(killed) => killed,
        Err(e) => {
            let stderr = proc.stderr_text().await;
            let stderr = stderr.trim();
            let escalation = launch_escalation_failure(
                hello,
                &prefix,
                &r.params.escalate_user,
                stderr,
                login.as_ref(),
            );
            let e = if stderr.is_empty() {
                e
            } else {
                e.context(format!("the playbook binary said: {stderr}"))
            };
            return Err(match escalation {
                Some(line) => e.context(line),
                None => e,
            });
        }
    };
    if killed {
        // The binary was killed on purpose. Its exit status says only how it
        // died, and over SSH `wait` reports the channel's own view of that
        // ("the remote process has terminated"), so the host is closed with
        // the reason rather than with that.
        let _ = proc.wait().await;
        let stderr = proc.stderr_text().await;
        if !stderr.trim().is_empty() {
            out.lock().unwrap().stderr(host, stderr.trim_end());
        }
        out.lock().unwrap().failed(
            host,
            &format!(
                "cancelled: the running step did not finish within {CANCEL_GRACE:?}, the binary was killed"
            ),
        );
        return Ok(());
    }
    let exit = proc.wait().await?;
    let stderr = proc.stderr_text().await;
    if !stderr.trim().is_empty() {
        out.lock().unwrap().stderr(host, stderr.trim_end());
    }
    if exit != 0
        && let Some(line) = launch_escalation_failure(
            hello,
            &prefix,
            &r.params.escalate_user,
            &stderr,
            login.as_ref(),
        )
    {
        out.lock().unwrap().failed(host, &line);
    }
    out.lock().unwrap().exited(host, exit);
    Ok(())
}

/// A binary started by [`launch_streamed`]: the process, the escalation
/// words it runs behind, and where it had to be installed, if anywhere.
struct Streamed {
    proc: crate::transport::Proc,
    prefix: Vec<String>,
    installed: Option<Place>,
}

/// Start the binary as an `escalate_user` that cannot read the login
/// user's cache: stream it to that account's own cache, or to a private
/// per-run temp directory, through the spawns `rustible_sdk::launch`
/// describes, and run it from there (vision 11.3, #62). The SDK's helper
/// spawner drives the same plan with local processes.
///
/// Every spawn runs from `/` (the login's home may be unreadable to the
/// account, and macOS's `/bin/sh` says so on stderr), behind `sudo -n -H`
/// or `doas -n`: the launch never sends a password, so nothing here can
/// wait on one, and the first-byte deadline is only a backstop.
async fn launch_streamed(
    tr: &Transport,
    r: &Resolved,
    name: &str,
    bytes: &[u8],
    run_id: &str,
    login: Option<&LoginOverride>,
) -> Result<Streamed> {
    let user = r.params.escalate_user.as_str();
    let words = launch::escalation(r.params.escalate.as_str(), user, false, true)?;
    let mut plan = Launch::new(
        name,
        bytes.len() as u64,
        Mode::Remote,
        user,
        format!("escalating to `{user}`"),
    );
    loop {
        let spawn = plan.spawn();
        let mut argv: Vec<String> = ["sh", "-c", "cd / && exec \"$@\"", "rustible"]
            .map(String::from)
            .to_vec();
        argv.extend(words.iter().cloned());
        argv.extend(plan.argv(spawn));
        let next = match spawn {
            Spawn::Try(place) => {
                let kill = KillTarget {
                    binary: name.to_string(),
                    by_name: true,
                    run_dir: run_dir_name(run_id),
                    escalate: words.clone(),
                    ephemeral: (place == Place::Temp).then(|| plan.suffix().to_string()),
                };
                let mut proc = tr.spawn(&argv, Some(kill)).await?;
                let byte = first_byte(tr, &mut proc, user).await?;
                let Some(answer) = byte.and_then(Answer::from_byte) else {
                    return Err(launch_died(tr, &mut proc, byte, &words, user, login).await);
                };
                let next = plan.after_try(answer);
                if next == Next::Ready {
                    return Ok(Streamed {
                        proc,
                        prefix: words,
                        installed: plan.installed(),
                    });
                }
                proc.wait().await?;
                next
            }
            Spawn::Install(_) => {
                let mut proc = tr.spawn(&argv, None).await?;
                let byte = first_byte(tr, &mut proc, user).await?;
                if byte != Some(launch::INSTALL_READY) {
                    return Err(launch_died(tr, &mut proc, byte, &words, user, login).await);
                }
                // A write the script stopped reading fails; its exit status
                // and stderr say why, so that is what is reported.
                let _ = proc.stdin.write_all(bytes).await;
                let _ = proc.stdin.shutdown().await;
                proc.stdin = Box::pin(tokio::io::sink());
                let code = proc.wait().await?;
                let stderr = proc.stderr_text().await;
                plan.after_install(code, &stderr)
            }
        };
        match next {
            Next::Spawn(_) => continue,
            Next::Refuse(msg) => bail!(msg),
            Next::Ready => unreachable!("only a try answers Ready"),
        }
    }
}

/// A launch spawn's first byte, `None` at EOF. Nothing past
/// [`launch::FIRST_BYTE_DEADLINE`]: the process is killed and the launch
/// refused.
async fn first_byte(
    tr: &Transport,
    proc: &mut crate::transport::Proc,
    user: &str,
) -> Result<Option<u8>> {
    let mut b = [0u8];
    match tokio::time::timeout(launch::FIRST_BYTE_DEADLINE, proc.stdout.read(&mut b)).await {
        Ok(Ok(0)) => Ok(None),
        Ok(Ok(_)) => Ok(Some(b[0])),
        Ok(Err(e)) => Err(e.into()),
        Err(_) => {
            let _ = tr.kill(proc).await;
            bail!(launch::deadline_message(
                user,
                launch::FIRST_BYTE_DEADLINE,
                ""
            ))
        }
    }
}

/// A launch spawn that exited before it answered, or answered something no
/// script writes: the escalation tool's own refusal, with the playbook's
/// `ssh_user` named when it chose the account that escalated.
///
/// Its stdin is closed first, and one that answered nonsense is killed
/// before it is waited for: it is still running, and may be reading.
async fn launch_died(
    tr: &Transport,
    proc: &mut crate::transport::Proc,
    byte: Option<u8>,
    words: &[String],
    user: &str,
    login: Option<&LoginOverride>,
) -> anyhow::Error {
    proc.stdin = Box::pin(tokio::io::sink());
    if byte.is_some() {
        let _ = tr.kill(proc).await;
    }
    let code = proc.wait().await.unwrap_or(-1);
    let stderr = proc.stderr_text().await;
    let said = match stderr.trim() {
        "" => String::new(),
        s => format!(": {}", s.replace('\n', " / ")),
    };
    let what = match byte {
        Some(b) => format!(" after answering {:?}", b as char),
        None => String::new(),
    };
    let e = anyhow::anyhow!(
        "escalating to `{user}` with `{}` exited {code}{what} before the playbook started{said}",
        words.join(" ")
    );
    match launch_escalation_failure(false, words, user, &stderr, login) {
        Some(line) => e.context(line),
        None => e,
    }
}

/// `Start` down, every `Up` frame to the renderer, until the binary closes
/// its stdout. `true` when the binary had to be killed after ignoring
/// `Cancel` for the whole grace period. `hello` is set once the binary's
/// `Hello` arrives, and stays set if a later frame fails, so the caller can
/// tell a binary that never started from one that broke mid-run.
#[allow(clippy::too_many_arguments)]
async fn drive_frames(
    tr: &Transport,
    proc: &mut crate::transport::Proc,
    plan: &Plan,
    r: &Resolved,
    login_override: Option<LoginOverride>,
    vars: Value,
    out: &Shared,
    host: &str,
    name: &str,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    run_id: &str,
    hello: &mut bool,
) -> Result<bool> {
    let start = Down::Start {
        run_id: run_id.to_string(),
        playbook: name.to_string(),
        host: host_info(r, login_override),
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
    let mut killed = false;
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(up) = frame else { break };
                let up = up?;
                *hello |= matches!(up, Up::Hello { .. });
                handle_frame(plan, proc, out, host, name, up, &cancel_rx).await?;
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
                killed = true;
                break;
            }
        }
    }
    Ok(killed)
}

/// One `Up` frame: the `Hello` check, an event to render, a file to serve,
/// or a chunk of a fetched file to write.
///
/// Serving a file is the one arm that can run for minutes (a 50 MB stream
/// over a slow link), and while it does, nothing else watches `cancel_rx`.
/// It therefore checks the flag between chunks and abandons the transfer,
/// so ctrl-c during a stream is answered rather than queued behind it.
#[allow(clippy::too_many_arguments)]
async fn handle_frame(
    plan: &Plan,
    proc: &mut crate::transport::Proc,
    out: &Shared,
    host: &str,
    name: &str,
    up: Up,
    cancel_rx: &tokio::sync::watch::Receiver<bool>,
) -> Result<()> {
    match &up {
        Up::Hello { protocol, playbook } => {
            if *protocol != PROTOCOL_VERSION {
                bail!(
                    "protocol mismatch: rustible speaks {PROTOCOL_VERSION}, the binary speaks {protocol}; \
                     the CLI and every rustible crate in the workspace must be the same release. \
                     To move the workspace to this CLI's version, see {}",
                    crate::describe::UPGRADING_URL
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
                        // `borrow`, not `borrow_and_update`: the select loop
                        // still has to see the change and send `Cancel`.
                        if *cancel_rx.borrow() {
                            let reason = "cancelled while the file was being sent".to_string();
                            out.lock().unwrap().note(
                                host,
                                &format!("abandoning `{path}` after {total} bytes: cancelled"),
                            );
                            // Denied rather than silence: the binary's
                            // `local_file` fails now instead of waiting for
                            // chunks that will never come.
                            write_frame(&mut proc.stdin, &Down::FileDenied { req, reason }).await?;
                            return Ok(());
                        }
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

/// Capped at the SDK's own `MAX_FRAME`, the one its framing trims a failed
/// command's stderr to fit. A playbook that writes to stdout desyncs the
/// stream, and four bytes of prose read as a huge length; the cap turns that
/// into a protocol error instead of an allocation.
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
    host "arm" addr="10.0.3.11" port=2222 escalate="doas" escalate_user="admin"
}
host "solo" addr="10.0.0.9"
"#;

    fn inv() -> Inventory {
        Inventory::parse(INV, "hosts.kdl").unwrap()
    }

    #[test]
    fn escalation_argv() {
        let bin = "/home/cadu/.cache/rustible/bin/demo_mc-abc";
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

    /// Why `Transport::spawn` is handed the binary's path instead of
    /// recovering it from `argv`. A filter that skips the escalation words
    /// finds the binary in three of these four shapes and `-u` in the
    /// fourth, so a host with a non-root `escalate_user` would hunt for a
    /// process running `-u`, find none, and cancel nothing while reporting
    /// that it had killed the binary.
    #[test]
    fn the_binary_is_not_at_a_fixed_place_in_exec_argv() {
        let bin = "/home/admin/.cache/rustible/bin/cadu_slow-abc";
        let skip_escalation = |argv: &Vec<String>| -> String {
            argv.iter()
                .find(|a| !matches!(a.as_str(), "sudo" | "doas" | "-n"))
                .cloned()
                .unwrap_or_default()
        };
        assert_eq!(
            skip_escalation(&exec_argv(bin, true, Escalate::Sudo, "root")),
            bin
        );
        assert_eq!(
            skip_escalation(&exec_argv(bin, true, Escalate::Sudo, "admin")),
            "-u",
            "a non-root escalate_user puts a flag where the scan expects the binary"
        );
    }

    #[test]
    fn remote_path_is_absolute_and_flat() {
        assert_eq!(
            remote_path("/home/cadu", "demo/mc", "abc"),
            "/home/cadu/.cache/rustible/bin/demo_mc-abc"
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
        let arm = ssh_target(&inv.resolve("arm").unwrap(), None).unwrap();
        assert_eq!(
            arm,
            SshTarget {
                addr: "10.0.3.11".into(),
                user: Some("cadu".into()),
                port: Some(2222),
                args: vec![],
            }
        );
        let solo = ssh_target(&inv.resolve("solo").unwrap(), None).unwrap();
        assert_eq!(solo.port, None, "built-in port stays with ssh");
        assert_eq!(solo.user.as_deref(), Some("cadu"), "from defaults");
    }

    /// `--json`'s verdict reads `failed` and nothing else: a host that only
    /// recovered from failures did not fail, and the frame carries
    /// `recovered` to the script as it is.
    #[test]
    fn json_fails_a_host_on_failed_and_never_on_recovered() {
        use rustible_sdk::event::Summary;
        let finished = |failed, recovered| {
            Up::Event(Event::Finished(Summary {
                failed,
                recovered,
                ..Default::default()
            }))
        };
        assert!(!reports_a_failed_host(&finished(0, 0)));
        assert!(!reports_a_failed_host(&finished(0, 3)));
        assert!(reports_a_failed_host(&finished(1, 0)));
        assert!(reports_a_failed_host(&finished(1, 4)));
        let json = serde_json::to_value(finished(0, 3)).unwrap();
        assert_eq!(json["Event"]["Finished"]["recovered"], 3, "{json}");
    }

    /// Every inventory level a playbook's `ssh_user` has to beat, and the
    /// built-in default.
    const LOGINS: &str = r#"
defaults ssh_user="cadu"
group "games" ssh_user="gamer" {
    host "by-group" addr="10.0.4.1"
    host "by-host" addr="10.0.4.2" ssh_user="admin"
}
host "by-defaults" addr="10.0.4.3"
host "laptop" connection="local"
"#;

    const BUILT_IN: &str = r#"host "bare" addr="10.0.4.4" port=2200"#;

    fn describe(ssh_user: Option<&str>) -> Describe {
        Describe {
            name: "games/minecraft".into(),
            hosts: "games".into(),
            escalate: false,
            ssh_user: ssh_user.map(Into::into),
            vars_schema: Value::Null,
        }
    }

    #[test]
    fn the_playbooks_ssh_user_beats_every_inventory_level() {
        let inv = Inventory::parse(LOGINS, "hosts.kdl").unwrap();
        for (host, inventory, source) in [
            ("by-host", "admin", "host"),
            ("by-group", "gamer", "group games"),
            ("by-defaults", "cadu", "defaults"),
        ] {
            let r = inv.resolve(host).unwrap();
            assert_eq!(
                ssh_target(&r, None).unwrap().user.as_deref(),
                Some(inventory),
                "{host} without the attribute"
            );
            assert_eq!(
                ssh_target(&r, Some("minecraft")).unwrap().user.as_deref(),
                Some("minecraft"),
                "{host} with the attribute"
            );
            assert_eq!(
                login_override(&r, Some("minecraft")),
                Some(LoginOverride {
                    ssh_user: "minecraft".into(),
                    inventory: Some(InventoryLogin {
                        ssh_user: inventory.into(),
                        source: source.into(),
                    }),
                }),
                "{host}"
            );
            assert_eq!(login_override(&r, None), None, "{host}");
        }
    }

    /// With nothing in the file, the host's login is left to ssh (no `-l`);
    /// the attribute still passes its own, and the override records that
    /// the inventory had only the built-in default.
    #[test]
    fn the_playbooks_ssh_user_is_passed_even_where_the_inventory_passes_none() {
        let inv = Inventory::parse(BUILT_IN, "hosts.kdl").unwrap();
        let r = inv.resolve("bare").unwrap();
        assert_eq!(ssh_target(&r, None).unwrap().user, None);
        let t = ssh_target(&r, Some("minecraft")).unwrap();
        assert_eq!(t.user.as_deref(), Some("minecraft"));
        assert_eq!(t.port, Some(2200), "nothing else moves");
        let o = login_override(&r, Some("minecraft")).unwrap();
        assert_eq!(o.inventory, None, "ssh's own default, not a value");
    }

    #[test]
    fn a_playbook_ssh_user_on_a_local_host_is_refused() {
        let inv = Inventory::parse(LOGINS, "hosts.kdl").unwrap();
        let laptop = vec![inv.resolve("laptop").unwrap()];
        let refusal = local_login_refusal(&describe(Some("minecraft")), &laptop).unwrap();
        assert_eq!(
            refusal,
            "playbook `games/minecraft` sets `ssh_user = \"minecraft\"`, but this host is \
             reached without ssh: `laptop` (connection=\"local\", from host); there is no ssh \
             login to change"
        );
        // No attribute, nothing to refuse.
        assert_eq!(local_login_refusal(&describe(None), &laptop), None);
        // Only the hosts actually targeted count: `--limit` past the local
        // one leaves the run allowed.
        let remote: Vec<Resolved> = ["by-host", "by-group"]
            .iter()
            .map(|h| inv.resolve(h).unwrap())
            .collect();
        assert_eq!(
            local_login_refusal(&describe(Some("minecraft")), &remote),
            None
        );
        let mixed = vec![laptop[0].clone(), remote[0].clone(), laptop[0].clone()];
        let refusal = local_login_refusal(&describe(Some("minecraft")), &mixed).unwrap();
        assert!(refusal.contains("these hosts are"), "{refusal}");
        assert!(!refusal.contains("by-host"), "{refusal}");
    }

    #[test]
    fn a_launch_refused_by_sudo_names_where_the_login_came_from() {
        let login = LoginOverride {
            ssh_user: "minecraft".into(),
            inventory: Some(InventoryLogin {
                ssh_user: "cadu".into(),
                source: "defaults".into(),
            }),
        };
        let sudo = escalate_prefix(true, Escalate::Sudo, "root");
        let stderr = "sudo: a password is required\n";
        assert_eq!(
            launch_escalation_failure(false, &sudo, "root", stderr, Some(&login)).unwrap(),
            "escalating to `root` with `sudo -n` failed before the playbook started; the \
             login user `minecraft` comes from the playbook's `ssh_user` attribute, which \
             overrides the inventory's `cadu` (from defaults)"
        );
        let doas = escalate_prefix(true, Escalate::Doas, "admin");
        let line = launch_escalation_failure(
            false,
            &doas,
            "admin",
            "doas: Operation not permitted\n\n",
            Some(&login),
        )
        .unwrap();
        assert!(
            line.starts_with("escalating to `admin` with `doas -n`"),
            "{line}"
        );

        let blamed =
            |hello: bool, prefix: &[String], stderr: &str, login: Option<&LoginOverride>| {
                launch_escalation_failure(hello, prefix, "root", stderr, login).is_some()
            };
        // The inventory chose the login: the output stays what it was.
        assert!(!blamed(false, &sudo, stderr, None));
        // The binary started, so the launch did not fail.
        assert!(!blamed(true, &sudo, stderr, Some(&login)));
        // Nothing escalated: `escalate = false`, or a host with escalate="none".
        let none = escalate_prefix(true, Escalate::None, "root");
        assert!(!blamed(false, &none, stderr, Some(&login)));
        // Died before Hello for another reason: an exec failure, a crash, a
        // bad Start. sudo let it through, so it is not sudo's refusal.
        assert!(!blamed(false, &sudo, "", Some(&login)));
        assert!(!blamed(
            false,
            &sudo,
            "sh: 1: /home/x/bin: Exec format error\n",
            Some(&login)
        ));
        assert!(!blamed(
            false,
            &sudo,
            "sudo: unable to resolve host x\nthread 'main' panicked at src/main.rs:1:1\n",
            Some(&login)
        ));
        // The tool let the binary through and could not execute it: a
        // `noexec` home, a missing file.
        let bin = "/home/minecraft/.cache/rustible/bin/games_minecraft-ab12";
        for stderr in [
            format!("sudo: unable to execute {bin}: Permission denied"),
            format!("sudo: {bin}: command not found"),
        ] {
            assert!(!blamed(false, &sudo, &stderr, Some(&login)), "{stderr}");
        }
        let doas_root = escalate_prefix(true, Escalate::Doas, "root");
        assert!(!blamed(
            false,
            &doas_root,
            &format!("doas: {bin}: Permission denied"),
            Some(&login)
        ));
        assert!(blamed(
            false,
            &doas_root,
            "doas: a password is required",
            Some(&login)
        ));
        // `doas:` is not `sudo:`'s complaint.
        assert!(!blamed(
            false,
            &sudo,
            "doas: Operation not permitted",
            Some(&login)
        ));
    }

    // ---- the frame loop, against a local stand-in for the binary ----

    /// A shell script standing in for the playbook binary: it reads the
    /// `Start` frame into `start` (one byte at a time, so it takes exactly
    /// the frame off the pipe), then says `Hello` or not, prints `stderr`,
    /// and exits 3.
    fn stand_in(start: &Path, name: &str, hello: bool, stderr: &str) -> String {
        stand_in_speaking(start, name, hello.then_some(PROTOCOL_VERSION), stderr)
    }

    /// [`stand_in`], saying `Hello` with this protocol version when there is
    /// one.
    fn stand_in_speaking(start: &Path, name: &str, hello: Option<u32>, stderr: &str) -> String {
        let mut script = format!(
            "#!/bin/sh\nset -e\n\
             set -- $(dd bs=1 count=4 2>/dev/null | od -An -tu1)\n\
             n=$(( ($1 << 24) + ($2 << 16) + ($3 << 8) + $4 ))\n\
             dd bs=1 count=$n of='{}' 2>/dev/null\n",
            start.display()
        );
        if let Some(protocol) = hello {
            let json = serde_json::to_string(&Up::Hello {
                protocol,
                playbook: name.into(),
            })
            .unwrap();
            let len = (json.len() as u32).to_be_bytes();
            script.push_str(&format!(
                "printf '\\{:03o}\\{:03o}\\{:03o}\\{:03o}%s' '{json}'\n",
                len[0], len[1], len[2], len[3]
            ));
        }
        script.push_str(&format!("echo '{stderr}' >&2\nexit 3\n"));
        script
    }

    fn plan(dir: &Path, ssh_user: Option<&str>) -> Plan {
        Plan {
            name: "games/minecraft".into(),
            escalate: false,
            ssh_user: ssh_user.map(Into::into),
            check: false,
            verbosity: 0,
            files: Arc::new(WorkspaceFiles::new(dir).unwrap()),
            escalate_password: None,
        }
    }

    fn quiet(host: &str) -> Shared {
        Arc::new(Mutex::new(Output::Pretty(Renderer::new(
            std::io::stdout(),
            &[host.to_string()],
            0,
        ))))
    }

    fn by_defaults() -> Resolved {
        Inventory::parse(LOGINS, "hosts.kdl")
            .unwrap()
            .resolve("by-defaults")
            .unwrap()
    }

    /// `hello` is what tells a binary that never started from one that
    /// broke mid-run, and only the first may be blamed on `sudo -n`.
    #[tokio::test]
    async fn the_frame_loop_records_whether_the_binary_said_hello() {
        for said_hello in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let script = dir.path().join("bin");
            std::fs::write(
                &script,
                stand_in(
                    &dir.path().join("start"),
                    "games/minecraft",
                    said_hello,
                    "sudo: a password is required",
                ),
            )
            .unwrap();
            let tr = Transport::Local;
            let mut proc = tr
                .spawn(&["sh".into(), script.display().to_string()], None)
                .await
                .unwrap();
            let r = by_defaults();
            let (_tx, rx) = tokio::sync::watch::channel(false);
            let mut hello = false;
            let killed = drive_frames(
                &tr,
                &mut proc,
                &plan(dir.path(), None),
                &r,
                None,
                Value::Null,
                &quiet(&r.host),
                &r.host,
                "games/minecraft",
                rx,
                "run",
                &mut hello,
            )
            .await
            .unwrap();
            assert!(!killed);
            assert_eq!(hello, said_hello);
            assert_eq!(proc.wait().await.unwrap(), 3);
        }
    }

    /// A binary of the previous protocol is refused at its `Hello`, before
    /// any event of it is read: version 6's `Summary` counts every failed
    /// step as `failed`, and would be read as this version's verdict.
    #[tokio::test]
    async fn the_frame_loop_refuses_a_binary_of_the_previous_protocol() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("bin");
        std::fs::write(
            &script,
            stand_in_speaking(
                &dir.path().join("start"),
                "games/minecraft",
                Some(PROTOCOL_VERSION - 1),
                "",
            ),
        )
        .unwrap();
        let tr = Transport::Local;
        let mut proc = tr
            .spawn(&["sh".into(), script.display().to_string()], None)
            .await
            .unwrap();
        let r = by_defaults();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let mut hello = false;
        let e = drive_frames(
            &tr,
            &mut proc,
            &plan(dir.path(), None),
            &r,
            None,
            Value::Null,
            &quiet(&r.host),
            &r.host,
            "games/minecraft",
            rx,
            "run",
            &mut hello,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            e.starts_with(&format!(
                "protocol mismatch: rustible speaks {PROTOCOL_VERSION}, the binary speaks {}",
                PROTOCOL_VERSION - 1
            )),
            "{e}"
        );
        let _ = proc.wait().await;
    }

    /// The binary is told where its login came from, so its own escalation
    /// failures can say so: through the whole of `drive`, upload included.
    #[tokio::test]
    async fn drive_sends_the_login_override_in_start() {
        let dir = tempfile::tempdir().unwrap();
        let start = dir.path().join("start");
        let bytes = stand_in(&start, "games/minecraft", true, "").into_bytes();
        let artifact = (bytes, "abc".to_string());
        let probe = Probe {
            triple: "x86_64-unknown-linux-musl".into(),
            home: dir.path().display().to_string(),
            user: "cadu".into(),
        };
        let r = by_defaults();
        for ssh_user in [Some("minecraft"), None] {
            let (_tx, rx) = tokio::sync::watch::channel(false);
            drive(
                &plan(dir.path(), ssh_user),
                &r,
                &Transport::Local,
                &probe,
                &artifact,
                Value::Null,
                &quiet(&r.host),
                rx,
            )
            .await
            .unwrap();
            let frame = std::fs::read(&start).unwrap();
            let Down::Start { host, .. } = serde_json::from_slice(&frame).unwrap() else {
                panic!("not Start")
            };
            assert_eq!(host.name, "by-defaults");
            assert_eq!(
                host.login_override.map(|o| *o),
                login_override(&r, ssh_user),
                "{ssh_user:?}"
            );
        }
    }

    /// Both places `drive` blames a refused launch on the playbook's login:
    /// `sudo -n` refusing before it read `Start` (the write fails), and after
    /// (the binary's stdout just ends). The prefix is the literal `sudo`, so
    /// this runs the test binary again with a stand-in `sudo` first on
    /// `PATH`; changing `PATH` in this process would race every other test
    /// that spawns a program.
    #[test]
    fn drive_blames_a_refused_launch_on_the_playbooks_login() {
        use std::os::unix::fs::PermissionsExt;
        let line = "escalating to `root` with `sudo -n` failed before the playbook started; \
                    the login user `minecraft` comes from the playbook's `ssh_user` attribute";
        for mode in ["before-start", "after-start"] {
            let dir = tempfile::tempdir().unwrap();
            let sudo = dir.path().join("sudo");
            let script = match mode {
                "before-start" => {
                    "#!/bin/sh\necho 'sudo: a password is required' >&2\nexit 1\n".to_string()
                }
                _ => stand_in(
                    &dir.path().join("start"),
                    "games/minecraft",
                    false,
                    "sudo: a password is required",
                ),
            };
            std::fs::write(&sudo, script).unwrap();
            std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
            let path = format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap_or_default()
            );
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "run::tests::drive_behind_a_refusing_sudo",
                    "--exact",
                    "--ignored",
                    "--nocapture",
                ])
                .env("PATH", path)
                .env("RUSTIBLE_TEST_REFUSING_SUDO", mode)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{mode}:\n{stdout}\n{stderr}");
            assert!(stdout.contains("1 passed"), "{mode}: did not run\n{stdout}");
            if mode == "after-start" {
                // Reported as the host's error, next to sudo's own stderr.
                assert!(stdout.contains(line), "{mode}:\n{stdout}");
            }
        }
    }

    /// The half of the test above that runs behind the stand-in `sudo`.
    #[tokio::test]
    #[ignore = "run by drive_blames_a_refused_launch_on_the_playbooks_login"]
    async fn drive_behind_a_refusing_sudo() {
        let Ok(mode) = std::env::var("RUSTIBLE_TEST_REFUSING_SUDO") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let artifact = (b"never run".to_vec(), "abc".to_string());
        let probe = Probe {
            triple: "x86_64-unknown-linux-musl".into(),
            home: dir.path().display().to_string(),
            user: "cadu".into(),
        };
        let mut plan = plan(dir.path(), Some("minecraft"));
        plan.escalate = true;
        // Larger than a pipe holds, so the write of `Start` waits for a
        // reader and fails once `sudo` exits without reading it.
        let vars = match mode.as_str() {
            "before-start" => Value::String("x".repeat(1 << 20)),
            _ => Value::Null,
        };
        let out: Shared = Arc::new(Mutex::new(Output::Json {
            w: std::io::stdout(),
            failed: false,
        }));
        let r = by_defaults();
        let (_tx, rx) = tokio::sync::watch::channel(false);
        let result = drive(
            &plan,
            &r,
            &Transport::Local,
            &probe,
            &artifact,
            vars,
            &out,
            rx,
        )
        .await;
        match mode.as_str() {
            "before-start" => {
                let e = format!("{:#}", result.unwrap_err());
                assert!(
                    e.starts_with(
                        "escalating to `root` with `sudo -n` failed before the playbook \
                         started; the login user `minecraft`"
                    ),
                    "{e}"
                );
                assert!(e.contains("sudo: a password is required"), "{e}");
            }
            _ => result.unwrap(),
        }
    }

    // ---- the streamed launch (#62), behind a stand-in `sudo` ----

    /// Drops sudo's options, logs which script it was asked to run, and runs
    /// it as the test's own user with the `HOME` and `TMPDIR` the outer test
    /// chose.
    const STAND_IN_SUDO: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$RUSTIBLE_TEST_SUDO_LOG"
case "$RUSTIBLE_TEST_NO_READY $*" in
  1*"printf I"*) ( sleep 1; kill $$ ) & exec cat > "$RUSTIBLE_TEST_DIR/early" ;;
esac
while [ $# -gt 0 ]; do
  case $1 in -n|-H) shift ;; -u) shift 2 ;; *) break ;; esac
done
exec "$@"
"#;

    fn streamed_behind_stand_in_sudo(case: &str) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("sudo"), STAND_IN_SUDO).unwrap();
        std::fs::set_permissions(bin.join("sudo"), std::fs::Permissions::from_mode(0o755)).unwrap();
        for d in ["home", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "run::tests::drive_streams_behind_a_stand_in_sudo",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env("PATH", path)
            .env("HOME", dir.path().join("home"))
            .env("TMPDIR", dir.path().join("tmp"))
            .env("RUSTIBLE_TEST_SUDO_LOG", dir.path().join("sudo.log"))
            .env("RUSTIBLE_TEST_STREAMED", case)
            .env("RUSTIBLE_TEST_DIR", dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{case}:\n{stdout}\n{stderr}");
        assert!(stdout.contains("1 passed"), "{case}: did not run\n{stdout}");
    }

    #[test]
    fn an_unprivileged_escalate_user_runs_its_own_copy() {
        streamed_behind_stand_in_sudo("home");
    }

    #[test]
    fn an_escalate_user_without_a_home_runs_a_temp_copy() {
        streamed_behind_stand_in_sudo("no-home");
    }

    #[test]
    fn an_escalate_user_with_nowhere_to_run_is_refused() {
        streamed_behind_stand_in_sudo("nowhere");
    }

    #[test]
    fn no_byte_of_the_binary_is_streamed_before_the_install_answers() {
        streamed_behind_stand_in_sudo("no-ready");
    }

    /// The inner half of the three tests above: the real `drive`, through
    /// `Transport::Local`, with `escalate = true` and `escalate_user="svc"`.
    #[tokio::test]
    #[ignore = "run by streamed_behind_stand_in_sudo"]
    async fn drive_streams_behind_a_stand_in_sudo() {
        let Ok(case) = std::env::var("RUSTIBLE_TEST_STREAMED") else {
            return;
        };
        let dir = std::path::PathBuf::from(std::env::var("RUSTIBLE_TEST_DIR").unwrap());
        let (home, tmp) = (dir.join("home"), dir.join("tmp"));
        let start = dir.join("start");
        let hash = "0123456789abcdef".repeat(4);
        let name = "games/minecraft";
        let artifact = (stand_in(&start, name, true, "").into_bytes(), hash.clone());
        // The login user's home, which nothing may touch.
        let login_home = dir.join("login");
        let probe = Probe {
            triple: "x86_64-unknown-linux-musl".into(),
            home: login_home.display().to_string(),
            user: "cadu".into(),
        };
        let mut plan = plan(&dir, None);
        plan.escalate = true;
        let r = Inventory::parse(
            r#"host "svc-host" addr="10.0.4.5" escalate_user="svc""#,
            "hosts.kdl",
        )
        .unwrap()
        .resolve("svc-host")
        .unwrap();
        let run = || async {
            let (_tx, rx) = tokio::sync::watch::channel(false);
            drive(
                &plan,
                &r,
                &Transport::Local,
                &probe,
                &artifact,
                Value::Null,
                &quiet(&r.host),
                rx,
            )
            .await
        };
        let log = || std::fs::read_to_string(dir.join("sudo.log")).unwrap_or_default();
        let spawns = || {
            log()
                .lines()
                .map(|l| {
                    assert!(l.starts_with("-n -H -u svc /bin/sh -c "), "{l}");
                    if l.contains("printf I") {
                        "install"
                    } else {
                        "try"
                    }
                })
                .collect::<Vec<_>>()
        };
        let started = || {
            let frame = std::fs::read(&start).unwrap();
            let Down::Start { host, .. } = serde_json::from_slice(&frame).unwrap() else {
                panic!("not Start")
            };
            assert_eq!(host.name, "svc-host");
            std::fs::remove_file(&start).unwrap();
        };
        let cached = home
            .join(".cache/rustible/bin")
            .join(binary_name(name, &hash));
        match case.as_str() {
            "home" => {
                run().await.unwrap();
                started();
                assert_eq!(spawns(), ["try", "install", "try"]);
                assert_eq!(std::fs::read(&cached).unwrap(), artifact.0);
                // Nothing went to the login user's cache.
                assert!(!login_home.exists());
                std::fs::remove_file(dir.join("sudo.log")).unwrap();
                run().await.unwrap();
                started();
                assert_eq!(spawns(), ["try"]);
            }
            "no-home" => {
                std::fs::remove_dir(&home).unwrap();
                run().await.unwrap();
                started();
                assert_eq!(spawns(), ["try", "install", "install", "try"]);
                let dirs: Vec<_> = std::fs::read_dir(&tmp)
                    .unwrap()
                    .map(|e| e.unwrap().file_name())
                    .collect();
                assert_eq!(dirs.len(), 1, "{dirs:?}");
                assert!(
                    dirs[0].to_string_lossy().starts_with("rustible-"),
                    "{dirs:?}"
                );
            }
            "nowhere" => {
                std::fs::remove_dir(&home).unwrap();
                std::fs::remove_dir(&tmp).unwrap();
                let e = format!("{:#}", run().await.unwrap_err());
                assert!(
                    e.starts_with(&format!(
                        "escalating to `svc`: no usable home ({} does not exist) and cannot create {}/rustible-",
                        home.display(),
                        tmp.display()
                    )),
                    "{e}"
                );
                assert!(
                    e.ends_with("so svc cannot run its copy of the playbook binary"),
                    "{e}"
                );
                assert!(!start.exists());
            }
            "no-ready" => {
                // SAFETY: the inner test runs alone in its process.
                unsafe { std::env::set_var("RUSTIBLE_TEST_NO_READY", "1") };
                // The stand-in's install never answers `I`: it reads what it
                // is sent for a second, then dies.
                let e = format!("{:#}", run().await.unwrap_err());
                assert!(e.contains("before the playbook started"), "{e}");
                let early = std::fs::read(dir.join("early")).unwrap();
                assert!(early.is_empty(), "{} bytes sent before `I`", early.len());
            }
            other => panic!("unknown case {other}"),
        }
    }

    #[test]
    fn only_an_unprivileged_other_account_is_streamed() {
        assert!(streams_launch(true, Escalate::Sudo, "svc", "cadu"));
        assert!(streams_launch(true, Escalate::Doas, "svc", "cadu"));
        assert!(!streams_launch(true, Escalate::Sudo, "root", "cadu"));
        assert!(!streams_launch(true, Escalate::Sudo, "cadu", "cadu"));
        assert!(!streams_launch(true, Escalate::None, "svc", "cadu"));
        assert!(!streams_launch(false, Escalate::Sudo, "svc", "cadu"));
        // The probe found no name for the login.
        assert!(!streams_launch(true, Escalate::Sudo, "svc", ""));
    }

    #[test]
    fn host_info_carries_the_override() {
        let r = by_defaults();
        let login = login_override(&r, Some("minecraft"));
        let h = host_info(&r, login.clone());
        assert_eq!(h.login_override.map(|o| *o), login);
        assert_eq!(h.escalate_method, "sudo");
        assert!(host_info(&r, None).login_override.is_none());
    }

    /// The run reaches each host through `reach`, so this is the login ssh
    /// is handed.
    #[test]
    fn reach_hands_ssh_the_playbooks_login() {
        let inv = Inventory::parse(LOGINS, "hosts.kdl").unwrap();
        let Reach::Ssh(t) = reach(&inv.resolve("by-host").unwrap(), Some("minecraft")).unwrap()
        else {
            panic!("expected ssh")
        };
        assert_eq!(t.user.as_deref(), Some("minecraft"));
        let Reach::Ssh(t) = reach(&inv.resolve("by-host").unwrap(), None).unwrap() else {
            panic!("expected ssh")
        };
        assert_eq!(t.user.as_deref(), Some("admin"));
        assert_eq!(
            reach(&inv.resolve("laptop").unwrap(), None).unwrap(),
            Reach::Local
        );
    }

    #[test]
    fn the_run_refuses_a_playbook_login_on_a_local_host_after_limit() {
        let inv = Inventory::parse(
            r#"
group "games" {
    host "laptop" connection="local"
    host "box" addr="10.0.4.5"
}
"#,
            "hosts.kdl",
        )
        .unwrap();
        let d = describe(Some("minecraft"));
        let e = run_targets(&inv, &d, None).unwrap_err();
        assert!(e.downcast_ref::<Usage>().is_some());
        let e = e.to_string();
        assert!(
            e.contains("`laptop` (connection=\"local\", from host)"),
            "{e}"
        );
        assert!(
            e.ends_with("--limit the run to hosts reached over ssh"),
            "{e}"
        );
        let hosts = run_targets(&inv, &d, Some("box")).unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(run_targets(&inv, &describe(None), None).unwrap().len(), 2);
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
