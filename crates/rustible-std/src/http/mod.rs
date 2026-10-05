//! HTTP from the target: [`Download`] puts a URL's content in a file
//! (Ansible's `ansible.builtin.get_url`), and [`Request`] makes one request
//! and returns the response (Ansible's `ansible.builtin.uri`), for driving a
//! service's API from a playbook.
//!
//! Both go through one client: `ureq` over `rustls`, with `ring` as the
//! crypto provider, taken from [`crate::tls`] so every HTTPS request in
//! Rustible shares one crypto path. Certificates are checked against
//! Mozilla's bundled roots (`webpki-roots`); there is no `validate_certs:
//! no`. `ring` detects CPU features at runtime, so the binary runs on any
//! x86-64 or aarch64 target; it compiles a little C, which zig does on the
//! operator's machine (vision 5.3, M8). The target needs nothing.
//!
//! **Under `--check` nothing is sent**, by either op (vision 12): `check`
//! never opens a connection. `Download` decides from the file on disk;
//! `Request` plans the request and reports `would change`.
//!
//! # `Request`, typed
//!
//! A response is read into a struct naming the fields the playbook uses,
//! and a request body is written from one; [`json!`] and [`Value`] are here
//! for the untyped case, so a playbook needs no `serde_json` of its own.
//!
//! ```no_run
//! use rustible::prelude::*;
//! use rustible_std::http;
//! use serde::{Deserialize, Serialize};
//!
//! #[derive(Deserialize)]
//! struct Folder {
//!     #[serde(rename = "type")] // `type` is a Rust keyword
//!     kind: String,
//! }
//!
//! #[derive(Serialize)]
//! struct SetType {
//!     #[serde(rename = "type")]
//!     kind: &'static str,
//! }
//!
//! fn receive_only(ctx: &mut Ctx, url: &str, key: &Secret) -> Result<()> {
//!     ctx.block("folder is receive-only", |ctx| {
//!         let got = ctx.step("Read folder", http::Request::get(url).header_secret("X-API-Key", key))?;
//!         if got.json_as::<Folder>()?.kind != "receiveonly" {
//!             let set = SetType { kind: "receiveonly" };
//!             ctx.step("Set type", http::Request::patch(url).header_secret("X-API-Key", key).json(&set))?;
//!         }
//!         // Untyped, when a struct is not worth it:
//!         let raw = got.json()?;
//!         ctx.log(format!("label: {}", raw["label"]));
//!         let _ = http::json!({ "type": "receiveonly" });
//!         Ok(())
//!     })?;
//!     Ok(())
//! }
//! # fn main() {}
//! ```
//!
//! # From Ansible's `uri`
//!
//! | `uri` | `http::Request` |
//! |---|---|
//! | `url`, `method` | the constructor: `Request::get(url)`, `post`, `put`, `patch`, `delete`, `head`, `Request::method("OPTIONS", url)` |
//! | `headers` | `.header(name, value)`, `.header_secret(name, &secret)`, `.bearer(&token)` |
//! | `url_username` / `url_password` / `force_basic_auth` | `.basic_auth(user, &password)`, always sent with the first request |
//! | `body` with `body_format: json` / `form-urlencoded` / `raw` | `.json(&value)` / `.form([(k, v)])` / `.body(bytes)` |
//! | `status_code` | `.status([..])`; any `2xx` by default where Ansible accepts only `200` |
//! | `return_content` | always: [`Response`] holds the body |
//! | `timeout` | `.timeout(..)`, 30 seconds by default as in Ansible, here covering the body too |
//! | `follow_redirects` | `.follow_redirects(bool)`; `safe` (`GET` and `HEAD` only) by default, as in Ansible |
//! | `changed_when` | `.changed_when(\|resp\| ..)`; without it, a method other than `GET`, `HEAD` or `OPTIONS` is `changed` (Ansible says `changed: false` for every method) |
//! | `dest` | use [`Download`] |
//! | `src` | `.body(ctx.sys().read(path)?)` |
//! | `creates` / `removes` | a plain `if` |
//! | `validate_certs: no` | refused, as in [`Download`] |
//! | check mode | nothing is sent; the step reports `would change` (Ansible skips the task) |
//!
//! # Redirects and secrets
//!
//! Redirects are followed by Rustible's own loop, not `ureq`'s, because
//! `ureq` strips only `Authorization` and `Cookie` when it follows one, and
//! a key in `X-API-Key` would follow a redirect to any host. At most
//! [`MAX_REDIRECTS`] hops. On each:
//!
//! - `301` and `302` keep a `GET` or `HEAD` and turn any other method into a
//!   `GET` without its body; `303` is a `GET` without the body (a `HEAD`
//!   stays a `HEAD`); `307` and `308` keep the method and the body. Browsers
//!   and curl turn only a `POST` into a `GET` on `301`/`302` and keep a
//!   `PUT`, `PATCH` or `DELETE`; Rustible turns those into a `GET` too, so a
//!   mutating request is resent only when a `307`/`308` says to keep the
//!   method (and only with `.follow_redirects(true)`).
//! - When the scheme, host or port changes, every credential is dropped: a
//!   header given with `header_secret`, the `.bearer`/`.basic_auth`
//!   credentials, and `Authorization`, `Proxy-Authorization` and `Cookie`
//!   however they were given.
//! - A redirect from `https://` to `http://` is refused while any of those,
//!   a secret body or a `user:pass@` in the URL is attached, and so is a
//!   `307`/`308` that would resend a secret body to another origin. The
//!   error names both URLs, masked.
//!
//! A value given as a [`Secret`](rustible_sdk::prelude::Secret) shows as `<secret, N bytes>` in `Debug`,
//! diffs and messages, and so never reaches `--json` output; a URL's
//! userinfo is masked as `user:********@` everywhere a URL is printed
//! ([`mask_url`]).

mod client;
mod download;
mod request;
#[cfg(test)]
mod test_server;

pub use client::{MAX_REDIRECTS, mask_url};
pub use download::{
    Algorithm, Checksum, DEFAULT_MAX_BYTES, DEFAULT_TIMEOUT, Download, DownloadBuilder,
    DownloadIntent, DownloadReport, digest, parse_checksum,
};
pub use request::{REQUEST_MAX_BYTES, Request, RequestIntent, Response};
/// Re-exported from `serde_json`, so a playbook building or reading JSON
/// needs no dependency of its own and cannot end up on another version.
pub use serde_json::{Value, json};

/// Why a URL cannot be requested. Pure. Only `http://` and `https://`.
/// Plain `http://` is accepted, `localhost` included: a local admin API is
/// the common case. Messages mask the URL's userinfo.
pub fn validate_url(url: &str) -> std::result::Result<(), String> {
    let lower = url.to_ascii_lowercase();
    // Measure against the scheme that actually matched: using the longer
    // one for both refuses `http://x`, and a single-label host is real
    // (an `/etc/hosts` name, a container alias, a service on a LAN).
    let scheme = if lower.starts_with("https://") {
        Some("https://")
    } else if lower.starts_with("http://") {
        Some("http://")
    } else {
        None
    };
    let shown = mask_url(url);
    if let Some(scheme) = scheme {
        if url.len() <= scheme.len() || url.contains(char::is_whitespace) {
            return Err(format!("`{shown}` is not a valid http(s) URL"));
        }
        return Ok(());
    }
    if lower.starts_with("file:") {
        return Err(format!(
            "`{shown}`: file:// URLs are not supported; use file::Copy::from_local_path"
        ));
    }
    Err(format!("`{shown}` is not an http:// or https:// URL"))
}
