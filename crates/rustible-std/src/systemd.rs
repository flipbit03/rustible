//! systemd units: enablement, running state, restart and reload. Ansible's
//! `ansible.builtin.systemd` (and `ansible.builtin.service` on systemd hosts).
//!
//! One type per desired state (vision 6.3): [`Enabled`], [`Disabled`],
//! [`Running`], [`Stopped`], each returning a [`UnitState`]. Three actions
//! (vision 6.4): [`Restart`] and [`Reload`], which also return a
//! [`UnitState`], and [`DaemonReload`], which names no unit and so returns
//! nothing. All of them go through `systemctl` via `sys.cmd`; `check` only
//! runs the read-only probes `is-enabled` and `is-active`, and the actions
//! run nothing at all in `check` (with `.user(true)`, both also run `id -u`;
//! see below).
//!
//! Every op refuses a host whose init is not systemd and, unless `.user(true)`
//! selects a `systemctl --user` manager, refuses to run without root: reading
//! unit state works unprivileged, but the purpose of each op is the change,
//! and `systemctl enable` as a plain user only produces a polkit prompt the
//! binary cannot answer.
//!
//! ```no_run
//! # use rustible_sdk::prelude::*;
//! # use rustible_std::systemd;
//! # fn playbook(ctx: &mut Ctx) -> Result<()> {
//! let sshd = ctx.step("sshd enabled", systemd::Enabled::new("ssh").now(true))?;
//! if sshd.changed {
//!     ctx.step("sshd restarted", systemd::Restart::new("ssh").daemon_reload(true))?;
//! }
//! # Ok(()) }
//! ```
//!
//! After writing a *new* unit file, systemd has to re-read it before any of
//! these ops can find it. Use [`DaemonReload`] on its own, or
//! `.daemon_reload(true)` on the [`Restart`] or [`Reload`] that follows.
//!
//! # User units
//!
//! `.user(true)` manages the units of whichever account the step runs as:
//! the binary's own user, or another account through `ctx.as_user(..)`,
//! which is how a playbook logged in as one account manages a service
//! account's units:
//!
//! ```no_run
//! # use rustible_sdk::prelude::*;
//! # use rustible_std::systemd;
//! # fn playbook(ctx: &mut Ctx) -> Result<()> {
//! ctx.as_user("minecraft").step(
//!     "server enabled",
//!     systemd::Enabled::new("mine2026").user(true).now(true),
//! )?;
//! # Ok(()) }
//! ```
//!
//! The account needs a running user manager, which systemd keeps while the
//! account has a login session or linger (`loginctl enable-linger <name>`, as
//! root). `check` asks `id -u` for the account's uid and passes
//! `XDG_RUNTIME_DIR=/run/user/<uid>` to every `systemctl --user` and
//! `journalctl --user`, since `sudo` drops it on the way to another account.
//! The manager is up when its socket, `/run/user/<uid>/systemd/private`,
//! exists. `loginctl enable-linger` returns before logind has started the
//! manager, so when the socket is missing but the account has linger, a real
//! run polls for it every 100 ms for up to `.manager_timeout(..)`
//! ([`DEFAULT_MANAGER_TIMEOUT`], 30 s) and refuses, naming
//! `systemctl status user@<uid>.service` and, for a manager that stopped or
//! failed, `systemctl start user@<uid>.service`, if it does not appear. Without
//! linger nothing is starting a manager, so the step refuses at once and
//! says how to give the account one. Under `--check`, where an earlier step
//! may be the one enabling linger, nothing waits: the step reports `would
//! change` and the diff says it waits for that manager.

use std::time::{Duration, Instant};

use rustible_sdk::IoAt;
use rustible_sdk::prelude::*;
use rustible_sdk::system::{Cmd, Identity};

/// How long a real run waits, by default, for a user manager that linger is
/// still starting: see [`Enabled::manager_timeout`] and [the module
/// docs](self#user-units).
pub const DEFAULT_MANAGER_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the wait for a user manager looks again.
const MANAGER_POLL: Duration = Duration::from_millis(100);

/// The state of a unit as `systemctl` reports it. Output of every op here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitState {
    /// The unit name as given to the op (`nginx`, `getty@tty1.service`).
    pub unit: String,
    /// `systemctl is-enabled` counts as enabled (see [`EnabledState::is_enabled`]).
    pub enabled: bool,
    /// `systemctl is-active` counts as running (see [`ActiveState::is_running`]).
    pub active: bool,
}

/// What `systemctl is-enabled` printed, one variant per documented word.
/// `Other` carries anything a newer systemd may add.
#[derive(Debug, Clone, PartialEq, Eq)]
// Variants transcribe the words `systemctl is-enabled` prints; the meaning is
// in the type's own docs and in `as_str`. Individually documenting each would
// restate the name.
#[allow(missing_docs)]
pub enum EnabledState {
    Enabled,
    EnabledRuntime,
    Linked,
    LinkedRuntime,
    Alias,
    Masked,
    MaskedRuntime,
    Static,
    Indirect,
    Disabled,
    Generated,
    Transient,
    /// `is-enabled` printed `not-found` (systemd 253+) or nothing with a
    /// non-zero exit (older systemd writes the error to stderr).
    NotFound,
    Other(String),
}

impl EnabledState {
    /// True for the states `systemctl is-enabled` itself exits 0 on: the unit
    /// starts at boot, or has no installation config and needs none
    /// (`static`, `alias`, `indirect`, `generated`, `transient`).
    pub fn is_enabled(&self) -> bool {
        matches!(
            self,
            EnabledState::Enabled
                | EnabledState::EnabledRuntime
                | EnabledState::Alias
                | EnabledState::Static
                | EnabledState::Indirect
                | EnabledState::Generated
                | EnabledState::Transient
        )
    }

    /// True for units that have no `[Install]` section to switch: `systemctl
    /// disable` exits 0 on them, changes nothing, and `is-enabled` keeps
    /// answering the same word, so a `Disabled` op would report `changed`
    /// forever.
    pub fn cannot_be_disabled(&self) -> bool {
        matches!(
            self,
            EnabledState::Static | EnabledState::Generated | EnabledState::Transient
        )
    }

    /// True for `masked` and `masked-runtime`: the unit is symlinked to
    /// `/dev/null` and can be neither started nor enabled until `systemctl
    /// unmask` undoes it. [`Enabled`] and [`Running`] refuse such a unit
    /// instead of unmasking it on the caller's behalf.
    pub fn is_masked(&self) -> bool {
        matches!(self, EnabledState::Masked | EnabledState::MaskedRuntime)
    }

    /// The word `systemctl` printed, for diffs and messages.
    pub fn as_str(&self) -> &str {
        match self {
            EnabledState::Enabled => "enabled",
            EnabledState::EnabledRuntime => "enabled-runtime",
            EnabledState::Linked => "linked",
            EnabledState::LinkedRuntime => "linked-runtime",
            EnabledState::Alias => "alias",
            EnabledState::Masked => "masked",
            EnabledState::MaskedRuntime => "masked-runtime",
            EnabledState::Static => "static",
            EnabledState::Indirect => "indirect",
            EnabledState::Disabled => "disabled",
            EnabledState::Generated => "generated",
            EnabledState::Transient => "transient",
            EnabledState::NotFound => "not-found",
            EnabledState::Other(s) => s,
        }
    }
}

/// What `systemctl is-active` printed.
/// [`ActiveState::is_running`] sorts those words into up and not up; `Other`
/// carries one this build does not know.
#[derive(Debug, Clone, PartialEq, Eq)]
// Variants transcribe the words `systemctl is-active` prints; the meaning is
// in the type's own docs and in `as_str`. Individually documenting each would
// restate the name.
#[allow(missing_docs)]
pub enum ActiveState {
    Active,
    Reloading,
    /// systemd 257+: a reload that re-executes the service.
    Refreshing,
    Activating,
    Inactive,
    Failed,
    Deactivating,
    Maintenance,
    Other(String),
}

impl ActiveState {
    /// True while the unit is up or on its way up (`active`, `reloading`,
    /// `refreshing`, `activating`). `failed`, `inactive`, `deactivating` and
    /// `maintenance` are not running.
    pub fn is_running(&self) -> bool {
        matches!(
            self,
            ActiveState::Active
                | ActiveState::Reloading
                | ActiveState::Refreshing
                | ActiveState::Activating
        )
    }

    /// The word `systemctl` printed, for diffs and messages.
    pub fn as_str(&self) -> &str {
        match self {
            ActiveState::Active => "active",
            ActiveState::Reloading => "reloading",
            ActiveState::Refreshing => "refreshing",
            ActiveState::Activating => "activating",
            ActiveState::Inactive => "inactive",
            ActiveState::Failed => "failed",
            ActiveState::Deactivating => "deactivating",
            ActiveState::Maintenance => "maintenance",
            ActiveState::Other(s) => s,
        }
    }
}

/// Parse `systemctl is-enabled <unit>`. The exit code is meaningful but not
/// the message: `disabled` and `masked` exit 1 with the word on stdout, and a
/// missing unit exits 1 (older) or 4 (systemd 253+) with `not-found` or
/// nothing on stdout.
///
/// Nothing on stdout with a non-zero exit is `not-found` only when stderr
/// says so (older systemd writes `No such file or directory` there) or says
/// nothing. Any other stderr — `Failed to connect to bus`, a permission
/// error — is `systemctl` failing to answer at all, which is an error and
/// not a state, so it is refused in check mode as in a real run rather than
/// read as a unit an earlier step would install.
pub fn parse_is_enabled(stdout: &str, exit: i32, stderr: &str) -> Result<EnabledState> {
    let word = stdout.lines().next().unwrap_or("").trim();
    if word.is_empty() && exit != 0 {
        let e = stderr.trim();
        let lower = e.to_ascii_lowercase();
        // A bus failure ends in the same `No such file or directory` a
        // missing unit does (`Failed to connect to bus: No such file or
        // directory` when the user manager is not running), so it is ruled
        // out first.
        let could_not_answer = lower.contains("failed to connect to");
        let says_not_found = !could_not_answer
            && (e.is_empty()
                || lower.contains("no such file or directory")
                || lower.contains("not found")
                || lower.contains("not-found")
                || lower.contains("could not be found"));
        if !says_not_found {
            bail!("`systemctl is-enabled` failed without an answer (exit {exit}): {e}");
        }
        return Ok(EnabledState::NotFound);
    }
    Ok(match word {
        "enabled" => EnabledState::Enabled,
        "enabled-runtime" => EnabledState::EnabledRuntime,
        "linked" => EnabledState::Linked,
        "linked-runtime" => EnabledState::LinkedRuntime,
        "alias" => EnabledState::Alias,
        "masked" => EnabledState::Masked,
        "masked-runtime" => EnabledState::MaskedRuntime,
        "static" => EnabledState::Static,
        "indirect" => EnabledState::Indirect,
        "disabled" => EnabledState::Disabled,
        "generated" => EnabledState::Generated,
        "transient" => EnabledState::Transient,
        "not-found" => EnabledState::NotFound,
        other => EnabledState::Other(other.to_string()),
    })
}

/// Parse `systemctl is-active <unit>`. `inactive` and `failed` exit 3 (4 on
/// systemd 255+ for an unknown unit) with the word on stdout; the word wins
/// over the exit code, and an empty stdout with a non-zero exit is reported
/// as `inactive`, which is what systemd means by it.
pub fn parse_is_active(stdout: &str, exit: i32) -> ActiveState {
    let word = stdout.lines().next().unwrap_or("").trim();
    match word {
        "active" => ActiveState::Active,
        "reloading" => ActiveState::Reloading,
        "refreshing" => ActiveState::Refreshing,
        "activating" => ActiveState::Activating,
        "inactive" => ActiveState::Inactive,
        "failed" => ActiveState::Failed,
        "deactivating" => ActiveState::Deactivating,
        "maintenance" => ActiveState::Maintenance,
        "" if exit != 0 => ActiveState::Inactive,
        other => ActiveState::Other(other.to_string()),
    }
}

/// Reject names `systemctl` would misread: empty, whitespace, or a leading
/// dash that would turn the unit into a flag.
pub fn validate_unit(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "systemd: unit name is empty");
    ensure!(
        !name.chars().any(char::is_whitespace),
        "systemd: unit name {name:?} contains whitespace"
    );
    ensure!(
        !name.starts_with('-'),
        "systemd: unit name {name:?} starts with `-`"
    );
    Ok(())
}

/// The unit an op targets plus the manager to talk to (system or `--user`).
/// Shared by all six ops.
#[derive(Debug, Clone)]
struct Unit {
    name: String,
    user: bool,
    /// How long `check` waits for a user manager that linger is starting.
    manager_timeout: Duration,
    /// The user manager this step talks to, found by [`Unit::guard`]. `None`
    /// for the system manager, and on the op's own copy: `check` binds a
    /// clone, which is what the probes and the intent use.
    runtime: Option<Runtime>,
}

/// Where `systemctl --user` finds the manager of the account a step runs as.
///
/// The commands run as that account, but with whatever environment reached
/// them, and `sudo` (how [`System::as_user`] gets there) drops
/// `XDG_RUNTIME_DIR`, without which `systemctl --user` cannot find the bus.
/// So every `--user` command gets `XDG_RUNTIME_DIR` set to `/run/user/<uid>`
/// for the uid `id -u` answers as that account, which is Ansible's fix
/// (`systemd_service.py`, `/run/user/%s % os.geteuid()`).
#[derive(Debug, Clone)]
struct Runtime {
    /// The account the step runs as, for messages: the `as_user` target, or
    /// the binary's own user.
    account: String,
    /// `/run/user/<uid>`.
    dir: String,
    /// `/run/user/<uid>/systemd/private`: the manager's own socket, which
    /// `systemctl --user` connects to. Its existence is what "the manager is
    /// up" means here; `dir` appears before it, and `bus` belongs to the
    /// session bus, which a host without `dbus-user-session` never has.
    socket: String,
    /// Whether `socket` exists. False only under `--check` (a real run's
    /// `check` waits or refuses), where an earlier step may enable linger:
    /// the probes are then skipped and the diff says what the step waits for.
    ready: bool,
}

impl Runtime {
    /// Find the user manager of the account `sys` runs as. In a real run,
    /// wait up to `timeout` for one that linger is still starting, and
    /// refuse when there is none.
    fn find(sys: &System, op: &str, timeout: Duration) -> Result<Runtime> {
        let account = match sys.identity() {
            Identity::User(u) => u.clone(),
            Identity::Own => sys.facts().user.clone(),
        };
        let out = sys
            .cmd("id")
            .arg("-u")
            .run()
            .with_context(|| format!("systemd::{op}: finding the uid of `{account}`"))?;
        let uid = out.stdout_str();
        let Ok(uid) = uid.trim().parse::<u32>() else {
            bail!("systemd::{op}: `id -u` as `{account}` printed {uid:?}, not a uid");
        };
        let dir = format!("/run/user/{uid}");
        let socket = format!("{dir}/systemd/private");
        let ready = socket_up(sys, &socket)?
            || (!sys.check_mode() && wait(sys, &account, uid, &socket, op, timeout)?);
        if !ready && !sys.check_mode() {
            if sys.is_root() {
                bail!(
                    "systemd::{op}: no user manager for `root`: {socket} does not exist. \
                     `.user(true)` as root targets root's own user manager; to manage another \
                     account's user units, step into it with `ctx.as_user(\"<account>\")`"
                );
            }
            if sys.exists(&dir)? {
                bail!(
                    "systemd::{op}: the user manager of `{account}` is not running: {dir} exists \
                     but {socket} does not, and `{account}` has no linger to start it. logind does \
                     not restart a user@{uid}.service that stopped or failed: check `systemctl \
                     status user@{uid}.service`, then start it as root (`systemctl start \
                     user@{uid}.service`), or give `{account}` linger (`loginctl enable-linger \
                     {account}`) to keep one running"
                );
            }
            bail!(
                "systemd::{op}: no user manager for `{account}`: {dir} does not exist, so \
                 `{account}` has neither a login session nor linger. Enable linger as root \
                 (`loginctl enable-linger {account}`) or log in as `{account}` first"
            );
        }
        Ok(Runtime {
            account,
            dir,
            socket,
            ready,
        })
    }
}

