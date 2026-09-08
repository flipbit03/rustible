//! Rustible: configuration management as real code.
//!
//! This is the facade crate a Rustible workspace depends on. It re-exports the
//! SDK, the playbook macros, and the standard operations so a playbook only
//! ever writes `use rustible::prelude::*;`.
//!
//! Status: placeholder release reserving the crate name. The design is at
//! <https://github.com/flipbit03/rustible/blob/main/docs/01_VISION.md>.

pub use rustible_macros::playbook;
pub use rustible_sdk as sdk;
pub use rustible_std as std_ops;

pub mod prelude {
    pub use rustible_macros::playbook;
    pub use rustible_sdk::prelude::*;
}
