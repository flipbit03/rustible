//! Downloads over HTTP(S). Ansible's `ansible.builtin.get_url`.
//!
//! [`Download`] fetches a URL into a file on the target with `ureq` over
//! `rustls`, pure Rust all the way down (vision 5.3): the TLS crypto provider
//! is `rustls-graviola`, from [`crate::tls`], because rustls's default
//! providers (`ring`, `aws-lc-rs`) bundle C and do not cross-link with
//! `rust-lld`. Certificates are checked against Mozilla's bundled roots
//! (`webpki-roots`); there is no `validate_certs: no`.
//!
//! Graviola asserts on the CPU extensions it needs, so an `https://` `apply`
//! runs [`tls::preflight_url`](crate::tls::preflight_url) first and fails the
//! step with a readable error on a machine below the floor (pre-Broadwell
//! x86_64, Raspberry Pi 4 and earlier). It never panics mid-run, and there is
//! no fallback provider. A plain `http://` download is not gated: it never
//! reaches the provider, so it keeps working on those machines.
//!
//! `check` never touches the network. It decides from the file on disk
//! whether a download is due, so a dry run is fast and honest (vision 12).

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;
use sha2::Digest;

use crate::file::{Owner, apply_attrs, plan_attrs, write_with_backup};

/// Default connect and response-header timeout. Ansible's `timeout` is 10s.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default ceiling on a response body, raised or lowered with
/// [`Download::max_bytes`]. The body is held in memory before the atomic
/// write, so an unbounded download is an out-of-memory kill on the target,
/// not a slow one. One gibibyte fits the release tarballs this op is for.
pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;

/// A checksum algorithm the `.checksum("<algorithm>:<hex>")` option accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    Sha224,
    Sha256,
    Sha384,
    Sha512,
}

impl Algorithm {
    /// Hex digits a digest of this algorithm has.
    pub fn hex_len(self) -> usize {
        match self {
            Algorithm::Sha224 => 56,
            Algorithm::Sha256 => 64,
            Algorithm::Sha384 => 96,
            Algorithm::Sha512 => 128,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Algorithm::Sha224 => "sha224",
            Algorithm::Sha256 => "sha256",
            Algorithm::Sha384 => "sha384",
            Algorithm::Sha512 => "sha512",
        }
    }
}

/// An expected digest, as given to [`Download::checksum`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checksum {
    pub algorithm: Algorithm,
    /// Lowercase hex.
    pub hex: String,
}

/// Parse `"<algorithm>:<hex>"` (Ansible's `checksum` format). Pure. The
/// algorithm is one of `sha224`, `sha256`, `sha384`, `sha512`; `md5` and
/// `sha1` are not offered because they no longer prove anything about a
/// download. The hex may be upper case and must have the algorithm's
/// length. Ansible's `sha256:<url>` (fetch the digest from a URL) is not
/// supported.
pub fn parse_checksum(spec: &str) -> std::result::Result<Checksum, String> {
    let Some((algo, hex)) = spec.split_once(':') else {
        return Err(format!(
            "checksum `{spec}` is not `<algorithm>:<hex>`; algorithms: sha224, sha256, sha384, sha512"
        ));
    };
    let algorithm = match algo.trim().to_ascii_lowercase().as_str() {
        "sha224" => Algorithm::Sha224,
        "sha256" => Algorithm::Sha256,
        "sha384" => Algorithm::Sha384,
        "sha512" => Algorithm::Sha512,
        other => {
            return Err(format!(
                "checksum algorithm `{other}` is not supported; use sha224, sha256, sha384 or sha512"
            ));
        }
    };
    let hex = hex.trim().to_ascii_lowercase();
    if hex.len() != algorithm.hex_len() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "checksum `{spec}`: a {} digest is {} hex digits, got {} characters",
            algorithm.name(),
            algorithm.hex_len(),
            hex.len()
        ));
    }
    Ok(Checksum { algorithm, hex })
}

