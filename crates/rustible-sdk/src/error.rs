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

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// A plain message error. `bail!` uses this.
    pub fn msg(m: impl fmt::Display + fmt::Debug + Send + Sync + 'static) -> Self {
        Error(anyhow::Error::msg(m))
    }

    /// Wrap with a context layer: "installing nginx: <inner>".
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
    fn context<C: fmt::Display + fmt::Debug + Send + Sync + 'static>(self, c: C) -> Result<T>;
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
    pub path: PathBuf,
}

/// A would-change step's output was read in check mode (vision 12).
#[derive(Debug, thiserror::Error)]
#[error("step `{step}` would have changed; its output is unavailable in check mode")]
pub struct OutputUnavailable {
    pub step: String,
}

/// A command exited non-zero. The message names the command and status;
/// stderr travels in the struct and is rendered once, at `-v`, from the
/// `Failed` event rather than being repeated in every chain that quotes it.
#[derive(Debug, Clone, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[error("`{}` exited {status}", argv.join(" "))]
pub struct CmdFailed {
    pub argv: Vec<String>,
    pub status: i32,
    pub stderr: String,
}

/// An I/O primitive failed on a path.
#[derive(Debug, thiserror::Error)]
#[error("{path}: {source}")]
pub struct IoAt {
    pub path: PathBuf,
    #[source]
    pub source: std::io::Error,
}

/// A program could not be spawned at all (not found, not executable).
#[derive(Debug, thiserror::Error)]
#[error("could not spawn `{program}`: {source}")]
pub struct SpawnFailed {
    pub program: String,
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