/// In a real run, wait for the manager of an account with linger:
/// `loginctl enable-linger` returns before logind has started it, so a step
/// right after it would otherwise find no manager. Polls `socket` every
/// [`MANAGER_POLL`] for `timeout`. False without linger (nothing is starting
/// a manager, so there is nothing to wait for); a refusal when the manager
/// does not come up in time.
fn wait(
    sys: &System,
    account: &str,
    uid: u32,
    socket: &str,
    op: &str,
    timeout: Duration,
) -> Result<bool> {
    let linger = format!("/var/lib/systemd/linger/{account}");
    if !sys.exists(&linger)? {
        return Ok(false);
    }
    // A timeout past what the clock can represent (`Duration::MAX`) has no
    // deadline: wait until the socket appears.
    let deadline = Instant::now().checked_add(timeout);
    sys.debug(match deadline {
        Some(_) => format!(
            "waiting up to {} for the user manager of `{account}` ({socket})",
            human(timeout)
        ),
        None => format!("waiting with no deadline for the user manager of `{account}` ({socket})"),
    });
    loop {
        let left = deadline.map_or(MANAGER_POLL, |d| {
            d.saturating_duration_since(Instant::now())
        });
        if left.is_zero() {
            bail!(
                "systemd::{op}: the user manager of `{account}` (uid {uid}) did not come up: \
                 {socket} was still missing after waiting {} although linger is enabled \
                 ({linger}). logind starts user@{uid}.service for linger but does not restart \
                 it once it has stopped or failed: check `systemctl status user@{uid}.service`, \
                 then start it as root with `systemctl start user@{uid}.service`",
                human(timeout)
            );
        }
        std::thread::sleep(MANAGER_POLL.min(left));
        if socket_up(sys, socket)? {
            return Ok(true);
        }
    }
}

/// Whether the manager's socket exists yet. A stat refused with `EACCES`
/// counts as "not yet": `user-runtime-dir@.service` creates `/run/user/<uid>`
/// as root, mode 0700, before it mounts the account's tmpfs there, and a
/// stat made as the account in between is denied rather than answered.
fn socket_up(sys: &System, socket: &str) -> Result<bool> {
    match sys.exists(socket) {
        Err(e) if denied(&e) => Ok(false),
        r => r,
    }
}

/// Whether `e` is a path refused with `EACCES`, which [`socket_up`] reads
/// as a runtime directory not yet handed to its account.
fn denied(e: &Error) -> bool {
    e.downcast_ref::<IoAt>()
        .is_some_and(|io| io.source.kind() == std::io::ErrorKind::PermissionDenied)
}

/// A timeout as a reader wrote it: `30s`, `250ms`, and anything finer as
/// `Duration`'s own `500µs` or `1.5ms`.
fn human(d: Duration) -> String {
    if d.subsec_nanos() == 0 {
        format!("{}s", d.as_secs())
    } else if d.subsec_nanos().is_multiple_of(1_000_000) {
        format!("{}ms", d.as_millis())
    } else {
        format!("{d:?}")
    }
}

/// One round of the read-only probes.
struct Probe {
    enabled: EnabledState,
    active: ActiveState,
}

impl Unit {
    fn new(name: impl Into<String>) -> Self {
        Unit {
            name: name.into(),
            user: false,
            manager_timeout: DEFAULT_MANAGER_TIMEOUT,
            runtime: None,
        }
    }

    fn systemctl(&self, sys: &System) -> Cmd {
        self.scoped(sys.cmd("systemctl"))
    }

    /// `cmd` aimed at this unit's manager: unchanged for the system
    /// manager; `--user`, with the user manager's `XDG_RUNTIME_DIR`, for a
    /// user one.
    fn scoped(&self, cmd: Cmd) -> Cmd {
        if !self.user {
            return cmd;
        }
        let cmd = cmd.arg("--user");
        match &self.runtime {
            Some(r) => cmd.env("XDG_RUNTIME_DIR", &r.dir),
            None => cmd,
        }
    }

    /// Whose manager answered, for messages about the unit: empty for the
    /// system manager, and for a user one a phrase naming the account, so an
    /// operator under `as_user` does not go and look in their own.
    fn whose(&self) -> String {
        self.runtime
            .as_ref()
            .map(|r| format!(" in the user manager of `{}`", r.account))
            .unwrap_or_default()
    }

    /// Under `--check` with no user manager yet, what the step waits for.
    fn waits_for(&self) -> Option<String> {
        self.runtime.as_ref().filter(|r| !r.ready).map(|r| {
            format!(
                "waits for the user manager of `{}` ({} does not exist yet)",
                r.account, r.socket
            )
        })
    }

    /// An intent's attribute diff, followed by what it waits for, if
    /// anything.
    fn noted(&self, diff: Diff) -> Diff {
        match self.waits_for() {
            Some(w) => Diff::many([diff, Diff::summary(w)]).expect("two parts"),
            None => diff,
        }
    }

    /// An intent's summary line, followed by what it waits for, if anything,
    /// on the same line: the step line shows it, as it does for [`noted`]'s
    /// attribute diffs (`Diff::short` keeps only a summary's first line).
    ///
    /// [`noted`]: Self::noted
    fn noted_summary(&self, line: String) -> Diff {
        Diff::summary(match self.waits_for() {
            Some(w) => format!("{line}; {w}"),
            None => line,
        })
    }

    /// `systemctl` or `systemctl --user`, for diff summaries and messages.
    fn prefix(&self) -> &'static str {
        if self.user {
            "systemctl --user"
        } else {
            "systemctl"
        }
    }

    /// A manager with no unit. [`DaemonReload`] talks to systemd itself, so
    /// it carries the `--user` choice and an empty name; only `systemctl`,
    /// `prefix` and `guard_manager` are meaningful on such a `Unit`.
    fn manager() -> Self {
        Unit {
            name: String::new(),
            user: false,
            manager_timeout: DEFAULT_MANAGER_TIMEOUT,
            runtime: None,
        }
    }

    /// The preconditions every op shares: a sane name, systemd as init, and
    /// root unless a user manager is the target. Returns the unit bound to
    /// its manager, which is what the probes and the intent use.
    fn guard(&self, sys: &System, op: &str) -> Result<Unit> {
        validate_unit(&self.name)?;
        self.guard_manager(sys, op)
    }

    /// The half of [`Unit::guard`] that is about the host and the manager
    /// rather than the unit: systemd as init, root unless `--user`, and for
    /// `--user` the [`Runtime`] of the account the step runs as.
    fn guard_manager(&self, sys: &System, op: &str) -> Result<Unit> {
        // Explicit, though `Init::Systemd` already implies Linux: this op
        // reads `/proc/1/comm`'s answer and drives `systemctl`, and a reader
        // of the refusal should not have to know that the init check covers
        // the kernel too.
        match sys.facts().os {
            Os::Linux => {}
            ref other => bail!(
                "systemd::{op} manages systemd units and runs on Linux only; this host is {}",
                other.name()
            ),
        }
        if sys.facts().init != Init::Systemd {
            bail!(
                "systemd::{op} needs systemd, but this host's init is {}",
                match &sys.facts().init {
                    Init::OpenRc => "OpenRC".to_string(),
                    Init::Launchd => "launchd".to_string(),
                    Init::Other(name) => format!("`{name}`"),
                    Init::Systemd => unreachable!(),
                }
            );
        }
        if !self.user && !sys.is_root() {
            bail!(
                "systemd::{op} needs root to manage system units (this binary runs as `{}`); \
                 use `.user(true)` for the `systemctl --user` units of the account the step \
                 runs as",
                sys.facts().user
            );
        }
        let mut bound = self.clone();
        if self.user {
            bound.runtime = Some(Runtime::find(sys, op, self.manager_timeout)?);
        }
        Ok(bound)
    }

    fn probe(&self, sys: &System) -> Result<Probe> {
        // No user manager to ask, which only `--check` lets through: report
        // the unit as one an earlier step has yet to make reachable, the
        // shape `probe_existing` already tolerates under `--check`.
        if self.waits_for().is_some() {
            return Ok(Probe {
                enabled: EnabledState::NotFound,
                active: ActiveState::Inactive,
            });
        }
        let out = self
            .systemctl(sys)
            .args(["is-enabled", &self.name])
            .allow_failure()
            .run()?;
        let enabled = parse_is_enabled(&out.stdout_str(), out.status, &out.stderr_str())
            .with_context(|| format!("probing unit `{}` with `{}`", self.name, self.prefix()))?;
        let out = self
            .systemctl(sys)
            .args(["is-active", &self.name])
            .allow_failure()
            .run()?;
        let active = parse_is_active(&out.stdout_str(), out.status);
        Ok(Probe { enabled, active })
    }

    /// `probe` plus the not-found check every state op wants first.
    ///
    /// Under `--check` a unit `systemctl` cannot find is not refused: an
    /// earlier step in the run may install the package or copy the unit
    /// file, and a dry run verifies such a prerequisite only when it is
    /// about to act (vision 12). The op then reports the state it would set,
    /// from `not-found`. A real run refuses, because it runs `check` with
    /// check mode off.
    fn probe_existing(&self, sys: &System, op: &str) -> Result<Probe> {
        let p = self.probe(sys)?;
        if p.enabled == EnabledState::NotFound && !sys.check_mode() {
            bail!(
                "systemd::{op}: unit `{}` not found by `{} is-enabled`{}",
                self.name,
                self.prefix(),
                self.whose()
            );
        }
        Ok(p)
    }

    fn state(&self, p: &Probe) -> UnitState {
        UnitState {
            unit: self.name.clone(),
            enabled: p.enabled.is_enabled(),
            active: p.active.is_running(),
        }
    }

    /// The last 20 journal lines of the unit, for failure messages. Never
    /// fails: a missing journal is reported as such.
    fn journal_tail(&self, sys: &System) -> String {
        match self
            .scoped(sys.cmd("journalctl"))
            .args(["-u", &self.name, "--no-pager", "-n", "20"])
            .allow_failure()
            .run()
        {
            Ok(out) if out.success() && !out.stdout.is_empty() => out.stdout_str(),
            Ok(out) => format!(
                "(journalctl exited {}: {})",
                out.status,
                out.stderr_str().trim()
            ),
            Err(e) => format!("(journalctl unavailable: {e})"),
        }
    }

    /// Re-read after a command that should have left the unit running, and
    /// fail with the journal tail if it did not come up.
    fn verify_running(&self, sys: &System, after: &str) -> Result<UnitState> {
        let p = self.probe(sys)?;
        if !p.active.is_running() {
            bail!(
                "unit `{}` is {} after `{after}`; last journal lines:\n{}",
                self.name,
                p.active.as_str(),
                self.journal_tail(sys).trim_end()
            );
        }
        Ok(self.state(&p))
    }

    /// Re-read after a command that should have left the unit stopped.
    fn verify_stopped(&self, sys: &System, after: &str) -> Result<UnitState> {
        let p = self.probe(sys)?;
        if p.active.is_running() {
            bail!(
                "unit `{}` is still {} after `{after}`; last journal lines:\n{}",
                self.name,
                p.active.as_str(),
                self.journal_tail(sys).trim_end()
            );
        }
        Ok(self.state(&p))
    }
}

fn attr(name: &str, from: &str, to: &str) -> AttrChange {
    AttrChange::new(name, from, to)
}

/// What [`Enabled`]'s `check` decided: `systemctl enable`, with `--now`
/// when the op asks to start the unit too, from the states the probes read.
#[derive(Debug)]
pub struct Enable {
    unit: Unit,
    now: bool,
    /// What `is-enabled` answered, when the unit is not enabled yet.
    enabled: Option<EnabledState>,
    /// What `is-active` answered, when `--now` has a unit to start.
    active: Option<ActiveState>,
}

impl Intent for Enable {
    fn diff(&self) -> Diff {
        let mut changes = vec![];
        if let Some(from) = &self.enabled {
            changes.push(attr("enabled", from.as_str(), "enabled"));
        }
        if let Some(from) = &self.active {
            changes.push(attr("active", from.as_str(), "active"));
        }
        self.unit
            .noted(Diff::attrs(self.unit.name.clone(), changes))
    }
}

/// What [`Disabled`]'s `check` decided: `systemctl disable`, with `--now`
/// when the op asks to stop the unit too, from the states the probes read.
#[derive(Debug)]
pub struct Disable {
    unit: Unit,
    now: bool,
    /// What `is-enabled` answered, when the unit is enabled (or, under
    /// `--check`, not installed yet).
    enabled: Option<EnabledState>,
    /// What `is-active` answered, when `--now` has a running unit to stop.
    active: Option<ActiveState>,
}

impl Intent for Disable {
    fn diff(&self) -> Diff {
        let mut changes = vec![];
        if let Some(from) = &self.enabled {
            changes.push(attr("enabled", from.as_str(), "disabled"));
        }
        if let Some(from) = &self.active {
            changes.push(attr("active", from.as_str(), "inactive"));
        }
        self.unit
            .noted(Diff::attrs(self.unit.name.clone(), changes))
    }
}

/// What [`Running`]'s `check` decided: `systemctl start`, from the state
/// `is-active` answered.
#[derive(Debug)]
pub struct Start {
    unit: Unit,
    from: ActiveState,
}

impl Intent for Start {
    fn diff(&self) -> Diff {
        self.unit.noted(Diff::attrs(
            self.unit.name.clone(),
            vec![attr("active", self.from.as_str(), "active")],
        ))
    }
}

/// What [`Stopped`]'s `check` decided: `systemctl stop`, from the state
/// `is-active` answered; `None` when the unit is not installed yet, which
/// only a dry run reports.
#[derive(Debug)]
pub struct Stop {
    unit: Unit,
    from: Option<ActiveState>,
}

impl Intent for Stop {
    fn diff(&self) -> Diff {
        let from = self.from.as_ref().map_or("not-found", ActiveState::as_str);
        self.unit.noted(Diff::attrs(
            self.unit.name.clone(),
            vec![attr("active", from, "inactive")],
        ))
    }
}

/// What [`Restart`] and [`Reload`] run: an optional `daemon-reload`, then
/// the verb on the unit. The report is the command line.
#[derive(Debug)]
pub struct Bounce {
    unit: Unit,
    daemon_reload: bool,
    verb: &'static str,
}

impl Intent for Bounce {
    fn diff(&self) -> Diff {
        let p = self.unit.prefix();
        self.unit.noted_summary(if self.daemon_reload {
            format!("{p} daemon-reload && {p} {} {}", self.verb, self.unit.name)
        } else {
            format!("{p} {} {}", self.verb, self.unit.name)
        })
    }
}

