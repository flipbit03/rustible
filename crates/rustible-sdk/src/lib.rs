//! Rustible SDK: everything a playbook or an operation crate needs.

pub mod backend;
pub mod ctx;
pub mod diff;
pub mod error;
pub mod event;
pub mod facts;
pub mod op;
pub mod protocol;
pub mod registry;
pub mod runtime;
pub mod system;
pub mod vars;

/// Re-exports the macros expand to. Not part of the public API.
#[doc(hidden)]
pub mod __private {
    pub use schemars;
    pub use serde;
    pub use serde_json;
}

pub use ctx::{Ctx, HostInfo};
pub use diff::{AttrChange, Diff};
pub use error::{
    CmdFailed, Context, Error, IoAt, MutationDuringCheck, OutputUnavailable, Result, SpawnFailed,
};
pub use facts::{Arch, Distro, Facts, Init, Os, Pm};
pub use op::{Applied, Change, Op, Plan};
pub use system::{Cmd, System};

pub mod prelude {
    pub use crate::ctx::Ctx;
    pub use crate::diff::{AttrChange, Diff};
    pub use crate::error::{Context, Error, Result};
    pub use crate::facts::{Arch, Distro, Facts, Init, Os, Pm};
    pub use crate::op::{Applied, Change, Op, Plan};
    pub use crate::system::System;
    pub use crate::{bail, ensure};
}
