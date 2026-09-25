//! The center of the SDK: an `Op` describes a desired state and knows how to
//! check the system against it and reconcile.

use std::ops::Deref;
use std::time::Duration;

use crate::diff::Diff;
use crate::error::Result;
use crate::system::System;

/// One unit of work a playbook hands to `ctx.step`: a typed value whose
/// fields are the desired state, plus the two halves of reconciling it.
///
/// Splitting [`Op::check`] from [`Op::apply`] is what makes `--check` a real
/// dry run rather than a simulation: the runtime calls `check` and stops.
/// `check` must not touch the system, and `System` enforces that for the
/// writes it mediates by raising
/// [`MutationDuringCheck`](crate::error::MutationDuringCheck) while a step
/// is checking.
///
/// Idempotence is not a property an op declares; it falls out of `check`
/// returning [`Plan::Satisfied`] the second time round. An op that cannot
/// tell (a command, a restart) says so with [`Op::always_changes`].
pub trait Op {
    /// Typed result the playbook gets back.
    type Output;

    /// Inspect the system. Never mutates. Returns what would need to happen.
    ///
    /// A prerequisite that another op in the same run could create — a
    /// group, an account, its home, a parent directory, a unit — is refused
    /// here in a real run and tolerated under [`System::check_mode`], where
    /// the step reports `would change` and its diff shows the state it would
    /// set or says what it waits for. A real run's `check` runs with check
    /// mode off and takes the
    /// refusal, and a dry run's plan never reaches `apply`, so the refusal
    /// is never skipped on a run that can act (vision doc 6.7, 12).
    /// A refusal about the machine or the request itself — wrong platform,
    /// not root, a malformed input — stands in both modes.
    fn check(&self, sys: &System) -> Result<Plan<Self::Output>>;

    /// Perform the change described by the plan. Only called when `check`
    /// returned `Plan::Change` and we are not in check mode. Executes the
    /// diff `check` produced rather than deciding again; whatever the output
    /// needs that the diff does not carry, `apply` reads for itself.
    fn apply(&self, sys: &System, change: Change) -> Result<Self::Output>;

    /// True for actions whose `check` always returns `Change` (restart, command).
    /// Only a reporting hint.
    fn always_changes(&self) -> bool {
        false
    }

    /// Asked after `apply` ran: does the step count as `changed`? The default
    /// is yes, which is right for every desired-state op (`check` found a
    /// difference, `apply` removed it). An action that can only tell after
    /// running whether anything happened (Ansible's `changed_when` on
    /// `command`) overrides this; `false` reports the step `ok`, with the
    /// diff kept so `-v` still shows what ran. Never consulted in check mode,
    /// where such a step honestly reports `would change`.
    fn changed_by_apply(&self, output: &Self::Output) -> bool {
        let _ = output;
        true
    }
}

/// What [`Op::check`] concluded. `Satisfied` finishes the step as `ok`
/// without calling [`Op::apply`]; `Change` is the only route to `apply`, and
/// in check mode it finishes the step as `would change` instead.
#[derive(Debug)]
pub enum Plan<T> {
    /// Already in desired state. Carries the output so `step` returns it without applying.
    Satisfied(T),
    /// Something must change.
    Change(Change),
}

/// The payload of [`Plan::Change`]: the diff `check` produced, handed
/// straight to [`Op::apply`] so the work of deciding is not repeated. It
/// carries nothing else. A would-change step has no output in check mode
/// (vision doc 12), and `apply` reads for itself whatever its output needs
/// beyond the diff.
#[derive(Debug)]
pub struct Change {
    /// What the report shows, and what `apply` executes.
    pub diff: Diff,
}

impl<T> Plan<T> {
    /// A change. In check mode the step finishes `would change` and has no
    /// output: a later step that reads it gets
    /// [`OutputUnavailable`](crate::error::OutputUnavailable), which is the
    /// honest answer for a value that only exists after `apply`.
    pub fn change(diff: Diff) -> Self {
        Plan::Change(Change { diff })
    }

    /// True for [`Plan::Change`]. Mostly useful to op authors testing their
    /// own `check` without running a step.
    pub fn is_change(&self) -> bool {
        matches!(self, Plan::Change(_))
    }
}

/// What `ctx.step` returns. Derefs to the op's output.
#[derive(Debug)]
pub struct Applied<T> {
    step: String,
    value: Option<T>,
    /// Whether the system was, or in check mode would be, altered. False
    /// also for a step that ran and reported nothing changed through
    /// [`Op::changed_by_apply`].
    pub changed: bool,
    /// What `check` reported, `None` when the step was already satisfied.
    /// Kept even when the step is reported `ok`, so `-v` still shows what ran.
    pub diff: Option<Diff>,
    /// Wall time of the whole step, `check` and `apply` together, as measured
    /// by `ctx.step`.
    pub elapsed: Duration,
}

impl<T> Applied<T> {
    pub(crate) fn new(
        step: String,
        value: Option<T>,
        changed: bool,
        diff: Option<Diff>,
        elapsed: Duration,
    ) -> Self {
        Applied {
            step,
            value,
            changed,
            diff,
            elapsed,
        }
    }

    /// The output, or a clear error when there is none: the step would
    /// change and this is check mode, so `apply` never produced one.
    pub fn output(&self) -> Result<&T> {
        self.value.as_ref().ok_or_else(|| {
            crate::error::OutputUnavailable {
                step: self.step.clone(),
            }
            .into()
        })
    }

    /// [`Applied::output`] by value, for handing the result to the next step
    /// instead of borrowing it.
    pub fn into_output(self) -> Result<T> {
        let step = self.step;
        self.value
            .ok_or_else(|| crate::error::OutputUnavailable { step }.into())
    }

    /// Whether an output is there to read: false exactly when
    /// [`Applied::output`] would fail and `Deref` would panic. A playbook
    /// that wants to keep going in check mode branches on this.
    pub fn is_available(&self) -> bool {
        self.value.is_some()
    }
}

impl<T> Deref for Applied<T> {
    type Target = T;

    /// Panics with the same message as `output()` when the value is
    /// unavailable. The runtime turns the panic into a failed step.
    fn deref(&self) -> &T {
        match &self.value {
            Some(v) => v,
            None => panic!(
                "step `{}` would have changed; its output is unavailable in check mode",
                self.step
            ),
        }
    }
}
