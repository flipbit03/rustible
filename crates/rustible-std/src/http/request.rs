//! [`Request`]: one HTTP request, as a step or from an operation's own
//! code. Ansible's `ansible.builtin.uri`.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use rustible_sdk::prelude::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use ureq::http::Method;

use super::client::{
    self, Field, HeaderSpec, Outgoing, ReadError, Timeout, base64, mask_url, secret_note,
};
use super::{DEFAULT_TIMEOUT, validate_url};

/// The default ceiling on a [`Request`]'s response body, raised or lowered
/// with [`Request::max_bytes`]. The body is held in memory.
pub const REQUEST_MAX_BYTES: u64 = 16 << 20;

/// How much of an unexpected response's body a status failure quotes.
const QUOTED_BODY_BYTES: usize = 512;

type Predicate = Arc<dyn Fn(&Response) -> bool + Send + Sync>;

/// One HTTP request. Ansible's `ansible.builtin.uri`; the
/// [module docs](super) have the full mapping.
///
/// ```no_run
/// use rustible::prelude::*;
/// use rustible_std::http::{self, json};
/// use serde::Deserialize;
///
/// #[derive(Deserialize)]
/// struct Folder {
///     id: String,
///     #[serde(rename = "type")]
///     kind: String,
/// }
///
/// fn folder(ctx: &mut Ctx, key: &Secret) -> Result<()> {
///     let url = "http://127.0.0.1:8384/rest/config/folders/dcim";
///     // In a block: under --check the GET has no output, and reading it
///     // ends this block rather than the host's dry run.
///     ctx.block("DCIM is receive-only", |ctx| {
///         let got = ctx.step("Read folder", http::Request::get(url).header_secret("X-API-Key", key))?;
///         let folder: Folder = got.json_as()?;
///         if folder.kind != "receiveonly" {
///             ctx.step(
///                 format!("Make {} receive-only", folder.id),
///                 http::Request::patch(url)
///                     .header_secret("X-API-Key", key)
///                     .json(&json!({ "type": "receiveonly" })),
///             )?;
///         }
///         Ok(())
///     })?;
///     Ok(())
/// }
/// # fn main() {}
/// ```
///
/// **Under `--check` nothing is sent**, whatever the method (vision 12): a
/// dry run touches nothing outside the target, and even a `GET` can be
/// logged, rate-limited or billed. `check` contacts nothing in either mode;
/// it validates the request and plans it, so a dry run reports the step
/// `would change` with no output, its diff one line: the method, the URL
/// and, for a body, its size and type (`PATCH <url> (22 bytes,
/// application/json)`); never the body's content. A later read
/// of the response ends the enclosing `ctx.block` with a warning, so put
/// the code that reads it in a block.
///
/// **In a real run** `apply` sends it. A `GET`, `HEAD` or `OPTIONS` reports
/// `ok` (with the note `ran, unchanged`): reading changes nothing. Any other
/// method reports `changed`, and is marked as an action, because Rustible
/// cannot know what a `POST` did; [`Request::changed_when`] decides from the
/// response instead, either way. This is a departure from `uri`, which
/// reports `changed: false` for every method.
///
/// **Status.** Any `2xx` is success by default; [`Request::status`] lists
/// the codes to accept instead (Ansible's `status_code`, whose default is
/// `[200]` alone). Anything else fails the step with
/// `<METHOD> <url> returned <code> <reason>, expected <codes>: <the first 512
/// bytes of the body>`.
///
/// **Secrets.** A header given with [`Request::header_secret`], the
/// credentials of [`Request::bearer`] and [`Request::basic_auth`], and a
/// body given with [`Request::body_secret`] show as `<secret, N bytes>` in
/// `Debug`, and are scrubbed from every message, including a server's
/// answer quoted in a status failure, so they never reach `--json` output.
/// A URL's `user:pass@` is masked everywhere too. A body given with `.json`,
/// `.form` or `.body` is not a secret: to send a password in a JSON body,
/// use `.body_secret(&s).content_type("application/json")`.
///
/// **Redirects** are followed for `GET` and `HEAD` and not for anything
/// else (Ansible's `safe`); [`Request::follow_redirects`] changes that. At
/// most ten hops; credentials are dropped when one leaves the scheme, host
/// and port; and a redirect from `https://` to `http://` with a credential
/// attached is refused. The [module docs](super) have the method rules.
///
/// **Limits.** [`Request::timeout`] bounds the whole exchange, redirects
/// and body included, 30 seconds by default as in Ansible. The response
/// body is held in memory, up to [`REQUEST_MAX_BYTES`] unless
/// [`Request::max_bytes`] says otherwise. `validate_certs: no` has no
/// equivalent: certificates are always checked against Mozilla's roots.
#[derive(Clone)]
pub struct Request {
    method: String,
    url: String,
    headers: Vec<HeaderSpec>,
    auth: Option<Auth>,
    body: Body,
    content_type: Option<String>,
    status: Option<Vec<u16>>,
    timeout: Duration,
    max_bytes: u64,
    follow: Option<bool>,
    changed_when: Option<Predicate>,
}

#[derive(Clone)]
enum Auth {
    Bearer(Secret),
    Basic { user: String, password: Secret },
}

#[derive(Clone)]
enum Body {
    None,
    Plain {
        bytes: Vec<u8>,
        kind: Kind,
    },
    Secret(Secret),
    /// A `.json(..)` value that did not serialize, reported at `check`.
    Invalid(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Raw,
    Json,
    Form,
}

/// Text that `Debug` prints as it is, unquoted.
struct Raw(String);

impl fmt::Debug for Raw {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `{:?}` shows what a request *is* without its secrets: the URL masked,
/// a secret header, credential or body as `<secret, N bytes>`, and a plain
/// body as its size.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let auth = self.auth.as_ref().map(|a| match a {
            Auth::Bearer(s) => Raw(format!("bearer {}", secret_note(s))),
            Auth::Basic { user, password } => {
                Raw(format!("basic {user:?} {}", secret_note(password)))
            }
        });
        let body = match &self.body {
            Body::None => None,
            Body::Plain { bytes, kind } => Some(Raw(format!("{kind:?}, {} bytes", bytes.len()))),
            Body::Secret(s) => Some(Raw(secret_note(s))),
            Body::Invalid(e) => Some(Raw(format!("invalid: {e}"))),
        };
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("url", &mask_url(&self.url))
            .field("headers", &self.headers)
            .field("auth", &auth)
            .field("body", &body)
            .field("content_type", &self.content_type)
            .field("status", &self.status)
            .field("timeout", &self.timeout)
            .field("max_bytes", &self.max_bytes)
            .field("follow_redirects", &self.follow)
            .field(
                "changed_when",
                &self.changed_when.as_ref().map(|_| "<closure>"),
            )
            .finish()
    }
}

impl Request {
    /// A request with any method: `Request::method("OPTIONS", url)`, or an
    /// extension method such as `PROPFIND`. The name is upper-cased. Only
    /// `GET`, `HEAD` and `OPTIONS` count as reads (see the type's docs).
    pub fn method(method: impl Into<String>, url: impl Into<String>) -> Self {
        Request {
            method: method.into().to_ascii_uppercase(),
            url: url.into(),
            headers: vec![],
            auth: None,
            body: Body::None,
            content_type: None,
            status: None,
            timeout: DEFAULT_TIMEOUT,
            max_bytes: REQUEST_MAX_BYTES,
            follow: None,
            changed_when: None,
        }
    }

    /// `GET url`. Reports `ok` in a real run.
    pub fn get(url: impl Into<String>) -> Self {
        Self::method("GET", url)
    }

    /// `HEAD url`. Reports `ok` in a real run; the response has no body.
    pub fn head(url: impl Into<String>) -> Self {
        Self::method("HEAD", url)
    }