/// What [`DaemonReload`] runs: `daemon-reload` on this manager.
#[derive(Debug)]
pub struct ReloadUnits {
    manager: Unit,
}

impl Intent for ReloadUnits {
    fn diff(&self) -> Diff {
        self.manager
            .noted_summary(format!("{} daemon-reload", self.manager.prefix()))
    }
}

// ---------------------------------------------------------------------------
// Enabled / Disabled
// ---------------------------------------------------------------------------

/// Ensure a unit starts at boot. `ansible.builtin.systemd` with
/// `enabled: true`; `.now(true)` is Ansible's `enabled: true` plus
/// `state: started` in one step (`systemctl enable --now`).
///
/// Satisfied when `systemctl is-enabled` answers `enabled`, `enabled-runtime`,
/// `static`, `alias`, `indirect`, `generated` or `transient` (the unit starts
/// at boot or needs no enabling). Refuses a `masked` unit rather than
/// unmasking it (vision 6.7). A unit `systemctl` does not know is refused in
/// a real run and reported `would change` under `--check`, where an earlier
/// step may install it (vision 12).
#[derive(Debug, Clone)]
pub struct Enabled {
    unit: Unit,
    now: bool,
}

impl Enabled {
    /// Enable `unit`, given either as a bare name (`nginx`) or in full
    /// (`getty@tty1.service`). Targets the system manager and only enables;
    /// [`Enabled::now`] starts the unit too, [`Enabled::user`] switches
    /// managers. The name is validated in `check`, not here.
    pub fn new(unit: impl Into<String>) -> Self {
        Enabled {
            unit: Unit::new(unit),
            now: false,
        }
    }

    /// Also start the unit (`systemctl enable --now`); the step then verifies
    /// `is-active` like [`Running`] does.
    pub fn now(mut self, on: bool) -> Self {
        self.now = on;
        self
    }

    /// Manage the user units (`systemctl --user`) of the account the step
    /// runs as: the binary's own user, or the target of `ctx.as_user(..)`.
    /// Needs no root, but needs that account's user manager, which runs
    /// while it has a login session or linger. A real run waits for a
    /// manager that linger is still starting ([`Self::manager_timeout`]) and
    /// refuses when there is none. See [the module docs](self#user-units).
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }

    /// With `.user(true)`, how long a real run waits for the account's user
    /// manager when linger is enabled but logind has not finished starting
    /// it, as right after `loginctl enable-linger`. Default
    /// [`DEFAULT_MANAGER_TIMEOUT`]; `Duration::ZERO` does not wait and
    /// refuses at once; `Duration::MAX`, or any timeout too long for the
    /// clock to hold, has no deadline and waits until the manager is up.
    /// Ignored otherwise: without `.user(true)`, when the manager is up,
    /// without linger (the step refuses), and under `--check`, which never
    /// waits. See [the module docs](self#user-units).
    pub fn manager_timeout(mut self, timeout: Duration) -> Self {
        self.unit.manager_timeout = timeout;
        self
    }
}

impl Op for Enabled {
    type Output = UnitState;
    type Intent = Enable;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        let unit = self.unit.guard(sys, "Enabled")?;
        let p = unit.probe_existing(sys, "Enabled")?;
        if p.enabled.is_masked() {
            bail!(
                "systemd::Enabled: unit `{}` is {}{}; unmask it first (`{} unmask {}`)",
                unit.name,
                p.enabled.as_str(),
                unit.whose(),
                unit.prefix(),
                unit.name
            );
        }
        let enabled = (!p.enabled.is_enabled()).then(|| p.enabled.clone());
        let active = (self.now && !p.active.is_running()).then(|| p.active.clone());
        if enabled.is_none() && active.is_none() {
            return Ok(Plan::Satisfied(UnitState {
                unit: unit.name,
                enabled: true,
                active: self.now || p.active.is_running(),
            }));
        }
        Ok(Plan::Change(Enable {
            unit,
            now: self.now,
            enabled,
            active,
        }))
    }

    fn apply(&self, sys: &System, intent: Enable) -> Result<UnitState> {
        let Enable { unit, now, .. } = intent;
        let mut cmd = unit.systemctl(sys).arg("enable");
        if now {
            cmd = cmd.arg("--now");
        }
        cmd.arg(&unit.name).run()?;
        let after = format!(
            "{} enable{} {}",
            unit.prefix(),
            if now { " --now" } else { "" },
            unit.name
        );
        // Read back, not predicted: the unit's state is the tool's answer.
        let state = if now {
            unit.verify_running(sys, &after)?
        } else {
            unit.state(&unit.probe(sys)?)
        };
        ensure!(
            state.enabled,
            "unit `{}` is still not enabled after `{after}`",
            unit.name
        );
        Ok(state)
    }
}

/// Ensure a unit does not start at boot. `ansible.builtin.systemd` with
/// `enabled: false`; `.now(true)` also stops it (`systemctl disable --now`).
///
/// Satisfied when `is-enabled` answers `disabled`, `masked` or `linked`.
/// Refuses `static`, `generated` and `transient` units with a message, because
/// they have no `[Install]` section to switch and `systemctl disable` would
/// silently change nothing. A unit `systemctl` does not know is refused in a
/// real run and reported `would change` under `--check` (vision 12).
#[derive(Debug, Clone)]
pub struct Disabled {
    unit: Unit,
    now: bool,
}

impl Disabled {
    /// Disable `unit` on the system manager. A unit that is running keeps
    /// running: it just no longer starts at the next boot. [`Disabled::now`]
    /// stops it as well.
    pub fn new(unit: impl Into<String>) -> Self {
        Disabled {
            unit: Unit::new(unit),
            now: false,
        }
    }

    /// Also stop the unit (`systemctl disable --now`).
    pub fn now(mut self, on: bool) -> Self {
        self.now = on;
        self
    }

    /// Manage the user units (`systemctl --user`) of the account the step
    /// runs as: the binary's own user, or the target of `ctx.as_user(..)`.
    /// Needs no root, but needs that account's user manager, which runs
    /// while it has a login session or linger. A real run waits for a
    /// manager that linger is still starting ([`Self::manager_timeout`]) and
    /// refuses when there is none. See [the module docs](self#user-units).
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }

    /// With `.user(true)`, how long a real run waits for the account's user
    /// manager when linger is enabled but logind has not finished starting
    /// it, as right after `loginctl enable-linger`. Default
    /// [`DEFAULT_MANAGER_TIMEOUT`]; `Duration::ZERO` does not wait and
    /// refuses at once; `Duration::MAX`, or any timeout too long for the
    /// clock to hold, has no deadline and waits until the manager is up.
    /// Ignored otherwise: without `.user(true)`, when the manager is up,
    /// without linger (the step refuses), and under `--check`, which never
    /// waits. See [the module docs](self#user-units).
    pub fn manager_timeout(mut self, timeout: Duration) -> Self {
        self.unit.manager_timeout = timeout;
        self
    }
}

impl Op for Disabled {
    type Output = UnitState;
    type Intent = Disable;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        let unit = self.unit.guard(sys, "Disabled")?;
        let p = unit.probe_existing(sys, "Disabled")?;
        if p.enabled.cannot_be_disabled() {
            bail!(
                "systemd::Disabled: unit `{}` is {}{}: it has no [Install] section, so it cannot \
                 be disabled; mask it (`{} mask {}`) or stop it instead",
                unit.name,
                p.enabled.as_str(),
                unit.whose(),
                unit.prefix(),
                unit.name
            );
        }
        // `not-found` only reaches here under --check (`probe_existing`): the
        // unit an earlier step would install is reported as due, not as
        // already disabled (vision 12).
        let enabled = (p.enabled.is_enabled() || p.enabled == EnabledState::NotFound)
            .then(|| p.enabled.clone());
        let active = (self.now && p.active.is_running()).then(|| p.active.clone());
        if enabled.is_none() && active.is_none() {
            return Ok(Plan::Satisfied(UnitState {
                unit: unit.name,
                enabled: false,
                active: !self.now && p.active.is_running(),
            }));
        }
        Ok(Plan::Change(Disable {
            unit,
            now: self.now,
            enabled,
            active,
        }))
    }

    fn apply(&self, sys: &System, intent: Disable) -> Result<UnitState> {
        let Disable { unit, now, .. } = intent;
        let mut cmd = unit.systemctl(sys).arg("disable");
        if now {
            cmd = cmd.arg("--now");
        }
        cmd.arg(&unit.name).run()?;
        let after = format!(
            "{} disable{} {}",
            unit.prefix(),
            if now { " --now" } else { "" },
            unit.name
        );
        // Read back, not predicted: the unit's state is the tool's answer.
        let state = if now {
            unit.verify_stopped(sys, &after)?
        } else {
            unit.state(&unit.probe(sys)?)
        };
        ensure!(
            !state.enabled,
            "unit `{}` is still enabled after `{after}`",
            unit.name
        );
        Ok(state)
    }
}

// ---------------------------------------------------------------------------
// Running / Stopped
// ---------------------------------------------------------------------------

/// Ensure a unit is running. `ansible.builtin.systemd` / `service` with
/// `state: started`.
///
/// Satisfied when `systemctl is-active` answers `active`, `reloading`,
/// `refreshing` or `activating`. `apply` runs `systemctl start` and then
/// re-reads `is-active`; if the unit is not up, the step fails with the last
/// 20 lines of its journal. Refuses a `masked` unit (start would fail anyway)
/// and, in a real run, a unit `systemctl` does not know; under `--check` that
/// unit is reported `would change`, since an earlier step may install it
/// (vision 12).
#[derive(Debug, Clone)]
pub struct Running {
    unit: Unit,
}

impl Running {
    /// Start `unit` on the system manager if it is not up. Says nothing about
    /// boot: pair it with [`Enabled`], or use `Enabled::new(unit).now(true)`,
    /// for a unit that must also come back after a reboot.
    pub fn new(unit: impl Into<String>) -> Self {
        Running {
            unit: Unit::new(unit),
        }
    }

    /// Manage the user units (`systemctl --user`) of the account the step
    /// runs as: the binary's own user, or the target of `ctx.as_user(..)`.
    /// Needs no root, but needs that account's user manager, which runs
    /// while it has a login session or linger. A real run waits for a
    /// manager that linger is still starting ([`Self::manager_timeout`]) and
    /// refuses when there is none. See [the module docs](self#user-units).
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }

    /// With `.user(true)`, how long a real run waits for the account's user
    /// manager when linger is enabled but logind has not finished starting
    /// it, as right after `loginctl enable-linger`. Default
    /// [`DEFAULT_MANAGER_TIMEOUT`]; `Duration::ZERO` does not wait and
    /// refuses at once; `Duration::MAX`, or any timeout too long for the
    /// clock to hold, has no deadline and waits until the manager is up.
    /// Ignored otherwise: without `.user(true)`, when the manager is up,
    /// without linger (the step refuses), and under `--check`, which never
    /// waits. See [the module docs](self#user-units).
    pub fn manager_timeout(mut self, timeout: Duration) -> Self {
        self.unit.manager_timeout = timeout;
        self
    }
}

impl Op for Running {
    type Output = UnitState;
    type Intent = Start;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        let unit = self.unit.guard(sys, "Running")?;
        let p = unit.probe_existing(sys, "Running")?;
        if p.enabled.is_masked() {
            bail!(
                "systemd::Running: unit `{}` is {}{} and cannot be started; unmask it first",
                unit.name,
                p.enabled.as_str(),
                unit.whose()
            );
        }
        if p.active.is_running() {
            return Ok(Plan::Satisfied(unit.state(&p)));
        }
        Ok(Plan::Change(Start {
            unit,
            from: p.active,
        }))
    }

    fn apply(&self, sys: &System, intent: Start) -> Result<UnitState> {
        let unit = intent.unit;
        unit.systemctl(sys).args(["start", &unit.name]).run()?;
        let after = format!("{} start {}", unit.prefix(), unit.name);
        unit.verify_running(sys, &after)
    }
}

/// Ensure a unit is not running. `ansible.builtin.systemd` / `service` with
/// `state: stopped`.
///
/// Satisfied when `is-active` answers `inactive`, `failed`, `deactivating` or
/// `maintenance`. `apply` runs `systemctl stop` and re-reads `is-active`,
/// failing with the journal tail if the unit is still up. A unit `systemctl`
/// does not know is refused in a real run and reported `would change` under
/// `--check` (vision 12).
#[derive(Debug, Clone)]
pub struct Stopped {
    unit: Unit,
}

impl Stopped {
    /// Stop `unit` on the system manager. The unit stays enabled and will come
    /// back at the next boot; [`Disabled`] with `.now(true)` does both.
    pub fn new(unit: impl Into<String>) -> Self {
        Stopped {
            unit: Unit::new(unit),
        }
    }

    /// Manage the user units (`systemctl --user`) of the account the step
    /// runs as: the binary's own user, or the target of `ctx.as_user(..)`.
    /// Needs no root, but needs that account's user manager, which runs
    /// while it has a login session or linger. A real run waits for a
    /// manager that linger is still starting ([`Self::manager_timeout`]) and
    /// refuses when there is none. See [the module docs](self#user-units).
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }

    /// With `.user(true)`, how long a real run waits for the account's user
    /// manager when linger is enabled but logind has not finished starting
    /// it, as right after `loginctl enable-linger`. Default
    /// [`DEFAULT_MANAGER_TIMEOUT`]; `Duration::ZERO` does not wait and
    /// refuses at once; `Duration::MAX`, or any timeout too long for the
    /// clock to hold, has no deadline and waits until the manager is up.
    /// Ignored otherwise: without `.user(true)`, when the manager is up,
    /// without linger (the step refuses), and under `--check`, which never
    /// waits. See [the module docs](self#user-units).
    pub fn manager_timeout(mut self, timeout: Duration) -> Self {
        self.unit.manager_timeout = timeout;
        self
    }
}

impl Op for Stopped {
    type Output = UnitState;
    type Intent = Stop;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        let unit = self.unit.guard(sys, "Stopped")?;
        let p = unit.probe_existing(sys, "Stopped")?;
        // `not-found` only reaches here under --check (`probe_existing`): the
        // unit an earlier step would install is reported as due, not as
        // already stopped (vision 12).
        if p.enabled == EnabledState::NotFound {
            return Ok(Plan::Change(Stop { unit, from: None }));
        }
        if !p.active.is_running() {
            return Ok(Plan::Satisfied(unit.state(&p)));
        }
        Ok(Plan::Change(Stop {
            unit,
            from: Some(p.active),
        }))
    }

    fn apply(&self, sys: &System, intent: Stop) -> Result<UnitState> {
        let unit = intent.unit;
        unit.systemctl(sys).args(["stop", &unit.name]).run()?;
        let after = format!("{} stop {}", unit.prefix(), unit.name);
        unit.verify_stopped(sys, &after)
    }
}

// ---------------------------------------------------------------------------
// Restart / Reload (actions)
// ---------------------------------------------------------------------------

/// Restart a unit. An action (vision 6.4): `ansible.builtin.systemd` /
/// `service` with `state: restarted`. `check` always reports a change and
/// runs nothing; `apply` runs an optional `systemctl daemon-reload`, then
/// `systemctl restart`, then verifies `is-active` and fails with the last 20
/// journal lines if the unit did not come back up. In check mode its output
/// is unavailable (vision 12).
#[derive(Debug, Clone)]
pub struct Restart {
    unit: Unit,
    daemon_reload: bool,
}

