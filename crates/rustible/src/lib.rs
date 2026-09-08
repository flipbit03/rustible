//! Rustible: configuration management as real code.
//!
//! This is the facade crate a Rustible workspace depends on. It re-exports the
//! SDK, the playbook macros, and the standard operations so a playbook only
//! ever writes `use rustible::prelude::*;` and `use rustible_std::{..}`.
//!
//! Design: <https://github.com/flipbit03/rustible/blob/main/docs/01_VISION.md>.

/// The SDK: `Op`, `System`, `Ctx`, facts, protocol, registry, runtime.
pub use rustible_sdk as sdk;

/// The standard operations, also available as the `rustible_std` crate.
pub use rustible_std as std_ops;

pub use rustible_macros::{integration_test, playbook, vars};
pub use rustible_sdk::{registry, runtime};

pub mod prelude {
    pub use rustible_macros::{playbook, vars};
    pub use rustible_sdk::prelude::*;
}