    /// `POST url`. Reports `changed` unless [`Request::changed_when`] says.
    pub fn post(url: impl Into<String>) -> Self {
        Self::method("POST", url)
    }

    /// `PUT url`. Reports `changed` unless [`Request::changed_when`] says.
    pub fn put(url: impl Into<String>) -> Self {
        Self::method("PUT", url)
    }

    /// `PATCH url`. Reports `changed` unless [`Request::changed_when`] says.
    pub fn patch(url: impl Into<String>) -> Self {
        Self::method("PATCH", url)
    }

    /// `DELETE url`. Reports `changed` unless [`Request::changed_when`] says.
    pub fn delete(url: impl Into<String>) -> Self {
        Self::method("DELETE", url)
    }

    /// A request header, e.g. `("Accept", "application/json")`. For a token
    /// or a key, use [`Request::header_secret`].
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(HeaderSpec {
            name: name.into(),
            value: Field::Plain(value.into()),
        });
        self
    }

    /// A header whose value is secret, e.g. `("X-API-Key", &key)`. A
    /// trailing newline is stripped. Never shown; dropped on a redirect to
    /// another origin.
    pub fn header_secret(mut self, name: impl Into<String>, value: &Secret) -> Self {
        self.headers.push(HeaderSpec {
            name: name.into(),
            value: Field::Secret(value.clone()),
        });
        self
    }

    /// `Authorization: Bearer <token>`. A trailing newline is stripped.
    pub fn bearer(mut self, token: &Secret) -> Self {
        self.auth = Some(Auth::Bearer(token.clone()));
        self
    }

    /// `Authorization: Basic`, sent with the first request rather than in
    /// answer to a `401` (Ansible's `force_basic_auth: true`). A trailing
    /// newline on the password is stripped.
    pub fn basic_auth(mut self, user: impl Into<String>, password: &Secret) -> Self {
        self.auth = Some(Auth::Basic {
            user: user.into(),
            password: password.clone(),
        });
        self
    }

    /// The body, as bytes or a string (Ansible's `body_format: raw`). To send
    /// a file from the target: `.body(ctx.sys().read(path)?)`.
    pub fn body(mut self, bytes: impl Into<Vec<u8>>) -> Self {
        self.body = Body::Plain {
            bytes: bytes.into(),
            kind: Kind::Raw,
        };
        self
    }

    /// The body as JSON, with `Content-Type: application/json` unless one is
    /// set. Takes any `Serialize`: a `#[derive(Serialize)]` struct, or a
    /// [`json!`](super::json) value.
    pub fn json(mut self, value: &impl Serialize) -> Self {
        self.body = match serde_json::to_vec(value) {
            Ok(bytes) => Body::Plain {
                bytes,
                kind: Kind::Json,
            },
            Err(e) => Body::Invalid(format!("the JSON body does not serialize: {e}")),
        };
        self
    }

    /// The body as `application/x-www-form-urlencoded` pairs (Ansible's
    /// `body_format: form-urlencoded`), with that content type unless one
    /// is set.
    pub fn form<I, K, V>(mut self, pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let pairs: Vec<(String, String)> = pairs
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self.body = Body::Plain {
            bytes: form_encode(&pairs).into_bytes(),
            kind: Kind::Form,
        };
        self
    }

    /// A body that is secret, sent as it is: `(secret body, N bytes)` in
    /// the diff, scrubbed from any message that would quote it, and never
    /// resent to another origin by a redirect. Set its type with
    /// [`Request::content_type`].
    pub fn body_secret(mut self, body: &Secret) -> Self {
        self.body = Body::Secret(body.clone());
        self
    }

    /// The `Content-Type` to send, over the one `.json` or `.form` implies
    /// and over a `Content-Type` given with [`Request::header`].
    pub fn content_type(mut self, value: impl Into<String>) -> Self {
        self.content_type = Some(value.into());
        self
    }

    /// The status codes that mean success, in place of the default of any
    /// `2xx`: `.status([200, 204])`, or a range, `.status(200..400)`.
    pub fn status(mut self, codes: impl IntoIterator<Item = u16>) -> Self {
        self.status = Some(codes.into_iter().collect());
        self
    }

    /// The most the whole exchange may take, connecting, redirects and the
    /// body included ([`DEFAULT_TIMEOUT`] by default).
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// The largest response body to accept, in bytes ([`REQUEST_MAX_BYTES`]
    /// by default). A `Content-Length` above it fails before the body is
    /// read; a body without one fails as soon as the read passes it.
    pub fn max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }

    /// Whether to follow redirects. By default a `GET` or `HEAD` does and
    /// any other method does not, so a `3xx` answer is its status (and fails
    /// unless [`Request::status`] lists it).
    pub fn follow_redirects(mut self, on: bool) -> Self {
        self.follow = Some(on);
        self
    }

    /// Decide from the response whether the step counts as `changed`, over
    /// the default (reads `ok`, anything else `changed`). The request is
    /// still sent; under `--check` it is not, and the step reports `would
    /// change` either way.
    pub fn changed_when(
        mut self,
        predicate: impl Fn(&Response) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.changed_when = Some(Arc::new(predicate));
        self
    }

    /// Send the request now and return the response, for an **operation's
    /// own code** that talks to an API (a collection, say), rather than a
    /// playbook, which uses `ctx.step`. The status, redirect, secret and
    /// size rules are the step's.
    ///
    /// It contacts the network, so **an operation must not call it from
    /// `check` under `--check`**: a dry run touches nothing outside the
    /// target (vision 12). Call it from `apply`, and have `check` plan the
    /// request without sending it, as [`Request`] itself does.
    pub fn send(&self) -> Result<Response> {
        let method = self.validate()?;
        // No hint about `.max_bytes()` here: whoever calls `send` owns the
        // limit, and their own users may have no such method to call.
        self.exchange(&method, &self.url).map_err(Error::from)
    }

    /// Whether this is a read: `GET`, `HEAD` or `OPTIONS`.
    fn is_read(&self) -> bool {
        matches!(self.method.as_str(), "GET" | "HEAD" | "OPTIONS")
    }

    /// Everything `check` can judge without the network.
    fn validate(&self) -> Result<Method> {
        validate_url(&self.url).map_err(Error::msg)?;
        let method = Method::from_bytes(self.method.as_bytes())
            .map_err(|_| Error::msg(format!("`{}` is not an HTTP method", self.method)))?;
        if let Body::Invalid(e) = &self.body {
            bail!("{} {}: {e}", self.method, mask_url(&self.url));
        }
        if self.status.as_ref().is_some_and(Vec::is_empty) {
            bail!(
                "{} {}: .status([]) accepts no status at all; list the codes that mean success",
                self.method,
                mask_url(&self.url)
            );
        }
        if self.auth.is_some()
            && self
                .headers
                .iter()
                .any(|h| h.name.eq_ignore_ascii_case("authorization"))
        {
            bail!(
                "{} {}: an Authorization header and .bearer()/.basic_auth() both set it; keep one",
                self.method,
                mask_url(&self.url)
            );
        }
        // A header that cannot be sent, a secret one included, fails here,
        // in `check`, rather than on the wire.
        for h in self.wire_headers()? {
            h.to_wire()
                .map_err(|e| Error::msg(format!("{} {}: {e}", self.method, mask_url(&self.url))))?;
        }
        Ok(method)
    }

    /// What the client cannot find in the headers for itself to scrub from
    /// messages: the basic-auth password, which crosses the wire only in
    /// base64 but which a server that decodes it can name back. (The
    /// header's value whole and the part after `Bearer `/`Basic ` — the
    /// token, the base64 pair — a secret body and the URL's userinfo, the
    /// client finds itself.)
    fn raw_secrets(&self) -> Vec<Secret> {
        match &self.auth {
            Some(Auth::Basic { password, .. }) => vec![password.clone()],
            None | Some(Auth::Bearer(_)) => vec![],
        }
    }

    /// The headers as sent: the given ones, the content type, and the
    /// credentials as a secret `Authorization` header.
    fn wire_headers(&self) -> Result<Vec<HeaderSpec>> {
        let mut headers: Vec<HeaderSpec> = self
            .headers
            .iter()
            .filter(|h| self.content_type.is_none() || !h.name.eq_ignore_ascii_case("content-type"))
            .cloned()
            .collect();
        for h in &headers {
            if let Field::Secret(s) = &h.value {
                client::secret_text(&h.name, s)?;
            }
        }
        let given = headers
            .iter()
            .any(|h| h.name.eq_ignore_ascii_case("content-type"));
        if !given && let Some(ct) = self.content_type_to_send() {
            headers.push(HeaderSpec {
                name: "Content-Type".into(),
                value: Field::Plain(ct),
            });
        }
        if let Some(auth) = &self.auth {
            let value = match auth {
                Auth::Bearer(token) => {
                    let token = token
                        .as_str()
                        .map_err(|_| Error::msg("the bearer token is not UTF-8"))?;
                    Secret::new(format!("Bearer {token}"))
                }
                Auth::Basic { user, password } => {
                    let password = password
                        .as_str()
                        .map_err(|_| Error::msg("the basic-auth password is not UTF-8"))?;
                    // Built in a `Secret` too, so the pair is wiped on drop.
                    let pair = Secret::new(format!("{user}:{password}"));
                    Secret::new(format!("Basic {}", base64(pair.as_bytes())))
                }
            };
            headers.push(HeaderSpec {
                name: "Authorization".into(),
                value: Field::Secret(value),
            });
        }
        Ok(headers)
    }

    /// The content type this request sends: `.content_type`, else a
    /// `Content-Type` header, else what `.json` or `.form` implies.
    fn content_type_to_send(&self) -> Option<String> {
        if let Some(ct) = &self.content_type {
            return Some(ct.clone());
        }
        let header = self.headers.iter().find_map(|h| match &h.value {
            Field::Plain(v) if h.name.eq_ignore_ascii_case("content-type") => Some(v.clone()),
            _ => None,
        });
        header.or(match &self.body {
            Body::Plain {
                kind: Kind::Json, ..
            } => Some("application/json".into()),
            Body::Plain {
                kind: Kind::Form, ..
            } => Some("application/x-www-form-urlencoded".into()),
            _ => None,
        })
    }

    /// The body as a diff shows it: its size and type, never its content.
    fn shown_body(&self) -> Shown {
        match &self.body {
            Body::None | Body::Invalid(_) => Shown::None,
            Body::Plain { bytes, .. } if bytes.is_empty() => Shown::None,
            Body::Plain { bytes, .. } => Shown::Plain {
                len: bytes.len(),
                content_type: self.content_type_to_send(),
            },
            Body::Secret(s) => Shown::Secret(s.len()),
        }
    }

    /// Send, follow, and judge the status. The one place a request leaves.
    fn exchange(&self, method: &Method, url: &str) -> std::result::Result<Response, ReadError> {
        let other = |e: Error| ReadError::Other(e.chain());
        let headers = self.wire_headers().map_err(other)?;
        let body: Option<(&[u8], bool)> = match &self.body {
            Body::Plain { bytes, .. } => Some((bytes.as_slice(), false)),
            Body::Secret(s) => Some((s.as_bytes(), true)),
            // A POST, PUT or PATCH with no body still says so, with
            // `Content-Length: 0`.
            Body::None if matches!(*method, Method::POST | Method::PUT | Method::PATCH) => {
                Some((&[][..], false))
            }
            Body::None | Body::Invalid(_) => None,
        };
        let follow = self
            .follow
            .unwrap_or(*method == Method::GET || *method == Method::HEAD);
        let mut resp = client::send(Outgoing {
            method: method.clone(),
            url: url.to_string(),
            headers,
            body,
            follow,
            timeout: Timeout::Total(self.timeout),
            secrets: self.raw_secrets(),
        })
        .map_err(other)?;
        let status = resp.status;
        if !status_accepted(self.status.as_deref(), status) {
            let reason = resp.reason();
            let at = resp.url.clone();
            let redirected = (at != mask_url(url)).then_some(at);
            let what = format!("{method} {}", mask_url(url));
            let quoted = resp.snippet(QUOTED_BODY_BYTES);
            return Err(ReadError::Other(status_failure(
                &what,
                status,
                reason,
                redirected.as_deref(),
                self.status.as_deref(),
                &quoted,
            )));
        }
        let headers = std::mem::take(&mut resp.headers);
        let body = resp.read(self.max_bytes)?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

/// What [`Request`]'s `check` decided: send this method to this URL. The
/// body and the headers stay on the op; the intent holds the body's size and
/// type, which its diff shows.
pub struct RequestIntent {
    method: Method,
    url: String,
    body: Shown,
}

#[derive(Debug)]
enum Shown {
    None,
    Secret(usize),
    Plain {
        len: usize,
        content_type: Option<String>,
    },
}

impl fmt::Debug for RequestIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestIntent")
            .field("method", &self.method)
            .field("url", &mask_url(&self.url))
            .field("body", &self.body)
            .finish()
    }
}