/// Lowercase hex digest of `bytes`. Pure.
pub fn digest(algorithm: Algorithm, bytes: &[u8]) -> String {
    fn hex(d: impl AsRef<[u8]>) -> String {
        d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }
    match algorithm {
        Algorithm::Sha224 => hex(sha2::Sha224::digest(bytes)),
        Algorithm::Sha256 => hex(sha2::Sha256::digest(bytes)),
        Algorithm::Sha384 => hex(sha2::Sha384::digest(bytes)),
        Algorithm::Sha512 => hex(sha2::Sha512::digest(bytes)),
    }
}

/// Why a URL cannot be downloaded. Pure. Only `http://` and `https://`.
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
    if let Some(scheme) = scheme {
        if url.len() <= scheme.len() || url.contains(char::is_whitespace) {
            return Err(format!("`{url}` is not a valid http(s) URL"));
        }
        return Ok(());
    }
    if lower.starts_with("file:") {
        return Err(format!(
            "`{url}`: file:// URLs are not supported; use file::Copy::from_local_path"
        ));
    }
    Err(format!("`{url}` is not an http:// or https:// URL"))
}

/// Ensure a URL's content is at `dest`. Ansible's `get_url` with `url`,
/// `dest`, `checksum`, `mode`, `owner`/`group`, `force`, `backup`,
/// `timeout`, `headers`.
///
/// ```no_run
/// # use rustible_std::http;
/// let op = http::Download::get("https://example.com/tool-1.2.tar.gz")
///     .to("/opt/src/tool-1.2.tar.gz")
///     .checksum("sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
///     .mode(0o644);
/// ```
///
/// **When it downloads.** `check` looks only at the file on disk:
///
/// | `dest`            | `.checksum` | `.force` | result |
/// |-------------------|-------------|----------|--------|
/// | missing           | any         | any      | download |
/// | exists, matches   | given       | any      | `ok` (a matching file is never re-fetched) |
/// | exists, differs   | given       | any      | download |
/// | exists            | none        | `false`  | `ok` |
/// | exists            | none        | `true`   | download |
///
/// This is Ansible's behaviour too: without `force: yes` (the default) or a
/// checksum, `get_url` does not re-download a file that already exists, so
/// a URL whose content moves is only tracked through `.checksum` or
/// `.force(true)`. A `.mode`/`.owner` that differs is fixed without a
/// download and shown as an attribute diff.
///
/// **Honesty.** The write is atomic (`sys.write_atomic`): the old file
/// stays untouched until the new bytes are complete. A checksum mismatch
/// after the download fails the step and writes nothing. A non-2xx status
/// fails the step naming the status and the URL. Redirects are followed.
/// `check` does not predict the report when a download is due (size and
/// digest are unknown until fetched), so a check-mode run that chains from
/// the step stops with the vision's clear message (vision 12); an
/// attributes-only change predicts.
///
/// **Limits.** The body is held in memory before the atomic write, so this
/// op is for release tarballs, not disk images, and a body over
/// [`DEFAULT_MAX_BYTES`] fails rather than filling the target's memory.
/// Raise or lower that with [`Download::max_bytes`]. Fails if `dest` exists
/// and is not a regular file, or if its parent directory does not exist
/// (vision 6.7: create it with [`crate::file::Directory`]).
#[derive(Debug, Clone)]
pub struct Download {
    url: String,
    dest: PathBuf,
    checksum: Option<String>,
    mode: Option<u32>,
    owner: Option<Owner>,
    force: bool,
    backup: bool,
    timeout: Duration,
    max_bytes: u64,
    headers: Vec<(String, String)>,
}

/// A `Download` with a URL but no destination yet; `.to(dest)` finishes it.
#[derive(Debug, Clone)]
pub struct DownloadBuilder {
    url: String,
}

/// Output of [`Download`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadReport {
    pub url: String,
    pub path: PathBuf,
    /// Whether the URL was (or would be) fetched, as opposed to an
    /// attributes-only change or nothing at all.
    pub downloaded: bool,
    /// Size of the file now at `path`.
    pub bytes: u64,
    /// SHA-256 of the file now at `path`, whatever `.checksum` asked for.
    ///
    /// `None` when the op did not have to read the file: no `.checksum`
    /// was configured and the file was already in place, so hashing it
    /// would have decided nothing and cost a full read of, say, a 500 MB
    /// tarball on every run, check mode included. Always `Some` after a
    /// download.
    pub sha256: Option<String>,
    /// Set only when `.backup(true)` and a previous version was saved.
    pub backup_path: Option<PathBuf>,
}

