//! The HTTP boundary. [`UserKeys`](crate::UserKeys) talks to the network
//! through [`Fetch`], a one-method trait, so everything above it (URL,
//! status handling, parsing, the `Op` contract) is testable with a canned
//! response and no socket. [`Https`] is the real implementation and the
//! default.
//!
//! Why not through `System`? `System` (vision 7) models the *target machine*:
//! files and processes, with a `Fake` for tests. An HTTP request is neither.
//! The network primitive is `rustible_std::http`: [`Https`] is a
//! [`Request::send`](rustible_std::http::Request::send), so this collection
//! shares its TLS, its redirect and secret policy and its size limits with
//! the standard ops, and a collection of your own builds on it the same way.
//! [`Fetch`] stays as the seam a test or a proxy plugs into.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rustible_sdk::prelude::*;
use rustible_std::http;

/// The most a response body may be. GitHub's `.keys` output for a user with
/// dozens of keys is a few kilobytes; a megabyte means something other than
/// keys came back.
pub const MAX_BODY_BYTES: u64 = 1024 * 1024;

/// The timeout, body included, when none is given.
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

/// The default [`Fetch`]: a [`rustible_std::http::Request`] `GET`, sent with
/// [`Request::send`](rustible_std::http::Request::send). That gives it
/// `rustls` with the `ring` provider from [`rustible_std::tls`] and the
/// bundled `webpki-roots` trust store (no system certificate lookup), and the
/// redirect policy of `rustible_std::http`: redirects followed, at most ten,
/// credentials dropped when one leaves the origin. On top, this client caps
/// the body at [`MAX_BODY_BYTES`], bounds the whole exchange, body included,
/// by its timeout, and sends a `rustible-github/<version>` user agent (GitHub
/// rejects requests without one). Any status is returned, not failed: the op
/// decides what a `404` means.
///
/// `ring` dispatches on the CPU at runtime, so there is no instruction-set
/// floor to check for and no pre-flight: a request works on any x86-64 or
/// aarch64 machine. See [`rustible_std::tls`] for what the provider costs at
/// build time.
#[derive(Debug, Clone)]
pub struct Https {
    timeout: Duration,
}

impl Https {
    /// A client with the default 20-second timeout.
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    /// A client whose requests take at most `timeout`, from connecting to
    /// the last byte of the body.
    pub fn with_timeout(timeout: Duration) -> Self {
        Https { timeout }
    }
}

impl Default for Https {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetch for Https {
    fn get(&self, url: &str) -> Result<Response> {
        let resp = http::Request::get(url)
            .header(
                "User-Agent",
                concat!("rustible-github/", env!("CARGO_PKG_VERSION")),
            )
            .timeout(self.timeout)
            .max_bytes(MAX_BODY_BYTES)
            // Every status is an answer for the op to judge, not a failure.
            .status(100..=599)
            .send()?;
        let status = resp.status;
        let body = String::from_utf8(resp.body).map_err(|e| {
            Error::msg(format!(
                "GET {}: the body is not UTF-8: {e}",
                http::mask_url(url)
            ))
        })?;
        Ok(Response { status, body })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    use super::*;

    /// A loopback server answering each path from a table, recording each
    /// request's head. Connections close after one response.
    fn serve(routes: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let heads = Arc::new(Mutex::new(Vec::new()));
        let record = heads.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
                record.lock().unwrap().push(head);
                let answer = routes
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, a)| a.clone())
                    .unwrap_or_else(|| {
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\nConnection: close\r\n\r\nNot Found"
                            .into()
                    });
                let _ = s.write_all(answer.as_bytes());
            }
        });
        (base, heads)
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// What `Https` promised before it moved onto `Request::send`, held at
    /// T1 against a loopback server: the user agent, redirects followed, any
    /// status returned rather than failed, and the body cap.
    #[test]
    fn https_keeps_its_behaviour_on_request_send() {
        let big = "k".repeat(MAX_BODY_BYTES as usize + 1);
        let (base, heads) = serve(vec![
            ("/flipbit03.keys", ok("ssh-ed25519 AAAA\n")),
            (
                "/moved.keys",
                "HTTP/1.1 301 Moved Permanently\r\nLocation: /flipbit03.keys\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            ),
            ("/big.keys", ok(&big)),
        ]);
        let https = Https::new();
        let r = https.get(&format!("{base}/flipbit03.keys")).unwrap();
        assert_eq!(r, Response::ok("ssh-ed25519 AAAA\n"));
        let head = heads.lock().unwrap()[0].to_ascii_lowercase();
        assert!(
            head.contains(&format!(
                "user-agent: rustible-github/{}",
                env!("CARGO_PKG_VERSION")
            )),
            "{head}"
        );

        let r = https.get(&format!("{base}/moved.keys")).unwrap();
        assert_eq!(r.status, 200, "the redirect was followed");

        let r = https.get(&format!("{base}/nobody.keys")).unwrap();
        assert_eq!(r, Response::with_status(404, "Not Found"));

        let e = https.get(&format!("{base}/big.keys")).unwrap_err().chain();
        assert!(
            e.contains(&format!("exceeds the {MAX_BODY_BYTES} byte limit")),
            "{e}"
        );
    }
}
