//! The one HTTP client behind [`Download`](super::Download) and
//! [`Request`](super::Request): the `ureq` agent, built once with the
//! [`crate::tls`] provider, and the redirect loop both of them go through.
//!
//! **Redirects are followed here, not by `ureq`.** The agent's own limit is
//! zero, so every 3xx comes back to [`send`], which decides. `ureq` 3.4
//! strips only `Authorization` and `Cookie` when it follows a redirect
//! (`ureq-proto` 0.6, `src/client/redirect.rs`), so a key sent as
//! `X-API-Key` would follow a redirect to any host. Here:
//!
//! - at most [`MAX_REDIRECTS`] hops;
//! - `301`/`302` keep a `GET` or `HEAD` and turn any other method into a
//!   bodiless `GET`; `303` is a bodiless `GET` (a `HEAD` stays `HEAD`);
//!   `307`/`308` keep the method and the body. Browsers and curl turn only
//!   a `POST` into a `GET` on `301`/`302`; this does it for every method
//!   but `GET` and `HEAD` (Rustible's choice, not theirs: browsers keep a
//!   `PUT`, `PATCH` or `DELETE`), so a mutating request is resent only when a
//!   `307`/`308` says the method is to be kept;
//! - when the scheme, host or port changes, every credential is dropped: the
//!   headers given as a [`Secret`], the `.bearer`/`.basic_auth` credentials,
//!   and `Authorization`, `Proxy-Authorization` and `Cookie` however they
//!   were given;
//! - an `https://` to `http://` redirect is refused while any credential is
//!   attached, and so is a `307`/`308` that would resend a secret body to
//!   another origin.

use std::io::Read;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rustible_sdk::prelude::*;
use ureq::http::{self, HeaderName, HeaderValue, Method};

use super::validate_url;

/// The most redirects a request follows before it fails.
pub const MAX_REDIRECTS: usize = 10;

/// The agent every request goes through, built once: an `Agent` carries its
/// connection pool and its lazily built rustls configuration, so building
/// one per request would re-parse the bundled root store each time.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        let tls = ureq::tls::TlsConfig::builder()
            .unversioned_rustls_crypto_provider(crate::tls::provider())
            .build();
        ureq::Agent::config_builder()
            .tls_config(tls)
            .http_status_as_error(false)
            // `send` follows redirects itself; see the module docs.
            .max_redirects(0)
            .allow_non_standard_methods(true)
            .user_agent(concat!("rustible/", env!("CARGO_PKG_VERSION")))
            .build()
            .into()
    })
}

/// A request header's value: plain, or a [`Secret`] that never appears in
/// `Debug`, a diff or a message and is dropped on a redirect to another
/// origin.
#[derive(Clone)]
pub(crate) enum Field {
    Plain(String),
    Secret(Secret),
}

impl std::fmt::Debug for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Field::Plain(v) => write!(f, "{v:?}"),
            Field::Secret(s) => f.write_str(&secret_note(s)),
        }
    }
}

/// How a secret reads wherever it is mentioned: its size and nothing else.
pub(crate) fn secret_note(s: &Secret) -> String {
    format!("<secret, {} bytes>", s.len())
}

/// One request header.
#[derive(Clone, Debug)]
pub(crate) struct HeaderSpec {
    pub(crate) name: String,
    pub(crate) value: Field,
}

impl HeaderSpec {
    /// The header as `ureq` sends it, or why it cannot be sent. An op calls
    /// this from `check` too, so a malformed header is refused before the
    /// wire; a message never quotes a secret value.
    pub(crate) fn to_wire(&self) -> std::result::Result<(HeaderName, HeaderValue), String> {
        let name = HeaderName::from_bytes(self.name.as_bytes())
            .map_err(|_| format!("`{}` is not a valid header name", self.name))?;
        let value = match &self.value {
            Field::Plain(v) => HeaderValue::from_str(v).map_err(|_| {
                format!(
                    "the value of header `{}` is not a valid header value (a line break, or a control character)",
                    self.name
                )
            })?,
            Field::Secret(s) => {
                let text = secret_text(&self.name, s).map_err(|e| e.to_string())?;
                let mut v = HeaderValue::from_str(text).map_err(|_| {
                    format!(
                        "the secret for header `{}` is not a valid header value (a line break, or a control character)",
                        self.name
                    )
                })?;
                v.set_sensitive(true);
                v
            }
        };
        Ok((name, value))
    }

    /// Dropped when a redirect leaves the origin: anything given as a
    /// secret, and the headers that carry credentials whatever their type.
    fn is_credential(&self) -> bool {
        matches!(self.value, Field::Secret(_))
            || ["authorization", "proxy-authorization", "cookie"]
                .iter()
                .any(|n| self.name.eq_ignore_ascii_case(n))
    }
}

