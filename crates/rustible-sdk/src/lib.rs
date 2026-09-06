//! Rustible SDK: everything a playbook or an operation crate needs.

pub mod backend;
pub mod ctx;
pub mod diff;
pub mod error;
pub mod event;
pub mod facts;
pub mod op;
pub mod runtime;
pub mod system;

pub use ctx::{Ctx, HostInfo};
pub use diff::{AttrChange, Diff};
pub use error::{Error, Result};
pub use facts::{Arch, Distro, Facts, Init, Os, Pm};
pub use op::{Applied, Change, Op, Plan};
pub use system::{Cmd, System};

pub mod prelude {
    pub use crate::bail;
    pub use crate::ctx::Ctx;
    pub use crate::diff::{AttrChange, Diff};
    pub use crate::error::{Error, Result};
    pub use crate::facts::{Arch, Distro, Facts, Init, Os, Pm};
    pub use crate::op::{Applied, Change, Op, Plan};
    pub use crate::system::System;
}
