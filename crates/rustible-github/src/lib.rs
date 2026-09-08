//! Rustible collection for GitHub: the first collection, and the shape every
//! third-party collection takes (vision 6.9 and 9).
//!
//! A collection is a plain crate on `rustible-sdk`. It implements
//! [`Op`](rustible_sdk::Op) for
//! its own types, reuses ops from `rustible-std` where it composes with them,
//! and is added to a playbook workspace with `cargo add`. Nothing in the core
//! knows this crate exists: playbooks link it directly, so there is no
//! registration, no plugin path, no module index to update. This crate ships
//! from the Rustible repository for convenience, but it touches nothing a
//! crate from another author could not.
//!
//! ## What is here
//!
//! - [`UserKeys`]: a read-only op (a lookup, vision 6.5) returning a GitHub
//!   user's public SSH keys from `https://github.com/<user>.keys` as typed
//!   [`PublicKey`]s. It can never report `changed`.
//! - [`keys_to_user`] and [`KeysToUser`]: the composition the Ansible
//!   `ssh_keys_from_github` role does with `lookup('url', ...)`, `set_fact`,
//!   `combine`, `product` and a loop over `ansible.posix.authorized_key`.
//!   Here it is one function running two steps through `ctx.step`.
//! - [`Fetch`] and [`Https`]: the HTTP boundary. `UserKeys` calls the
//!   network through a trait, so tests inject canned responses and a
//!   playbook behind a proxy or against GitHub Enterprise can supply its own.
//!
//! ## Ansible equivalent
//!
//! ```yaml
//! - name: Install cadu's GitHub keys
//!   ansible.posix.authorized_key:
//!     user: cadu
//!     key: https://github.com/flipbit03.keys
//! ```
//!
//! Ansible fetches the URL on the controller. **Rustible runs the playbook
//! binary on the target host** (vision 5), so the *target* needs HTTPS egress
//! to `github.com`. Air-gapped targets should fetch the keys elsewhere and
//! pass them to `ssh::authorized_keys::Present` directly.
//!
//! ## A playbook using it
//!
//! ```ignore
//! use rustible::prelude::*;
//! use rustible_github as github;
//! use rustible_std::{file, user};
//!
//! #[rustible::playbook(hosts = "all", escalate = true)]
//! fn main(ctx: &mut Ctx) -> Result<()> {
//!     let account = ctx.step("Ensure cadu exists", user::Present::new("cadu").create_home(true))?;
//!     ctx.step("Ensure ~/.ssh", file::Directory::at(account.home.join(".ssh")).owner(&account).mode(0o700))?;
//!
//!     // Two steps in the run output: the fetch (always `ok`) and the install.
//!     let installed = github::keys_to_user(ctx, "flipbit03", "cadu")?;
//!     if installed.changed {
//!         ctx.log(format!("installed {} GitHub key(s)", installed.added.len()));
//!     }
//!
//!     // Or take the keys and do something else with them.
//!     let keys = ctx.step("Fetch keys", github::UserKeys::of("flipbit03"))?;
//!     for k in keys.iter() {
//!         ctx.log(format!("{} {}...", k.key_type, &k.key[..16]));
//!     }
//!     Ok(())
//! }
//! ```
//!
//! ## Errors versus empty
//!
//! A GitHub login that does not exist (HTTP 404) **fails the step**, naming
//! the user: a typo must not silently install nothing. A user who exists and
//! has **no keys** is a successful step whose output is an empty `Vec`.
//! [`KeysToUser`] adds one guard on top: in exclusive mode an empty list is
//! refused rather than emptying `authorized_keys`.
//!
//! Pure Rust all the way down (vision 5.3): `ureq` over `rustls` with the
//! `rustls-graviola` provider, taken from `rustible_std::tls` so this crate
//! and `rustible_std::http` share one crypto path; no `ring`, no OpenSSL, no
//! C. That provider requires a CPU from roughly 2015 onwards, and
//! [`Https`] checks for it before every request rather than letting the
//! handshake panic; see `rustible_std::tls` for what is excluded.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod fetch;
mod keys_to_user;
mod login;
mod user_keys;

pub use fetch::{Fetch, Https, MAX_BODY_BYTES, Response};
pub use keys_to_user::{KeysToUser, keys_to_user};
pub use login::validate_login;
pub use user_keys::{KEYS_URL_BASE, UserKeys, parse_keys_body};

/// Re-exported from `rustible_std::ssh::authorized_keys`: the parsed public
/// key type [`UserKeys`] returns and `Present` consumes, so the two compose
/// with no conversion.
pub use rustible_std::ssh::authorized_keys::PublicKey;

/// Re-exported from `rustible_std::ssh::authorized_keys`: what
/// [`keys_to_user`] returns (`added`, `removed`, `already_present`, `path`).
pub use rustible_std::ssh::authorized_keys::KeysReport;
