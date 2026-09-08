//! Proc macros for Rustible. Not used directly: the `rustible` facade
//! re-exports them.
//!
//! Status: placeholder release reserving the crate name. `#[playbook]` is
//! currently an identity attribute.

use proc_macro::TokenStream;

/// Marks a playbook's `main`. Placeholder: passes the item through unchanged.
#[proc_macro_attribute]
pub fn playbook(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