/// The text of a secret header value, a trailing newline stripped (a token
/// read from a file usually ends with one).
pub(crate) fn secret_text<'a>(name: &str, s: &'a Secret) -> Result<&'a str> {
    s.as_str()
        .map_err(|_| Error::msg(format!("the secret for header `{name}` is not UTF-8")))
}

/// How long a request may take.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Timeout {
    /// The whole exchange, redirects and body included.
    Total(Duration),
    /// Connecting and receiving the response head, per hop; the body has no
    /// deadline (a large download is slow, not stuck).
    Head(Duration),
}

/// A request as the client sends it.
pub(crate) struct Outgoing<'a> {
    pub(crate) method: Method,
    pub(crate) url: String,
    pub(crate) headers: Vec<HeaderSpec>,
    /// The body and whether it is secret.
    pub(crate) body: Option<(&'a [u8], bool)>,
    pub(crate) follow: bool,
    pub(crate) timeout: Timeout,
    /// Secrets that reach the wire inside a header but are not one whole
    /// (a bearer token, a basic-auth password), so they are scrubbed from
    /// any message on their own too.
    pub(crate) secrets: Vec<Secret>,
}

/// Why reading a body failed: over the size limit, which the op's own
/// message can tell the user how to raise, or anything else.
#[derive(Debug)]
pub(crate) enum ReadError {
    TooLarge(String),
    Other(String),
}

impl ReadError {
    /// The error, with `hint` added to a size-limit message: the op that
    /// owns the limit knows how it is raised, the client does not.
    pub(crate) fn hinted(self, hint: &str) -> Error {
        match self {
            ReadError::TooLarge(m) => Error::msg(format!("{m}; {hint}")),
            ReadError::Other(m) => Error::msg(m),
        }
    }
}

impl From<ReadError> for Error {
    fn from(e: ReadError) -> Error {
        match e {
            ReadError::TooLarge(m) | ReadError::Other(m) => Error::msg(m),
        }
    }
}

/// A response whose head has arrived and whose body is still unread, so a
/// caller can judge the status before reading what may be a large body.
pub(crate) struct Incoming {
    pub(crate) status: u16,
    /// Names lowercased, in the order received, repeats kept.
    pub(crate) headers: Vec<(String, String)>,
    /// The URL that answered, after any redirect, masked.
    pub(crate) url: String,
    /// `<METHOD> <masked url>` of the request as it was made, for messages.
    pub(crate) what: String,
    head: bool,
    deadline: Option<(Instant, Duration)>,
    resp: http::Response<ureq::Body>,
    scrub: Vec<String>,
}

impl Incoming {
    /// The reason phrase for the status, or nothing for a code without one.
    pub(crate) fn reason(&self) -> &'static str {
        http::StatusCode::from_u16(self.status)
            .ok()
            .and_then(|s| s.canonical_reason())
            .unwrap_or("")
    }

    /// The whole body, in memory, at most `max_bytes` when there is a limit:
    /// [`Incoming::body`] read to its end.
    pub(crate) fn read(self, max_bytes: Option<u64>) -> std::result::Result<Vec<u8>, ReadError> {
        let mut body = self.body(max_bytes)?;
        let mut out = Vec::new();
        let mut buf = vec![0; BODY_BUF];
        loop {
            match body.read(&mut buf)? {
                0 => return Ok(out),
                n => out.extend_from_slice(&buf[..n]),
            }
        }
    }

    /// The body as it arrives, for a caller that streams it rather than
    /// holding it. With a limit, a `Content-Length` above it fails here,
    /// before the body is read, and a body without one, or one that lies,
    /// fails as soon as a read passes it. Without one, a body is as large as
    /// the server sends.
    pub(crate) fn body(self, max_bytes: Option<u64>) -> std::result::Result<Body, ReadError> {
        let what = self.what;
        if let Some(max_bytes) = max_bytes
            && let Some(len) = self
                .resp
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok())
            && len > max_bytes
        {
            return Err(ReadError::TooLarge(format!(
                "{what}: the server declares a {len} byte body (Content-Length), over the {max_bytes} byte limit"
            )));
        }
        Ok(Body {
            reader: (!self.head).then(|| self.resp.into_body().into_reader()),
            what,
            max_bytes,
            read: 0,
            deadline: self.deadline,
            scrub: self.scrub,
        })
    }

    /// Up to `n` bytes of the body as one line of text, for a message about
    /// a response that is already a failure: every secret of the request
    /// scrubbed (a server that echoes the request back would otherwise put
    /// them in the error), control characters escaped, whitespace runs
    /// collapsed to one space. A read error ends it early rather than
    /// replacing the failure being reported.
    pub(crate) fn snippet(mut self, n: usize) -> String {
        if self.head {
            return String::new();
        }
        // Read past `n` by the longest secret, so one that straddles the
        // cut is still whole when it is scrubbed.
        let extra = self.scrub.iter().map(String::len).max().unwrap_or(0);
        let mut out = Vec::new();
        let _ = self
            .resp
            .body_mut()
            .as_reader()
            .take((n + extra) as u64)
            .read_to_end(&mut out);
        one_line(&scrub(&String::from_utf8_lossy(&out), &self.scrub), n)
    }
}