impl Download {
    /// The URL to fetch. `http://` or `https://`.
    pub fn get(url: impl Into<String>) -> DownloadBuilder {
        DownloadBuilder { url: url.into() }
    }

    /// Expected digest as `"<algorithm>:<hex>"`, see [`parse_checksum`]. A
    /// file on disk with this digest is `ok` without a download; a
    /// downloaded body with another digest fails the step.
    pub fn checksum(mut self, spec: impl Into<String>) -> Self {
        self.checksum = Some(spec.into());
        self
    }

    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Numeric owner (`chown uid:gid`).
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some(Owner { uid, gid });
        self
    }

    /// Download even when `dest` exists (Ansible's `force: yes`). A matching
    /// `.checksum` still wins: that file is not re-fetched.
    pub fn force(mut self, on: bool) -> Self {
        self.force = on;
        self
    }

    /// Keep a copy of the previous file next to it before overwriting
    /// (`<name>.~rustible.<unix-ts>`).
    pub fn backup(mut self, on: bool) -> Self {
        self.backup = on;
        self
    }

    /// Connect and response-header timeout (default [`DEFAULT_TIMEOUT`]).
    /// The body itself has no deadline, so a slow large download is not cut.
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = d;
        self
    }

    /// Largest response body to accept, in bytes (default
    /// [`DEFAULT_MAX_BYTES`]). The body is buffered in memory before the
    /// atomic write, so this is what stands between a hostile or misbehaving
    /// server and an out-of-memory kill on the target. A `Content-Length`
    /// above it fails before the body is read; a response without one, or
    /// one that lies, fails as soon as the read passes it.
    pub fn max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = n;
        self
    }

    /// An extra request header, e.g. `("Authorization", "Bearer ...")`.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn parsed_checksum(&self) -> Result<Option<Checksum>> {
        match &self.checksum {
            None => Ok(None),
            Some(spec) => parse_checksum(spec).map(Some).map_err(Error::msg),
        }
    }

    /// The file as it is, or why it must be fetched.
    fn content_state(
        &self,
        sys: &System,
        checksum: Option<&Checksum>,
    ) -> Result<(Option<rustible_sdk::backend::Stat>, ContentState)> {
        let stat = sys.stat(&self.dest)?;
        let Some(stat) = stat else {
            let parent = self.dest.parent().unwrap_or(Path::new("/"));
            match sys.stat_follow(parent)? {
                Some(s) if s.kind == FileKind::Dir => {}
                Some(_) => bail!("{} is not a directory", parent.display()),
                None => bail!(
                    "{} does not exist; create it first with file::Directory",
                    parent.display()
                ),
            }
            return Ok((None, ContentState::Missing));
        };
        if stat.kind != FileKind::File {
            bail!(
                "{} exists and is not a regular file ({:?}); remove it first with file::Absent",
                self.dest.display(),
                stat.kind
            );
        }
        // The file is read and hashed only when a `.checksum` makes the
        // digest decide something. Without one the answer is `ok` (or a
        // re-download under `.force`) whatever the bytes are, so hashing
        // would cost a full read of the destination on every run, check
        // mode included, to fill a report field nobody asked for.
        let state = match checksum {
            Some(c) => {
                let bytes = sys.read(&self.dest)?;
                let sha256 = digest(Algorithm::Sha256, &bytes);
                let actual = if c.algorithm == Algorithm::Sha256 {
                    sha256.clone()
                } else {
                    digest(c.algorithm, &bytes)
                };
                if actual == c.hex {
                    ContentState::Current {
                        sha256: Some(sha256),
                    }
                } else {
                    ContentState::Stale(format!(
                        "{} is {}..., want {}...",
                        c.algorithm.name(),
                        &actual[..12],
                        &c.hex[..12]
                    ))
                }
            }
            None if self.force => ContentState::Stale("force".into()),
            None => ContentState::Current { sha256: None },
        };
        Ok((Some(stat), state))
    }

    fn fetch(&self) -> Result<Vec<u8>> {
        // Before anything reaches the handshake: graviola panics on a CPU
        // without the extensions it needs, and a panic here would take the
        // whole playbook down instead of failing this step. Only `https://`
        // is gated; a plain HTTP download never touches the provider.
        crate::tls::preflight_url("http::Download", &self.url)?;
        let agent = agent(self.timeout);
        let mut req = agent.get(&self.url);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        let mut resp = req
            .call()
            .map_err(|e| Error::msg(format!("GET {}: {e}", self.url)))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("GET {} returned {status}", self.url);
        }
        // Refuse before reading when the server declares a size over the
        // ceiling. A missing or lying `Content-Length` is caught below.
        if let Some(len) = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            && len > self.max_bytes
        {
            bail!(
                "GET {}: Content-Length {len} exceeds the {} byte limit; raise it with .max_bytes()",
                self.url,
                self.max_bytes
            );
        }
        // `limit` is the real guard: ureq stops the read past it and errors,
        // so a chunked or mislabelled body cannot grow without bound. Read
        // one byte past the ceiling to tell "exactly at the limit" from
        // "over it".
        let body = resp
            .body_mut()
            .with_config()
            .limit(self.max_bytes.saturating_add(1))
            .read_to_vec()
            .map_err(|e| {
                Error::msg(format!(
                    "GET {}: reading the body (limit {} bytes, raise it with .max_bytes()): {e}",
                    self.url, self.max_bytes
                ))
            })?;
        if body.len() as u64 > self.max_bytes {
            bail!(
                "GET {}: the body is larger than the {} byte limit; raise it with .max_bytes()",
                self.url,
                self.max_bytes
            );
        }
        Ok(body)
    }
}