impl Restart {
    /// Restart `unit` on the system manager, with no `daemon-reload` first
    /// ([`Restart::daemon_reload`] adds one). Being an action, it runs whether
    /// or not the unit is up, and `systemctl restart` starts a stopped unit.
    pub fn new(unit: impl Into<String>) -> Self {
        Restart {
            unit: Unit::new(unit),
            daemon_reload: false,
        }
    }

    /// Run `systemctl daemon-reload` first (after editing unit files).
    /// Ansible's `daemon_reload: true`.
    pub fn daemon_reload(mut self, on: bool) -> Self {
        self.daemon_reload = on;
        self
    }

    /// Manage the user units (`systemctl --user`) of the account the step
    /// runs as: the binary's own user, or the target of `ctx.as_user(..)`.
    /// Needs no root, but needs that account's user manager, which runs
    /// while it has a login session or linger. A real run waits for a
    /// manager that linger is still starting ([`Self::manager_timeout`]) and
    /// refuses when there is none. See [the module docs](self#user-units).
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }

    /// With `.user(true)`, how long a real run waits for the account's user
    /// manager when linger is enabled but logind has not finished starting
    /// it, as right after `loginctl enable-linger`. Default
    /// [`DEFAULT_MANAGER_TIMEOUT`]; `Duration::ZERO` does not wait and
    /// refuses at once; `Duration::MAX`, or any timeout too long for the
    /// clock to hold, has no deadline and waits until the manager is up.
    /// Ignored otherwise: without `.user(true)`, when the manager is up,
    /// without linger (the step refuses), and under `--check`, which never
    /// waits. See [the module docs](self#user-units).
    pub fn manager_timeout(mut self, timeout: Duration) -> Self {
        self.unit.manager_timeout = timeout;
        self
    }
}

impl Bounce {
    fn run(self, sys: &System) -> Result<UnitState> {
        let Bounce {
            unit,
            daemon_reload,
            verb,
        } = self;
        if daemon_reload {
            unit.systemctl(sys).arg("daemon-reload").run()?;
        }
        unit.systemctl(sys).args([verb, &unit.name]).run()?;
        let after = format!("{} {verb} {}", unit.prefix(), unit.name);
        unit.verify_running(sys, &after)
    }
}

impl Op for Restart {
    type Output = UnitState;
    type Intent = Bounce;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        Ok(Plan::Change(Bounce {
            unit: self.unit.guard(sys, "Restart")?,
            daemon_reload: self.daemon_reload,
            verb: "restart",
        }))
    }

    fn apply(&self, sys: &System, intent: Bounce) -> Result<UnitState> {
        intent.run(sys)
    }

    fn always_changes(&self) -> bool {
        true
    }
}

/// Reload a unit's configuration. An action (vision 6.4):
/// `ansible.builtin.systemd` / `service` with `state: reloaded`.
/// `systemctl reload` fails on a unit that is not running or has no
/// `ExecReload=`; `.or_restart(true)` uses `systemctl reload-or-restart`
/// instead, which restarts in those cases. Same `daemon_reload` and
/// verification as [`Restart`].
#[derive(Debug, Clone)]
pub struct Reload {
    unit: Unit,
    daemon_reload: bool,
    or_restart: bool,
}

impl Reload {
    /// Reload `unit` on the system manager with plain `systemctl reload` and
    /// no `daemon-reload` first. [`Reload::or_restart`] picks the forgiving
    /// verb, [`Reload::daemon_reload`] adds the reload of the unit files.
    pub fn new(unit: impl Into<String>) -> Self {
        Reload {
            unit: Unit::new(unit),
            daemon_reload: false,
            or_restart: false,
        }
    }

    /// Run `systemctl daemon-reload` first. Ansible's `daemon_reload: true`.
    pub fn daemon_reload(mut self, on: bool) -> Self {
        self.daemon_reload = on;
        self
    }

    /// Use `systemctl reload-or-restart`: restart when the unit is not
    /// running or cannot reload.
    pub fn or_restart(mut self, on: bool) -> Self {
        self.or_restart = on;
        self
    }

    /// Manage the user units (`systemctl --user`) of the account the step
    /// runs as: the binary's own user, or the target of `ctx.as_user(..)`.
    /// Needs no root, but needs that account's user manager, which runs
    /// while it has a login session or linger. A real run waits for a
    /// manager that linger is still starting ([`Self::manager_timeout`]) and
    /// refuses when there is none. See [the module docs](self#user-units).
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }

    /// With `.user(true)`, how long a real run waits for the account's user
    /// manager when linger is enabled but logind has not finished starting
    /// it, as right after `loginctl enable-linger`. Default
    /// [`DEFAULT_MANAGER_TIMEOUT`]; `Duration::ZERO` does not wait and
    /// refuses at once; `Duration::MAX`, or any timeout too long for the
    /// clock to hold, has no deadline and waits until the manager is up.
    /// Ignored otherwise: without `.user(true)`, when the manager is up,
    /// without linger (the step refuses), and under `--check`, which never
    /// waits. See [the module docs](self#user-units).
    pub fn manager_timeout(mut self, timeout: Duration) -> Self {
        self.unit.manager_timeout = timeout;
        self
    }

    fn verb(&self) -> &'static str {
        if self.or_restart {
            "reload-or-restart"
        } else {
            "reload"
        }
    }
}

impl Op for Reload {
    type Output = UnitState;
    type Intent = Bounce;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        Ok(Plan::Change(Bounce {
            unit: self.unit.guard(sys, "Reload")?,
            daemon_reload: self.daemon_reload,
            verb: self.verb(),
        }))
    }

    fn apply(&self, sys: &System, intent: Bounce) -> Result<UnitState> {
        intent.run(sys)
    }

    fn always_changes(&self) -> bool {
        true
    }
}

/// Make systemd re-read its unit files. An action (vision 6.4):
/// `ansible.builtin.systemd` with `daemon_reload: true` and no unit.
/// `check` always reports a change and runs nothing; `apply` runs
/// `systemctl daemon-reload` (or `systemctl --user daemon-reload`).
///
/// The step after writing a unit file, when nothing is being restarted yet:
/// until systemd re-reads its files, `systemctl enable rustible-test` on a
/// brand new `rustible-test.service` fails with `not found`. [`Restart`] and
/// [`Reload`] carry the same reload as a `.daemon_reload(true)` flag, for
/// when a unit is being bounced anyway.
///
/// Refuses a host whose init is not systemd, and refuses to run without root
/// unless `.user(true)` selects a user manager, like every other op here.
///
/// Its output is `()`. The other ops here return a [`UnitState`], which needs
/// a unit; this one names none, and the manager exposes nothing worth reading
/// back after a reload. Echoing the `--user` flag as if it were a result
/// would be an input dressed up as an output.
///
/// ```no_run
/// # use rustible_sdk::prelude::*;
/// # use rustible_std::systemd;
/// # fn playbook(ctx: &mut Ctx) -> Result<()> {
/// ctx.sys().write_atomic(
///     "/etc/systemd/system/my-app.service",
///     b"[Unit]\nDescription=my app\n\n[Service]\nExecStart=/usr/bin/my-app\n",
/// )?;
/// ctx.step("systemd re-reads its units", systemd::DaemonReload::new())?;
/// ctx.step("my-app enabled", systemd::Enabled::new("my-app").now(true))?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct DaemonReload {
    manager: Unit,
}

impl Default for DaemonReload {
    fn default() -> Self {
        DaemonReload::new()
    }
}

impl DaemonReload {
    /// Reload the system manager's unit files; [`DaemonReload::user`] switches
    /// to a user manager. There is no unit to name, so nothing here
    /// is validated and nothing is read back afterwards.
    pub fn new() -> Self {
        DaemonReload {
            manager: Unit::manager(),
        }
    }

    /// Reload the user manager (`systemctl --user daemon-reload`) of the
    /// account the step runs as: the binary's own user, or the target of
    /// `ctx.as_user(..)`. Needs no root, but needs that account's user
    /// manager, which runs while it has a login session or linger. A real
    /// run waits for a manager that linger is still starting
    /// ([`Self::manager_timeout`]) and refuses when there is none. See [the
    /// module docs](self#user-units).
    pub fn user(mut self, on: bool) -> Self {
        self.manager.user = on;
        self
    }

    /// With `.user(true)`, how long a real run waits for the account's user
    /// manager when linger is enabled but logind has not finished starting
    /// it, as right after `loginctl enable-linger`. Default
    /// [`DEFAULT_MANAGER_TIMEOUT`]; `Duration::ZERO` does not wait and
    /// refuses at once; `Duration::MAX`, or any timeout too long for the
    /// clock to hold, has no deadline and waits until the manager is up.
    /// Ignored otherwise: without `.user(true)`, when the manager is up,
    /// without linger (the step refuses), and under `--check`, which never
    /// waits. See [the module docs](self#user-units).
    pub fn manager_timeout(mut self, timeout: Duration) -> Self {
        self.manager.manager_timeout = timeout;
        self
    }
}

