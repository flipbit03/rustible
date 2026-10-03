//! The center of the SDK: an `Op` describes a desired state and knows how to
//! check the system against it and reconcile.

use std::convert::Infallible;
use std::fmt;
use std::ops::Deref;
use std::time::Duration;

use crate::diff::Diff;
use crate::error::Result;
use crate::system::System;

/// What [`Op::check`] decided, and what [`Op::apply`] executes. The step's
/// [`Diff`] is rendered from it by `Ctx::step`, so what is reported is what
/// runs: one value, two readers.
///
/// Each op has its own intent type, holding typed fields: the state `check`
/// observed and the change it decided on. A file's text as `check` read it
/// and the text it decided to write belong here; so does whether to create a
/// directory, or which packages to install and which to upgrade. Prefer an
/// enum when `apply` has distinct branches, so `apply` is a `match` over what
/// `check` decided. Hold decisions, not payloads the op already has: `apply`
/// still receives `&self`, so a large input (a file's source bytes, a
/// download) stays on the op.
///
/// An intent never holds what `apply` will produce (vision doc 12): a uid not
/// yet allocated, a version the package manager has not resolved. A
/// would-change step has no output in check mode, and the intent is dropped
/// there unexecuted.
///
/// **An intent never contains a [`Diff`], and `apply` never reads one.** This
/// compiles, and it is forbidden:
///
/// ```ignore
/// #[derive(Debug)]
/// struct SysctlIntent(Diff);                                    // DO NOT
/// impl Intent for SysctlIntent { fn diff(&self) -> Diff { self.0.clone() } }
///
/// fn apply(&self, sys: &System, i: SysctlIntent) -> Result<SysctlReport> {
///     // ... and then decide what to do from the report's display strings
/// }
/// ```
///
/// It restores the seam this type exists to close: rewording a report would
/// change what runs. [`Diff`] is opaque outside the SDK for the same reason,
/// so such an `apply` has no variant to match and no field to read. The one
/// structural exception is a composite op, whose intent holds its children's
/// *intents*, never their diffs.
///
/// `Debug` is required because [`Plan`] is `Debug`, which every test calling
/// `.check(..).unwrap_err()` relies on. An intent holding secret material
/// writes a redacting `Debug` by hand.
///
/// Intents never leave the target process: there is no `Serialize` bound,
/// and nothing persists or sends one. The [`Diff`] is what crosses the wire.
pub trait Intent: fmt::Debug {
    /// This intent as the report shows it. Called by `Ctx::step`, never by
    /// `apply`.
    fn diff(&self) -> Diff;
}

/// For read-only ops, whose `check` never returns [`Plan::Change`]. Their
/// `apply` is `match intent {}`, and the compiler proves it unreachable.
impl Intent for Infallible {
    fn diff(&self) -> Diff {
        match *self {}
    }
}

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

    /// What `check` decided and `apply` executes; see [`Intent`]. A read-only
    /// op whose `check` never returns [`Plan::Change`] uses
    /// [`Infallible`].
    type Intent: Intent;

    /// Inspect the system. Never mutates. Returns what would need to happen,
    /// as the op's [`Intent`] when something does.
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
    fn check(&self, sys: &System) -> Result<Plan<Self>>;

    /// Execute the intent `check` produced. Only called when `check`
    /// returned [`Plan::Change`] and we are not in check mode.
    ///
    /// `apply` does not decide again and does not plan again from the system
    /// as it is now: the decision is in the intent, and the report the user
    /// saw was rendered from that same intent. Whatever the output needs
    /// beyond it (a gid to report, a digest), `apply` reads for itself; a
    /// read is not a decision. The race between `check` and `apply` is
    /// accepted, as in Ansible.
    ///
    /// The intent never wraps a [`Diff`], and `apply` never reads one: see
    /// [`Intent`] for the shape that compiles and is forbidden.
    fn apply(&self, sys: &System, intent: Self::Intent) -> Result<Self::Output>;

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
///
/// `Change` carries the op's [`Intent`] and nothing else: a would-change step
/// has no output in check mode (vision doc 12). A later step that reads it
/// gets [`OutputUnavailable`](crate::error::OutputUnavailable), which is the
/// honest answer for a value that only exists after `apply`.
pub enum Plan<O: Op + ?Sized> {
    /// Already in desired state. Carries the output so `step` returns it without applying.
    Satisfied(O::Output),
    /// Something must change: what `check` decided, for `apply` to execute
    /// and `ctx.step` to render.
    Change(O::Intent),
}

impl<O: Op + ?Sized> Plan<O> {
    /// True for [`Plan::Change`]. Mostly useful to op authors testing their
    /// own `check` without running a step.
    pub fn is_change(&self) -> bool {
        matches!(self, Plan::Change(_))
    }
}

// Written by hand: a derive would bound on `O: Debug`, which no op needs to
// be, rather than on what the variants hold.
impl<O: Op + ?Sized> fmt::Debug for Plan<O>
where
    O::Output: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Plan::Satisfied(out) => f.debug_tuple("Satisfied").field(out).finish(),
            Plan::Change(intent) => f.debug_tuple("Change").field(intent).finish(),
        }
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
    /// What the report showed, rendered from the op's intent; `None` when the
    /// step was already satisfied. Kept even when the step is reported `ok`,
    /// so `-v` still shows what ran. It is for rendering
    /// ([`Diff::render`], [`Diff::short`]): a `Diff` cannot be read field by
    /// field.
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
