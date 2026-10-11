//! Rustible SDK: everything a playbook or an operation crate needs.
//!
//! The test API sits behind the `testing` feature: the container harness
//! (the `testing` module), the `Fake` backend, and the constructors that build a
//! `System` or a `Ctx` from parts. A crate whose tests use it enables the
//! feature in `[dev-dependencies]` only, so no playbook binary can reach it:
//!
//! ```toml
//! [dev-dependencies]
//! rustible-sdk = { version = "X.Y", features = ["testing"] }
//! ```

#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
// This crate implements the `Local` backend that vision 7.2's rule routes
// everything else through, so it is the one place real filesystem and
// process calls belong. The lint stays on for the op crates, which is where
// reaching past `sys` would make every `Fake` test a fiction.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod backend;
pub mod channel;
pub mod ctx;
pub mod diff;
pub mod error;
pub mod event;
#[cfg(test)]
mod executable;
pub mod facts;
#[cfg(test)]
mod freed;
pub mod launch;
pub mod mask;
pub mod op;
pub mod protocol;
pub mod registry;
pub mod runtime;
pub mod secret;
pub mod stream;
pub mod system;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod vars;

/// Re-exports the macros expand to. Not part of the public API.
#[doc(hidden)]
pub mod __private {
    pub use schemars;
    pub use serde;
    pub use serde_json;
}

pub use ctx::{Block, Ctx, HostInfo, InventoryLogin, LoginOverride};
pub use diff::{AttrChange, Diff};
pub use error::{
    CmdFailed, Context, Error, IoAt, MutationDuringCheck, OutputUnavailable, Result, SpawnFailed,
    StepFailed,
};
pub use facts::{Arch, Distro, Facts, Init, Os, Pm};
pub use op::{Applied, Intent, Op, Plan};
pub use secret::Secret;
pub use system::{Cmd, System};

/// What a playbook file imports. Everything needed to write steps and to
/// implement an [`Op`], and nothing from the runtime, protocol, backend or
/// channel modules, which a playbook never names.
pub mod prelude {
    pub use crate::ctx::{Block, Ctx};
    pub use crate::diff::{AttrChange, Diff};
    pub use crate::error::{Context, Error, Result};
    pub use crate::facts::{Arch, Distro, Facts, Init, Os, Pm};
    pub use crate::op::{Applied, Intent, Op, Plan};
    pub use crate::secret::Secret;
    pub use crate::system::System;
    pub use crate::{bail, ensure};
}
