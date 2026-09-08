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
    fn check(&self, sys: &System) -> Result<Plan<Self::Output>>;

    /// Perform the change described by the plan. Only called when `check`
    /// returned `Plan::Change` and we are not in check mode.
    fn apply(&self, sys: &System, change: Change<Self::Output>) -> Result<Self::Output>;

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
    Change(Change<T>),
}

/// The payload of [`Plan::Change`]: what `check` found, handed straight to
/// [`Op::apply`] so the work of deciding is not repeated. Ops that predict
/// commonly return `predicted` as `apply`'s own output.
#[derive(Debug)]
pub struct Change<T> {
    /// What the report shows.
    pub diff: Diff,
    /// Optional: what the output would be after apply. Lets chained steps
    /// continue in check mode. Ops opt in when prediction is cheap and honest.
    pub predicted: Option<T>,
}

impl<T> Plan<T> {
    /// A change with no predicted output. A later step that reads this
    /// step's output gets
    /// [`OutputUnavailable`](crate::error::OutputUnavailable) in check mode,
    /// which is the honest answer when the value only exists after `apply`.
    pub fn change(diff: Diff) -> Self {
        Plan::Change(Change {
            diff,
            predicted: None,
        })
    }

    /// A change carrying the output `apply` would produce, so a check-mode
    /// run can keep walking through steps that read it. The value is
    /// returned to the playbook as if it were real, flagged by
    /// [`Applied::predicted`], so predict only what the op is certain of.
    pub fn change_predicting(diff: Diff, predicted: T) -> Self {
        Plan::Change(Change {
            diff,
            predicted: Some(predicted),
        })
    }

    /// True for [`Plan::Change`], predicting or not. Mostly useful to op
    /// authors testing their own `check` without running a step.
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
    /// True if `value` is a prediction (check mode with an op that predicted).
    pub predicted: bool,
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
        predicted: bool,
        diff: Option<Diff>,
        elapsed: Duration,
    ) -> Self {
        Applied {
            step,
            value,
            changed,
            predicted,
            diff,
            elapsed,
        }
    }

    /// The output, or a clear error if unavailable (check mode, would change,
    /// op did not predict).
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
