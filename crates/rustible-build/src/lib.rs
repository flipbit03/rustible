//! Build-script helper for Rustible workspaces. The generated `build.rs` of a
//! workspace calls into this crate to discover playbook files and write the
//! registry that `src/main.rs` includes.
//!
//! Status: placeholder release reserving the crate name.

/// Placeholder entry point. The real implementation scans `playbooks/**/*.rs`
/// for `#[rustible::playbook]` and writes `$OUT_DIR/playbooks.rs`.
pub fn discover() {}
