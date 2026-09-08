//! The HTTP boundary. [`UserKeys`](crate::UserKeys) talks to the network
//! through [`Fetch`], a one-method trait, so everything above it (URL,
//! status handling, parsing, the `Op` contract) is testable with a canned
//! response and no socket. [`Https`] is the real implementation and the
//! default.
//!
//! Why not through `System`? `System` (vision 7) models the *target machine*:
//! files and processes, with a `Fake` for tests. An HTTP request is neither,
//! and the SDK deliberately has no network primitive. A collection that needs
//! one owns its own seam, which is what this module is.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rustible_sdk::prelude::*;

/// The most a response body may be. GitHub's `.keys` output for a user with
/// dozens of keys is a few kilobytes; a megabyte means something other than
/// keys came back.
pub const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// Connect and read timeout when none is given.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

/// An HTTP response reduced to what the ops need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// The HTTP status code (`200`, `404`, ...).
    pub status: u16,
    /// The body, decoded as UTF-8.
    pub body: String,
}

impl Response {
    /// A `200 OK` with this body. Handy in tests.
    pub fn ok(body: impl Into<String>) -> Self {
        Response {
            status: 200,
            body: body.into(),
        }
    }

    /// A response with this status and body.
    pub fn with_status(status: u16, body: impl Into<String>) -> Self {
        Response {
            status,
            body: body.into(),
        }
    }
}

/// How an op performs a `GET`. Implement it to route requests through a
/// proxy, to hit a GitHub Enterprise host, or (in tests) to answer from a
/// table. Non-2xx statuses are **not** errors here: the op decides what a
/// 404 means. `Err` is for transport failures only (DNS, TLS, timeout).
pub trait Fetch: Send + Sync + fmt::Debug {
    /// Perform `GET url` and return the status and the UTF-8 body.
    fn get(&self, url: &str) -> Result<Response>;
}

impl<F: Fetch + ?Sized> Fetch for Arc<F> {
    fn get(&self, url: &str) -> Result<Response> {
        (**self).get(url)
    }
}

/// The default [`Fetch`]: `ureq` over `rustls` with the pure-Rust
/// `rustls-rustcrypto` crypto provider and the bundled `webpki-roots` trust
/// store (vision 5.3: no C, no system certificate lookup). Follows redirects,
/// caps the body at [`MAX_BODY_BYTES`], and sends a `rustible-github/<version>`
/// user agent (GitHub rejects requests without one).
#[derive(Debug, Clone)]
pub struct Https {
    timeout: Duration,
}

impl Https {
    /// A client with the default 20-second connect and read timeout.
    pub fn new() -> Self {
        Https {
            timeout: DEFAULT_TIMEOUT,
        }
    }

    /// A client with this connect and read timeout.
    pub fn with_timeout(timeout: Duration) -> Self {
        Https { timeout }
    }

    fn agent(&self) -> ureq::Agent {
        let tls = ureq::tls::TlsConfig::builder()
            .unversioned_rustls_crypto_provider(Arc::new(rustls_rustcrypto::provider()))
            .build();
        ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            .timeout_connect(Some(self.timeout))
            .timeout_recv_response(Some(self.timeout))
            .timeout_recv_body(Some(self.timeout))
            .user_agent(concat!("rustible-github/", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    }
}

impl Default for Https {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetch for Https {
    fn get(&self, url: &str) -> Result<Response> {
        let mut resp = self
            .agent()
            .get(url)
            .call()
            .map_err(|e| Error::msg(format!("GET {url}: {e}")))?;
        let status = resp.status().as_u16();
        let body = resp
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_to_string()
            .map_err(|e| Error::msg(format!("GET {url}: reading the body: {e}")))?;
        Ok(Response { status, body })
    }
}
