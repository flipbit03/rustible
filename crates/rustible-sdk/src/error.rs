//! The error model (vision doc section 14).
//!
//! An opaque, `anyhow`-style error with a context chain. Any
//! `std::error::Error` converts into it with `?`, so op and playbook authors
//! never map errors by hand. A handful of SDK signals stay as concrete types
//! inside the chain so the orchestrator can render them specially.

use std::fmt;
use std::path::PathBuf;

/// The one error type. Wraps `anyhow::Error`; converts from anything.
pub struct Error(anyhow::Error);

/// The result type every SDK, op, and playbook signature uses. Fixing the
/// error side to [`Error`] is what makes `?` accept a foreign error without
/// a conversion written by hand.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// A plain message error. `bail!` uses this.
    pub fn msg(m: impl fmt::Display + fmt::Debug + Send + Sync + 'static) -> Self {
        Error(anyhow::Error::msg(m))
    }

    /// Wrap with a context layer: ``"installing nginx: <inner>"``.
    pub fn context(self, c: impl fmt::Display + fmt::Debug + Send + Sync + 'static) -> Self {
        Error(self.0.context(c))
    }

    /// The whole chain rendered on one line, outermost first.
    pub fn chain(&self) -> String {
        format!("{self:#}")
    }

    /// Look for a typed signal anywhere in the chain.
    pub fn downcast_ref<T: std::error::Error + 'static>(&self) -> Option<&T> {
        self.0.chain().find_map(|e| e.downcast_ref::<T>())
    }

    /// The failed command, if one is in the chain.
    pub fn cmd_failed(&self) -> Option<&CmdFailed> {
        self.downcast_ref::<CmdFailed>()
    }

    /// The step the failure happened in, if it came from
    /// [`Ctx::step`](crate::ctx::Ctx::step).
    ///
    /// `Ctx::step` adds a [`StepFailed`] layer to every error it returns, so
    /// a reporter can name the step instead of parsing it back out of
    /// [`Error::chain`]. Nested steps layer more than once and the outermost
    /// wins, which is the one whose text opens the rendered chain.
    ///
    /// Unlike [`Error::downcast_ref`] this looks at context layers as well
    /// as at the errors themselves, because a context layer is what this is.
    pub fn step_failed(&self) -> Option<&StepFailed> {
        self.0.downcast_ref::<StepFailed>()
    }
}

impl fmt::Display for Error {
    /// `{}` prints the outermost message, `{:#}` the whole chain (anyhow's
    /// own convention).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

/// Blanket conversion. Legal because `Error` itself does not implement
/// `std::error::Error` (the `anyhow` design; see vision 14).
impl<E> From<E> for Error
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn from(e: E) -> Self {
        Error(anyhow::Error::new(e))
    }
}

/// `.context(..)` and `.with_context(..)` on results and options.
pub trait Context<T> {
    /// Wrap the error in one more layer, e.g. `"installing nginx"`. The
    /// message is built whether or not there is an error, so keep it a
    /// constant or something cheap. On an `Option` there is no inner error
    /// to wrap and the message becomes the whole error.
    fn context<C: fmt::Display + fmt::Debug + Send + Sync + 'static>(self, c: C) -> Result<T>;
    /// [`Context::context`] with the message built only on the error path.
    /// Use this whenever the message needs a `format!`, which is most of the
    /// time, since a useful layer names the path or host it was working on.
    fn with_context<C: fmt::Display + fmt::Debug + Send + Sync + 'static, F: FnOnce() -> C>(
        self,
        f: F,
    ) -> Result<T>;
}

impl<T, E: Into<Error>> Context<T> for std::result::Result<T, E> {
    fn context<C: fmt::Display + fmt::Debug + Send + Sync + 'static>(self, c: C) -> Result<T> {
        self.map_err(|e| e.into().context(c))
    }
    fn with_context<C: fmt::Display + fmt::Debug + Send + Sync + 'static, F: FnOnce() -> C>(
        self,
        f: F,
    ) -> Result<T> {
        self.map_err(|e| e.into().context(f()))
    }
}

impl<T> Context<T> for Option<T> {
    fn context<C: fmt::Display + fmt::Debug + Send + Sync + 'static>(self, c: C) -> Result<T> {
        self.ok_or_else(|| Error::msg(c))
    }
    fn with_context<C: fmt::Display + fmt::Debug + Send + Sync + 'static, F: FnOnce() -> C>(
        self,
        f: F,
    ) -> Result<T> {
        self.ok_or_else(|| Error::msg(f()))
    }
}

// ---- typed signals that live inside the chain ----

/// An op mutated the system inside `check()`.
#[derive(Debug, thiserror::Error)]
#[error("op attempted to mutate `{path}` during check(); mutations belong in apply()")]
pub struct MutationDuringCheck {
    /// The path the write was aimed at. `System` raises this from its phase
    /// guard, which sees the path and nothing else, so the op and the call
    /// have to be identified from the step name in the surrounding chain.
    pub path: PathBuf,
}