/// How much of a body [`Incoming::read`] asks for at a time.
const BODY_BUF: usize = 64 << 10;

/// A response body being read: [`Incoming::body`]. Each read is counted
/// against the limit, if there is one, and a failure is worded as
/// [`Incoming::read`]'s, a timeout named for its deadline and every secret
/// of the request scrubbed.
pub(crate) struct Body {
    /// `None` for a `HEAD`, which has no body.
    reader: Option<ureq::BodyReader<'static>>,
    what: String,
    max_bytes: Option<u64>,
    read: u64,
    deadline: Option<(Instant, Duration)>,
    scrub: Vec<String>,
}

impl Body {
    /// Up to `buf.len()` more bytes of the body, `0` at its end.
    pub(crate) fn read(&mut self, buf: &mut [u8]) -> std::result::Result<usize, ReadError> {
        let Some(reader) = &mut self.reader else {
            return Ok(0);
        };
        let what = &self.what;
        let n = loop {
            match reader.read(buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    // Back to the `ureq::Error` it wraps, so a timeout is
                    // recognised and the text is what reading whole gave.
                    let e = ureq::Error::from(e);
                    let e = timed_out(&e, self.deadline).unwrap_or_else(|| e.to_string());
                    return Err(ReadError::Other(scrub(
                        &format!("{what}: reading the body: {e}"),
                        &self.scrub,
                    )));
                }
            }
        };
        self.read += n as u64;
        if let Some(max_bytes) = self.max_bytes
            && self.read > max_bytes
        {
            return Err(ReadError::TooLarge(format!(
                "{what}: the body is larger than the {max_bytes} byte limit"
            )));
        }
        Ok(n)
    }
}

/// Send `req`, following redirects as the module docs describe. Contacts
/// the network: never call it from an op's `check` (vision 12).
pub(crate) fn send(req: Outgoing<'_>) -> Result<Incoming> {
    let Outgoing {
        mut method,
        mut url,
        mut headers,
        mut body,
        follow,
        timeout,
        secrets,
    } = req;
    let first = format!("{method} {}", mask_url(&url));
    // Every secret is scrubbed from the text of a transport error, and from
    // the quoted body of a failed response, before it becomes a message.
    let scrubs = scrub_list(&url, &headers, body, &secrets);
    let deadline = match timeout {
        Timeout::Total(d) => Some((Instant::now() + d, d)),
        Timeout::Head(_) => None,
    };
    let mut hops = 0;
    loop {
        let what = format!("{method} {}", mask_url(&url));
        let resp = hop(
            &method,
            &url,
            &headers,
            body.map(|(b, _)| b),
            timeout,
            deadline,
        )
        .map_err(|e| Error::msg(scrub(&format!("{what}: {e}"), &scrubs)))?;
        let status = resp.status().as_u16();
        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        if follow && let Some(location) = location {
            let secret_body = body.is_some_and(|(_, secret)| secret);
            let next = follow_redirect(&method, &url, &headers, secret_body, status, &location)
                .map_err(|e| Error::msg(format!("{what}: {e}")))?;
            if let Some(next) = next {
                if hops == MAX_REDIRECTS {
                    bail!(
                        "{first}: stopped after {MAX_REDIRECTS} redirects; the last pointed at {}",
                        mask_url(&next.url)
                    );
                }
                hops += 1;
                if !next.keep_body {
                    body = None;
                }
                (method, url, headers) = (next.method, next.url, next.headers);
                continue;
            }
        }
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        return Ok(Incoming {
            status,
            headers,
            url: mask_url(&url),
            what: first,
            head: method == Method::HEAD,
            deadline,
            resp,
            scrub: scrubs,
        });
    }
}

/// The request a redirect leads to.
#[derive(Debug)]
pub(crate) struct Next {
    pub(crate) method: Method,
    pub(crate) url: String,
    pub(crate) headers: Vec<HeaderSpec>,
    /// Whether the body goes with it (`307`, `308`, or a `GET`/`HEAD`
    /// that had none).
    pub(crate) keep_body: bool,
}

