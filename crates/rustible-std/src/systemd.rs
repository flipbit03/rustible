//! systemd units: enablement, running state, restart and reload. Ansible's
//! `ansible.builtin.systemd` (and `ansible.builtin.service` on systemd hosts).
//!
//! One type per desired state (vision 6.3): [`Enabled`], [`Disabled`],
//! [`Running`], [`Stopped`], each returning a [`UnitState`]. Three actions
//! (vision 6.4): [`Restart`] and [`Reload`], which also return a
//! [`UnitState`], and [`DaemonReload`], which names no unit and so returns
//! nothing. All of them go through `systemctl` via `sys.cmd`; `check` only
//! runs the read-only probes `is-enabled` and `is-active`, and the actions
//! run nothing at all in `check`.
//!
//! Every op refuses a host whose init is not systemd and, unless `.user(true)`
//! selects the caller's own `systemctl --user` manager, refuses to run without
//! root: reading unit state works unprivileged, but the purpose of each op is
//! the change, and `systemctl enable` as a plain user only produces a polkit
//! prompt the binary cannot answer.
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

use rustible_sdk::prelude::*;
use rustible_sdk::system::Cmd;

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
pub fn parse_is_enabled(stdout: &str, exit: i32) -> EnabledState {
    let word = stdout.lines().next().unwrap_or("").trim();
    match word {
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
        "" if exit != 0 => EnabledState::NotFound,
        other => EnabledState::Other(other.to_string()),
    }
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
        }
    }

    fn systemctl(&self, sys: &System) -> Cmd {
        let cmd = sys.cmd("systemctl");
        if self.user { cmd.arg("--user") } else { cmd }
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
        }
    }

    /// The preconditions every op shares: a sane name, systemd as init, and
    /// root unless the caller's own manager is the target.
    fn guard(&self, sys: &System, op: &str) -> Result<()> {
        validate_unit(&self.name)?;
        self.guard_manager(sys, op)
    }

    /// The half of [`Unit::guard`] that is about the host and the manager
    /// rather than the unit: systemd as init, and root unless `--user`.
    fn guard_manager(&self, sys: &System, op: &str) -> Result<()> {
        if sys.facts().init != Init::Systemd {
            bail!(
                "systemd::{op} needs systemd, but this host's init is {}",
                match &sys.facts().init {
                    Init::OpenRc => "OpenRC".to_string(),
                    Init::Other(name) => format!("`{name}`"),
                    Init::Systemd => unreachable!(),
                }
            );
        }
        if !self.user && !sys.is_root() {
            bail!(
                "systemd::{op} needs root to manage system units (this binary runs as `{}`); \
                 use `.user(true)` for the caller's own `systemctl --user` units",
                sys.facts().user
            );
        }
        Ok(())
    }

    fn probe(&self, sys: &System) -> Result<Probe> {
        let out = self
            .systemctl(sys)
            .args(["is-enabled", &self.name])
            .allow_failure()
            .run()?;
        let enabled = parse_is_enabled(&out.stdout_str(), out.status);
        let out = self
            .systemctl(sys)
            .args(["is-active", &self.name])
            .allow_failure()
            .run()?;
        let active = parse_is_active(&out.stdout_str(), out.status);
        Ok(Probe { enabled, active })
    }

    /// `probe` plus the not-found check every state op wants first.
    fn probe_existing(&self, sys: &System, op: &str) -> Result<Probe> {
        let p = self.probe(sys)?;
        if p.enabled == EnabledState::NotFound {
            bail!(
                "systemd::{op}: unit `{}` not found by `{} is-enabled`",
                self.name,
                self.prefix()
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
        let mut cmd = sys.cmd("journalctl");
        if self.user {
            cmd = cmd.arg("--user");
        }
        match cmd
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
    AttrChange {
        name: name.into(),
        from: from.into(),
        to: to.into(),
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
/// unmasking it (vision 6.7). Predicts its output, so check mode can chain.
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

    /// Manage the calling user's own units (`systemctl --user`). Needs no root.
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }
}

impl Op for Enabled {
    type Output = UnitState;

    fn check(&self, sys: &System) -> Result<Plan<UnitState>> {
        self.unit.guard(sys, "Enabled")?;
        let p = self.unit.probe_existing(sys, "Enabled")?;
        if p.enabled.is_masked() {
            bail!(
                "systemd::Enabled: unit `{}` is {}; unmask it first (`{} unmask {}`)",
                self.unit.name,
                p.enabled.as_str(),
                self.unit.prefix(),
                self.unit.name
            );
        }
        let mut changes = vec![];
        if !p.enabled.is_enabled() {
            changes.push(attr("enabled", p.enabled.as_str(), "enabled"));
        }
        if self.now && !p.active.is_running() {
            changes.push(attr("active", p.active.as_str(), "active"));
        }
        let out = UnitState {
            unit: self.unit.name.clone(),
            enabled: true,
            active: self.now || p.active.is_running(),
        };
        if changes.is_empty() {
            return Ok(Plan::Satisfied(out));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.unit.name.clone(),
                changes,
            },
            out,
        ))
    }

    fn apply(&self, sys: &System, _: Change<UnitState>) -> Result<UnitState> {
        let mut cmd = self.unit.systemctl(sys).arg("enable");
        if self.now {
            cmd = cmd.arg("--now");
        }
        cmd.arg(&self.unit.name).run()?;
        let after = format!(
            "{} enable{} {}",
            self.unit.prefix(),
            if self.now { " --now" } else { "" },
            self.unit.name
        );
        let state = if self.now {
            self.unit.verify_running(sys, &after)?
        } else {
            self.unit.state(&self.unit.probe(sys)?)
        };
        ensure!(
            state.enabled,
            "unit `{}` is still not enabled after `{after}`",
            self.unit.name
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
/// silently change nothing. Predicts its output.
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

    /// Manage the calling user's own units (`systemctl --user`). Needs no root.
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }
}

impl Op for Disabled {
    type Output = UnitState;

    fn check(&self, sys: &System) -> Result<Plan<UnitState>> {
        self.unit.guard(sys, "Disabled")?;
        let p = self.unit.probe_existing(sys, "Disabled")?;
        if p.enabled.cannot_be_disabled() {
            bail!(
                "systemd::Disabled: unit `{}` is {}: it has no [Install] section, so it cannot be \
                 disabled; mask it (`{} mask {}`) or stop it instead",
                self.unit.name,
                p.enabled.as_str(),
                self.unit.prefix(),
                self.unit.name
            );
        }
        let mut changes = vec![];
        if p.enabled.is_enabled() {
            changes.push(attr("enabled", p.enabled.as_str(), "disabled"));
        }
        if self.now && p.active.is_running() {
            changes.push(attr("active", p.active.as_str(), "inactive"));
        }
        let out = UnitState {
            unit: self.unit.name.clone(),
            enabled: false,
            active: !self.now && p.active.is_running(),
        };
        if changes.is_empty() {
            return Ok(Plan::Satisfied(out));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.unit.name.clone(),
                changes,
            },
            out,
        ))
    }

    fn apply(&self, sys: &System, _: Change<UnitState>) -> Result<UnitState> {
        let mut cmd = self.unit.systemctl(sys).arg("disable");
        if self.now {
            cmd = cmd.arg("--now");
        }
        cmd.arg(&self.unit.name).run()?;
        let after = format!(
            "{} disable{} {}",
            self.unit.prefix(),
            if self.now { " --now" } else { "" },
            self.unit.name
        );
        let state = if self.now {
            self.unit.verify_stopped(sys, &after)?
        } else {
            self.unit.state(&self.unit.probe(sys)?)
        };
        ensure!(
            !state.enabled,
            "unit `{}` is still enabled after `{after}`",
            self.unit.name
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
/// and a unit `systemctl` does not know. Predicts its output.
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

    /// Manage the calling user's own units (`systemctl --user`). Needs no root.
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }
}