/// A would-change step's output was read in check mode (vision 12).
#[derive(Debug, thiserror::Error)]
#[error("step `{step}` would have changed; its output is unavailable in check mode")]
pub struct OutputUnavailable {
    /// The name passed to `ctx.step`, so the message names the playbook line
    /// whose output was read rather than the one that would have produced it.
    pub step: String,
}

/// The step a failure happened in, as [`Ctx::step`](crate::ctx::Ctx::step)
/// attaches it.
///
/// It is a context layer, not a cause: it renders as the outermost part of
/// [`Error::chain`] exactly as the plain string it replaced did, and its
/// reason for being a type is [`Error::step_failed`], which lets the runtime
/// fill [`Event::Failed::step`](crate::event::Event::Failed) without parsing
/// the chain back apart.
#[derive(Debug, thiserror::Error)]
#[error("step `{step}`{suffix}")]
pub struct StepFailed {
    /// The name passed to `ctx.step`, verbatim: it is a label, so it may
    /// hold a backtick, a colon, or both, which is exactly what parsing the
    /// rendered chain gets wrong.
    pub step: String,
    /// What follows the name in the rendered chain. Empty when `check` or
    /// `apply` failed; `" not started"` or `" not applied"` when the run was
    /// cancelled on one side of `apply` and the step never ran.
    pub suffix: String,
}

impl StepFailed {
    /// The layer for a step whose `check` or `apply` returned an error.
    pub fn at(step: impl Into<String>) -> Self {
        StepFailed {
            step: step.into(),
            suffix: String::new(),
        }
    }

    /// The layer for a step a `Cancel` frame stopped, with `suffix` naming
    /// which side of `apply` the run was cancelled on.
    pub fn cancelled(step: impl Into<String>, suffix: &str) -> Self {
        StepFailed {
            step: step.into(),
            suffix: format!(" {suffix}"),
        }
    }
}

/// A command exited non-zero. The message names the command and status;
/// stderr travels in the struct and is rendered once, at `-v`, from the
/// `Failed` event rather than being repeated in every chain that quotes it.
#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[error("`{}` exited {status}", argv.join(" "))]
pub struct CmdFailed {
    /// Program first, then its arguments, exactly as spawned. The message
    /// joins them with spaces and does not quote, so an argument containing
    /// a space reads ambiguously there; the field itself is exact.
    pub argv: Vec<String>,
    /// The exit code, or `-1` when the process was killed by a signal and
    /// so has no code of its own.
    pub status: i32,
    /// Everything the command wrote to stderr, decoded lossily as UTF-8 and
    /// not truncated. Absent from the `Display` message on purpose.
    pub stderr: String,
}

/// An I/O primitive failed on a path.
#[derive(Debug, thiserror::Error)]
#[error("{path}: {source}")]
pub struct IoAt {
    /// The path as the caller gave it, never canonicalized: a failure on a
    /// relative path should still read the way the playbook wrote it.
    pub path: PathBuf,
    /// The underlying `std::io::Error`, kept as the `source` so its kind
    /// survives a `downcast_ref` further up the chain.
    #[source]
    pub source: std::io::Error,
}

/// A program could not be spawned at all (not found, not executable).
#[derive(Debug, thiserror::Error)]
#[error("could not spawn `{program}`: {source}")]
pub struct SpawnFailed {
    /// The program as named by the op, before any `PATH` lookup.
    pub program: String,
    /// Almost always `NotFound` or `PermissionDenied`. A program that did
    /// start and then exited non-zero produces a [`CmdFailed`], not this.
    #[source]
    pub source: std::io::Error,
}

/// Fail the current host with a formatted message. Ansible's `fail` module.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::Error::msg(format!($($arg)*)))
    };
}

/// Like `assert!` but returns an error instead of panicking.
#[macro_export]
macro_rules! ensure {
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            return Err($crate::Error::msg(format!($($arg)*)));
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foreign_errors_convert_with_question_mark() {
        fn f() -> Result<u32> {
            let n: u32 = "x".parse()?; // ParseIntError
            Ok(n)
        }
        let e = f().unwrap_err();
        assert!(e.chain().contains("invalid digit"), "{}", e.chain());
    }

    #[test]
    fn context_chains_outermost_first() {
        fn inner() -> Result<()> {
            Err(CmdFailed {
                argv: vec!["apt-get".into(), "install".into()],
                status: 100,
                stderr: "E: nope".into(),
            }
            .into())
        }
        let e = inner()
            .context("installing nginx")
            .unwrap_err()
            .context("web tier");
        assert_eq!(
            e.chain(),
            "web tier: installing nginx: `apt-get install` exited 100"
        );
        assert_eq!(
            e.to_string(),
            "web tier",
            "`{{}}` is the outermost layer only"
        );
        let cf = e.cmd_failed().expect("typed value survives the chain");
        assert_eq!((cf.status, cf.stderr.as_str()), (100, "E: nope"));
    }

    #[test]
    fn option_context() {
        let v: Option<u8> = None;
        let e = v.context("no value").unwrap_err();
        assert_eq!(e.chain(), "no value");
    }
}