/// Decide a redirect: `None` when `status` is not one to follow, the next
/// request when it is, and why not when it is refused. Pure: everything the
/// module docs promise about redirects is decided here, and the loop in
/// [`send`] only carries it out.
pub(crate) fn follow_redirect(
    method: &Method,
    url: &str,
    headers: &[HeaderSpec],
    secret_body: bool,
    status: u16,
    location: &str,
) -> std::result::Result<Option<Next>, String> {
    let Some((next_method, keep_body)) = redirect_method(status, method) else {
        return Ok(None);
    };
    let next = resolve_location(url, location)
        .map_err(|e| format!("redirect to `{}`: {e}", mask_url(location)))?;
    validate_url(&next).map_err(|e| format!("redirect refused: {e}"))?;
    let secret_body = keep_body && secret_body;
    let attached =
        headers.iter().any(HeaderSpec::is_credential) || secret_body || userinfo(url).is_some();
    let err = |e: Error| e.to_string();
    if let Some(why) = refuse_redirect(url, &next, attached, secret_body).map_err(err)? {
        return Err(why);
    }
    let mut headers = headers.to_vec();
    if Origin::of(url).map_err(err)? != Origin::of(&next).map_err(err)? {
        headers.retain(|h| !h.is_credential());
    }
    if !keep_body {
        headers.retain(|h| !h.name.eq_ignore_ascii_case("content-type"));
    }
    Ok(Some(Next {
        method: next_method,
        url: next,
        headers,
        keep_body,
    }))
}

/// One exchange, no redirect followed.
fn hop(
    method: &Method,
    url: &str,
    headers: &[HeaderSpec],
    body: Option<&[u8]>,
    timeout: Timeout,
    deadline: Option<(Instant, Duration)>,
) -> std::result::Result<http::Response<ureq::Body>, String> {
    let mut builder = http::Request::builder().method(method.clone()).uri(url);
    for h in headers {
        let (name, value) = h.to_wire()?;
        builder = builder.header(name, value);
    }
    let remaining = match deadline {
        Some((at, total)) => {
            let left = at.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(format!("timed out after {total:?}"));
            }
            Some(left)
        }
        None => None,
    };
    let result = match body {
        Some(bytes) => run(builder.body(bytes), timeout, remaining)?,
        None => run(builder.body(()), timeout, remaining)?,
    };
    result.map_err(|e| {
        timed_out(&e, deadline)
            .or_else(|| untrusted_certificate(&e))
            .unwrap_or_else(|| e.to_string())
    })
}

/// A certificate that failed verification, with what to do about it.
fn untrusted_certificate(e: &ureq::Error) -> Option<String> {
    let tls = match e {
        ureq::Error::Rustls(t) => t,
        ureq::Error::Io(io) => io.get_ref()?.downcast_ref::<rustls::Error>()?,
        _ => return None,
    };
    match tls {
        rustls::Error::InvalidCertificate(why) => Some(format!(
            "the server's TLS certificate failed verification ({why:?}): it is not trusted \
             by Mozilla's roots (self-signed, or from a private CA?), has expired, or names \
             another host. Verification is never skipped; for an admin API on the target \
             itself, use http://127.0.0.1"
        )),
        _ => None,
    }
}

/// Run one built request on the shared agent with its timeouts.
fn run<B: ureq::AsSendBody>(
    req: std::result::Result<http::Request<B>, http::Error>,
    timeout: Timeout,
    remaining: Option<Duration>,
) -> std::result::Result<std::result::Result<http::Response<ureq::Body>, ureq::Error>, String> {
    let req = req.map_err(|e| e.to_string())?;
    let agent = agent();
    let config = agent.configure_request(req).timeout_global(remaining);
    let config = match timeout {
        Timeout::Head(d) => config
            .timeout_connect(Some(d))
            .timeout_recv_response(Some(d)),
        Timeout::Total(_) => config,
    };
    Ok(agent.run(config.build()))
}

/// A global-deadline error says what the deadline was, rather than ureq's
/// name for the phase it happened to fall in.
fn timed_out(e: &ureq::Error, deadline: Option<(Instant, Duration)>) -> Option<String> {
    match (e, deadline) {
        (ureq::Error::Timeout(_), Some((_, total))) => Some(format!(
            "timed out after {total:?} (the whole request, body included; raise it with .timeout())"
        )),
        _ => None,
    }
}

/// `text` with every non-empty string in `secrets` replaced, longest first
/// so a secret that contains another is replaced whole.
pub(crate) fn scrub(text: &str, secrets: &[String]) -> String {
    let mut sorted: Vec<&String> = secrets.iter().filter(|s| !s.is_empty()).collect();
    sorted.sort_by_key(|s| std::cmp::Reverse(s.len()));
    sorted
        .into_iter()
        .fold(text.to_string(), |t, s| t.replace(s.as_str(), "<secret>"))
}

