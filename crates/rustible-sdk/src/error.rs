use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Msg(String),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("command `{argv}` failed with exit status {status}\nstderr: {stderr}")]
    Cmd {
        argv: String,
        status: i32,
        stderr: String,
    },

    #[error("command `{program}` could not be spawned: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },

    #[error("op attempted to mutate `{path}` during check(); mutations belong in apply()")]
    MutationDuringCheck { path: PathBuf },

    #[error("step `{step}` would have changed; its output is unavailable in check mode")]
    OutputUnavailable { step: String },
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn msg(m: impl Into<String>) -> Self {
        Error::Msg(m.into())
    }
}

/// Fail the current host with a formatted message. Ansible's `fail` module.
#[macro_export]
macro_rules! bail {
    ($($arg:tt)*) => {
        return Err($crate::Error::msg(format!($($arg)*)))
    };
}
