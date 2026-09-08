//! The center of the SDK: an `Op` describes a desired state and knows how to
//! check the system against it and reconcile.

use std::ops::Deref;
use std::time::Duration;

use crate::diff::Diff;
use crate::error::Result;
use crate::system::System;

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
}

#[derive(Debug)]
pub enum Plan<T> {
    /// Already in desired state. Carries the output so `step` returns it without applying.
    Satisfied(T),
    /// Something must change.
    Change(Change<T>),
}

#[derive(Debug)]
pub struct Change<T> {
    /// What the report shows.
    pub diff: Diff,
    /// Optional: what the output would be after apply. Lets chained steps
    /// continue in check mode. Ops opt in when prediction is cheap and honest.
    pub predicted: Option<T>,
}

impl<T> Plan<T> {
    pub fn change(diff: Diff) -> Self {
        Plan::Change(Change {
            diff,
            predicted: None,
        })
    }

    pub fn change_predicting(diff: Diff, predicted: T) -> Self {
        Plan::Change(Change {
            diff,
            predicted: Some(predicted),
        })
    }

    pub fn is_change(&self) -> bool {
        matches!(self, Plan::Change(_))
    }
}

/// What `ctx.step` returns. Derefs to the op's output.
#[derive(Debug)]
pub struct Applied<T> {
    step: String,
    value: Option<T>,
    pub changed: bool,
    /// True if `value` is a prediction (check mode with an op that predicted).
    pub predicted: bool,
    pub diff: Option<Diff>,
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

    pub fn into_output(self) -> Result<T> {
        let step = self.step;
        self.value
            .ok_or_else(|| crate::error::OutputUnavailable { step }.into())
    }

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