/// Everything a message must never quote: each secret header's value whole
/// and, for a value with a scheme in front (`Bearer x`, `Basic x`), the part
/// after it; the `extra` secrets; a secret body; and the URL's userinfo
/// whole, its password, and a lone user part (often the token itself).
pub(crate) fn scrub_list(
    url: &str,
    headers: &[HeaderSpec],
    body: Option<(&[u8], bool)>,
    extra: &[Secret],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |s: &str| {
        let s = s.trim_end_matches(['\r', '\n']);
        if !s.is_empty() && !out.iter().any(|o| o == s) {
            out.push(s.to_string());
        }
    };
    for h in headers {
        if let Field::Secret(s) = &h.value
            && let Ok(v) = s.as_str()
        {
            add(v);
            if let Some((_, rest)) = v.split_once(' ') {
                add(rest.trim());
            }
        }
    }
    for s in extra {
        if let Ok(v) = s.as_str() {
            add(v);
        }
    }
    if let Some((bytes, true)) = body
        && let Ok(v) = std::str::from_utf8(bytes)
    {
        add(v);
    }
    if let Some(info) = userinfo(url) {
        add(info);
        if let Some((_, password)) = info.split_once(':') {
            add(password);
        }
    }
    out
}

/// `text` as one line of at most `max` bytes: control characters escaped
/// (`\u{1b}`), every run of whitespace a single space, a cut marked `...`.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    if out.len() > max {
        let mut cut = max;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("...");
    }
    out
}

// ---- pure ----

/// The URL's `user:pass` (or `user`), if it has one.
pub(crate) fn userinfo(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (info, _) = rest[..end].rsplit_once('@')?;
    Some(info)
}

/// `url` with any userinfo masked, as Ansible's `uri` masks it:
/// `https://user:********@host/`, or `https://********@host/` when there is
/// no password (a lone user part is often the token itself). Every message
/// and diff that names a URL goes through this.
pub fn mask_url(url: &str) -> String {
    let Some(info) = userinfo(url) else {
        return url.to_string();
    };
    // The userinfo starts right after `://`.
    let at = url.find("://").expect("userinfo found one") + 3;
    let masked = match info.split_once(':') {
        Some((user, _)) => format!("{user}:********"),
        None => "********".to_string(),
    };
    format!("{}{masked}{}", &url[..at], &url[at + info.len()..])
}

/// Scheme, host and port: what a redirect must keep for credentials to
/// follow it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl Origin {
    pub(crate) fn of(url: &str) -> Result<Origin> {
        let uri: http::Uri = url
            .parse()
            .map_err(|e| Error::msg(format!("`{}` is not a valid URL: {e}", mask_url(url))))?;
        let scheme = uri.scheme_str().unwrap_or_default().to_ascii_lowercase();
        let host = uri
            .host()
            .ok_or_else(|| Error::msg(format!("`{}` names no host", mask_url(url))))?
            .to_ascii_lowercase();
        let port = uri
            .port_u16()
            .unwrap_or(if scheme == "https" { 443 } else { 80 });
        Ok(Origin { scheme, host, port })
    }
}

/// Why a redirect from `from` to `to` is refused, if it is: a downgrade
/// from `https://` to `http://` while a credential is attached, or a secret
/// body that a `307`/`308` would resend to another origin.
pub(crate) fn refuse_redirect(
    from: &str,
    to: &str,
    attached: bool,
    secret_body: bool,
) -> Result<Option<String>> {
    let (a, b) = (Origin::of(from)?, Origin::of(to)?);
    if attached && a.scheme == "https" && b.scheme == "http" {
        return Ok(Some(format!(
            "refusing the redirect from {} to {}: it leaves https for plain http while a secret \
             or a credential is attached; ask for the https URL, or send no secret",
            mask_url(from),
            mask_url(to)
        )));
    }
    if secret_body && a != b {
        return Ok(Some(format!(
            "refusing the redirect from {} to {}: it would send the secret body to another \
             origin; request the final URL directly",
            mask_url(from),
            mask_url(to)
        )));
    }
    Ok(None)
}

/// The method a redirect with this status continues with, and whether the
/// body goes with it; `None` for a status that is not a redirect to follow.
pub(crate) fn redirect_method(status: u16, method: &Method) -> Option<(Method, bool)> {
    let read = *method == Method::GET || *method == Method::HEAD;
    match status {
        301 | 302 if read => Some((method.clone(), true)),
        301 | 302 => Some((Method::GET, false)),
        303 if *method == Method::HEAD => Some((Method::HEAD, false)),
        303 => Some((Method::GET, false)),
        307 | 308 => Some((method.clone(), true)),
        _ => None,
    }
}

