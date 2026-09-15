//! Library half of the `rustible` command. The inventory lives here so it is
//! unit-testable; the binary in `main.rs` adds the CLI on top.

#![deny(missing_docs)]
// The CLI manages workspaces, caches and playbook builds on the operator's
// own machine, which is not a managed host and has no `sys` to go through.
// Vision 7.2's rule is about operations; see clippy.toml.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod inventory;