impl Op for Running {
    type Output = UnitState;

    fn check(&self, sys: &System) -> Result<Plan<UnitState>> {
        self.unit.guard(sys, "Running")?;
        let p = self.unit.probe_existing(sys, "Running")?;
        if p.enabled.is_masked() {
            bail!(
                "systemd::Running: unit `{}` is {} and cannot be started; unmask it first",
                self.unit.name,
                p.enabled.as_str()
            );
        }
        if p.active.is_running() {
            return Ok(Plan::Satisfied(self.unit.state(&p)));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.unit.name.clone(),
                changes: vec![attr("active", p.active.as_str(), "active")],
            },
            UnitState {
                unit: self.unit.name.clone(),
                enabled: p.enabled.is_enabled(),
                active: true,
            },
        ))
    }

    fn apply(&self, sys: &System, _: Change<UnitState>) -> Result<UnitState> {
        self.unit
            .systemctl(sys)
            .args(["start", &self.unit.name])
            .run()?;
        let after = format!("{} start {}", self.unit.prefix(), self.unit.name);
        self.unit.verify_running(sys, &after)
    }
}

/// Ensure a unit is not running. `ansible.builtin.systemd` / `service` with
/// `state: stopped`.
///
/// Satisfied when `is-active` answers `inactive`, `failed`, `deactivating` or
/// `maintenance`. `apply` runs `systemctl stop` and re-reads `is-active`,
/// failing with the journal tail if the unit is still up. Predicts its output.
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

    /// Manage the calling user's own units (`systemctl --user`). Needs no root.
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }
}

