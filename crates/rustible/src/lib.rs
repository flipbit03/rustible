//! Rustible: configuration management as real code.
//!
//! This is the facade crate a Rustible workspace depends on. It re-exports the
//! SDK, the playbook macros, and the standard operations so a playbook only
//! ever writes `use rustible::prelude::*;` and `use rustible_std::{..}`.
//!
//! Design: <https://github.com/flipbit03/rustible/blob/main/docs/01_VISION.md>.

#![deny(missing_docs)]

/// The SDK: `Op`, `System`, `Ctx`, facts, protocol, registry, runtime.
pub use rustible_sdk as sdk;

/// The standard operations, also available as the `rustible_std` crate.
pub use rustible_std as std_ops;

pub use rustible_macros::{integration_test, playbook, vars};
pub use rustible_sdk::{registry, runtime};

/// What every playbook file starts with: `use rustible::prelude::*;`.
///
/// Brings in [`Ctx`](sdk::ctx::Ctx), the [`Op`](sdk::op::Op) machinery,
/// [`Result`](sdk::error::Result) and its `Context` extension, [`Facts`] and
/// the enums a distro branch matches on, [`Diff`], [`Secret`], [`System`],
/// and the `bail!` and `ensure!` macros, plus the `#[playbook]` and
/// `#[vars]` attributes themselves. Ops come from `rustible_std` and are
/// deliberately not here: a playbook names the modules it uses, so
/// `file::Copy` and `systemd::Enabled` stay distinguishable at the call site.
///
/// [`Facts`]: sdk::facts::Facts
/// [`Diff`]: sdk::diff::Diff
/// [`Secret`]: sdk::secret::Secret
/// [`System`]: sdk::system::System
pub mod prelude {
    pub use rustible_macros::{playbook, vars};
    pub use rustible_sdk::prelude::*;
}