impl Intent for RequestIntent {
    /// One line, because the step line prints it in every run: `<METHOD>
    /// <masked url>`, and for a body its size and type, `(22 bytes,
    /// application/json)`, or `(secret body, 17 bytes)`. Never the body's
    /// content, and nothing about `--check`.
    fn diff(&self) -> Diff {
        let mut s = format!("{} {}", self.method, mask_url(&self.url));
        match &self.body {
            Shown::None => {}
            Shown::Secret(n) => s.push_str(&format!(" (secret body, {n} bytes)")),
            Shown::Plain {
                len,
                content_type: Some(ct),
            } => s.push_str(&format!(" ({len} bytes, {ct})")),
            Shown::Plain {
                len,
                content_type: None,
            } => s.push_str(&format!(" ({len} bytes)")),
        }
        Diff::summary(s)
    }
}

impl Op for Request {
    type Output = Response;
    type Intent = RequestIntent;

    /// Contacts nothing, in either mode: it validates what it can without
    /// the network (the URL, the method, the body, the secrets) and plans
    /// the request, which is `apply`'s to send (vision 12).
    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable: pure-Rust HTTP and TLS, nothing on the host but the
        // network. The supported set is written out rather than left open,
        // so a new platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!("http::Request has no implementation for {}", other.name()),
        }
        let method = self.validate()?;
        Ok(Plan::Change(RequestIntent {
            method,
            url: self.url.clone(),
            body: self.shown_body(),
        }))
    }

    fn apply(&self, _: &System, intent: RequestIntent) -> Result<Response> {
        self.exchange(&intent.method, &intent.url)
            .map_err(|e| e.hinted("raise it with .max_bytes()"))
    }

    /// A method that changes things on the server, with no `changed_when`
    /// to say otherwise: it changes every time it runs.
    fn always_changes(&self) -> bool {
        self.changed_when.is_none() && !self.is_read()
    }

    fn changed_by_apply(&self, output: &Response) -> bool {
        match &self.changed_when {
            Some(pred) => pred(output),
            None => !self.is_read(),
        }
    }
}

/// What a [`Request`] returns: the status, the headers, the body.
#[derive(Clone, PartialEq, Eq)]
pub struct Response {
    /// The status code, after any redirect followed.
    pub status: u16,
    /// The response headers in the order received, names lowercased (as
    /// Ansible returns them), repeated headers kept. [`Response::header`]
    /// looks one up.
    pub headers: Vec<(String, String)>,
    /// The body. Empty for a `HEAD`.
    pub body: Vec<u8>,
}

/// `{:?}` shows the body as its size: an API's answer can hold a key of its
/// own (Syncthing's `/rest/config` returns its API key).
impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &format!("{} bytes", self.body.len()))
            .finish()
    }
}