/// Resolve a `Location` against the URL that sent it (RFC 3986 section 5),
/// dropping any fragment.
pub(crate) fn resolve_location(base: &str, location: &str) -> std::result::Result<String, String> {
    let location = location.trim();
    let location = location.split('#').next().unwrap_or_default();
    let (scheme, rest) = base
        .split_once("://")
        .ok_or_else(|| format!("`{}` has no scheme", mask_url(base)))?;
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let tail = &rest[auth_end..];
    let tail = tail.split('#').next().unwrap_or_default();
    let (path, _query) = tail.split_once('?').unwrap_or((tail, ""));

    let has_scheme = location.split_once(':').is_some_and(|(s, _)| {
        !s.is_empty()
            && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    if has_scheme {
        return Ok(location.to_string());
    }
    if let Some(net) = location.strip_prefix("//") {
        return Ok(format!("{scheme}://{net}"));
    }
    if location.is_empty() {
        return Ok(format!("{scheme}://{authority}{}", tail));
    }
    if location.starts_with('?') {
        return Ok(format!("{scheme}://{authority}{path}{location}"));
    }
    let (loc_path, loc_query) = match location.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (location, None),
    };
    let merged = if loc_path.starts_with('/') {
        loc_path.to_string()
    } else {
        let dir = match path.rfind('/') {
            Some(i) => &path[..=i],
            None => "/",
        };
        format!("{dir}{loc_path}")
    };
    let mut out = format!("{scheme}://{authority}{}", remove_dot_segments(&merged));
    if let Some(q) = loc_query {
        out.push('?');
        out.push_str(q);
    }
    Ok(out)
}

/// RFC 3986 section 5.2.4, on a path that starts with `/`.
fn remove_dot_segments(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match *seg {
            "." => {
                if last {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// Standard base64 with padding, for `Authorization: Basic`.
pub(crate) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            3 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8 | chunk[2] as u32,
            2 => (chunk[0] as u32) << 16 | (chunk[1] as u32) << 8,
            _ => (chunk[0] as u32) << 16,
        };
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- pure ----

    #[test]
    fn userinfo_is_masked_everywhere_it_can_be() {
        assert_eq!(mask_url("http://h/x"), "http://h/x");
        assert_eq!(
            mask_url("https://bob:hunter2@h:8384/rest?x=1"),
            "https://bob:********@h:8384/rest?x=1"
        );
        assert_eq!(mask_url("https://tok3n@h/"), "https://********@h/");
        // An `@` after the authority is not userinfo.
        assert_eq!(mask_url("http://h/a@b"), "http://h/a@b");
        assert_eq!(mask_url("http://h?q=a@b"), "http://h?q=a@b");
        // The last `@` in the authority ends the userinfo.
        assert_eq!(mask_url("http://a@b:c@h/"), "http://a@b:********@h/");
        // The user part may also appear in the scheme or the host.
        assert_eq!(mask_url("http://h@h/"), "http://********@h/");
    }

    #[test]
    fn origin_is_scheme_host_and_port() {
        let o = |u| Origin::of(u).unwrap();
        assert_eq!(o("http://H/x"), o("http://h:80/y"));
        assert_eq!(o("https://h/x"), o("https://u:p@h:443/"));
        assert_ne!(o("http://h/"), o("https://h/"));
        assert_ne!(o("http://h:8080/"), o("http://h/"));
        assert_ne!(o("http://a/"), o("http://b/"));
    }

    #[test]
    fn redirect_methods_follow_browsers_and_curl() {
        let g = Method::GET;
        let h = Method::HEAD;
        let p = Method::POST;
        let patch = Method::PATCH;
        assert_eq!(redirect_method(301, &g), Some((g.clone(), true)));
        assert_eq!(redirect_method(302, &h), Some((h.clone(), true)));
        assert_eq!(redirect_method(301, &p), Some((g.clone(), false)));
        assert_eq!(redirect_method(302, &patch), Some((g.clone(), false)));
        assert_eq!(redirect_method(303, &p), Some((g.clone(), false)));
        assert_eq!(redirect_method(303, &g), Some((g.clone(), false)));
        assert_eq!(redirect_method(303, &h), Some((h.clone(), false)));
        assert_eq!(redirect_method(307, &p), Some((p.clone(), true)));
        assert_eq!(redirect_method(308, &patch), Some((patch.clone(), true)));
        for not in [200, 204, 300, 304, 305, 404] {
            assert_eq!(redirect_method(not, &g), None, "{not}");
        }
    }

    #[test]
    fn a_downgrade_is_refused_only_with_a_credential_attached() {
        let why = refuse_redirect("https://h/a", "http://h/b", true, false)
            .unwrap()
            .unwrap();
        assert!(why.contains("https://h/a to http://h/b"), "{why}");
        assert!(why.contains("plain http"), "{why}");
        // Masked in the message.
        let why = refuse_redirect("https://u:pw@h/a", "http://h/b", true, false)
            .unwrap()
            .unwrap();
        assert!(why.contains("https://u:********@h/a"), "{why}");
        assert!(!why.contains("pw"), "{why}");
        assert_eq!(
            refuse_redirect("https://h/a", "http://h/b", false, false).unwrap(),
            None
        );
        assert_eq!(
            refuse_redirect("http://h/a", "https://h/b", true, false).unwrap(),
            None,
            "an upgrade is fine"
        );
        assert_eq!(
            refuse_redirect("https://h/a", "https://other/b", true, false).unwrap(),
            None,
            "another origin drops the credentials instead"
        );
        // A secret body is never resent to another origin.
        assert!(
            refuse_redirect("http://h/a", "http://other/b", true, true)
                .unwrap()
                .unwrap()
                .contains("secret body")
        );
        assert_eq!(
            refuse_redirect("http://h/a", "http://h/b", true, true).unwrap(),
            None
        );
    }

    fn headers() -> Vec<HeaderSpec> {
        vec![
            HeaderSpec {
                name: "X-API-Key".into(),
                value: Field::Secret(Secret::new("k3y")),
            },
            HeaderSpec {
                name: "Authorization".into(),
                value: Field::Secret(Secret::new("Bearer t")),
            },
            HeaderSpec {
                name: "Cookie".into(),
                value: Field::Plain("s=1".into()),
            },
            HeaderSpec {
                name: "proxy-authorization".into(),
                value: Field::Plain("Basic cHJveHk6cHc=".into()),
            },
            HeaderSpec {
                name: "Accept".into(),
                value: Field::Plain("application/json".into()),
            },
            HeaderSpec {
                name: "Content-Type".into(),
                value: Field::Plain("application/json".into()),
            },
        ]
    }

    fn names(next: &Next) -> Vec<&str> {
        next.headers.iter().map(|h| h.name.as_str()).collect()
    }

    /// The redirect decision on its own, every rule of it, with no socket:
    /// what the loopback tests in `request.rs` see end to end, and the
    /// `https://` cases they cannot reach without a TLS server.
    #[test]
    fn a_redirect_to_another_origin_drops_every_credential() {
        let get = Method::GET;
        let next = follow_redirect(&get, "http://a/x", &headers(), false, 302, "http://b/y")
            .unwrap()
            .unwrap();
        assert_eq!(next.url, "http://b/y");
        assert_eq!(names(&next), ["Accept", "Content-Type"]);
        // Another port is another origin.
        let next = follow_redirect(&get, "http://a/x", &headers(), false, 302, "http://a:81/y")
            .unwrap()
            .unwrap();
        assert_eq!(names(&next), ["Accept", "Content-Type"]);
        // So is another scheme, upgrading.
        let next = follow_redirect(&get, "http://a/x", &headers(), false, 301, "https://a/x")
            .unwrap()
            .unwrap();
        assert_eq!(names(&next), ["Accept", "Content-Type"]);
        // The same origin keeps them all.
        let next = follow_redirect(&get, "http://a/x", &headers(), false, 307, "/y")
            .unwrap()
            .unwrap();
        assert_eq!(next.url, "http://a/y");
        assert_eq!(next.headers.len(), 6);
    }

    #[test]
    fn a_downgrade_with_a_credential_attached_is_refused() {
        let get = Method::GET;
        let err =
            follow_redirect(&get, "https://u:pw@a/x", &[], false, 302, "http://a/y").unwrap_err();
        assert!(
            err.contains("https://u:********@a/x to http://a/y"),
            "{err}"
        );
        let err =
            follow_redirect(&get, "https://a/x", &headers(), false, 302, "http://b/y").unwrap_err();
        assert!(err.contains("leaves https for plain http"), "{err}");
        // Nothing secret attached: followed.
        let plain = &headers()[4..];
        assert!(
            follow_redirect(&get, "https://a/x", plain, false, 302, "http://a/y")
                .unwrap()
                .is_some()
        );
        // A secret body that a 307 would resend counts; one a 303 drops
        // does not.
        let post = Method::POST;
        assert!(follow_redirect(&post, "https://a/x", plain, true, 307, "http://a/y").is_err());
        let next = follow_redirect(&post, "https://a/x", plain, true, 303, "http://a/y")
            .unwrap()
            .unwrap();
        assert_eq!((&next.method, next.keep_body), (&Method::GET, false));
        assert_eq!(names(&next), ["Accept"], "no body, so no Content-Type");
    }

    #[test]
    fn a_redirect_that_is_not_one_or_leaves_http_is_not_followed() {
        let get = Method::GET;
        assert!(
            follow_redirect(&get, "http://a/", &headers(), false, 200, "/x")
                .unwrap()
                .is_none()
        );
        assert!(
            follow_redirect(&get, "http://a/", &headers(), false, 304, "/x")
                .unwrap()
                .is_none()
        );
        let err = follow_redirect(
            &get,
            "http://a/",
            &headers(),
            false,
            302,
            "file:///etc/passwd",
        )
        .unwrap_err();
        assert!(err.contains("redirect refused"), "{err}");
    }

    #[test]
    fn locations_resolve_against_the_url_that_sent_them() {
        let r = |b, l| resolve_location(b, l).unwrap();
        assert_eq!(r("http://h/a/b?q", "https://x/y"), "https://x/y");
        assert_eq!(r("https://h/a/b", "//x:81/y"), "https://x:81/y");
        assert_eq!(r("http://h:8/a/b?q", "/c"), "http://h:8/c");
        assert_eq!(r("http://h/a/b?q", "c?d=1"), "http://h/a/c?d=1");
        assert_eq!(r("http://h/a/b", "../c"), "http://h/c");
        assert_eq!(r("http://h/a/b/", "./c"), "http://h/a/b/c");
        assert_eq!(r("http://h", "c"), "http://h/c");
        assert_eq!(r("http://h/a?x", "?y"), "http://h/a?y");
        assert_eq!(r("http://h/a#f", "/b#g"), "http://h/b");
        assert_eq!(r("http://u:p@h/a", "/b"), "http://u:p@h/b");
        assert_eq!(r("http://h/a", "ftp://x/"), "ftp://x/");
    }

    #[test]
    fn credential_headers_are_known_by_type_and_by_name() {
        let h = |n: &str, v: Field| HeaderSpec {
            name: n.into(),
            value: v,
        };
        assert!(h("X-API-Key", Field::Secret(Secret::new("k"))).is_credential());
        assert!(h("authorization", Field::Plain("Bearer x".into())).is_credential());
        assert!(h("Cookie", Field::Plain("a=b".into())).is_credential());
        assert!(!h("Accept", Field::Plain("*/*".into())).is_credential());
    }

    #[test]
    fn a_failed_certificate_check_says_what_to_do() {
        use rustls::CertificateError;
        let bad = || rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer);
        // As ureq reports a handshake failure: inside an I/O error.
        let io = ureq::Error::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, bad()));
        let direct = ureq::Error::Rustls(bad());
        for e in [io, direct] {
            let m = untrusted_certificate(&e).unwrap();
            assert!(m.contains("UnknownIssuer"), "{m}");
            assert!(m.contains("Mozilla's roots"), "{m}");
            assert!(m.contains("never skipped"), "{m}");
            assert!(m.contains("http://127.0.0.1"), "{m}");
        }
        let other = ureq::Error::Rustls(rustls::Error::HandshakeNotComplete);
        assert_eq!(untrusted_certificate(&other), None);
        let refused = ureq::Error::Io(std::io::ErrorKind::ConnectionRefused.into());
        assert_eq!(untrusted_certificate(&refused), None);
    }

    #[test]
    fn one_line_escapes_controls_and_collapses_whitespace() {
        assert_eq!(one_line("  a\n\n b\t\tc \r\n", 100), "a b c");
        assert_eq!(one_line("a\x1bb\x07", 100), "a\\u{1b}b\\u{7}");
        assert_eq!(one_line("abcdef", 3), "abc...");
        assert_eq!(one_line("ééé", 3), "é...", "cut on a character boundary");
        assert_eq!(one_line("", 3), "");
    }

    /// What a message must never quote, every form of it.
    #[test]
    fn the_scrub_list_has_every_form_of_every_secret() {
        let h = |n: &str, v: &str| HeaderSpec {
            name: n.into(),
            value: Field::Secret(Secret::new(v)),
        };
        let list = scrub_list(
            "https://bob:pw0rd@h/x",
            &[
                h("X-API-Key", "k3y\n"),
                h("Authorization", "Basic Ym9iOnB3"),
            ],
            Some((b"body-s3cret", true)),
            &[Secret::new("raw-t0ken")],
        );
        for want in [
            "k3y",
            "Basic Ym9iOnB3",
            "Ym9iOnB3",
            "raw-t0ken",
            "body-s3cret",
            "bob:pw0rd",
            "pw0rd",
        ] {
            assert!(list.iter().any(|s| s == want), "{want}: {list:?}");
        }
        // A plain body is not a secret; a lone user part is.
        assert!(scrub_list("http://h/", &[], Some((b"plain", false)), &[]).is_empty());
        assert_eq!(scrub_list("http://t0k@h/", &[], None, &[]), ["t0k"]);
        // Longest first, so `Basic x` goes whole rather than leaving `Basic`.
        assert_eq!(
            scrub("Basic Ym9iOnB3 and Ym9iOnB3", &list),
            "<secret> and <secret>"
        );
    }

    #[test]
    fn base64_matches_rfc_4648() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("martin:secret", "bWFydGluOnNlY3JldA=="),
        ] {
            assert_eq!(base64(plain.as_bytes()), enc);
        }
    }

    #[test]
    fn scrub_replaces_every_secret() {
        assert_eq!(
            scrub("a tok b tok", &["tok".to_string()]),
            "a <secret> b <secret>"
        );
    }

    #[test]
    fn a_secret_header_value_debugs_as_its_size() {
        let h = HeaderSpec {
            name: "X-API-Key".into(),
            value: Field::Secret(Secret::new("hunter2")),
        };
        let dbg = format!("{h:?}");
        assert!(dbg.contains("<secret, 7 bytes>"), "{dbg}");
        assert!(!dbg.contains("hunter2"), "{dbg}");
    }
}
