//! The `rustible` command (vision doc section 3): `init`, `playbook
//! run|list|create`, `inventory show|check`. Noun first, then verb. The
//! orchestrator pipeline is `run.rs`; the inventory is the library half
//! (`rustible_cli::inventory`).
//!
//! Exit codes: 0 ok; 1 the inventory, the vars, or a build is wrong; 2 a
//! host failed; 3 the command line was wrong.

mod create;
mod describe;
mod init;
mod render;
mod run;
mod transport;
mod workspace;

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rustible_cli::inventory::{Inventory, Severity, VarError, format_vars_report, render_show};
use rustible_sdk::runtime::{self, HostVars};

use describe::Cargo;
use workspace::Workspace;

const EXIT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 3;

/// The command line was wrong: exit 3 rather than 1.
#[derive(Debug)]
pub struct Usage(pub String);

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

pub fn usage(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Usage(msg.into()))
}

#[derive(Parser, Debug)]
#[command(
    name = "rustible",
    about = "Configuration management as real code",
    version
)]
struct Cli {
    /// Workspace root (the directory holding rustible.toml). Default: walk
    /// up from the current directory.
    #[arg(long, global = true, value_name = "DIR")]
    workspace: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Create a Rustible workspace in a directory.
    Init(init::InitArgs),
    /// Run, list, or scaffold playbooks.
    Playbook {
        #[command(subcommand)]
        cmd: PlaybookCmd,
    },
    /// Inspect and validate the inventory.
    Inventory {
        #[command(subcommand)]
        cmd: InventoryCmd,
    },
}

#[derive(Subcommand, Debug)]
enum PlaybookCmd {
    /// Build a playbook for its hosts, ship it, run it, and render the
    /// result.
    Run(run::RunArgs),
    /// The playbooks under `playbooks/`, by name.
    List,
    /// Scaffold a playbook file.
    Create(create::CreateArgs),
}

#[derive(Subcommand, Debug)]
enum InventoryCmd {
    /// Resolved parameters and vars of one host, with the source of each.
    Show {
        /// Host name as written in the inventory.
        host: String,
        /// Inventory file (default: the workspace's).
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Load the inventory, then check every playbook's vars against every
    /// host it targets. Exit 1 on any error.
    Check {
        /// Inventory file (default: the workspace's).
        #[arg(long)]
        file: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let code = if e.use_stderr() { EXIT_USAGE } else { 0 };
            let _ = e.print();
            std::process::exit(code as i32);
        }
    };
    let code = match dispatch(cli).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            if e.downcast_ref::<Usage>().is_some() {
                EXIT_USAGE
            } else {
                EXIT_ERROR
            }
        }
    };
    std::process::exit(code as i32);
}

async fn dispatch(cli: Cli) -> Result<u8> {
    let ws = cli.workspace.as_deref();
    match cli.cmd {
        Cmd::Init(args) => init::run(args).map(|()| 0),
        Cmd::Playbook { cmd } => match cmd {
            PlaybookCmd::Run(args) => {
                let ws = Workspace::discover(ws).map_err(|e| usage(format!("{e:#}")))?;
                let inv = load_or_exit(&ws.inventory_path());
                run::run(&ws, &inv, args).await
            }
            PlaybookCmd::List => {
                let ws = Workspace::discover(ws).map_err(|e| usage(format!("{e:#}")))?;
                list(&ws)
            }
            PlaybookCmd::Create(args) => create::run(args).map(|()| 0),
        },
        Cmd::Inventory { cmd } => inventory(ws, cmd).await,
    }
}

/// `playbook list`: the same scan the build script runs (vision 9).
fn list(ws: &Workspace) -> Result<u8> {
    let dir = ws.playbooks_dir();
    if !dir.is_dir() {
        eprintln!("no playbooks/ directory in {}", ws.root.display());
        return Ok(0);
    }
    let found = rustible_build::scan(&dir).map_err(|e| anyhow::anyhow!("{e}"))?;
    if found.is_empty() {
        eprintln!(
            "no playbooks under {} (create one with `rustible playbook create playbooks/<name>.rs`)",
            dir.display()
        );
        return Ok(0);
    }
    let width = found.iter().map(|d| d.name.len()).max().unwrap_or(0);
    for d in &found {
        println!("{:<width$}  {}", d.name, ws.playbook_file(&d.name));
    }
    Ok(0)
}