impl Response {
    /// The first header with this name, compared without case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as text. Fails when it is not UTF-8.
    pub fn text(&self) -> Result<&str> {
        std::str::from_utf8(&self.body).map_err(|e| {
            Error::msg(format!(
                "the response body ({} bytes, {}) is not UTF-8 text: {e}",
                self.body.len(),
                self.content_type()
            ))
        })
    }

    /// The body as a typed value: a `#[derive(Deserialize)]` struct naming
    /// the fields the playbook reads (fields it does not name are ignored).
    /// Prefer this to [`Response::json`]: a missing or mistyped field fails
    /// here, naming it, rather than further down as a `None`.
    ///
    /// The error says what was wrong and where (a missing field by name, a
    /// value of the wrong kind and what was expected, the line and column),
    /// but never quotes a value from the body: an API's answer can hold a
    /// key of its own.
    pub fn json_as<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(|e| {
            Error::msg(format!(
                "the response body ({}) is not the JSON `{}` expects: {}",
                self.content_type(),
                short_type_name::<T>(),
                describe_json_error(&e)
            ))
        })
    }

    /// The body as untyped JSON. Parsed whatever the `Content-Type` says.
    pub fn json(&self) -> Result<Value> {
        serde_json::from_slice(&self.body).map_err(|e| {
            Error::msg(format!(
                "the response body ({}) is not JSON: {}",
                self.content_type(),
                describe_json_error(&e)
            ))
        })
    }

    fn content_type(&self) -> String {
        match self.header("content-type") {
            Some(ct) => format!("Content-Type `{ct}`"),
            None => "no Content-Type".to_string(),
        }
    }
}

// ---- pure ----

/// Whether `status` is a success: any `2xx` when no codes were given.
fn status_accepted(expected: Option<&[u16]>, status: u16) -> bool {
    match expected {
        None => (200..300).contains(&status),
        Some(codes) => codes.contains(&status),
    }
}

/// The codes a request accepts, as a message names them: `2xx`, or the
/// list with runs collapsed (`200, 204, 300-399`).
fn describe_expected(expected: Option<&[u16]>) -> String {
    let Some(codes) = expected else {
        return "2xx".to_string();
    };
    let mut codes = codes.to_vec();
    codes.sort_unstable();
    codes.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < codes.len() {
        let start = codes[i];
        let mut end = start;
        while i + 1 < codes.len() && codes[i + 1] == end + 1 {
            i += 1;
            end = codes[i];
        }
        parts.push(if start == end {
            start.to_string()
        } else {
            format!("{start}-{end}")
        });
        i += 1;
    }
    parts.join(", ")
}

/// The message for a status outside the expected set.
fn status_failure(
    what: &str,
    status: u16,
    reason: &str,
    redirected_to: Option<&str>,
    expected: Option<&[u16]>,
    quoted: &str,
) -> String {
    let mut s = format!("{what} returned {status}");
    if !reason.is_empty() {
        s.push(' ');
        s.push_str(reason);
    }
    if let Some(at) = redirected_to {
        s.push_str(&format!(" (after redirects, at {at})"));
    }
    s.push_str(&format!(", expected {}", describe_expected(expected)));
    if !quoted.is_empty() {
        s.push_str(": ");
        s.push_str(quoted);
    }
    s
}

/// A type's name as a message shows it: the last path segment of each
/// type in it, so `ws::__pb::Folder` reads `Folder` and
/// `alloc::vec::Vec<my::Key>` reads `Vec<Key>`.
fn short_type_name<T>() -> String {
    let full = std::any::type_name::<T>();
    let mut out = String::new();
    let mut word = String::new();
    for c in full.chars() {
        if c.is_alphanumeric() || c == '_' || c == ':' {
            word.push(c);
        } else {
            out.push_str(word.rsplit("::").next().unwrap_or_default());
            word.clear();
            out.push(c);
        }
    }
    out.push_str(word.rsplit("::").next().unwrap_or_default());
    out
}

/// A `serde_json` error without the text of any value from the input, which
/// serde quotes (`invalid type: string "hunter2", expected u32`). What is
/// kept comes from the type being read (a field's name, what it expected)
/// or from the parser (the line and column).
fn describe_json_error(e: &serde_json::Error) -> String {
    use serde_json::error::Category;
    let at = format!("line {}, column {}", e.line(), e.column());
    match e.classify() {
        Category::Syntax => format!("not valid JSON (at {at})"),
        Category::Eof => format!("the JSON ends early (at {at})"),
        Category::Io => "the body could not be read".to_string(),
        Category::Data => {
            let msg = e.to_string();
            let msg = msg.rsplit_once(" at line ").map(|(m, _)| m).unwrap_or(&msg);
            let expected = msg
                .rsplit_once(", expected ")
                .map(|(_, x)| format!(", expected {x}"))
                .unwrap_or_default();
            let what = if let Some(rest) = msg.strip_prefix("missing field `") {
                // The field's name comes from the type, not the body.
                format!(
                    "missing field `{}`",
                    rest.split('`').next().unwrap_or_default()
                )
            } else if let Some(rest) = msg
                .strip_prefix("invalid type: ")
                .or_else(|| msg.strip_prefix("invalid value: "))
            {
                format!(
                    "a value of the wrong kind ({}{expected})",
                    unexpected_kind(rest)
                )
            } else if msg.starts_with("invalid length ") {
                format!("a list or map of the wrong length{expected}")
            } else if msg.starts_with("unknown field ") {
                format!("a field it does not know{expected}")
            } else if msg.starts_with("unknown variant ") {
                format!("a value it does not know{expected}")
            } else if msg.starts_with("duplicate field ") {
                "a field given twice".to_string()
            } else {
                "a value that does not fit".to_string()
            };
            format!("{what} at {at}")
        }
    }
}

/// The kind of value serde's `Unexpected` names, without the value: the
/// words before the quoted value (`string`, `integer`, `map`, ...).
fn unexpected_kind(rest: &str) -> &'static str {
    const KINDS: [&str; 16] = [
        "boolean",
        "integer",
        "floating point",
        "character",
        "string",
        "byte array",
        "unit value",
        "Option value",
        "newtype struct",
        "sequence",
        "map",
        "enum",
        "unit variant",
        "newtype variant",
        "tuple variant",
        "struct variant",
    ];
    if rest.starts_with("null") {
        return "null";
    }
    KINDS
        .iter()
        .find(|k| rest.starts_with(*k))
        .copied()
        .unwrap_or("a value")
}

