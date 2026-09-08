//! What `main` receives.

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::event::{Event, Level, Status, Summary};
use crate::facts::Facts;
use crate::op::{Applied, Op, Plan};
use crate::system::{Phase, System};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostInfo {
    pub name: String,
    pub groups: Vec<String>,
    /// The inventory's privileged account for this host (default `root`);
    /// what `escalate = true` launches as and `as_escalated()` switches to.
    #[serde(default = "default_escalate_user")]
    pub escalate_user: String,
    /// `"ssh"` or `"local"`.
    #[serde(default = "default_connection")]
    pub connection: String,
}

fn default_escalate_user() -> String {
    "root".into()
}

fn default_connection() -> String {
    "local".into()
}

impl HostInfo {
    pub fn local() -> Self {
        HostInfo {
            name: "local".into(),
            groups: vec![],
            escalate_user: default_escalate_user(),
            connection: default_connection(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Shared {
    pub(crate) step_counter: Cell<u32>,
    pub(crate) summary: std::cell::RefCell<Summary>,
}

pub struct Ctx {
    sys: System,
    host: HostInfo,
    shared: Rc<Shared>,
    depth: u8,
}

impl Ctx {
    pub fn new(sys: System, host: HostInfo) -> Self {
        Ctx {
            sys,
            host,
            shared: Rc::new(Shared::default()),
            depth: 0,
        }
    }

    // ---- tier 1 ----

    /// The one verb. Reconcile an op, report it, return its typed output.
    pub fn step<O: Op>(&mut self, name: impl Into<String>, op: O) -> Result<Applied<O::Output>> {
        let name = name.into();
        let id = self.next_id();
        let identity = self.sys.identity().label();
        let sink = self.sys.sink().clone();
        sink.emit(Event::StepStarted {
            id,
            depth: self.depth,
            name: name.clone(),
            identity: identity.clone(),
        });
        let t0 = Instant::now();

        let finish = |status: Status, diff: Option<crate::Diff>, note: Option<String>| {
            sink.emit(Event::StepFinished {
                id,
                depth: self.depth,
                name: name.clone(),
                identity: identity.clone(),
                status,
                diff: diff.clone(),
                note,
                elapsed_ms: t0.elapsed().as_millis() as u64,
            });
        };

        self.sys.set_phase(Phase::Checking);
        let plan = op.check(&self.sys);
        self.sys.set_phase(Phase::Idle);

        let result = match plan {
            Err(e) => {
                finish(Status::Failed, None, Some(e.chain()));
                self.bump(|s| s.failed += 1);
                return Err(e.context(format!("step `{name}`")));
            }
            Ok(Plan::Satisfied(out)) => {
                finish(Status::Ok, None, None);
                self.bump(|s| s.ok += 1);
                Applied::new(name, Some(out), false, false, None, t0.elapsed())
            }
            Ok(Plan::Change(change)) if self.sys.check_mode() => {
                let note = if op.always_changes() {
                    Some("action".into())
                } else {
                    None
                };
                finish(Status::WouldChange, Some(change.diff.clone()), note);
                self.bump(|s| s.would_change += 1);
                let predicted = change.predicted.is_some();
                Applied::new(
                    name,
                    change.predicted,
                    true,
                    predicted,
                    Some(change.diff),
                    t0.elapsed(),
                )
            }
            Ok(Plan::Change(change)) => {
                let diff = change.diff.clone();
                self.sys.set_phase(Phase::Applying);
                let applied = op.apply(&self.sys, change);
                self.sys.set_phase(Phase::Idle);
                match applied {
                    Err(e) => {
                        finish(Status::Failed, Some(diff), Some(e.chain()));
                        self.bump(|s| s.failed += 1);
                        return Err(e.context(format!("step `{name}`")));
                    }
                    Ok(out) if !op.changed_by_apply(&out) => {
                        // The op ran and decided nothing changed (a command
                        // with `changed_when`). Reported `ok`; the diff stays
                        // so verbose output shows what ran.
                        finish(
                            Status::Ok,
                            Some(diff.clone()),
                            Some("ran, unchanged".into()),
                        );
                        self.bump(|s| s.ok += 1);
                        Applied::new(name, Some(out), false, false, Some(diff), t0.elapsed())
                    }
                    Ok(out) => {
                        let note = if op.always_changes() {
                            Some("action".into())
                        } else {
                            None
                        };
                        finish(Status::Changed, Some(diff.clone()), note);
                        self.bump(|s| s.changed += 1);
                        Applied::new(name, Some(out), true, false, Some(diff), t0.elapsed())
                    }
                }
            }
        };
        Ok(result)
    }

    pub fn host(&self) -> &HostInfo {
        &self.host
    }

    pub fn facts(&self) -> &Facts {
        self.sys.facts()
    }

    pub fn check_mode(&self) -> bool {
        self.sys.check_mode()
    }

    pub fn log(&self, msg: impl Into<String>) {
        self.sys.sink().emit(Event::Log {
            level: Level::Info,
            msg: msg.into(),
        });
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.bump(|s| s.warnings += 1);
        self.sys.warn(msg);
    }

    pub fn debug(&self, msg: impl Into<String>) {
        self.sys.debug(msg);
    }

    /// Direct access to the machine. Reads are fine; mutations should be steps.
    pub fn sys(&self) -> &System {
        &self.sys
    }

    /// Streamed from the orchestrator. In this local spike there is no
    /// orchestrator, so it resolves against the current directory.
    pub fn local_file(&mut self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let p = path.as_ref();
        if p.exists() {
            Ok(p.to_path_buf())
        } else {
            Err(Error::msg(format!("local file not found: {}", p.display())))
        }
    }

    // ---- tier 2 ----

    /// Record a step that was deliberately not run.
    pub fn skip(&mut self, name: impl Into<String>, reason: impl Into<String>) {
        let id = self.next_id();
        self.bump(|s| s.skipped += 1);
        self.sys.sink().emit(Event::StepSkipped {
            id,
            depth: self.depth,
            name: name.into(),
            reason: reason.into(),
        });
    }

    /// Group steps under a heading in the output. Output only.
    pub fn section<T>(
        &mut self,
        name: impl Into<String>,
        f: impl FnOnce(&mut Ctx) -> Result<T>,
    ) -> Result<T> {
        let name = name.into();
        let sink = self.sys.sink().clone();
        sink.emit(Event::SectionStarted {
            depth: self.depth,
            name: name.clone(),
        });
        let mut inner = Ctx {
            sys: self.sys.clone(),
            host: self.host.clone(),
            shared: self.shared.clone(),
            depth: self.depth + 1,
        };
        let r = f(&mut inner);
        sink.emit(Event::SectionFinished {
            depth: self.depth,
            name,
        });
        r
    }

    /// A `Ctx` whose ops run as another user. Same host, same counters.
    pub fn as_user(&self, name: &str) -> Ctx {
        Ctx {
            sys: self.sys.as_user(name),
            host: self.host.clone(),
            shared: self.shared.clone(),
            depth: self.depth,
        }
    }

    /// Literally root. Never follows the inventory (vision 11.3).
    pub fn as_root(&self) -> Ctx {
        self.as_user("root")
    }

    /// The inventory's privileged account for this host (`escalate_user`).
    pub fn as_escalated(&self) -> Ctx {
        let user = self.host.escalate_user.clone();
        self.as_user(&user)
    }

    // ---- internals ----

    fn next_id(&self) -> u32 {
        let id = self.shared.step_counter.get() + 1;
        self.shared.step_counter.set(id);
        id
    }

    fn bump(&self, f: impl FnOnce(&mut Summary)) {
        f(&mut self.shared.summary.borrow_mut());
    }

    pub(crate) fn summary(&self) -> Summary {
        self.shared.summary.borrow().clone()
    }
}