impl Op for DaemonReload {
    type Output = ();
    type Intent = ReloadUnits;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        Ok(Plan::Change(ReloadUnits {
            manager: self.manager.guard_manager(sys, "DaemonReload")?,
        }))
    }

    fn apply(&self, sys: &System, intent: ReloadUnits) -> Result<()> {
        intent.manager.systemctl(sys).arg("daemon-reload").run()?;
        Ok(())
    }

    fn always_changes(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rustible_sdk::backend::{Backend, CmdSpec, Fake, Output, Stat, WriteAttrs};
    use rustible_sdk::event::Collect;
    use rustible_sdk::{Ctx, HostInfo};

    use super::*;

    // ---- pure ----

    #[test]
    fn parse_is_enabled_every_documented_word() {
        let table = [
            ("enabled", 0, EnabledState::Enabled),
            ("enabled-runtime", 0, EnabledState::EnabledRuntime),
            ("linked", 1, EnabledState::Linked),
            ("linked-runtime", 1, EnabledState::LinkedRuntime),
            ("alias", 0, EnabledState::Alias),
            ("masked", 1, EnabledState::Masked),
            ("masked-runtime", 1, EnabledState::MaskedRuntime),
            ("static", 0, EnabledState::Static),
            ("indirect", 0, EnabledState::Indirect),
            ("disabled", 1, EnabledState::Disabled),
            ("generated", 0, EnabledState::Generated),
            ("transient", 0, EnabledState::Transient),
            ("not-found", 4, EnabledState::NotFound),
        ];
        for (word, exit, want) in table {
            assert_eq!(
                parse_is_enabled(&format!("{word}\n"), exit, "").unwrap(),
                want,
                "{word}"
            );
            assert_eq!(want.as_str(), word);
        }
    }

    #[test]
    fn parse_is_enabled_nonzero_exit_with_meaningful_stdout() {
        // `disabled` exits 1, the word still wins.
        assert_eq!(
            parse_is_enabled("disabled\n", 1, "").unwrap(),
            EnabledState::Disabled
        );
        // Older systemd: nothing on stdout, the reason on stderr, exit 1.
        assert_eq!(parse_is_enabled("", 1, "").unwrap(), EnabledState::NotFound);
        assert_eq!(
            parse_is_enabled(
                "",
                1,
                "Failed to get unit file state for nginx.service: No such file or directory\n"
            )
            .unwrap(),
            EnabledState::NotFound
        );
        // Nothing on stdout because systemctl could not answer at all: an
        // error in both modes, never a unit an earlier step would install.
        let err = parse_is_enabled("", 1, "Failed to connect to bus: No medium found\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("failed without an answer (exit 1)"), "{err}");
        assert!(err.contains("Failed to connect to bus"), "{err}");
        // The bus failure that ends in the not-found phrase: still an error.
        for stderr in [
            "Failed to connect to bus: No such file or directory\n",
            "Failed to connect to user scope bus via local transport: No such file or directory\n",
        ] {
            let err = parse_is_enabled("", 1, stderr).unwrap_err().to_string();
            assert!(err.contains("failed without an answer"), "{stderr}: {err}");
        }
        // Empty stdout with exit 0 is not a known state.
        assert_eq!(
            parse_is_enabled("", 0, "").unwrap(),
            EnabledState::Other(String::new())
        );
        assert_eq!(
            parse_is_enabled("bogus\n", 0, "").unwrap(),
            EnabledState::Other("bogus".into())
        );
    }

    #[test]
    fn enabled_state_classification() {
        for s in [
            EnabledState::Enabled,
            EnabledState::EnabledRuntime,
            EnabledState::Alias,
            EnabledState::Static,
            EnabledState::Indirect,
            EnabledState::Generated,
            EnabledState::Transient,
        ] {
            assert!(s.is_enabled(), "{s:?}");
        }
        for s in [
            EnabledState::Disabled,
            EnabledState::Masked,
            EnabledState::MaskedRuntime,
            EnabledState::Linked,
            EnabledState::LinkedRuntime,
            EnabledState::NotFound,
            EnabledState::Other("x".into()),
        ] {
            assert!(!s.is_enabled(), "{s:?}");
        }
        assert!(EnabledState::Static.cannot_be_disabled());
        assert!(EnabledState::Generated.cannot_be_disabled());
        assert!(!EnabledState::Enabled.cannot_be_disabled());
        assert!(!EnabledState::Alias.cannot_be_disabled());
        assert!(EnabledState::MaskedRuntime.is_masked());
    }

    #[test]
    fn parse_is_active_every_documented_word() {
        let table = [
            ("active", 0, ActiveState::Active),
            ("reloading", 0, ActiveState::Reloading),
            ("refreshing", 0, ActiveState::Refreshing),
            ("activating", 3, ActiveState::Activating),
            ("inactive", 3, ActiveState::Inactive),
            ("failed", 3, ActiveState::Failed),
            ("deactivating", 3, ActiveState::Deactivating),
            ("maintenance", 3, ActiveState::Maintenance),
        ];
        for (word, exit, want) in table {
            assert_eq!(parse_is_active(&format!("{word}\n"), exit), want, "{word}");
            assert_eq!(want.as_str(), word);
        }
    }

    #[test]
    fn parse_is_active_exit_codes_and_unknowns() {
        // Unknown unit on systemd 255: `inactive` with exit 4.
        assert_eq!(parse_is_active("inactive\n", 4), ActiveState::Inactive);
        assert_eq!(parse_is_active("", 3), ActiveState::Inactive);
        assert_eq!(parse_is_active("", 0), ActiveState::Other(String::new()));
        assert_eq!(
            parse_is_active("weird\n", 0),
            ActiveState::Other("weird".into())
        );
    }

    #[test]
    fn active_state_classification() {
        for s in [
            ActiveState::Active,
            ActiveState::Reloading,
            ActiveState::Refreshing,
            ActiveState::Activating,
        ] {
            assert!(s.is_running(), "{s:?}");
        }
        for s in [
            ActiveState::Inactive,
            ActiveState::Failed,
            ActiveState::Deactivating,
            ActiveState::Maintenance,
            ActiveState::Other("x".into()),
        ] {
            assert!(!s.is_running(), "{s:?}");
        }
    }

    #[test]
    fn validate_unit_rejects_unusable_names() {
        assert!(validate_unit("nginx").is_ok());
        assert!(validate_unit("getty@tty1.service").is_ok());
        for bad in ["", "a b", "-x", "a\nb", "\tnginx"] {
            assert!(validate_unit(bad).is_err(), "{bad:?}");
        }
    }

    /// Whole seconds and whole milliseconds as written; anything finer is
    /// not rounded down to `0s`.
    #[test]
    fn human_renders_a_timeout_as_written() {
        for (d, want) in [
            (Duration::ZERO, "0s"),
            (Duration::from_secs(30), "30s"),
            (Duration::from_millis(250), "250ms"),
            (Duration::from_millis(1500), "1500ms"),
            (Duration::from_micros(500), "500µs"),
            (Duration::from_micros(1500), "1.5ms"),
            (Duration::from_nanos(1), "1ns"),
        ] {
            assert_eq!(human(d), want, "{d:?}");
        }
    }

    /// Only a stat refused with `EACCES` is "not yet"; any other failure,
    /// or a message that merely reads like one, is still an error. That last
    /// is also what `System` makes of an `as_user` helper that died: its
    /// report becomes a message, never an `IoAt`, so it fails the step.
    #[test]
    fn denied_is_an_eacces_on_a_path_and_nothing_else() {
        let at = |kind| {
            Error::from(IoAt {
                path: SOCKET.into(),
                source: io::Error::from(kind),
            })
        };
        assert!(denied(&at(io::ErrorKind::PermissionDenied)));
        assert!(denied(
            &at(io::ErrorKind::PermissionDenied).context("looking for the manager")
        ));
        assert!(!denied(&at(io::ErrorKind::NotFound)));
        assert!(!denied(&at(io::ErrorKind::Other)));
        assert!(!denied(&Error::msg("Permission denied (os error 13)")));
    }

    // ---- fake helpers ----

    fn sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    /// Facts for a mac.
    fn macos(sys: System) -> System {
        let mut facts = sys.facts().clone();
        facts.os = Os::Macos;
        facts.distro = Distro::Macos;
        facts.package_managers = [Pm::Brew].into_iter().collect();
        facts.init = Init::Launchd;
        sys.with_facts(facts)
    }

    /// launchd is pid 1 on a mac, so the init check would refuse anyway;
    /// the OS check is asserted because it is the one a reader of the
    /// message is owed.
    #[test]
    fn systemd_refuses_a_mac_on_the_os() {
        let fake = Arc::new(Fake::new());
        let s = macos(sys(&fake));
        let err = Enabled::new("sshd").check(&s).unwrap_err().chain();
        assert!(err.contains("runs on Linux only"), "{err}");
        assert!(err.contains("macos"), "{err}");
    }

    fn not_root(s: System) -> System {
        let mut facts = s.facts().clone();
        facts.is_root = false;
        facts.user = "cadu".into();
        s.with_facts(facts)
    }

    /// A fake whose probes answer `enabled` / `active` as given.
    fn probes(enabled: &str, active: &str) -> Fake {
        probes_for("nginx", enabled, active)
    }

    fn probes_for(unit: &str, enabled: &str, active: &str) -> Fake {
        let enabled_status = if EnabledState::is_enabled(&parse_is_enabled(enabled, 0, "").unwrap())
        {
            0
        } else {
            1
        };
        let active_status = if parse_is_active(active, 0).is_running() {
            0
        } else {
            3
        };
        Fake::new()
            .with_cmd(
                "systemctl",
                Some(&["is-enabled", unit]),
                enabled_status,
                &format!("{enabled}\n"),
            )
            .with_cmd(
                "systemctl",
                Some(&["is-active", unit]),
                active_status,
                &format!("{active}\n"),
            )
    }

    fn with_ok(fake: Fake, args: &[&str]) -> Fake {
        fake.with_cmd("systemctl", Some(args), 0, "")
    }

    fn with_journal(fake: Fake) -> Fake {
        fake.with_cmd(
            "journalctl",
            Some(&["-u", "nginx", "--no-pager", "-n", "20"]),
            0,
            "Sep 08 10:00:00 host nginx[1]: bind() to 0.0.0.0:80 failed\nSep 08 10:00:00 host systemd[1]: Failed to start nginx.\n",
        )
    }

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A box where `id -u` answers `uid` and that uid's user manager is up
    /// (its socket exists), which is what every `--user` step needs.
    fn user_manager(fake: Fake, uid: u32) -> Fake {
        with_id(fake, uid)
            .with_dir(format!("/run/user/{uid}/systemd"))
            .with_file(format!("/run/user/{uid}/systemd/private"), "")
    }

    /// `id -u` answers `uid`, and nothing else is planted.
    fn with_id(fake: Fake, uid: u32) -> Fake {
        fake.with_cmd("id", Some(&["-u"]), 0, &format!("{uid}\n"))
    }

    /// What each command ran with as `XDG_RUNTIME_DIR`, in order.
    fn runtime_dirs(fake: &Fake) -> Vec<Option<String>> {
        fake.commands()
            .iter()
            .map(|c| c.env.get("XDG_RUNTIME_DIR").cloned())
            .collect()
    }

    const PROBES: [&[&str]; 2] = [
        &["systemctl", "is-enabled", "nginx"],
        &["systemctl", "is-active", "nginx"],
    ];

    fn assert_only_probes(fake: &Fake) {
        assert_eq!(
            fake.argvs(),
            PROBES.iter().map(|a| argv(a)).collect::<Vec<_>>()
        );
    }

    fn state(enabled: bool, active: bool) -> UnitState {
        UnitState {
            unit: "nginx".into(),
            enabled,
            active,
        }
    }

    /// The rows of the report a plan renders, as (name, from, to). Read off
    /// the rendered text, which is what a user sees: a `Diff` has no fields
    /// to read.
    fn attrs<O: Op>(plan: &Plan<O>) -> Vec<(String, String, String)>
    where
        O::Output: std::fmt::Debug,
    {
        let Plan::Change(c) = plan else {
            panic!("expected change, got {plan:?}")
        };
        let rendered = c.diff().render();
        let mut lines = rendered.lines();
        assert_eq!(lines.next(), Some("nginx:"), "{rendered}");
        lines
            .map(|l| {
                let (name, rest) = l.trim().split_once(": ").expect("a row");
                let (from, to) = rest.split_once(" -> ").expect("a change");
                triple(name, from, to)
            })
            .collect()
    }

    fn change<O: Op>(plan: Plan<O>) -> O::Intent
    where
        O::Output: std::fmt::Debug,
    {
        match plan {
            Plan::Change(c) => c,
            Plan::Satisfied(s) => panic!("expected change, got satisfied {s:?}"),
        }
    }

    fn triple(name: &str, from: &str, to: &str) -> (String, String, String) {
        (name.into(), from.into(), to.into())
    }

    // ---- Enabled ----

    #[test]
    fn enabled_satisfied_and_only_probes_ran() {
        let fake = Arc::new(probes("enabled", "active"));
        let plan = Enabled::new("nginx").check(&sys(&fake)).unwrap();
        let Plan::Satisfied(s) = plan else {
            panic!("expected satisfied")
        };
        assert_eq!(s, state(true, true));
        assert_only_probes(&fake);
    }

    #[test]
    fn enabled_satisfied_on_static_alias_indirect_even_when_inactive() {
        for word in ["static", "alias", "indirect", "generated"] {
            let fake = Arc::new(probes(word, "inactive"));
            let plan = Enabled::new("nginx").check(&sys(&fake)).unwrap();
            let Plan::Satisfied(s) = plan else {
                panic!("{word}: expected satisfied")
            };
            assert_eq!(s, state(true, false), "{word}");
        }
    }

    #[test]
    fn enabled_plans_change_with_diff() {
        let fake = Arc::new(probes("disabled", "inactive"));
        let plan = Enabled::new("nginx").check(&sys(&fake)).unwrap();
        // Without `now`, `active` is not the op's business.
        assert_eq!(attrs(&plan), vec![triple("enabled", "disabled", "enabled")]);
        assert_only_probes(&fake);
    }

    #[test]
    fn enabled_apply_runs_enable_then_reads_both_probes() {
        let before = Arc::new(probes("disabled", "inactive"));
        let plan = Enabled::new("nginx").check(&sys(&before)).unwrap();
        // The post-apply probes share their argv with the check probes and
        // the fake answers statically, so apply runs against a second fake
        // that reflects the world after `systemctl enable`.
        let after = Arc::new(with_ok(probes("enabled", "inactive"), &["enable", "nginx"]));
        let out = Enabled::new("nginx")
            .apply(&sys(&after), change(plan))
            .unwrap();
        assert_eq!(out, state(true, false));
        assert_eq!(
            after.argvs(),
            vec![
                argv(&["systemctl", "enable", "nginx"]),
                argv(&["systemctl", "is-enabled", "nginx"]),
                argv(&["systemctl", "is-active", "nginx"]),
            ]
        );
        // The system manager needs no runtime directory, so none is set.
        assert!(runtime_dirs(&after).iter().all(Option::is_none));
    }

    #[test]
    fn enabled_now_adds_active_change_and_the_flag() {
        let before = Arc::new(probes("enabled", "inactive"));
        let plan = Enabled::new("nginx")
            .now(true)
            .check(&sys(&before))
            .unwrap();
        assert_eq!(attrs(&plan), vec![triple("active", "inactive", "active")]);

        let after = Arc::new(with_ok(
            probes("enabled", "active"),
            &["enable", "--now", "nginx"],
        ));
        let out = Enabled::new("nginx")
            .now(true)
            .apply(&sys(&after), change(plan))
            .unwrap();
        assert_eq!(out, state(true, true));
        assert_eq!(
            after.argvs()[0],
            argv(&["systemctl", "enable", "--now", "nginx"])
        );
    }

    #[test]
    fn enabled_now_fails_with_journal_when_unit_does_not_come_up() {
        let before = Arc::new(probes("disabled", "inactive"));
        let plan = Enabled::new("nginx")
            .now(true)
            .check(&sys(&before))
            .unwrap();
        let after = Arc::new(with_journal(with_ok(
            probes("enabled", "failed"),
            &["enable", "--now", "nginx"],
        )));
        let err = Enabled::new("nginx")
            .now(true)
            .apply(&sys(&after), change(plan))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("`nginx` is failed after `systemctl enable --now nginx`"),
            "{err}"
        );
        assert!(err.contains("bind() to 0.0.0.0:80 failed"), "{err}");
    }

    #[test]
    fn enabled_refuses_masked_and_missing_units() {
        let fake = Arc::new(probes("masked", "inactive"));
        let err = Enabled::new("nginx")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is masked"), "{err}");
        assert!(err.contains("systemctl unmask nginx"), "{err}");

        let fake = Arc::new(probes("not-found", "inactive"));
        let err = Enabled::new("nginx")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unit `nginx` not found"), "{err}");
    }

    /// Under --check a unit systemctl cannot find is one an earlier step may
    /// install (vision 12): the four state ops report the state they would
    /// set, from `not-found`, and nothing but the probes runs. A real run
    /// refuses, and a masked unit is refused in both modes: no step in the
    /// run unmasks it.
    #[test]
    fn missing_unit_is_would_change_under_check_and_refused_otherwise() {
        let dry = |fake: &Arc<Fake>| sys(fake).with_check_mode(true);

        let fake = Arc::new(probes("not-found", "inactive"));
        let plan = Enabled::new("nginx").check(&dry(&fake)).unwrap();
        assert_eq!(
            attrs(&plan),
            vec![triple("enabled", "not-found", "enabled")]
        );
        assert_only_probes(&fake);

        let fake = Arc::new(probes("not-found", "inactive"));
        let plan = Enabled::new("nginx").now(true).check(&dry(&fake)).unwrap();
        assert_eq!(
            attrs(&plan),
            vec![
                triple("enabled", "not-found", "enabled"),
                triple("active", "inactive", "active"),
            ]
        );

        let fake = Arc::new(probes("not-found", "inactive"));
        let plan = Running::new("nginx").check(&dry(&fake)).unwrap();
        assert_eq!(attrs(&plan), vec![triple("active", "inactive", "active")]);

        // `Disabled` and `Stopped` report the unit as due too, rather than
        // reading "not there" as "already disabled": the package a later
        // real run installs may well enable and start it (Debian does).
        let fake = Arc::new(probes("not-found", "inactive"));
        let plan = Disabled::new("nginx").check(&dry(&fake)).unwrap();
        assert_eq!(
            attrs(&plan),
            vec![triple("enabled", "not-found", "disabled")]
        );
        let plan = Stopped::new("nginx").check(&dry(&fake)).unwrap();
        assert_eq!(
            attrs(&plan),
            vec![triple("active", "not-found", "inactive")]
        );

        // The real run's `check` refuses, verbatim.
        for (name, err) in [
            (
                "Enabled",
                Enabled::new("nginx").check(&sys(&fake)).unwrap_err(),
            ),
            (
                "Disabled",
                Disabled::new("nginx").check(&sys(&fake)).unwrap_err(),
            ),
            (
                "Running",
                Running::new("nginx").check(&sys(&fake)).unwrap_err(),
            ),
            (
                "Stopped",
                Stopped::new("nginx").check(&sys(&fake)).unwrap_err(),
            ),
        ] {
            let err = err.chain();
            assert_eq!(
                err,
                format!("systemd::{name}: unit `nginx` not found by `systemctl is-enabled`")
            );
        }

        // Masked is about the unit itself, not about a step that has not run.
        let fake = Arc::new(probes("masked", "inactive"));
        let err = Enabled::new("nginx")
            .check(&dry(&fake))
            .unwrap_err()
            .chain();
        assert!(err.contains("is masked"), "{err}");
    }

    #[test]
    fn enabled_fails_when_still_disabled_after_enable() {
        // One fake: `enable` "succeeds" but is-enabled keeps saying disabled.
        let fake = Arc::new(with_ok(probes("disabled", "active"), &["enable", "nginx"]));
        let plan = Enabled::new("nginx").check(&sys(&fake)).unwrap();
        let err = Enabled::new("nginx")
            .apply(&sys(&fake), change(plan))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("still not enabled after `systemctl enable nginx`"),
            "{err}"
        );
    }

    #[test]
    fn disabled_fails_when_still_enabled_after_disable() {
        // One fake: `disable` "succeeds" but is-enabled keeps saying enabled.
        let fake = Arc::new(with_ok(probes("enabled", "active"), &["disable", "nginx"]));
        let plan = Disabled::new("nginx").check(&sys(&fake)).unwrap();
        let err = Disabled::new("nginx")
            .apply(&sys(&fake), change(plan))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("still enabled after `systemctl disable nginx`"),
            "{err}"
        );
    }

    #[test]
    fn enabled_surfaces_systemctl_failure() {
        let fake = Arc::new(probes("disabled", "inactive").with_cmd(
            "systemctl",
            Some(&["enable", "nginx"]),
            1,
            "",
        ));
        let plan = Enabled::new("nginx").check(&sys(&fake)).unwrap();
        let err = Enabled::new("nginx")
            .apply(&sys(&fake), change(plan))
            .unwrap_err();
        assert_eq!(err.cmd_failed().map(|c| c.status), Some(1), "{err:#}");
    }

    // ---- Disabled ----

    #[test]
    fn disabled_satisfied_on_disabled_masked_and_linked() {
        for word in ["disabled", "masked", "linked"] {
            let fake = Arc::new(probes(word, "active"));
            let plan = Disabled::new("nginx").check(&sys(&fake)).unwrap();
            let Plan::Satisfied(s) = plan else {
                panic!("{word}: expected satisfied")
            };
            assert_eq!(s, state(false, true), "{word}");
            assert_only_probes(&fake);
        }
    }

    #[test]
    fn disabled_plans_and_applies_disable() {
        let before = Arc::new(probes("enabled", "active"));
        let plan = Disabled::new("nginx").check(&sys(&before)).unwrap();
        assert_eq!(attrs(&plan), vec![triple("enabled", "enabled", "disabled")]);

        let after = Arc::new(with_ok(probes("disabled", "active"), &["disable", "nginx"]));
        let out = Disabled::new("nginx")
            .apply(&sys(&after), change(plan))
            .unwrap();
        assert_eq!(out, state(false, true));
        assert_eq!(
            after.argvs(),
            vec![
                argv(&["systemctl", "disable", "nginx"]),
                argv(&["systemctl", "is-enabled", "nginx"]),
                argv(&["systemctl", "is-active", "nginx"]),
            ]
        );
    }

    #[test]
    fn disabled_refuses_static_generated_transient() {
        for word in ["static", "generated", "transient"] {
            let fake = Arc::new(probes(word, "active"));
            let err = Disabled::new("nginx")
                .check(&sys(&fake))
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("is {word}")), "{word}: {err}");
            assert!(err.contains("cannot be disabled"), "{word}: {err}");
        }
    }

    #[test]
    fn disabled_now_stops_and_fails_with_journal_if_still_running() {
        let before = Arc::new(probes("enabled", "active"));
        let plan = Disabled::new("nginx")
            .now(true)
            .check(&sys(&before))
            .unwrap();
        assert_eq!(
            attrs(&plan),
            vec![
                triple("enabled", "enabled", "disabled"),
                triple("active", "active", "inactive"),
            ]
        );

        let after = Arc::new(with_journal(with_ok(
            probes("disabled", "active"),
            &["disable", "--now", "nginx"],
        )));
        let err = Disabled::new("nginx")
            .now(true)
            .apply(&sys(&after), change(plan))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("still active after `systemctl disable --now nginx`"),
            "{err}"
        );
        assert!(err.contains("Failed to start nginx"), "{err}");
        assert_eq!(
            after.argvs()[0],
            argv(&["systemctl", "disable", "--now", "nginx"])
        );
    }

    // ---- Running ----

    #[test]
    fn running_satisfied_on_active_and_transitional_states() {
        for word in ["active", "reloading", "activating"] {
            let fake = Arc::new(probes("disabled", word));
            let plan = Running::new("nginx").check(&sys(&fake)).unwrap();
            let Plan::Satisfied(s) = plan else {
                panic!("{word}: expected satisfied")
            };
            assert_eq!(s, state(false, true), "{word}");
            assert_only_probes(&fake);
        }
    }

    #[test]
    fn running_plans_change_from_failed() {
        let fake = Arc::new(probes("enabled", "failed"));
        let plan = Running::new("nginx").check(&sys(&fake)).unwrap();
        assert_eq!(attrs(&plan), vec![triple("active", "failed", "active")]);
        assert_only_probes(&fake);
    }

    #[test]
    fn running_apply_starts_then_verifies() {
        let before = Arc::new(probes("enabled", "inactive"));
        let plan = Running::new("nginx").check(&sys(&before)).unwrap();
        let after = Arc::new(with_ok(probes("enabled", "active"), &["start", "nginx"]));
        let out = Running::new("nginx")
            .apply(&sys(&after), change(plan))
            .unwrap();
        assert_eq!(out, state(true, true));
        assert_eq!(
            after.argvs(),
            vec![
                argv(&["systemctl", "start", "nginx"]),
                argv(&["systemctl", "is-enabled", "nginx"]),
                argv(&["systemctl", "is-active", "nginx"]),
            ]
        );
    }

    #[test]
    fn running_fails_with_journal_when_failed_after_start() {
        // One fake suffices: is-active says `failed` before and after.
        let fake = Arc::new(with_journal(with_ok(
            probes("enabled", "failed"),
            &["start", "nginx"],
        )));
        let plan = Running::new("nginx").check(&sys(&fake)).unwrap();
        let err = Running::new("nginx")
            .apply(&sys(&fake), change(plan))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unit `nginx` is failed after `systemctl start nginx`"),
            "{err}"
        );
        assert!(err.contains("last journal lines:"), "{err}");
        assert!(err.contains("bind() to 0.0.0.0:80 failed"), "{err}");
        assert!(
            fake.argvs().contains(&argv(&[
                "journalctl",
                "-u",
                "nginx",
                "--no-pager",
                "-n",
                "20"
            ])),
            "{:?}",
            fake.argvs()
        );
    }

    #[test]
    fn running_failure_message_survives_a_missing_journal() {
        let fake = Arc::new(
            with_ok(probes("enabled", "inactive"), &["start", "nginx"]).with_cmd(
                "journalctl",
                None,
                1,
                "",
            ),
        );
        let plan = Running::new("nginx").check(&sys(&fake)).unwrap();
        let err = Running::new("nginx")
            .apply(&sys(&fake), change(plan))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is inactive after"), "{err}");
        assert!(err.contains("journalctl exited 1"), "{err}");
    }

    #[test]
    fn running_refuses_masked_and_missing_units() {
        let fake = Arc::new(probes("masked", "inactive"));
        let err = Running::new("nginx")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is masked and cannot be started"), "{err}");

        let fake = Arc::new(probes("", "inactive"));
        let err = Running::new("nginx")
            .check(&sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unit `nginx` not found"), "{err}");
    }

    // ---- Stopped ----

    #[test]
    fn stopped_satisfied_on_inactive_and_failed() {
        for word in ["inactive", "failed", "deactivating"] {
            let fake = Arc::new(probes("enabled", word));
            let plan = Stopped::new("nginx").check(&sys(&fake)).unwrap();
            let Plan::Satisfied(s) = plan else {
                panic!("{word}: expected satisfied")
            };
            assert_eq!(s, state(true, false), "{word}");
        }
    }

    #[test]
    fn stopped_plans_and_applies_stop() {
        let before = Arc::new(probes("enabled", "active"));
        let plan = Stopped::new("nginx").check(&sys(&before)).unwrap();
        assert_eq!(attrs(&plan), vec![triple("active", "active", "inactive")]);

        let after = Arc::new(with_ok(probes("enabled", "inactive"), &["stop", "nginx"]));
        let out = Stopped::new("nginx")
            .apply(&sys(&after), change(plan))
            .unwrap();
        assert_eq!(out, state(true, false));
        assert_eq!(after.argvs()[0], argv(&["systemctl", "stop", "nginx"]));
    }

    #[test]
    fn stopped_fails_when_still_running_after_stop() {
        let fake = Arc::new(with_journal(with_ok(
            probes("enabled", "active"),
            &["stop", "nginx"],
        )));
        let plan = Stopped::new("nginx").check(&sys(&fake)).unwrap();
        let err = Stopped::new("nginx")
            .apply(&sys(&fake), change(plan))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("still active after `systemctl stop nginx`"),
            "{err}"
        );
    }

    // ---- Restart / Reload ----

    #[test]
    fn restart_always_changes_and_check_runs_nothing() {
        let fake = Arc::new(Fake::new());
        let op = Restart::new("nginx");
        assert!(op.always_changes());
        let plan = op.check(&sys(&fake)).unwrap();
        let Plan::Change(c) = plan else {
            panic!("expected change")
        };
        assert_eq!(c.diff().render(), "systemctl restart nginx");
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());

        let with_reload = Restart::new("nginx")
            .daemon_reload(true)
            .check(&sys(&fake))
            .unwrap();
        assert_eq!(
            change(with_reload).diff().render(),
            "systemctl daemon-reload && systemctl restart nginx"
        );
    }

    #[test]
    fn restart_apply_daemon_reloads_restarts_and_verifies() {
        let fake = Arc::new(with_ok(
            with_ok(probes("enabled", "active"), &["daemon-reload"]),
            &["restart", "nginx"],
        ));
        let op = Restart::new("nginx").daemon_reload(true);
        let plan = op.check(&sys(&fake)).unwrap();
        let out = op.apply(&sys(&fake), change(plan)).unwrap();
        assert_eq!(out, state(true, true));
        assert_eq!(
            fake.argvs(),
            vec![
                argv(&["systemctl", "daemon-reload"]),
                argv(&["systemctl", "restart", "nginx"]),
                argv(&["systemctl", "is-enabled", "nginx"]),
                argv(&["systemctl", "is-active", "nginx"]),
            ]
        );
    }

    #[test]
    fn restart_fails_with_journal_when_unit_stays_down() {
        let fake = Arc::new(with_journal(with_ok(
            probes("enabled", "failed"),
            &["restart", "nginx"],
        )));
        let op = Restart::new("nginx");
        let plan = op.check(&sys(&fake)).unwrap();
        let err = op.apply(&sys(&fake), change(plan)).unwrap_err().to_string();
        assert!(
            err.contains("is failed after `systemctl restart nginx`"),
            "{err}"
        );
        assert!(err.contains("Failed to start nginx"), "{err}");
    }

    #[test]
    fn reload_uses_reload_and_or_restart_switches_verb() {
        let fake = Arc::new(with_ok(probes("enabled", "active"), &["reload", "nginx"]));
        let op = Reload::new("nginx");
        assert!(op.always_changes());
        let plan = op.check(&sys(&fake)).unwrap();
        assert_eq!(change(plan).diff().render(), "systemctl reload nginx");
        let plan = op.check(&sys(&fake)).unwrap();
        let out = op.apply(&sys(&fake), change(plan)).unwrap();
        assert_eq!(out, state(true, true));
        assert_eq!(fake.argvs()[0], argv(&["systemctl", "reload", "nginx"]));

        let fake = Arc::new(with_ok(
            with_ok(probes("enabled", "active"), &["daemon-reload"]),
            &["reload-or-restart", "nginx"],
        ));
        let op = Reload::new("nginx").or_restart(true).daemon_reload(true);
        let plan = op.check(&sys(&fake)).unwrap();
        assert_eq!(
            change(plan).diff().render(),
            "systemctl daemon-reload && systemctl reload-or-restart nginx"
        );
        let plan = op.check(&sys(&fake)).unwrap();
        op.apply(&sys(&fake), change(plan)).unwrap();
        assert_eq!(
            &fake.argvs()[..2],
            &[
                argv(&["systemctl", "daemon-reload"]),
                argv(&["systemctl", "reload-or-restart", "nginx"]),
            ]
        );
    }

    #[test]
    fn reload_surfaces_systemctl_failure_on_inactive_unit() {
        // `systemctl reload` on a stopped unit exits 1; the CmdFailed travels.
        let fake = Arc::new(probes("enabled", "inactive").with_cmd(
            "systemctl",
            Some(&["reload", "nginx"]),
            1,
            "",
        ));
        let op = Reload::new("nginx");
        let plan = op.check(&sys(&fake)).unwrap();
        let err = op.apply(&sys(&fake), change(plan)).unwrap_err();
        let cf = err.cmd_failed().expect("cmd failed in chain");
        assert_eq!(cf.argv, argv(&["systemctl", "reload", "nginx"]));
    }

    // ---- DaemonReload ----

    #[test]
    fn daemon_reload_always_changes_names_no_unit_and_runs_nothing_in_check() {
        let fake = Arc::new(Fake::new());
        let op = DaemonReload::new();
        assert!(op.always_changes());
        let Plan::Change(c) = op.check(&sys(&fake)).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff().render(), "systemctl daemon-reload");
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    #[test]
    fn daemon_reload_apply_runs_exactly_one_command() {
        let fake = Arc::new(with_ok(Fake::new(), &["daemon-reload"]));
        let op = DaemonReload::new();
        let plan = op.check(&sys(&fake)).unwrap();
        op.apply(&sys(&fake), change(plan)).unwrap();
        assert_eq!(fake.argvs(), vec![argv(&["systemctl", "daemon-reload"])]);
    }

    #[test]
    fn daemon_reload_user_mode_needs_no_root_and_carries_the_flag() {
        let fake = Arc::new(with_ok(
            user_manager(Fake::new(), 1000),
            &["--user", "daemon-reload"],
        ));
        let s = not_root(sys(&fake));
        let op = DaemonReload::new().user(true);
        let plan = op.check(&s).unwrap();
        assert_eq!(
            change(op.check(&s).unwrap()).diff().render(),
            "systemctl --user daemon-reload"
        );
        op.apply(&s, change(plan)).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![
                argv(&["id", "-u"]),
                argv(&["id", "-u"]),
                argv(&["systemctl", "--user", "daemon-reload"])
            ]
        );
        assert_eq!(runtime_dirs(&fake)[2].as_deref(), Some("/run/user/1000"));
    }

    /// `Default` exists only so clippy's `new_without_default` is satisfied;
    /// it must not drift from `new`.
    #[test]
    fn daemon_reload_default_matches_new() {
        let fake = Arc::new(Fake::new());
        assert_eq!(
            change(DaemonReload::default().check(&sys(&fake)).unwrap())
                .diff()
                .render(),
            change(DaemonReload::new().check(&sys(&fake)).unwrap())
                .diff()
                .render()
        );
    }

    #[test]
    fn daemon_reload_through_ctx_in_check_mode_runs_nothing() {
        let fake = Arc::new(Fake::new());
        let s = sys(&fake).with_check_mode(true);
        let mut ctx = Ctx::new(s, HostInfo::local());
        let r = ctx
            .step("systemd re-reads its units", DaemonReload::new())
            .unwrap();
        assert!(r.changed);
        assert!(
            !r.is_available(),
            "a would-change step has no output (vision 12)"
        );
        assert_eq!(r.diff.as_ref().unwrap().render(), "systemctl daemon-reload");
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    // ---- shared guards ----

    /// Every op's `check` on `s`, against the system manager or, with
    /// `user`, against the user manager of the account `s` runs as.
    fn all_ops_check(s: &System, user: bool) -> Vec<(&'static str, Result<bool>)> {
        vec![
            (
                "Enabled",
                Enabled::new("nginx")
                    .user(user)
                    .check(s)
                    .map(|p| p.is_change()),
            ),
            (
                "Disabled",
                Disabled::new("nginx")
                    .user(user)
                    .check(s)
                    .map(|p| p.is_change()),
            ),
            (
                "Running",
                Running::new("nginx")
                    .user(user)
                    .check(s)
                    .map(|p| p.is_change()),
            ),
            (
                "Stopped",
                Stopped::new("nginx")
                    .user(user)
                    .check(s)
                    .map(|p| p.is_change()),
            ),
            (
                "Restart",
                Restart::new("nginx")
                    .user(user)
                    .check(s)
                    .map(|p| p.is_change()),
            ),
            (
                "Reload",
                Reload::new("nginx")
                    .user(user)
                    .check(s)
                    .map(|p| p.is_change()),
            ),
            (
                "DaemonReload",
                DaemonReload::new()
                    .user(user)
                    .check(s)
                    .map(|p| p.is_change()),
            ),
        ]
    }

    #[test]
    fn every_op_refuses_a_non_systemd_init_without_running_anything() {
        let fake = Arc::new(probes("enabled", "active"));
        for (init, shown) in [
            (Init::OpenRc, "OpenRC"),
            (Init::Other("busybox".into()), "`busybox`"),
        ] {
            let mut facts = sys(&fake).facts().clone();
            facts.init = init;
            let s = sys(&fake).with_facts(facts);
            for (name, r) in all_ops_check(&s, false) {
                let err = r.unwrap_err().to_string();
                assert!(
                    err.contains(&format!("systemd::{name} needs systemd")),
                    "{name}: {err}"
                );
                assert!(err.contains(shown), "{name}: {err}");
            }
        }
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    #[test]
    fn every_op_refuses_without_root_and_names_the_user() {
        let fake = Arc::new(probes("enabled", "active"));
        let s = not_root(sys(&fake));
        for (name, r) in all_ops_check(&s, false) {
            let err = r.unwrap_err().to_string();
            assert!(
                err.contains(&format!("systemd::{name} needs root")),
                "{name}: {err}"
            );
            assert!(err.contains("runs as `cadu`"), "{name}: {err}");
            assert!(err.contains(".user(true)"), "{name}: {err}");
        }
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    #[test]
    fn every_op_rejects_a_bad_unit_name() {
        let fake = Arc::new(Fake::new());
        let s = sys(&fake);
        for bad in ["", "-x", "a b"] {
            assert!(Enabled::new(bad).check(&s).is_err(), "{bad:?}");
            assert!(Disabled::new(bad).check(&s).is_err(), "{bad:?}");
            assert!(Running::new(bad).check(&s).is_err(), "{bad:?}");
            assert!(Stopped::new(bad).check(&s).is_err(), "{bad:?}");
            assert!(Restart::new(bad).check(&s).is_err(), "{bad:?}");
            assert!(Reload::new(bad).check(&s).is_err(), "{bad:?}");
        }
        // `DaemonReload` is absent on purpose: it takes no unit name.
        assert!(fake.argvs().is_empty());
    }

    #[test]
    fn user_mode_adds_the_flag_everywhere_and_needs_no_root() {
        let fake = Arc::new(
            user_manager(Fake::new(), 1000)
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-enabled", "syncthing"]),
                    1,
                    "disabled\n",
                )
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-active", "syncthing"]),
                    3,
                    "inactive\n",
                )
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "enable", "--now", "syncthing"]),
                    0,
                    "",
                )
                .with_cmd(
                    "journalctl",
                    Some(&["--user", "-u", "syncthing", "--no-pager", "-n", "20"]),
                    0,
                    "user journal line\n",
                ),
        );
        let s = not_root(sys(&fake));
        let op = Enabled::new("syncthing").now(true).user(true);
        let plan = op.check(&s).unwrap();
        assert!(plan.is_change());
        // The fake still says inactive after `enable --now`: the failure path
        // shows the `--user` journal and the `--user` command in the message.
        let err = op.apply(&s, change(plan)).unwrap_err().to_string();
        assert!(
            err.contains("after `systemctl --user enable --now syncthing`"),
            "{err}"
        );
        assert!(err.contains("user journal line"), "{err}");
        assert_eq!(
            fake.argvs(),
            vec![
                argv(&["id", "-u"]),
                argv(&["systemctl", "--user", "is-enabled", "syncthing"]),
                argv(&["systemctl", "--user", "is-active", "syncthing"]),
                argv(&["systemctl", "--user", "enable", "--now", "syncthing"]),
                argv(&["systemctl", "--user", "is-enabled", "syncthing"]),
                argv(&["systemctl", "--user", "is-active", "syncthing"]),
                argv(&[
                    "journalctl",
                    "--user",
                    "-u",
                    "syncthing",
                    "--no-pager",
                    "-n",
                    "20"
                ]),
            ]
        );

        // Actions in user mode carry the flag in their summary too.
        let plan = Restart::new("syncthing").user(true).check(&s).unwrap();
        assert_eq!(
            change(plan).diff().render(),
            "systemctl --user restart syncthing"
        );
    }

    /// Issue #55. Under `ctx.as_user(..)` every `--user` command must reach
    /// the target account's manager: `id -u` runs as that account, and the
    /// uid it answers names the `XDG_RUNTIME_DIR` that `sudo` drops on the
    /// way there. A `Fake` has no helper process, so the switch shows as a
    /// `sudo -n -u` prefix on each argv.
    #[test]
    fn user_mode_under_as_user_points_every_command_at_that_accounts_manager() {
        let before = Arc::new(
            user_manager(Fake::new(), 1002)
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-enabled", "mine2026"]),
                    1,
                    "disabled\n",
                )
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-active", "mine2026"]),
                    3,
                    "inactive\n",
                ),
        );
        let op = Enabled::new("mine2026").user(true).now(true);
        let plan = op.check(&sys(&before).as_user("minecraft")).unwrap();
        let intent = change(plan);
        assert_eq!(
            intent.diff().render(),
            "mine2026:\n  enabled: disabled -> enabled\n  active: inactive -> active\n"
        );
        let sudo = |rest: &[&str]| {
            let mut v = argv(&["sudo", "-n", "-u", "minecraft"]);
            v.extend(argv(rest));
            v
        };
        assert_eq!(
            before.argvs(),
            vec![
                sudo(&["id", "-u"]),
                sudo(&["systemctl", "--user", "is-enabled", "mine2026"]),
                sudo(&["systemctl", "--user", "is-active", "mine2026"]),
            ]
        );
        let rt = Some("/run/user/1002".to_string());
        assert_eq!(runtime_dirs(&before), vec![None, rt.clone(), rt.clone()]);

        // `apply` executes the intent: the manager `check` found travels in
        // it, so `apply` neither asks `id -u` again nor loses the variable.
        // A second fake answers as the world after `enable --now`.
        let after = Arc::new(
            Fake::new()
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "enable", "--now", "mine2026"]),
                    0,
                    "",
                )
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-enabled", "mine2026"]),
                    0,
                    "enabled\n",
                )
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-active", "mine2026"]),
                    0,
                    "active\n",
                ),
        );
        let out = op.apply(&sys(&after).as_user("minecraft"), intent).unwrap();
        assert!(out.enabled && out.active);
        assert_eq!(
            after.argvs(),
            vec![
                sudo(&["systemctl", "--user", "enable", "--now", "mine2026"]),
                sudo(&["systemctl", "--user", "is-enabled", "mine2026"]),
                sudo(&["systemctl", "--user", "is-active", "mine2026"]),
            ]
        );
        assert_eq!(runtime_dirs(&after), vec![rt.clone(), rt.clone(), rt]);
    }

    /// The journal in a failure message is read from the same manager.
    #[test]
    fn user_mode_reads_the_journal_with_the_runtime_dir_too() {
        let fake = Arc::new(
            user_manager(Fake::new(), 1002)
                .with_cmd("systemctl", Some(&["--user", "start", "nginx"]), 0, "")
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-enabled", "nginx"]),
                    0,
                    "enabled\n",
                )
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-active", "nginx"]),
                    3,
                    "failed\n",
                )
                .with_cmd("journalctl", None, 0, "user journal line\n"),
        );
        let s = sys(&fake).as_user("minecraft");
        let op = Running::new("nginx").user(true);
        let err = op
            .apply(&s, change(op.check(&s).unwrap()))
            .unwrap_err()
            .chain();
        assert!(err.contains("user journal line"), "{err}");
        let journal = fake.commands().pop().unwrap();
        assert_eq!(journal.program, "journalctl");
        assert_eq!(
            journal.env.get("XDG_RUNTIME_DIR").map(String::as_str),
            Some("/run/user/1002")
        );
    }

    /// Without `/run/user/<uid>` the account has no user manager, and
    /// `systemctl --user` would only say `Failed to connect to bus`. Every op
    /// refuses first, naming the account and how to give it a manager, and
    /// runs nothing but `id -u`.
    #[test]
    fn user_mode_refuses_without_a_user_manager_and_says_how_to_start_one() {
        let fake = Arc::new(with_id(Fake::new(), 1002));
        let s = sys(&fake).as_user("minecraft");
        for (name, r) in all_ops_check(&s, true) {
            assert_eq!(
                r.unwrap_err().chain(),
                format!(
                    "systemd::{name}: no user manager for `minecraft`: /run/user/1002 does not \
                     exist, so `minecraft` has neither a login session nor linger. Enable linger \
                     as root (`loginctl enable-linger minecraft`) or log in as `minecraft` first"
                )
            );
        }
        assert!(
            fake.argvs().iter().all(|a| a[4..] == ["id", "-u"]),
            "{:?}",
            fake.argvs()
        );
    }

    /// Under `--check` an earlier step may be the one enabling linger, so a
    /// missing user manager is a prerequisite and not a refusal (vision 12):
    /// each op reports `would change` without asking a manager that is not
    /// there, and the diff names what it waits for.
    #[test]
    fn user_mode_without_a_user_manager_is_would_change_under_check() {
        let fake = Arc::new(with_id(Fake::new(), 1002));
        let s = sys(&fake).as_user("minecraft").with_check_mode(true);
        let waits = "waits for the user manager of `minecraft` \
                     (/run/user/1002/systemd/private does not exist yet)";
        let render = |plan: Result<Plan<Enabled>>| change(plan.unwrap()).diff().render();
        assert_eq!(
            render(Enabled::new("nginx").user(true).now(true).check(&s)),
            format!(
                "nginx:\n  enabled: not-found -> enabled\n  active: inactive -> active\n{waits}"
            )
        );
        assert_eq!(
            change(Disabled::new("nginx").user(true).check(&s).unwrap())
                .diff()
                .render(),
            format!("nginx:\n  enabled: not-found -> disabled\n{waits}")
        );
        assert_eq!(
            change(Running::new("nginx").user(true).check(&s).unwrap())
                .diff()
                .render(),
            format!("nginx:\n  active: inactive -> active\n{waits}")
        );
        assert_eq!(
            change(Stopped::new("nginx").user(true).check(&s).unwrap())
                .diff()
                .render(),
            format!("nginx:\n  active: not-found -> inactive\n{waits}")
        );
        assert_eq!(
            change(
                Restart::new("nginx")
                    .user(true)
                    .daemon_reload(true)
                    .check(&s)
                    .unwrap()
            )
            .diff()
            .render(),
            format!("systemctl --user daemon-reload && systemctl --user restart nginx; {waits}")
        );
        assert_eq!(
            change(DaemonReload::new().user(true).check(&s).unwrap())
                .diff()
                .render(),
            format!("systemctl --user daemon-reload; {waits}")
        );
        // Both shapes put what they wait for on the step line, in one line.
        assert_eq!(
            change(Running::new("nginx").user(true).check(&s).unwrap())
                .diff()
                .short(),
            format!("active=active {waits}")
        );
        assert_eq!(
            change(DaemonReload::new().user(true).check(&s).unwrap())
                .diff()
                .short(),
            format!("systemctl --user daemon-reload; {waits}")
        );
        assert!(
            fake.argvs().iter().all(|a| a[4..] == ["id", "-u"]),
            "{:?}",
            fake.argvs()
        );
    }

    /// A `daemon-reload` that `Restart` or `Reload` run first talks to the
    /// same user manager as the verb after it.
    #[test]
    fn user_mode_daemon_reload_before_a_bounce_carries_the_runtime_dir() {
        for (op, verb) in [("Restart", "restart"), ("Reload", "reload-or-restart")] {
            let fake = Arc::new(with_ok(
                with_ok(
                    user_manager(Fake::new(), 1002)
                        .with_cmd(
                            "systemctl",
                            Some(&["--user", "is-enabled", "nginx"]),
                            0,
                            "enabled\n",
                        )
                        .with_cmd(
                            "systemctl",
                            Some(&["--user", "is-active", "nginx"]),
                            0,
                            "active\n",
                        ),
                    &["--user", "daemon-reload"],
                ),
                &["--user", verb, "nginx"],
            ));
            let s = sys(&fake).as_user("minecraft");
            if op == "Restart" {
                let op = Restart::new("nginx").user(true).daemon_reload(true);
                op.apply(&s, change(op.check(&s).unwrap())).unwrap();
            } else {
                let op = Reload::new("nginx")
                    .user(true)
                    .or_restart(true)
                    .daemon_reload(true);
                op.apply(&s, change(op.check(&s).unwrap())).unwrap();
            }
            let ran: Vec<_> = fake.argvs().iter().map(|a| a[4..].to_vec()).collect();
            assert_eq!(
                ran,
                vec![
                    argv(&["id", "-u"]),
                    argv(&["systemctl", "--user", "daemon-reload"]),
                    argv(&["systemctl", "--user", verb, "nginx"]),
                    argv(&["systemctl", "--user", "is-enabled", "nginx"]),
                    argv(&["systemctl", "--user", "is-active", "nginx"]),
                ],
                "{op}"
            );
            let rt = Some("/run/user/1002".to_string());
            assert_eq!(
                runtime_dirs(&fake)[1..],
                [rt.clone(), rt.clone(), rt.clone(), rt],
                "{op}"
            );
        }
    }

    /// A refusal about the unit names whose user manager answered, so an
    /// operator stepping into another account checks that account's units.
    #[test]
    fn user_mode_refusals_about_the_unit_name_the_account() {
        let fake = Arc::new(
            user_manager(Fake::new(), 1002)
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-enabled", "nginx"]),
                    1,
                    "not-found\n",
                )
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-active", "nginx"]),
                    3,
                    "inactive\n",
                ),
        );
        let s = sys(&fake).as_user("minecraft");
        assert_eq!(
            Enabled::new("nginx")
                .user(true)
                .check(&s)
                .unwrap_err()
                .chain(),
            "systemd::Enabled: unit `nginx` not found by `systemctl --user is-enabled` in the \
             user manager of `minecraft`"
        );
        let fake = Arc::new(
            user_manager(Fake::new(), 1002)
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-enabled", "nginx"]),
                    1,
                    "masked\n",
                )
                .with_cmd(
                    "systemctl",
                    Some(&["--user", "is-active", "nginx"]),
                    3,
                    "inactive\n",
                ),
        );
        let s = sys(&fake).as_user("minecraft");
        assert_eq!(
            Enabled::new("nginx")
                .user(true)
                .check(&s)
                .unwrap_err()
                .chain(),
            "systemd::Enabled: unit `nginx` is masked in the user manager of `minecraft`; \
             unmask it first (`systemctl --user unmask nginx`)"
        );
    }

    /// As root without `as_user`, `.user(true)` means root's own user
    /// manager, which is rarely what was meant: the refusal says so rather
    /// than suggesting linger for root.
    #[test]
    fn user_mode_as_root_without_a_manager_points_at_as_user() {
        let fake = Arc::new(with_id(Fake::new(), 0));
        let err = Enabled::new("nginx")
            .user(true)
            .check(&sys(&fake))
            .unwrap_err()
            .chain();
        assert_eq!(
            err,
            "systemd::Enabled: no user manager for `root`: /run/user/0/systemd/private does \
             not exist. \
             `.user(true)` as root targets root's own user manager; to manage another \
             account's user units, step into it with `ctx.as_user(\"<account>\")`"
        );
    }

    // ---- waiting for a user manager that linger is starting (#60) ----

    const LINGER: &str = "/var/lib/systemd/linger/minecraft";
    const SOCKET: &str = "/run/user/1002/systemd/private";

    /// A box where linger is on for `minecraft` (uid 1002) and logind has
    /// made its runtime directory but the manager is not up yet: the state
    /// right after `loginctl enable-linger`. The probes answer once asked.
    fn linger_starting() -> Fake {
        with_id(Fake::new(), 1002)
            .with_file(LINGER, "")
            .with_dir("/run/user/1002")
            .with_cmd(
                "systemctl",
                Some(&["--user", "is-enabled", "nginx"]),
                1,
                "disabled\n",
            )
            .with_cmd(
                "systemctl",
                Some(&["--user", "is-active", "nginx"]),
                3,
                "inactive\n",
            )
    }

    /// A `Fake` that counts the stats of [`SOCKET`] and refuses the first
    /// `denied` of them with `EACCES`, as a stat as the account does between
    /// `user-runtime-dir` creating `/run/user/<uid>` (root, 0700) and mounting
    /// the account's tmpfs there. The `Fake` itself cannot fail a stat, so
    /// this wraps it through [`System::new`]; everything else passes through.
    struct Watched {
        fake: Arc<Fake>,
        denied: AtomicUsize,
        looks: AtomicUsize,
    }

    impl Watched {
        fn new(fake: &Arc<Fake>, denied: usize) -> Arc<Self> {
            Arc::new(Watched {
                fake: fake.clone(),
                denied: AtomicUsize::new(denied),
                looks: AtomicUsize::new(0),
            })
        }

        /// A `System` over this backend with `sys(..)`'s facts, as `minecraft`.
        fn sys(self: &Arc<Self>) -> System {
            let facts = sys(&self.fake).facts().clone();
            System::new(self.clone(), facts, false, Arc::new(Collect::default()))
                .as_user("minecraft")
        }

        /// How many times the socket was looked at.
        fn looks(&self) -> usize {
            self.looks.load(Ordering::SeqCst)
        }
    }

    impl Backend for Watched {
        fn read(&self, p: &Path) -> io::Result<Vec<u8>> {
            self.fake.read(p)
        }
        fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()> {
            self.fake.write(p, bytes)
        }
        fn write_from(
            &self,
            p: &Path,
            src: &mut dyn io::Read,
            attrs: Option<WriteAttrs>,
        ) -> io::Result<u64> {
            self.fake.write_from(p, src, attrs)
        }
        fn open_read(&self, p: &Path) -> io::Result<Box<dyn io::Read + Send + '_>> {
            self.fake.open_read(p)
        }
        fn stat(&self, p: &Path) -> io::Result<Option<Stat>> {
            if p == Path::new(SOCKET) {
                self.looks.fetch_add(1, Ordering::SeqCst);
                let denying = self
                    .denied
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok();
                if denying {
                    // The shape a stat refused inside the `as_user` helper
                    // arrives in: the errno's kind around its message.
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Permission denied (os error 13)",
                    ));
                }
            }
            self.fake.stat(p)
        }
        fn stat_follow(&self, p: &Path) -> io::Result<Option<Stat>> {
            self.fake.stat_follow(p)
        }
        fn mkdir_all(&self, p: &Path) -> io::Result<()> {
            self.fake.mkdir_all(p)
        }
        fn remove(&self, p: &Path) -> io::Result<()> {
            self.fake.remove(p)
        }
        fn remove_all(&self, p: &Path) -> io::Result<()> {
            self.fake.remove_all(p)
        }
        fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.fake.rename(from, to)
        }
        fn set_mode(&self, p: &Path, mode: u32) -> io::Result<()> {
            self.fake.set_mode(p, mode)
        }
        fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> io::Result<()> {
            self.fake.set_owner(p, uid, gid)
        }
        fn copy(&self, from: &Path, to: &Path) -> io::Result<()> {
            self.fake.copy(from, to)
        }
        fn symlink(&self, target: &Path, link: &Path) -> io::Result<()> {
            self.fake.symlink(target, link)
        }
        fn read_link(&self, p: &Path) -> io::Result<PathBuf> {
            self.fake.read_link(p)
        }
        fn read_dir(&self, p: &Path) -> io::Result<Vec<PathBuf>> {
            self.fake.read_dir(p)
        }
        fn spawn(&self, spec: &CmdSpec) -> io::Result<Output> {
            self.fake.spawn(spec)
        }
    }

    /// The manager's socket appears while `check` waits, written into the
    /// box from another thread as logind would; `check` then probes it.
    #[test]
    fn user_mode_waits_for_a_manager_that_linger_is_starting() {
        let fake = Arc::new(linger_starting());
        let appear = {
            let fake = fake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(250));
                rustible_sdk::backend::Backend::write(&*fake, Path::new(SOCKET), b"").unwrap();
            })
        };
        let t0 = Instant::now();
        let plan = Enabled::new("nginx")
            .user(true)
            .check(&sys(&fake).as_user("minecraft"))
            .unwrap();
        appear.join().unwrap();
        // It waited for the socket rather than finding it on the first look.
        assert!(
            t0.elapsed() >= Duration::from_millis(250),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(
            change(plan).diff().render(),
            "nginx:\n  enabled: disabled -> enabled\n"
        );
        let ran: Vec<_> = fake.argvs().iter().map(|a| a[4..].to_vec()).collect();
        assert_eq!(
            ran,
            vec![
                argv(&["id", "-u"]),
                argv(&["systemctl", "--user", "is-enabled", "nginx"]),
                argv(&["systemctl", "--user", "is-active", "nginx"]),
            ]
        );
    }

    /// A manager that never comes up is refused once `manager_timeout` has
    /// passed, naming the account, the uid, the socket, the wait and where
    /// to look; no `systemctl` ran.
    #[test]
    fn user_mode_refuses_a_manager_that_does_not_come_up_in_time() {
        let fake = Arc::new(linger_starting());
        let s = sys(&fake).as_user("minecraft");
        let t0 = Instant::now();
        let err = Enabled::new("nginx")
            .user(true)
            .manager_timeout(Duration::from_millis(300))
            .check(&s)
            .unwrap_err()
            .chain();
        assert!(
            t0.elapsed() >= Duration::from_millis(300),
            "{:?}",
            t0.elapsed()
        );
        assert_eq!(
            err,
            "systemd::Enabled: the user manager of `minecraft` (uid 1002) did not come up: \
             /run/user/1002/systemd/private was still missing after waiting 300ms although \
             linger is enabled (/var/lib/systemd/linger/minecraft). logind starts \
             user@1002.service for linger but does not restart it once it has stopped or \
             failed: check `systemctl status user@1002.service`, then start it as root with \
             `systemctl start user@1002.service`"
        );
        assert_eq!(fake.argvs().len(), 1, "{:?}", fake.argvs());
    }

    /// `Duration::ZERO` does not wait: one look at the socket, then the
    /// refusal. The count is what proves no poll ran; the time bound only
    /// proves the 30 s default did not apply, loose enough for a slow runner.
    #[test]
    fn user_mode_with_a_zero_manager_timeout_refuses_at_once() {
        let fake = Arc::new(linger_starting());
        let watched = Watched::new(&fake, 0);
        let t0 = Instant::now();
        let err = Restart::new("nginx")
            .user(true)
            .manager_timeout(Duration::ZERO)
            .check(&watched.sys())
            .unwrap_err()
            .chain();
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert_eq!(watched.looks(), 1);
        assert!(err.contains("still missing after waiting 0s"), "{err}");
    }

    /// `Duration::MAX` is past what an `Instant` can hold, so it means no
    /// deadline rather than a panic adding it to the clock: the step waits
    /// for the socket and then probes. With no deadline it still polls every
    /// [`MANAGER_POLL`]: about three looks in 200 ms, where a spin makes
    /// thousands.
    #[test]
    fn user_mode_with_a_max_manager_timeout_waits_without_a_deadline() {
        let fake = Arc::new(linger_starting());
        let watched = Watched::new(&fake, 0);
        let appear = {
            let fake = fake.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                rustible_sdk::backend::Backend::write(&*fake, Path::new(SOCKET), b"").unwrap();
            })
        };
        let plan = Running::new("nginx")
            .user(true)
            .manager_timeout(Duration::MAX)
            .check(&watched.sys())
            .unwrap();
        appear.join().unwrap();
        assert!(watched.looks() <= 5, "{}", watched.looks());
        assert_eq!(
            change(plan).diff().render(),
            "nginx:\n  active: inactive -> active\n"
        );
    }

    /// A stat refused with `EACCES` while `user-runtime-dir` has made
    /// `/run/user/<uid>` but not yet mounted the account's tmpfs on it is
    /// "not yet", in the first look and while waiting: the step waits
    /// through it instead of failing with "Permission denied".
    #[test]
    fn user_mode_waits_through_a_runtime_dir_not_yet_handed_to_the_account() {
        let fake = Arc::new(linger_starting().with_file(SOCKET, ""));
        let watched = Watched::new(&fake, 3);
        let plan = Enabled::new("nginx")
            .user(true)
            .check(&watched.sys())
            .unwrap();
        // The first look and two polls were denied; the third poll found it.
        assert_eq!(watched.looks(), 4);
        assert_eq!(
            change(plan).diff().render(),
            "nginx:\n  enabled: disabled -> enabled\n"
        );
    }

    /// A denial that never ends is still a manager that did not come up, and
    /// refuses as one once the timeout has passed.
    #[test]
    fn user_mode_refuses_a_runtime_dir_that_stays_denied_as_a_timeout() {
        let fake = Arc::new(linger_starting());
        let watched = Watched::new(&fake, usize::MAX);
        let err = Enabled::new("nginx")
            .user(true)
            .manager_timeout(Duration::from_millis(300))
            .check(&watched.sys())
            .unwrap_err()
            .chain();
        assert!(
            err.starts_with(
                "systemd::Enabled: the user manager of `minecraft` (uid 1002) did not come up"
            ),
            "{err}"
        );
        assert!(watched.looks() > 1, "{}", watched.looks());
    }

    /// Every op takes the knob, and it reaches the wait.
    #[test]
    fn every_op_takes_a_manager_timeout() {
        let fake = Arc::new(linger_starting());
        let s = sys(&fake).as_user("minecraft");
        let z = Duration::ZERO;
        for (name, r) in [
            (
                "Enabled",
                Enabled::new("nginx")
                    .user(true)
                    .manager_timeout(z)
                    .check(&s)
                    .map(|_| ()),
            ),
            (
                "Disabled",
                Disabled::new("nginx")
                    .user(true)
                    .manager_timeout(z)
                    .check(&s)
                    .map(|_| ()),
            ),
            (
                "Running",
                Running::new("nginx")
                    .user(true)
                    .manager_timeout(z)
                    .check(&s)
                    .map(|_| ()),
            ),
            (
                "Stopped",
                Stopped::new("nginx")
                    .user(true)
                    .manager_timeout(z)
                    .check(&s)
                    .map(|_| ()),
            ),
            (
                "Restart",
                Restart::new("nginx")
                    .user(true)
                    .manager_timeout(z)
                    .check(&s)
                    .map(|_| ()),
            ),
            (
                "Reload",
                Reload::new("nginx")
                    .user(true)
                    .manager_timeout(z)
                    .check(&s)
                    .map(|_| ()),
            ),
            (
                "DaemonReload",
                DaemonReload::new()
                    .user(true)
                    .manager_timeout(z)
                    .check(&s)
                    .map(|_| ()),
            ),
        ] {
            let err = r.unwrap_err().chain();
            assert!(
                err.starts_with(&format!("systemd::{name}: the user manager of `minecraft`"))
                    && err.contains("after waiting 0s"),
                "{name}: {err}"
            );
        }
    }

    /// Without linger nothing is starting a manager, so there is nothing to
    /// wait for: the default timeout would take 30 s if this waited.
    #[test]
    fn user_mode_does_not_wait_without_linger() {
        let fake = Arc::new(with_id(Fake::new(), 1002).with_dir("/run/user/1002"));
        let t0 = Instant::now();
        let err = Enabled::new("nginx")
            .user(true)
            .check(&sys(&fake).as_user("minecraft"))
            .unwrap_err()
            .chain();
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        // The runtime directory without the socket: a session whose manager
        // is not running, not a missing session.
        assert_eq!(
            err,
            "systemd::Enabled: the user manager of `minecraft` is not running: /run/user/1002 \
             exists but /run/user/1002/systemd/private does not, and `minecraft` has no linger \
             to start it. logind does not restart a user@1002.service that stopped or failed: \
             check `systemctl status user@1002.service`, then start it as root (`systemctl \
             start user@1002.service`), or give `minecraft` linger (`loginctl enable-linger \
             minecraft`) to keep one running"
        );
    }

    /// A dry run never sleeps, linger or not: it looks once and reports
    /// what it waits for. The bound only has to beat the 30 s default.
    #[test]
    fn user_mode_does_not_wait_under_check() {
        let fake = Arc::new(linger_starting());
        let watched = Watched::new(&fake, 0);
        let t0 = Instant::now();
        let plan = Enabled::new("nginx")
            .user(true)
            .check(&watched.sys().with_check_mode(true))
            .unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
        assert_eq!(watched.looks(), 1);
        assert!(
            change(plan)
                .diff()
                .render()
                .ends_with("(/run/user/1002/systemd/private does not exist yet)")
        );
        assert_eq!(fake.argvs().len(), 1, "{:?}", fake.argvs());
    }

    /// An `id -u` that does not answer with a uid is about the machine, not
    /// a step that has yet to run, so `--check` refuses it too.
    #[test]
    fn user_mode_refuses_an_unreadable_uid_in_both_modes() {
        let fake = Arc::new(Fake::new().with_cmd("id", Some(&["-u"]), 0, "nope\n"));
        for s in [sys(&fake), sys(&fake).with_check_mode(true)] {
            let err = Enabled::new("nginx")
                .user(true)
                .check(&s)
                .unwrap_err()
                .chain();
            assert_eq!(
                err,
                "systemd::Enabled: `id -u` as `root` printed \"nope\\n\", not a uid"
            );
        }
    }

    // ---- through Ctx: check mode and the mutation guard ----

    #[test]
    fn check_mode_through_ctx_runs_only_probes_and_yields_no_output() {
        let fake = Arc::new(probes("disabled", "inactive"));
        let s = sys(&fake).with_check_mode(true);
        let mut ctx = Ctx::new(s, HostInfo::local());

        let en = ctx
            .step("nginx enabled", Enabled::new("nginx").now(true))
            .unwrap();
        // A would-change step has no output in check mode (vision 12).
        assert!(en.changed && !en.is_available());
        let err = en.output().unwrap_err().to_string();
        assert!(err.contains("would have changed"), "{err}");

        let rs = ctx
            .step("nginx restarted", Restart::new("nginx").daemon_reload(true))
            .unwrap();
        assert!(rs.changed && !rs.is_available());
        assert_eq!(
            rs.diff.as_ref().unwrap().render(),
            "systemctl daemon-reload && systemctl restart nginx"
        );

        let st = ctx.step("nginx stopped", Stopped::new("nginx")).unwrap();
        assert!(!st.changed);
        assert_eq!(*st, state(false, false));

        // Two state ops probed, the action ran nothing, nothing mutated.
        assert_eq!(fake.argvs().len(), 4, "{:?}", fake.argvs());
        assert!(
            fake.argvs().iter().all(|a| a[1].starts_with("is-")),
            "{:?}",
            fake.argvs()
        );
    }

    #[test]
    fn restart_through_ctx_passes_the_mutation_guard_and_reports_changed() {
        // The step driver runs `check` under the guard that refuses file
        // mutations (vision 7.3); an op that touched a file in check would
        // fail here. The action's full apply path also runs end to end.
        let fake = Arc::new(with_ok(probes("enabled", "active"), &["restart", "nginx"]));
        let mut ctx = Ctx::new(sys(&fake), HostInfo::local());
        let r = ctx.step("nginx restarted", Restart::new("nginx")).unwrap();
        assert!(r.changed);
        assert_eq!(*r, state(true, true));
        assert_eq!(fake.argvs()[0], argv(&["systemctl", "restart", "nginx"]));

        let ok = ctx.step("nginx running", Running::new("nginx")).unwrap();
        assert!(!ok.changed);
        assert_eq!(ok.unit, "nginx");
    }
}