impl Op for Stopped {
    type Output = UnitState;

    fn check(&self, sys: &System) -> Result<Plan<UnitState>> {
        self.unit.guard(sys, "Stopped")?;
        let p = self.unit.probe_existing(sys, "Stopped")?;
        if !p.active.is_running() {
            return Ok(Plan::Satisfied(self.unit.state(&p)));
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: self.unit.name.clone(),
                changes: vec![attr("active", p.active.as_str(), "inactive")],
            },
            UnitState {
                unit: self.unit.name.clone(),
                enabled: p.enabled.is_enabled(),
                active: false,
            },
        ))
    }

    fn apply(&self, sys: &System, _: Change<UnitState>) -> Result<UnitState> {
        self.unit
            .systemctl(sys)
            .args(["stop", &self.unit.name])
            .run()?;
        let after = format!("{} stop {}", self.unit.prefix(), self.unit.name);
        self.unit.verify_stopped(sys, &after)
    }
}

// ---------------------------------------------------------------------------
// Restart / Reload (actions)
// ---------------------------------------------------------------------------

/// Restart a unit. An action (vision 6.4): `ansible.builtin.systemd` /
/// `service` with `state: restarted`. `check` always reports a change and
/// runs nothing; `apply` runs an optional `systemctl daemon-reload`, then
/// `systemctl restart`, then verifies `is-active` and fails with the last 20
/// journal lines if the unit did not come back up. Predicts nothing: in check
/// mode its output is unavailable (vision 12).
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

    /// Manage the calling user's own units (`systemctl --user`). Needs no root.
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
        self
    }
}

fn action_summary(unit: &Unit, daemon_reload: bool, verb: &str) -> String {
    let p = unit.prefix();
    if daemon_reload {
        format!("{p} daemon-reload && {p} {verb} {}", unit.name)
    } else {
        format!("{p} {verb} {}", unit.name)
    }
}

fn run_action(sys: &System, unit: &Unit, daemon_reload: bool, verb: &str) -> Result<UnitState> {
    if daemon_reload {
        unit.systemctl(sys).arg("daemon-reload").run()?;
    }
    unit.systemctl(sys).args([verb, &unit.name]).run()?;
    let after = format!("{} {verb} {}", unit.prefix(), unit.name);
    unit.verify_running(sys, &after)
}

impl Op for Restart {
    type Output = UnitState;

    fn check(&self, sys: &System) -> Result<Plan<UnitState>> {
        self.unit.guard(sys, "Restart")?;
        Ok(Plan::change(Diff::summary(action_summary(
            &self.unit,
            self.daemon_reload,
            "restart",
        ))))
    }