/// `application/x-www-form-urlencoded`: unreserved characters as they are,
/// a space as `+`, everything else `%XX`.
fn form_encode(pairs: &[(String, String)]) -> String {
    fn enc(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                    out.push(b as char)
                }
                b' ' => out.push('+'),
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::{Collect, Event, Status};
    use rustible_sdk::{Ctx, HostInfo};
    use serde::Deserialize;
    use serde_json::json;

    use super::super::test_server::{Server, serve};
    use super::*;

    const KEY: &str = "hunter2-api-key";
    const TOKEN: &str = "tok3n-bearer";
    const PASSWORD: &str = "pa55w0rd-basic";
    const BODY_SECRET: &str = "s3cret-body-bytes";

    fn sys(check: bool) -> (System, Arc<Collect>) {
        let sink = Arc::new(Collect::default());
        let sys = System::fake(Arc::new(Fake::new()), sink.clone()).with_check_mode(check);
        (sys, sink)
    }

    /// Each step's verdict, its note, and its diff as `-v` renders it.
    fn finished(sink: &Collect) -> Vec<(Status, Option<String>, Option<String>)> {
        sink.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::StepFinished {
                    status, note, diff, ..
                } => Some((status, note, diff.map(|d| d.render()))),
                _ => None,
            })
            .collect()
    }

    /// One step in a fresh context: the result and what the sink saw.
    fn step(check: bool, op: Request) -> (Result<Applied<Response>>, Arc<Collect>) {
        let (sys, sink) = sys(check);
        let mut ctx = Ctx::new(sys, HostInfo::local());
        (ctx.step("request", op), sink)
    }

    fn folder_server() -> Server {
        serve(vec![
            (
                "/rest/config/folders/dcim",
                200,
                vec![("Content-Type", "application/json".into())],
                br#"{"id":"dcim","type":"sendreceive","label":"Camera"}"#.to_vec(),
            ),
            ("/created", 201, vec![], vec![]),
            ("/conflict", 409, vec![], b"folder is busy\n".to_vec()),
            ("/big", 200, vec![], vec![b'x'; 4096]),
        ])
    }

    /// Every way a secret can be given, at once.
    fn with_secrets(op: Request) -> Request {
        op.header_secret("X-API-Key", &Secret::new(format!("{KEY}\n")))
            .bearer(&Secret::new(TOKEN))
    }

    fn assert_no_secret(text: &str) {
        for s in [KEY, TOKEN, PASSWORD, BODY_SECRET, "u5er-pw"] {
            assert!(!text.contains(s), "`{s}` leaked into: {text}");
        }
    }

    // ---- pure ----

    #[test]
    fn status_matching_defaults_to_any_2xx() {
        for ok in [200, 201, 204, 299] {
            assert!(status_accepted(None, ok), "{ok}");
        }
        for not in [100, 199, 300, 302, 404, 500] {
            assert!(!status_accepted(None, not), "{not}");
        }
        assert!(status_accepted(Some(&[200, 404]), 404));
        assert!(!status_accepted(Some(&[200]), 201));
        assert_eq!(describe_expected(None), "2xx");
        assert_eq!(describe_expected(Some(&[204, 200, 200])), "200, 204");
        let wide: Vec<u16> = (100..600).collect();
        assert_eq!(describe_expected(Some(&wide)), "100-599");
        assert_eq!(
            describe_expected(Some(&[200, 201, 202, 304])),
            "200-202, 304"
        );
    }

    #[test]
    fn the_status_failure_message_names_everything() {
        assert_eq!(
            status_failure("PATCH http://h/x", 409, "Conflict", None, None, "busy"),
            "PATCH http://h/x returned 409 Conflict, expected 2xx: busy"
        );
        assert_eq!(
            status_failure(
                "GET http://h/x",
                599,
                "",
                Some("http://g/y"),
                Some(&[200]),
                ""
            ),
            "GET http://h/x returned 599 (after redirects, at http://g/y), expected 200"
        );
    }

    #[test]
    fn forms_are_url_encoded() {
        assert_eq!(
            form_encode(&[
                ("name".into(), "a b".into()),
                ("x&y".into(), "1=2/é".into())
            ]),
            "name=a+b&x%26y=1%3D2%2F%C3%A9"
        );
    }

    #[test]
    fn the_diff_is_one_line_naming_the_body_by_size_and_type() {
        let (s, _) = sys(false);
        let get = Request::get("http://bob:u5er-pw@127.0.0.1:8384/rest/x");
        let Plan::Change(i) = get.check(&s).unwrap() else {
            panic!()
        };
        let d = i.diff();
        assert_eq!(d.render(), "GET http://bob:********@127.0.0.1:8384/rest/x");
        assert_eq!(d.short(), d.render());
        assert!(!d.render().contains("check"), "nothing about --check");

        let patch = Request::patch("http://h/rest/x").json(&json!({"type": "receiveonly"}));
        let Plan::Change(i) = patch.check(&s).unwrap() else {
            panic!()
        };
        let d = i.diff();
        assert_eq!(
            d.render(),
            "PATCH http://h/rest/x (22 bytes, application/json)"
        );
        assert_eq!(d.short(), d.render(), "one line, and the step line's");
        assert!(!d.render().contains("receiveonly"), "never the content");

        let raw = Request::put("http://h/x").body("a=b");
        let Plan::Change(i) = raw.check(&s).unwrap() else {
            panic!()
        };
        assert_eq!(i.diff().render(), "PUT http://h/x (3 bytes)");
        let empty = Request::post("http://h/x").body("");
        let Plan::Change(i) = empty.check(&s).unwrap() else {
            panic!()
        };
        assert_eq!(i.diff().render(), "POST http://h/x", "no body to name");
        let form = Request::post("http://h/x").form([("a", "b")]);
        let Plan::Change(i) = form.check(&s).unwrap() else {
            panic!()
        };
        assert_eq!(
            i.diff().render(),
            "POST http://h/x (3 bytes, application/x-www-form-urlencoded)"
        );

        let secret = Request::post("http://h/x")
            .body_secret(&Secret::new(BODY_SECRET))
            .content_type("application/json");
        let Plan::Change(i) = secret.check(&s).unwrap() else {
            panic!()
        };
        assert_eq!(
            i.diff().render(),
            format!("POST http://h/x (secret body, {} bytes)", BODY_SECRET.len())
        );
        assert_no_secret(&format!("{i:?}"));
    }

    #[test]
    fn secrets_never_appear_in_debug() {
        let op = with_secrets(Request::post("http://bob:u5er-pw@h/x"))
            .body_secret(&Secret::new(BODY_SECRET));
        let dbg = format!("{op:?}");
        assert_no_secret(&dbg);
        assert!(
            dbg.contains("<secret, 16 bytes>"),
            "the key, newline included: {dbg}"
        );
        assert!(dbg.contains("bob:********@h"), "{dbg}");
        let basic = Request::get("http://h/").basic_auth("bob", &Secret::new(PASSWORD));
        let dbg = format!("{basic:?}");
        assert_no_secret(&dbg);
        assert!(dbg.contains("basic \"bob\" <secret, 14 bytes>"), "{dbg}");
        let (s, _) = sys(false);
        let Plan::Change(i) = op.check(&s).unwrap() else {
            panic!()
        };
        assert_no_secret(&format!("{i:?}"));
        assert_no_secret(&format!("{:?}", op.check(&s).unwrap()));
    }

    #[test]
    fn refusals_at_check_contact_nothing() {
        let (s, _) = sys(false);
        let err = |op: Request| op.check(&s).unwrap_err().chain();
        assert!(err(Request::get("ftp://h/x")).contains("not an http://"));
        assert!(err(Request::method("NOT A METHOD", "http://h/")).contains("not an HTTP method"));
        assert!(
            err(Request::get("http://h/").status([])).contains(".status([]) accepts no status")
        );
        assert!(
            err(Request::get("http://h/")
                .header("Authorization", "x")
                .bearer(&Secret::new("t")))
            .contains("both set it")
        );
        // A map with non-string keys is not JSON.
        let bad: std::collections::BTreeMap<(u8, u8), u8> = [((1, 2), 3)].into();
        assert!(err(Request::post("http://h/").json(&bad)).contains("does not serialize"));
        // A secret that cannot be a header value is refused before the
        // wire, and the refusal does not quote it.
        let e = err(Request::get("http://h/").header_secret("X-Key", &Secret::new("a\nb")));
        assert!(e.contains("`X-Key` is not a valid header value"), "{e}");
        assert!(!e.contains("a\nb"));
        let e = err(Request::get("http://h/").header_secret("X-Key", &Secret::new(vec![0xff])));
        assert!(e.contains("not UTF-8"), "{e}");
        let e = err(Request::get("ftp://bob:u5er-pw@h/"));
        assert_no_secret(&e);
    }

    #[test]
    fn runs_on_a_mac_and_refuses_an_unclaimed_platform() {
        let server = folder_server();
        let (base, _) = sys(false);
        let mut mac = base.facts().clone();
        mac.os = Os::Macos;
        let mac = base.clone().with_facts(mac);
        let op = Request::get(server.url("/created"));
        let Plan::Change(i) = op.check(&mac).unwrap() else {
            panic!()
        };
        assert_eq!(op.apply(&mac, i).unwrap().status, 201);
        let mut bsd = base.facts().clone();
        bsd.os = Os::Other("freebsd".into());
        let e = op.check(&base.with_facts(bsd)).unwrap_err().chain();
        assert!(
            e.contains("http::Request has no implementation for freebsd"),
            "{e}"
        );
        assert_eq!(server.hits(), 1, "the refusal sent nothing");
    }

    // ---- loopback ----

    /// Vision 12: under `--check` nothing is sent, a GET no more than a
    /// PATCH. Both report `would change` with no output, and the server's
    /// counter stays at zero.
    #[test]
    fn under_check_neither_a_get_nor_a_patch_is_sent() {
        let server = folder_server();
        let url = server.url("/rest/config/folders/dcim");
        let (sys, sink) = sys(true);
        let mut ctx = Ctx::new(sys, HostInfo::local());
        let got = ctx.step("read", with_secrets(Request::get(&url))).unwrap();
        let set = ctx
            .step(
                "set",
                with_secrets(Request::patch(&url)).json(&json!({"type": "receiveonly"})),
            )
            .unwrap();
        assert_eq!(server.hits(), 0, "{:?}", server.seen());
        assert!(got.changed && !got.is_available());
        assert!(set.changed && !set.is_available());
        let steps = finished(&sink);
        assert_eq!(steps[0].0, Status::WouldChange);
        assert_eq!(steps[0].1, None, "a read is not an action");
        assert_eq!(steps[0].2.as_deref(), Some(format!("GET {url}").as_str()));
        assert_eq!(steps[1].0, Status::WouldChange);
        assert_eq!(steps[1].1.as_deref(), Some("action"));
        assert_eq!(
            steps[1].2.as_deref(),
            Some(format!("PATCH {url} (22 bytes, application/json)").as_str())
        );
    }

    /// A real run sends: a GET reports `ok` (ran, unchanged) and a PATCH
    /// `changed` (an action), and the server saw both, secrets and body
    /// included.
    #[test]
    fn a_real_get_is_ok_and_a_real_patch_is_changed() {
        let server = folder_server();
        let url = server.url("/rest/config/folders/dcim");
        let (sys, sink) = sys(false);
        let mut ctx = Ctx::new(sys, HostInfo::local());
        let got = ctx.step("read", with_secrets(Request::get(&url))).unwrap();
        assert!(!got.changed);
        assert_eq!(got.status, 200);
        assert_eq!(got.header("CONTENT-TYPE"), Some("application/json"));
        assert!(
            got.headers
                .iter()
                .all(|(k, _)| *k == k.to_ascii_lowercase())
        );
        let set = ctx
            .step(
                "set",
                with_secrets(Request::patch(&url)).json(&json!({"type": "receiveonly"})),
            )
            .unwrap();
        assert!(set.changed);
        let steps = finished(&sink);
        assert_eq!(
            (steps[0].0, steps[0].1.as_deref()),
            (Status::Ok, Some("ran, unchanged"))
        );
        assert_eq!(
            (steps[1].0, steps[1].1.as_deref()),
            (Status::Changed, Some("action"))
        );
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!((seen[0].method.as_str(), seen[0].body.len()), ("GET", 0));
        assert_eq!(seen[0].header("x-api-key"), Some(KEY), "newline stripped");
        assert_eq!(
            seen[0].header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert!(
            seen[0]
                .header("user-agent")
                .unwrap()
                .starts_with("rustible/")
        );
        assert_eq!(seen[1].method, "PATCH");
        assert_eq!(seen[1].body, br#"{"type":"receiveonly"}"#);
        assert_eq!(seen[1].header("content-type"), Some("application/json"));
    }

    #[test]
    fn changed_when_decides_either_way() {
        let server = folder_server();
        let url = server.url("/rest/config/folders/dcim");
        let (got, sink) = step(false, Request::get(&url).changed_when(|r| r.status == 200));
        assert!(got.unwrap().changed);
        assert_eq!(finished(&sink)[0].0, Status::Changed);
        assert_eq!(finished(&sink)[0].1, None, "not an action once it decides");
        let (set, sink) = step(
            false,
            Request::patch(&url)
                .json(&json!({}))
                .changed_when(|r| r.status != 200),
        );
        assert!(!set.unwrap().changed);
        assert_eq!(
            (finished(&sink)[0].0, finished(&sink)[0].1.as_deref()),
            (Status::Ok, Some("ran, unchanged"))
        );
        // Under --check it is `would change` whatever the predicate says.
        let (got, _) = step(true, Request::get(&url).changed_when(|_| false));
        assert!(got.unwrap().changed);
        assert_eq!(server.hits(), 2);
    }

    #[test]
    fn a_status_outside_the_expected_set_fails_with_the_exact_message() {
        let server = folder_server();
        let url = server.url("/conflict");
        let (err, sink) = step(false, Request::post(&url).json(&json!({"a": 1})));
        assert_eq!(
            err.unwrap_err().chain(),
            format!(
                "step `request`: POST {url} returned 409 Conflict, expected 2xx: folder is busy"
            )
        );
        assert_eq!(finished(&sink)[0].0, Status::Failed);
        let created = server.url("/created");
        let e = Request::put(&created)
            .status([200, 204])
            .send()
            .unwrap_err()
            .chain();
        assert_eq!(
            e,
            format!("PUT {created} returned 201 Created, expected 200, 204")
        );
        // Listing it makes it a success; so does the default 2xx.
        assert_eq!(
            Request::put(&created).status([201]).send().unwrap().status,
            201
        );
        assert_eq!(Request::put(&created).send().unwrap().status, 201);
        // And a listed non-2xx is a success too.
        let r = Request::get(server.url("/conflict"))
            .status([409])
            .send()
            .unwrap();
        assert_eq!(r.text().unwrap(), "folder is busy\n");
    }

    /// Every secret, sent for real against a URL that fails: absent from
    /// `Debug`, from the diff, from the error, and from every event, which
    /// is what `--json` prints.
    #[test]
    fn secrets_are_absent_from_debug_diffs_errors_and_events() {
        let server = folder_server();
        let url = server
            .url("/conflict")
            .replace("http://", "http://bob:u5er-pw@");
        let op = with_secrets(Request::post(&url)).body_secret(&Secret::new(BODY_SECRET));
        assert_no_secret(&format!("{op:?}"));
        let (r, sink) = step(false, op);
        let e = r.unwrap_err().chain();
        assert!(e.contains("returned 409 Conflict"), "{e}");
        assert!(e.contains("bob:********@"), "{e}");
        assert_no_secret(&e);
        for ev in sink.events() {
            assert_no_secret(&format!("{ev:?}"));
            assert_no_secret(&serde_json::to_string(&ev).unwrap());
        }
        let diff = finished(&sink)[0].2.clone().unwrap();
        assert!(diff.ends_with("(secret body, 17 bytes)"), "{diff}");
        // Every one of them did reach the server.
        let seen = &server.seen()[0];
        assert_eq!(seen.header("x-api-key"), Some(KEY));
        assert_eq!(seen.body, BODY_SECRET.as_bytes());
        let basic = Request::get(server.url("/created"))
            .basic_auth("bob", &Secret::new(PASSWORD))
            .send()
            .unwrap();
        assert_eq!(basic.status, 201);
        assert_eq!(
            server.seen()[1].header("authorization"),
            Some(format!("Basic {}", base64(format!("bob:{PASSWORD}").as_bytes())).as_str())
        );
        // A transport failure does not quote them either.
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let e = with_secrets(Request::get(format!("http://bob:u5er-pw@{closed}/x")))
            .send()
            .unwrap_err()
            .chain();
        assert!(
            e.starts_with(&format!("GET http://bob:********@{closed}/x: ")),
            "{e}"
        );
        assert_no_secret(&e);
    }

    /// The end-to-end half of `client::follow_redirect`'s tests: a redirect
    /// to another origin (another port on the same host) arrives without
    /// the key and the bearer, and with the plain headers; a redirect within
    /// the origin keeps them.
    #[test]
    fn a_redirect_to_another_origin_arrives_without_credentials() {
        let target = serve(vec![("/x", 200, vec![], b"there".to_vec())]);
        let first = serve(vec![
            ("/away", 302, vec![("Location", target.url("/x"))], vec![]),
            ("/here", 307, vec![("Location", "/ok".into())], vec![]),
            ("/ok", 200, vec![], b"here".to_vec()),
        ]);
        let op =
            |path: &str| with_secrets(Request::get(first.url(path))).header("Accept", "text/plain");
        assert_eq!(op("/away").send().unwrap().text().unwrap(), "there");
        let arrived = &target.seen()[0];
        assert_eq!(arrived.header("x-api-key"), None, "{arrived:?}");
        assert_eq!(arrived.header("authorization"), None, "{arrived:?}");
        assert_eq!(arrived.header("accept"), Some("text/plain"));
        // The first hop had them.
        assert_eq!(first.seen()[0].header("x-api-key"), Some(KEY));

        assert_eq!(op("/here").send().unwrap().text().unwrap(), "here");
        let kept = &first.seen()[2];
        assert_eq!(kept.path, "/ok");
        assert_eq!(kept.header("x-api-key"), Some(KEY));
        assert!(kept.header("authorization").is_some());
    }

    #[test]
    fn redirects_follow_the_method_rules_and_stop_at_ten() {
        let s = serve(vec![
            (
                "/see-other",
                303,
                vec![("Location", "/landed".into())],
                vec![],
            ),
            (
                "/temporary",
                307,
                vec![("Location", "/landed".into())],
                vec![],
            ),
            ("/found", 302, vec![("Location", "/landed".into())], vec![]),
            ("/landed", 200, vec![], b"ok".to_vec()),
            ("/loop", 302, vec![("Location", "/loop".into())], vec![]),
        ]);
        // A PATCH does not follow by default: the 3xx is its answer.
        let e = Request::patch(s.url("/found"))
            .body("x")
            .send()
            .unwrap_err()
            .chain();
        assert!(e.contains("returned 302 Found, expected 2xx"), "{e}");
        assert_eq!(s.hits(), 1);
        // Told to: a 303 becomes a GET without the body or its type.
        Request::post(s.url("/see-other"))
            .json(&json!({"a": 1}))
            .follow_redirects(true)
            .send()
            .unwrap();
        let seen = s.seen();
        assert_eq!(
            (seen[2].method.as_str(), seen[2].path.as_str()),
            ("GET", "/landed")
        );
        assert!(seen[2].body.is_empty() && seen[2].header("content-type").is_none());
        // A 307 keeps both.
        Request::post(s.url("/temporary"))
            .json(&json!({"a": 1}))
            .follow_redirects(true)
            .send()
            .unwrap();
        let seen = s.seen();
        assert_eq!(
            (seen[4].method.as_str(), seen[4].body.as_slice()),
            ("POST", &br#"{"a":1}"#[..])
        );
        assert_eq!(seen[4].header("content-type"), Some("application/json"));
        // A GET follows by default, a 302 included, and gives up after ten.
        let e = Request::get(s.url("/loop")).send().unwrap_err().chain();
        assert!(e.contains("stopped after 10 redirects"), "{e}");
        assert_eq!(s.hits(), 5 + 11, "the first request and ten redirects");
        // Not following, a GET gets its 3xx.
        let r = Request::get(s.url("/found"))
            .follow_redirects(false)
            .status([302])
            .send()
            .unwrap();
        assert_eq!(r.header("location"), Some("/landed"));
    }

    #[test]
    fn max_bytes_is_enforced() {
        let server = folder_server();
        let e = Request::get(server.url("/big"))
            .max_bytes(100)
            .send()
            .unwrap_err()
            .chain();
        assert!(
            e.contains("declares a 4096 byte body (Content-Length), over the 100 byte limit"),
            "{e}"
        );
        assert!(!e.contains(".max_bytes()"), "{e}");
        assert_eq!(
            Request::get(server.url("/big"))
                .max_bytes(4096)
                .send()
                .unwrap()
                .body
                .len(),
            4096
        );
        let s = serve(vec![(
            "/big",
            200,
            vec![("X-Omit-Length", String::new())],
            vec![b'x'; 4096],
        )]);
        let e = Request::get(s.url("/big"))
            .max_bytes(100)
            .send()
            .unwrap_err()
            .chain();
        assert!(e.contains("larger than the 100 byte limit"), "{e}");
        // `send` is for op authors, whose users may have no `.max_bytes()`:
        // the hint is the step's alone.
        assert!(!e.contains(".max_bytes()"), "{e}");
        let (r, _) = step(false, Request::get(s.url("/big")).max_bytes(100));
        let e = r.unwrap_err().chain();
        assert!(
            e.ends_with("larger than the 100 byte limit; raise it with .max_bytes()"),
            "{e}"
        );
    }

    /// The timeout covers the body: a server that sends its head and then
    /// stalls is cut off at the deadline, not left to hang.
    #[test]
    fn the_timeout_covers_the_body() {
        let s = serve(vec![(
            "/slow",
            200,
            vec![("X-Stall-Body", "3000".into())],
            b"late".to_vec(),
        )]);
        let started = Instant::now();
        let e = Request::get(s.url("/slow"))
            .timeout(Duration::from_millis(300))
            .send()
            .unwrap_err()
            .chain();
        assert!(e.contains("timed out after 300ms"), "{e}");
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "{:?}",
            started.elapsed()
        );
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct Folder {
        id: String,
        #[serde(rename = "type")]
        kind: String,
    }

    #[test]
    fn json_as_round_trips_a_typed_struct() {
        let server = folder_server();
        let url = server.url("/rest/config/folders/dcim");
        let got = Request::get(&url).send().unwrap();
        let folder: Folder = got.json_as().unwrap();
        assert_eq!(
            folder,
            Folder {
                id: "dcim".into(),
                kind: "sendreceive".into()
            }
        );
        assert_eq!(got.json().unwrap()["label"], "Camera");
        // And back out, typed, as the request body.
        Request::put(&url).json(&folder).send().unwrap();
        let sent: Folder = serde_json::from_slice(&server.seen()[1].body).unwrap();
        assert_eq!(sent, folder);
        // A body that is not the struct fails naming the field.
        #[derive(Debug, Deserialize)]
        struct Wrong {
            #[allow(dead_code)]
            devices: Vec<String>,
        }
        let e = got.json_as::<Wrong>().unwrap_err().chain();
        assert!(e.contains("missing field `devices`"), "{e}");
        assert!(e.contains("Content-Type `application/json`"), "{e}");
    }

    #[test]
    fn a_head_has_no_body_and_a_form_is_encoded() {
        let server = folder_server();
        let r = Request::head(server.url("/rest/config/folders/dcim"))
            .send()
            .unwrap();
        assert!(r.body.is_empty());
        assert_eq!(r.status, 200);
        Request::post(server.url("/created"))
            .form([("name", "a b"), ("x", "1")])
            .send()
            .unwrap();
        Request::post(server.url("/created"))
            .header("Content-Type", "application/merge-patch+json")
            .json(&json!({}))
            .send()
            .unwrap();
        Request::post(server.url("/created")).send().unwrap();
        let seen = server.seen();
        assert_eq!(seen[1].body, b"name=a+b&x=1");
        assert_eq!(
            seen[1].header("content-type"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(
            seen[2].header("content-type"),
            Some("application/merge-patch+json"),
            "a given Content-Type wins over .json's"
        );
        assert_eq!(seen[3].header("content-length"), Some("0"));
    }

    // ---- review round: secrets in echoes, one-line errors, json_as ----

    /// A server that echoes the request into its error page (a `400` or a
    /// `422` often does) must not put the request's secrets into the step's
    /// error, and so into `StepFinished` and `--json`: every form of every
    /// secret is scrubbed from the quoted body.
    #[test]
    fn an_echoing_server_does_not_leak_secrets_into_the_status_error() {
        let s = serve(vec![(
            "/echo",
            422,
            vec![("X-Echo", String::new())],
            vec![],
        )]);
        let url = s.url("/echo").replace("http://", "http://bob:u5er-pw@");
        let op = with_secrets(Request::post(&url)).body_secret(&Secret::new(BODY_SECRET));
        let (r, sink) = step(false, op);
        let e = r.unwrap_err().chain();
        assert!(e.contains("returned 422 Unprocessable Entity"), "{e}");
        assert!(
            e.contains("x-api-key: <secret>"),
            "the echo is there, scrubbed: {e}"
        );
        assert_no_secret(&e);
        for ev in sink.events() {
            assert_no_secret(&serde_json::to_string(&ev).unwrap());
        }
        // The server did get them all.
        let seen = &s.seen()[0];
        assert_eq!(seen.header("x-api-key"), Some(KEY));
        assert_eq!(seen.body, BODY_SECRET.as_bytes());

        let basic = Request::get(s.url("/echo")).basic_auth("bob", &Secret::new(PASSWORD));
        let e = basic.send().unwrap_err().chain();
        let b64 = base64(format!("bob:{PASSWORD}").as_bytes());
        assert!(!e.contains(&b64), "{e}");
        assert!(e.contains("authorization: <secret>"), "{e}");
        assert_no_secret(&e);

        // A server that decodes the credentials and names them back: the
        // password never crossed the wire in clear, and is scrubbed anyway.
        let says = serve(vec![(
            "/login",
            401,
            vec![],
            format!("no user bob with password {PASSWORD}; got {b64}").into_bytes(),
        )]);
        let basic = Request::get(says.url("/login")).basic_auth("bob", &Secret::new(PASSWORD));
        let e = basic.send().unwrap_err().chain();
        assert!(
            e.ends_with("no user bob with password <secret>; got <secret>"),
            "{e}"
        );
    }

    /// A failed response is quoted from its first bytes and never read to
    /// its end: one whose body stalls past the quoted part does not hold
    /// the step for the stall.
    #[test]
    fn a_failed_response_is_not_read_to_its_end() {
        let s = serve(vec![(
            "/slow-error",
            500,
            vec![("X-Stall-After", "2000:4000".into())],
            vec![b'x'; 100_000],
        )]);
        let started = Instant::now();
        let e = Request::get(s.url("/slow-error"))
            .timeout(Duration::from_secs(10))
            .send()
            .unwrap_err()
            .chain();
        assert!(e.contains("returned 500"), "{e}");
        assert!(
            started.elapsed() < Duration::from_millis(2500),
            "{:?}",
            started.elapsed()
        );
    }

    /// A certificate that fails verification says what to do, against a
    /// real handshake with a self-signed server on loopback.
    #[test]
    fn an_untrusted_certificate_says_what_to_do() {
        let base = super::super::test_server::serve_untrusted_tls();
        let e = Request::get(format!("{base}/")).send().unwrap_err().chain();
        assert!(e.contains("TLS certificate failed verification"), "{e}");
        assert!(e.contains("never skipped"), "{e}");
        assert!(e.contains("http://127.0.0.1"), "{e}");
    }

    /// An error page is quoted as one line, its control characters escaped,
    /// at most 512 bytes of it, however long and however many lines it is.
    #[test]
    fn a_quoted_error_page_is_one_bounded_line() {
        let page = format!(
            "<html>\n  <body>\n\t<h1>Not Implemented</h1>\x1b[31m\r\n{}\n</body></html>\n",
            "x".repeat(100_000)
        );
        let s = serve(vec![("/page", 501, vec![], page.into_bytes())]);
        let e = Request::get(s.url("/page")).send().unwrap_err().chain();
        assert!(!e.contains('\n') && !e.contains('\x1b'), "{e:?}");
        assert!(
            e.contains(": <html> <body> <h1>Not Implemented</h1>\\u{1b}[31m xxx"),
            "{e}"
        );
        assert!(e.ends_with("..."), "{e}");
        assert!(e.len() < 512 + 200, "{} bytes: {e}", e.len());
    }

    /// The error of `json_as` names the type by its last path segment and
    /// says what was wrong and where, but never quotes a value from the
    /// body, which can hold a key (Syncthing's `/rest/config` does).
    #[test]
    fn json_as_errors_quote_no_value_from_the_body() {
        #[derive(Debug, Deserialize)]
        struct Config {
            #[allow(dead_code)]
            apikey: u32,
        }
        let r = Response {
            status: 200,
            headers: vec![],
            body: br#"{"apikey": "SUPERSECRETKEY123"}"#.to_vec(),
        };
        let e = r.json_as::<Config>().unwrap_err().chain();
        assert!(!e.contains("SUPERSECRETKEY123"), "{e}");
        assert_eq!(
            e,
            "the response body (no Content-Type) is not the JSON `Config` expects: a value of \
             the wrong kind (string, expected u32) at line 1, column 30"
        );
        let e = r.json_as::<Vec<Config>>().unwrap_err().chain();
        assert!(e.contains("JSON `Vec<Config>` expects"), "{e}");
        let bad = Response {
            body: br#"{"apikey": SUPERSECRET}"#.to_vec(),
            ..r.clone()
        };
        let e = bad.json().unwrap_err().chain();
        assert!(!e.contains("SUPERSECRET"), "{e}");
        assert!(
            e.ends_with("is not JSON: not valid JSON (at line 1, column 12)"),
            "{e}"
        );
        let e = bad.json_as::<Config>().unwrap_err().chain();
        assert!(!e.contains("SUPERSECRET"), "{e}");
    }

    #[test]
    fn an_options_request_is_a_read() {
        let server = folder_server();
        let (r, sink) = step(
            false,
            Request::method("options", server.url("/rest/config/folders/dcim")),
        );
        assert!(!r.unwrap().changed);
        assert_eq!(
            (finished(&sink)[0].0, finished(&sink)[0].1.as_deref()),
            (Status::Ok, Some("ran, unchanged"))
        );
        assert_eq!(server.seen()[0].method, "OPTIONS");
    }

    #[test]
    fn a_response_debugs_its_body_as_a_size() {
        let r = Response {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: br#"{"apikey":"SUPERSECRETKEY123"}"#.to_vec(),
        };
        let dbg = format!("{r:?}");
        assert!(!dbg.contains("SUPERSECRET"), "{dbg}");
        assert!(dbg.contains("\"30 bytes\""), "{dbg}");
    }

    /// The timeout is one deadline for the whole exchange: two hops that
    /// each take 60% of it fail, where a per-hop timeout would let both
    /// through.
    #[test]
    fn the_timeout_covers_every_redirect_hop_together() {
        let s = serve(vec![
            (
                "/first",
                302,
                vec![
                    ("Location", "/second".into()),
                    ("X-Stall-Head", "300".into()),
                ],
                vec![],
            ),
            (
                "/second",
                200,
                vec![("X-Stall-Head", "300".into())],
                b"ok".to_vec(),
            ),
        ]);
        let e = Request::get(s.url("/first"))
            .timeout(Duration::from_millis(500))
            .send()
            .unwrap_err()
            .chain();
        assert!(e.contains("timed out after 500ms"), "{e}");
        assert_eq!(s.hits(), 2, "the second hop was made, and cut short");
        // With room for both, both go through.
        let r = Request::get(s.url("/first"))
            .timeout(Duration::from_secs(5))
            .send()
            .unwrap();
        assert_eq!(r.text().unwrap(), "ok");
    }
}