impl DownloadBuilder {
    /// The destination path. Finishes the builder.
    pub fn to(self, dest: impl Into<PathBuf>) -> Download {
        Download {
            url: self.url,
            dest: dest.into(),
            checksum: None,
            mode: None,
            owner: None,
            force: false,
            backup: false,
            timeout: DEFAULT_TIMEOUT,
            max_bytes: DEFAULT_MAX_BYTES,
            headers: vec![],
        }
    }
}

enum ContentState {
    Missing,
    /// Present and acceptable; carries its digest for the report when one
    /// had to be computed anyway.
    Current {
        sha256: Option<String>,
    },
    /// Present but to be replaced, with the one-line reason for the diff.
    Stale(String),
}

fn agent(timeout: Duration) -> ureq::Agent {
    let tls = ureq::tls::TlsConfig::builder()
        .unversioned_rustls_crypto_provider(crate::tls::provider())
        .build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .timeout_connect(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .user_agent(concat!("rustible/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

impl Op for Download {
    type Output = DownloadReport;

    fn check(&self, sys: &System) -> Result<Plan<DownloadReport>> {
        validate_url(&self.url).map_err(Error::msg)?;
        let checksum = self.parsed_checksum()?;
        let (stat, state) = self.content_state(sys, checksum.as_ref())?;
        let attrs = plan_attrs(stat.as_ref(), self.mode, self.owner);
        let reason = match state {
            ContentState::Current { sha256 } => {
                let report = DownloadReport {
                    url: self.url.clone(),
                    path: self.dest.clone(),
                    downloaded: false,
                    bytes: stat.as_ref().map(|s| s.size).unwrap_or_default(),
                    sha256,
                    backup_path: None,
                };
                if attrs.is_empty() {
                    return Ok(Plan::Satisfied(report));
                }
                return Ok(Plan::change_predicting(
                    Diff::Attrs {
                        subject: self.dest.display().to_string(),
                        changes: attrs,
                    },
                    report,
                ));
            }
            ContentState::Missing => "missing".to_string(),
            ContentState::Stale(why) => why,
        };
        // Size and digest are unknown until fetched: no prediction.
        Ok(Plan::change(Diff::summary(format!(
            "GET {} -> {} ({reason})",
            self.url,
            self.dest.display()
        ))))
    }

    fn apply(&self, sys: &System, change: Change<DownloadReport>) -> Result<DownloadReport> {
        // A prediction is only ever attached to an attributes-only change.
        if let Some(report) = change.predicted {
            apply_attrs(sys, &self.dest, self.mode, self.owner)?;
            return Ok(report);
        }
        let checksum = self.parsed_checksum()?;
        let bytes = self.fetch()?;
        let sha256 = digest(Algorithm::Sha256, &bytes);
        if let Some(c) = &checksum {
            let actual = if c.algorithm == Algorithm::Sha256 {
                sha256.clone()
            } else {
                digest(c.algorithm, &bytes)
            };
            ensure!(
                actual == c.hex,
                "GET {}: {} checksum mismatch: got {actual}, want {}; nothing written to {}",
                self.url,
                c.algorithm.name(),
                c.hex,
                self.dest.display()
            );
        }
        let backup_path = write_with_backup(sys, &self.dest, self.backup, &bytes)?;
        apply_attrs(sys, &self.dest, self.mode, self.owner)?;
        Ok(DownloadReport {
            url: self.url.clone(),
            path: self.dest.clone(),
            downloaded: true,
            bytes: bytes.len() as u64,
            sha256: Some(sha256),
            backup_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::*;
    use crate::file::testing::{expect_change, fake_sys};

    const HELLO: &[u8] = b"hello from rustible\n";
    const HELLO_SHA256: &str = "86a9660ed95754054a62f1dbc68e53ab443dd67c84fa77362a699dbf8604da3d";

    /// `(path, status, extra headers, body)` the test server answers with.
    type Route = (&'static str, u16, Vec<(&'static str, String)>, Vec<u8>);

    /// A one-thread HTTP/1.1 server on 127.0.0.1 serving a fixed table of
    /// paths. Returns its base URL and a hit counter. Each response closes
    /// the connection, so ureq cannot pool.
    fn serve(routes: Vec<Route>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
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
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();
                counter.fetch_add(1, Ordering::SeqCst);
                let (status, headers, body) = routes
                    .iter()
                    .find(|(p, ..)| *p == path)
                    .map(|(_, st, h, b)| (*st, h.clone(), b.clone()))
                    .unwrap_or((404, vec![], b"no such route".to_vec()));
                let reason = match status {
                    200 => "OK",
                    302 => "Found",
                    404 => "Not Found",
                    500 => "Internal Server Error",
                    _ => "Whatever",
                };
                // A route carrying `X-Omit-Length` answers without a
                // `Content-Length`, so the body's size is unknown until it
                // is read: that is the case `.max_bytes` has to catch at
                // the read rather than from the header.
                let omit_length = headers.iter().any(|(k, _)| *k == "X-Omit-Length");
                let mut out = if omit_length {
                    format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n")
                } else {
                    format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        body.len()
                    )
                };
                for (k, v) in headers {
                    if k == "X-Omit-Length" {
                        continue;
                    }
                    out.push_str(&format!("{k}: {v}\r\n"));
                }
                out.push_str("\r\n");
                let _ = s.write_all(out.as_bytes());
                let _ = s.write_all(&body);
            }
        });
        (base, hits)
    }

    fn hello_server() -> (String, Arc<AtomicUsize>) {
        serve(vec![
            ("/hello.txt", 200, vec![], HELLO.to_vec()),
            (
                "/redir",
                302,
                vec![("Location", "/hello.txt".into())],
                vec![],
            ),
            ("/boom", 500, vec![], b"server on fire".to_vec()),
        ])
    }

    fn report(url: &str, downloaded: bool) -> DownloadReport {
        DownloadReport {
            url: url.into(),
            path: "/opt/hello.txt".into(),
            downloaded,
            bytes: HELLO.len() as u64,
            sha256: Some(HELLO_SHA256.into()),
            backup_path: None,
        }
    }

    // ---- pure ----

    #[test]
    fn checksum_parsing() {
        let c = parse_checksum(&format!("SHA256:{}", HELLO_SHA256.to_uppercase())).unwrap();
        assert_eq!(c.algorithm, Algorithm::Sha256);
        assert_eq!(c.hex, HELLO_SHA256, "normalised to lower case");
        assert_eq!(
            parse_checksum(&format!("sha512:{}", "a".repeat(128)))
                .unwrap()
                .algorithm,
            Algorithm::Sha512
        );
        assert!(
            parse_checksum(HELLO_SHA256)
                .unwrap_err()
                .contains("<algorithm>:<hex>")
        );
        assert!(
            parse_checksum("md5:d41d8cd98f00b204e9800998ecf8427e")
                .unwrap_err()
                .contains("`md5` is not supported")
        );
        assert!(
            parse_checksum("sha256:abc")
                .unwrap_err()
                .contains("64 hex digits, got 3")
        );
        assert!(
            parse_checksum(&format!("sha256:{}zz", &HELLO_SHA256[..62]))
                .unwrap_err()
                .contains("64 hex digits")
        );
    }

    #[test]
    fn digests_match_the_fixture() {
        assert_eq!(digest(Algorithm::Sha256, HELLO), HELLO_SHA256);
        assert_eq!(digest(Algorithm::Sha224, b"").len(), 56);
        assert_eq!(digest(Algorithm::Sha384, b"").len(), 96);
        assert_eq!(
            digest(Algorithm::Sha512, b""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
    }

    #[test]
    fn url_validation() {
        assert_eq!(validate_url("http://h/x"), Ok(()));
        assert_eq!(validate_url("HTTPS://h/x"), Ok(()));
        assert!(
            validate_url("ftp://h/x")
                .unwrap_err()
                .contains("not an http://")
        );
        assert!(
            validate_url("file:///etc/passwd")
                .unwrap_err()
                .contains("file::Copy::from_local_path")
        );
        assert!(
            validate_url("https://")
                .unwrap_err()
                .contains("not a valid")
        );
        assert!(
            validate_url("http://h/a b")
                .unwrap_err()
                .contains("not a valid")
        );
        // The length floor is the scheme that matched, not the longer one:
        // a single-label host is real (`/etc/hosts`, a container alias).
        assert_eq!(validate_url("http://x"), Ok(()), "8 chars, but valid");
        assert_eq!(validate_url("HTTP://x"), Ok(()));
        assert_eq!(validate_url("https://x"), Ok(()));
        assert!(validate_url("http://").unwrap_err().contains("not a valid"));
    }

    // ---- fake: check ----

    #[test]
    fn satisfied_when_checksum_matches_without_touching_the_network() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello.txt", HELLO),
        );
        let sys = fake_sys(&fake);
        // An unroutable URL: check must not care.
        let op = Download::get("https://nowhere.invalid/hello.txt")
            .to("/opt/hello.txt")
            .checksum(format!("sha256:{HELLO_SHA256}"))
            .force(true);
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r, report("https://nowhere.invalid/hello.txt", false));
    }

    #[test]
    fn existing_file_without_checksum_is_satisfied_unless_forced() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello.txt", "stale"),
        );
        let sys = fake_sys(&fake);
        let op = Download::get("http://h/hello.txt").to("/opt/hello.txt");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        let c = expect_change(&op.clone().force(true), &sys);
        assert_eq!(
            c.diff.short(),
            "GET http://h/hello.txt -> /opt/hello.txt (force)"
        );
        assert!(c.predicted.is_none(), "a download is never predicted");
    }

    #[test]
    fn missing_file_and_checksum_mismatch_plan_a_download() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/other.txt", "x"),
        );
        let sys = fake_sys(&fake);
        let c = expect_change(
            &Download::get("http://h/hello.txt").to("/opt/hello.txt"),
            &sys,
        );
        assert_eq!(
            c.diff.short(),
            "GET http://h/hello.txt -> /opt/hello.txt (missing)"
        );
        let c = expect_change(
            &Download::get("http://h/hello.txt")
                .to("/opt/other.txt")
                .checksum(format!("sha256:{HELLO_SHA256}")),
            &sys,
        );
        assert_eq!(
            c.diff.short(),
            "GET http://h/hello.txt -> /opt/other.txt (sha256 is 2d711642b726..., want 86a9660ed957...)"
        );
    }

    #[test]
    fn attrs_only_change_predicts_and_applies_without_downloading() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello.txt", HELLO),
        );
        let sys = fake_sys(&fake);
        let op = Download::get("http://nowhere.invalid/hello.txt")
            .to("/opt/hello.txt")
            .mode(0o755)
            .owner(10, 20);
        let c = expect_change(&op, &sys);
        assert_eq!(c.diff.short(), "mode=0755 owner=10:20");
        // No `.checksum`, so the destination is never read or hashed and
        // the report carries no digest: the whole point of the laziness.
        assert_eq!(
            c.predicted,
            Some(DownloadReport {
                sha256: None,
                ..report("http://nowhere.invalid/hello.txt", false)
            })
        );
        let r = op.apply(&sys, c).unwrap();
        assert!(!r.downloaded);
        assert_eq!(r.sha256, None);
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o755, 10, 20));
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn a_body_over_max_bytes_is_refused_by_content_length() {
        let big = vec![b'x'; 4096];
        let (base, hits) = serve(vec![("/big", 200, vec![], big)]);
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/big"))
            .to("/opt/big.bin")
            .max_bytes(100);
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().to_string();
        assert!(
            err.contains("Content-Length 4096 exceeds the 100 byte limit"),
            "{err}"
        );
        assert!(err.contains(".max_bytes()"), "{err}");
        assert!(fake.file("/opt/big.bin").is_none(), "nothing written");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_body_over_max_bytes_is_refused_without_a_content_length() {
        // No `Content-Length`, so the size is unknown until it is read and
        // only ureq's `limit` can stop it: the guard that actually bounds
        // memory, rather than trusting the server's header.
        let big = vec![b'x'; 4096];
        let (base, _) = serve(vec![(
            "/big",
            200,
            vec![("X-Omit-Length", String::new())],
            big,
        )]);
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/big"))
            .to("/opt/big.bin")
            .max_bytes(100);
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().to_string();
        assert!(err.contains("limit 100 bytes"), "{err}");
        assert!(err.contains(".max_bytes()"), "{err}");
        assert!(fake.file("/opt/big.bin").is_none(), "nothing written");
    }

    #[test]
    fn a_body_exactly_at_max_bytes_is_accepted() {
        let (base, _) = serve(vec![("/hello.txt", 200, vec![], HELLO.to_vec())]);
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .max_bytes(HELLO.len() as u64);
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.bytes, HELLO.len() as u64);
        assert_eq!(fake.content("/opt/hello.txt").unwrap().as_bytes(), HELLO);
    }

    #[test]
    fn check_hashes_the_destination_only_when_a_checksum_asks_for_it() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello.txt", HELLO),
        );
        let sys = fake_sys(&fake);
        // Present, no checksum, no force: nothing about the bytes can
        // change the answer, so they are not read and there is no digest.
        // `bytes` still comes from the stat.
        let op = Download::get("http://nowhere.invalid/hello.txt").to("/opt/hello.txt");
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected Satisfied");
        };
        assert_eq!(r.sha256, None, "no checksum configured, so no digest");
        assert_eq!(r.bytes, HELLO.len() as u64, "size comes from the stat");

        // With a checksum the read has to happen anyway, so the digest it
        // produces is reported.
        let op = op.checksum(format!("sha256:{HELLO_SHA256}"));
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected Satisfied");
        };
        assert_eq!(r.sha256.as_deref(), Some(HELLO_SHA256));
    }

    #[test]
    fn refusals_at_check() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_dir("/opt/d")
                .with_symlink("/opt/l", "/opt/d")
                .with_file("/etc/f", "x"),
        );
        let sys = fake_sys(&fake);
        let err = |op: Download| op.check(&sys).unwrap_err().to_string();
        assert!(err(Download::get("ftp://h/x").to("/opt/x")).contains("not an http://"));
        assert!(
            err(Download::get("http://h/x").to("/opt/x").checksum("nope"))
                .contains("<algorithm>:<hex>")
        );
        assert!(err(Download::get("http://h/x").to("/opt/d")).contains("not a regular file (Dir)"));
        assert!(
            err(Download::get("http://h/x").to("/opt/l")).contains("not a regular file (Symlink)")
        );
        assert!(
            err(Download::get("http://h/x").to("/missing/x"))
                .contains("/missing does not exist; create it first with file::Directory")
        );
        assert!(
            err(Download::get("http://h/x").to("/etc/f/x")).contains("/etc/f is not a directory")
        );
    }

    // ---- fake filesystem, real loopback HTTP ----

    #[test]
    fn download_writes_atomically_then_is_satisfied() {
        let (base, hits) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let url = format!("{base}/hello.txt");
        let op = Download::get(&url)
            .to("/opt/hello.txt")
            .checksum(format!("sha256:{HELLO_SHA256}"))
            .mode(0o600);
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r, report(&url, true));
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!((f.mode, f.bytes.as_slice()), (0o600, HELLO));
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "check never fetched");
    }

    #[test]
    fn download_follows_redirects_and_sends_headers() {
        let (base, hits) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/redir"))
            .to("/opt/hello.txt")
            .header("Accept", "application/octet-stream");
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.sha256.as_deref(), Some(HELLO_SHA256));
        assert_eq!(hits.load(Ordering::SeqCst), 2, "redirect then target");
    }

    #[test]
    fn force_redownloads_and_backs_up() {
        let (base, _) = hello_server();
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello.txt", "old"),
        );
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .force(true)
            .backup(true);
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        let bp = r.backup_path.expect("backup path");
        assert!(
            bp.to_string_lossy()
                .starts_with("/opt/hello.txt.~rustible.")
        );
        assert_eq!(fake.content(&bp).unwrap(), "old");
        assert_eq!(fake.file("/opt/hello.txt").unwrap().bytes, HELLO);
        // Force means it is never satisfied.
        assert!(op.check(&sys).unwrap().is_change());
    }

    #[test]
    fn http_errors_name_status_and_url_and_write_nothing() {
        let (base, _) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        for (path, status) in [
            ("/nope", "404 Not Found"),
            ("/boom", "500 Internal Server Error"),
        ] {
            let url = format!("{base}{path}");
            let op = Download::get(&url).to("/opt/x");
            let c = expect_change(&op, &sys);
            let err = op.apply(&sys, c).unwrap_err().to_string();
            assert_eq!(err, format!("GET {url} returned {status}"));
        }
        assert!(fake.file("/opt/x").is_none());
    }

    #[test]
    fn checksum_mismatch_after_download_fails_and_leaves_the_old_file() {
        let (base, _) = hello_server();
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello.txt", "old"),
        );
        let sys = fake_sys(&fake);
        let url = format!("{base}/hello.txt");
        let want = "0".repeat(64);
        let op = Download::get(&url)
            .to("/opt/hello.txt")
            .checksum(format!("sha256:{want}"));
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "GET {url}: sha256 checksum mismatch: got {HELLO_SHA256}, want {want}; nothing written to /opt/hello.txt"
            )
        );
        assert_eq!(fake.content("/opt/hello.txt").unwrap(), "old");
    }

    #[test]
    fn connection_refused_names_the_url() {
        // Bind and drop: the port is closed by the time we connect.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let url = format!("http://127.0.0.1:{port}/x");
        let op = Download::get(&url).to("/opt/x");
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().to_string();
        assert!(err.starts_with(&format!("GET {url}: ")), "{err}");
    }

    #[test]
    fn check_mode_reports_would_change_and_fetches_nothing() {
        let (base, hits) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step(
                "download",
                Download::get(format!("{base}/hello.txt")).to("/opt/hello.txt"),
            )
            .unwrap();
        assert!(r.changed && !r.predicted && !r.is_available());
        assert!(fake.file("/opt/hello.txt").is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// Real TLS through rustls-graviola against a public host, which also
    /// exercises the CPU pre-flight on the way in. Not part of `cargo test`:
    /// run with `cargo test -p rustible-std https_ -- --ignored`.
    #[test]
    #[ignore = "needs network"]
    fn https_download_from_github_with_graviola_tls() {
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let op =
            Download::get("https://raw.githubusercontent.com/flipbit03/rustible/main/LICENSE-MIT")
                .to("/opt/LICENSE-MIT");
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert!(r.bytes > 100, "{r:?}");
        assert!(fake.content("/opt/LICENSE-MIT").unwrap().contains("MIT"));
    }
}