    fn apply(&self, sys: &System, _: Change<UnitState>) -> Result<UnitState> {
        run_action(sys, &self.unit, self.daemon_reload, "restart")
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
/// verification as [`Restart`]; predicts nothing.
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

    /// Manage the calling user's own units (`systemctl --user`). Needs no root.
    pub fn user(mut self, on: bool) -> Self {
        self.unit.user = on;
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

    fn check(&self, sys: &System) -> Result<Plan<UnitState>> {
        self.unit.guard(sys, "Reload")?;
        Ok(Plan::change(Diff::summary(action_summary(
            &self.unit,
            self.daemon_reload,
            self.verb(),
        ))))
    }

    fn apply(&self, sys: &System, _: Change<UnitState>) -> Result<UnitState> {
        run_action(sys, &self.unit, self.daemon_reload, self.verb())
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
/// unless `.user(true)` selects the caller's own manager, like every other op
/// here.
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
    /// to the caller's own manager. There is no unit to name, so nothing here
    /// is validated and nothing is read back afterwards.
    pub fn new() -> Self {
        DaemonReload {
            manager: Unit::manager(),
        }
    }

    /// Reload the calling user's own manager (`systemctl --user
    /// daemon-reload`). Needs no root.
    pub fn user(mut self, on: bool) -> Self {
        self.manager.user = on;
        self
    }
}

impl Op for DaemonReload {
    type Output = ();

    fn check(&self, sys: &System) -> Result<Plan<()>> {
        self.manager.guard_manager(sys, "DaemonReload")?;
        Ok(Plan::change(Diff::summary(format!(
            "{} daemon-reload",
            self.manager.prefix()
        ))))
    }

    fn apply(&self, sys: &System, _: Change<()>) -> Result<()> {
        self.manager.systemctl(sys).arg("daemon-reload").run()?;
        Ok(())
    }

    fn always_changes(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
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
            assert_eq!(parse_is_enabled(&format!("{word}\n"), exit), want, "{word}");
            assert_eq!(want.as_str(), word);
        }
    }

    #[test]
    fn parse_is_enabled_nonzero_exit_with_meaningful_stdout() {
        // `disabled` exits 1, the word still wins.
        assert_eq!(parse_is_enabled("disabled\n", 1), EnabledState::Disabled);
        // Older systemd: nothing on stdout, error on stderr, exit 1.
        assert_eq!(parse_is_enabled("", 1), EnabledState::NotFound);
        // Empty stdout with exit 0 is not a known state.
        assert_eq!(parse_is_enabled("", 0), EnabledState::Other(String::new()));
        assert_eq!(
            parse_is_enabled("bogus\n", 0),
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

    // ---- fake helpers ----

    fn sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
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
        let enabled_status = if EnabledState::is_enabled(&parse_is_enabled(enabled, 0)) {
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

    fn attrs(plan: &Plan<UnitState>) -> Vec<(String, String, String)> {
        let Plan::Change(c) = plan else {
            panic!("expected change, got {plan:?}")
        };
        let Diff::Attrs { subject, changes } = &c.diff else {
            panic!("expected attrs diff, got {:?}", c.diff)
        };
        assert_eq!(subject, "nginx");
        changes
            .iter()
            .map(|a| (a.name.clone(), a.from.clone(), a.to.clone()))
            .collect()
    }

    fn predicted(plan: &Plan<UnitState>) -> UnitState {
        let Plan::Change(c) = plan else {
            panic!("expected change, got {plan:?}")
        };
        c.predicted.clone().expect("state ops predict")
    }

    fn change<T: std::fmt::Debug>(plan: Plan<T>) -> Change<T> {
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
    fn enabled_plans_change_with_diff_and_prediction() {
        let fake = Arc::new(probes("disabled", "inactive"));
        let plan = Enabled::new("nginx").check(&sys(&fake)).unwrap();
        assert_eq!(attrs(&plan), vec![triple("enabled", "disabled", "enabled")]);
        // Without `now`, `active` is whatever it is today.
        assert_eq!(predicted(&plan), state(true, false));
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
    }

    #[test]
    fn enabled_now_adds_active_change_and_the_flag() {
        let before = Arc::new(probes("enabled", "inactive"));
        let plan = Enabled::new("nginx")
            .now(true)
            .check(&sys(&before))
            .unwrap();
        assert_eq!(attrs(&plan), vec![triple("active", "inactive", "active")]);
        assert_eq!(predicted(&plan), state(true, true));

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
        assert_eq!(predicted(&plan), state(false, true));

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
        assert_eq!(predicted(&plan), state(false, false));

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
    fn running_plans_change_from_failed_with_prediction() {
        let fake = Arc::new(probes("enabled", "failed"));
        let plan = Running::new("nginx").check(&sys(&fake)).unwrap();
        assert_eq!(attrs(&plan), vec![triple("active", "failed", "active")]);
        assert_eq!(predicted(&plan), state(true, true));
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
        assert_eq!(predicted(&plan), state(true, false));

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
        assert_eq!(c.diff.render(), "systemctl restart nginx");
        assert!(c.predicted.is_none(), "actions predict nothing");
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());

        let with_reload = Restart::new("nginx")
            .daemon_reload(true)
            .check(&sys(&fake))
            .unwrap();
        assert_eq!(
            change(with_reload).diff.render(),
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
        assert_eq!(change(plan).diff.render(), "systemctl reload nginx");
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
            change(plan).diff.render(),
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
        assert_eq!(c.diff.render(), "systemctl daemon-reload");
        assert!(c.predicted.is_none(), "actions predict nothing");
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
        let fake = Arc::new(with_ok(Fake::new(), &["--user", "daemon-reload"]));
        let s = not_root(sys(&fake));
        let op = DaemonReload::new().user(true);
        let plan = op.check(&s).unwrap();
        assert_eq!(
            change(op.check(&s).unwrap()).diff.render(),
            "systemctl --user daemon-reload"
        );
        op.apply(&s, change(plan)).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![argv(&["systemctl", "--user", "daemon-reload"])]
        );
    }

    /// `Default` exists only so clippy's `new_without_default` is satisfied;
    /// it must not drift from `new`.
    #[test]
    fn daemon_reload_default_matches_new() {
        let fake = Arc::new(Fake::new());
        assert_eq!(
            change(DaemonReload::default().check(&sys(&fake)).unwrap())
                .diff
                .render(),
            change(DaemonReload::new().check(&sys(&fake)).unwrap())
                .diff
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
        assert!(r.changed && !r.predicted);
        assert!(!r.is_available(), "an action predicts nothing (vision 12)");
        assert_eq!(r.diff.as_ref().unwrap().render(), "systemctl daemon-reload");
        assert!(fake.argvs().is_empty(), "{:?}", fake.argvs());
    }

    // ---- shared guards ----

    fn all_ops_check(s: &System) -> Vec<(&'static str, Result<bool>)> {
        vec![
            (
                "Enabled",
                Enabled::new("nginx").check(s).map(|p| p.is_change()),
            ),
            (
                "Disabled",
                Disabled::new("nginx").check(s).map(|p| p.is_change()),
            ),
            (
                "Running",
                Running::new("nginx").check(s).map(|p| p.is_change()),
            ),
            (
                "Stopped",
                Stopped::new("nginx").check(s).map(|p| p.is_change()),
            ),
            (
                "Restart",
                Restart::new("nginx").check(s).map(|p| p.is_change()),
            ),
            (
                "Reload",
                Reload::new("nginx").check(s).map(|p| p.is_change()),
            ),
            (
                "DaemonReload",
                DaemonReload::new().check(s).map(|p| p.is_change()),
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
            for (name, r) in all_ops_check(&s) {
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
        for (name, r) in all_ops_check(&s) {
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
            Fake::new()
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
            change(plan).diff.render(),
            "systemctl --user restart syncthing"
        );
    }

    // ---- through Ctx: check mode and the mutation guard ----

    #[test]
    fn check_mode_through_ctx_runs_only_probes_and_predicts_state_ops() {
        let fake = Arc::new(probes("disabled", "inactive"));
        let s = sys(&fake).with_check_mode(true);
        let mut ctx = Ctx::new(s, HostInfo::local());

        let en = ctx
            .step("nginx enabled", Enabled::new("nginx").now(true))
            .unwrap();
        assert!(en.changed && en.predicted);
        assert_eq!(*en, state(true, true), "chained reads see the prediction");

        let rs = ctx
            .step("nginx restarted", Restart::new("nginx").daemon_reload(true))
            .unwrap();
        assert!(rs.changed && !rs.predicted);
        assert!(!rs.is_available(), "actions predict nothing (vision 12)");
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
        assert!(r.changed && !r.predicted);
        assert_eq!(*r, state(true, true));
        assert_eq!(fake.argvs()[0], argv(&["systemctl", "restart", "nginx"]));

        let ok = ctx.step("nginx running", Running::new("nginx")).unwrap();
        assert!(!ok.changed);
        assert_eq!(ok.unit, "nginx");
    }
}