/// `inventory show` and `inventory check`. Load errors go to stderr one
/// per line as `file:line:col: error: message`; any error exits 1.
async fn inventory(ws: Option<&Path>, cmd: InventoryCmd) -> Result<u8> {
    let (file, ws) = match &cmd {
        InventoryCmd::Show { file, .. } | InventoryCmd::Check { file } => {
            match (file, Workspace::discover(ws)) {
                (Some(f), ws) => (f.clone(), ws.ok()),
                (None, Ok(ws)) => (ws.inventory_path(), Some(ws)),
                (None, Err(e)) => return Err(usage(format!("{e:#} (or pass --file)"))),
            }
        }
    };
    match cmd {
        InventoryCmd::Show { host, .. } => {
            let inv = load_or_exit(&file);
            match inv.resolve(&host) {
                Ok(resolved) => print!("{}", render_show(&resolved)),
                Err(e) => {
                    eprintln!("error: {e}");
                    return Ok(EXIT_ERROR);
                }
            }
            Ok(0)
        }
        InventoryCmd::Check { .. } => {
            let inv = load_or_exit(&file);
            println!(
                "{}: ok ({} hosts, {} groups)",
                file.display(),
                inv.hosts.len(),
                inv.groups.len()
            );
            match ws {
                Some(ws) => check_playbooks(&ws, &inv, &file).await,
                None => {
                    eprintln!("not inside a rustible workspace: playbooks not checked");
                    Ok(0)
                }
            }
        }
    }
}

/// Vars of every playbook against every host it targets (vision 3, 10.3),
/// through one host-native build of the whole workspace and its
/// `--check-vars` mode.
async fn check_playbooks(ws: &Workspace, inv: &Inventory, file: &Path) -> Result<u8> {
    let cargo = Cargo::load(&ws.manifest()).await?;
    cargo
        .build(None, &[])
        .await
        .context("building the workspace")?;
    let bin = cargo.debug_bin();
    let playbooks = describe::describe_bin(&bin).await?;
    let inventory_file = ws.config.inventory.display().to_string();
    let mut errors = 0usize;
    for d in &playbooks {
        let src = ws.playbook_file(&d.name);
        let hosts = match inv.select(&d.hosts) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("{src}: targets `{}`: {e}", d.hosts);
                errors += 1;
                continue;
            }
        };
        let mut resolved = vec![];
        for h in &hosts {
            resolved.push(inv.resolve(&h.name).map_err(|e| anyhow::anyhow!("{e}"))?);
        }
        if d.vars_schema.is_null() {
            println!("{src}: ok ({} hosts, no vars)", hosts.len());
            continue;
        }
        let input: Vec<HostVars> = resolved
            .iter()
            .map(|r| HostVars {
                host: r.host.clone(),
                vars: rustible_cli::inventory::bag_to_json(&r.vars),
            })
            .collect();
        let checks = describe::check_vars(&bin, &d.name, &input).await?;
        let mut results = vec![];
        for c in checks {
            let mut errs = vec![];
            for p in c.problems {
                match p.severity {
                    runtime::Severity::Error => errs.push(VarError {
                        var: p.var,
                        severity: Severity::Error,
                        message: p.message,
                    }),
                    runtime::Severity::Warning => {
                        eprintln!("warning: {src} on host `{}`: {}", c.host, p.message);
                    }
                }
            }
            results.push((c.host, errs));
        }
        let is_group = inv.groups.contains_key(&d.hosts);
        match format_vars_report(&d.hosts, is_group, &src, &inventory_file, &results) {
            Some(report) => {
                eprint!("{report}");
                errors += 1;
            }
            None => println!("{src}: ok ({} hosts)", hosts.len()),
        }
    }
    if errors > 0 {
        eprintln!(
            "{}: {errors} playbook{} with vars errors",
            file.display(),
            if errors == 1 { "" } else { "s" }
        );
        return Ok(EXIT_ERROR);
    }
    Ok(0)
}

fn load_or_exit(file: &Path) -> Inventory {
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
            std::process::exit(EXIT_ERROR as i32);
        }
    }
}
