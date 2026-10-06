//! [`Download`]: a URL into a file on the target. Ansible's
//! `ansible.builtin.get_url`.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rustible_sdk::backend::{FileKind, WriteAttrs};
use rustible_sdk::prelude::*;
use sha2::Digest;

use ureq::http::Method;

use super::client::{self, Field, HeaderSpec, Outgoing, Timeout, mask_url};
use super::validate_url;
use crate::file::{AttrPlan, Owner, plan_attrs, write_from_with_backup};

/// The default timeout of both ops. For [`Download`] it bounds connecting
/// and the response head, and the body has none (Ansible's `get_url`
/// default is 10 seconds); for [`Request`](super::Request) it bounds the
/// whole exchange, body included (Ansible's `uri` default is 30 seconds).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of a file or a body is hashed at a time.
const BUF: usize = 64 << 10;

/// What a refusal over [`Download::max_bytes`] adds to the client's message.
const MAX_BYTES_HINT: &str = "raise it with .max_bytes()";

/// A checksum algorithm the `.checksum("<algorithm>:<hex>")` option accepts.
/// One variant per word the spec takes before the colon; [`Algorithm::name`]
/// gives that word back and [`Algorithm::hex_len`] the digest length. MD5 and
/// SHA-1 have no variant on purpose, see [`parse_checksum`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// Variants transcribe the words the `<algorithm>:<hex>` spec accepts; the
// meaning is in the type's own docs and in `name`. Individually documenting
// each would restate the name.
#[allow(missing_docs)]
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

    /// The lowercase word that selects this algorithm in a `.checksum` spec,
    /// and the one error messages print. The inverse of what
    /// [`parse_checksum`] accepts before the colon.
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
    /// Which digest [`Checksum::hex`] is, from the part of the spec before
    /// the colon.
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
    let mut h = Hasher::new(algorithm);
    h.update(bytes);
    h.hex()
}

/// A digest computed a piece at a time, so a file or a body of any size is
/// hashed in one buffer's memory.
enum Hasher {
    Sha224(sha2::Sha224),
    Sha256(sha2::Sha256),
    Sha384(sha2::Sha384),
    Sha512(sha2::Sha512),
}

impl Hasher {
    fn new(algorithm: Algorithm) -> Self {
        match algorithm {
            Algorithm::Sha224 => Hasher::Sha224(sha2::Sha224::new()),
            Algorithm::Sha256 => Hasher::Sha256(sha2::Sha256::new()),
            Algorithm::Sha384 => Hasher::Sha384(sha2::Sha384::new()),
            Algorithm::Sha512 => Hasher::Sha512(sha2::Sha512::new()),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Hasher::Sha224(h) => h.update(bytes),
            Hasher::Sha256(h) => h.update(bytes),
            Hasher::Sha384(h) => h.update(bytes),
            Hasher::Sha512(h) => h.update(bytes),
        }
    }

    /// The digest, lowercase hex.
    fn hex(self) -> String {
        fn hex(d: impl AsRef<[u8]>) -> String {
            d.as_ref().iter().map(|b| format!("{b:02x}")).collect()
        }
        match self {
            Hasher::Sha224(h) => hex(h.finalize()),
            Hasher::Sha256(h) => hex(h.finalize()),
            Hasher::Sha384(h) => hex(h.finalize()),
            Hasher::Sha512(h) => hex(h.finalize()),
        }
    }
}

/// The digests one pass over some bytes yields: SHA-256, which the report
/// carries whatever `.checksum` asked for, and the checksum's own
/// algorithm when that is another one.
struct Digests {
    sha256: Hasher,
    other: Option<Hasher>,
}

impl Digests {
    fn new(checksum: Option<&Checksum>) -> Self {
        Digests {
            sha256: Hasher::new(Algorithm::Sha256),
            other: checksum
                .map(|c| c.algorithm)
                .filter(|a| *a != Algorithm::Sha256)
                .map(Hasher::new),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.sha256.update(bytes);
        if let Some(h) = &mut self.other {
            h.update(bytes);
        }
    }

    /// The SHA-256, and the digest by the checksum's algorithm (the same
    /// SHA-256 when that is the one, or when there is no checksum).
    fn finish(self) -> (String, String) {
        let sha256 = self.sha256.hex();
        let actual = self.other.map_or_else(|| sha256.clone(), Hasher::hex);
        (sha256, actual)
    }
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
/// **Honesty.** The body streams into a file staged beside `dest`
/// (`sys.write_from`), hashed as it arrives, and is renamed over `dest`
/// only once it is whole: the old file stays untouched until then. A
/// checksum mismatch at the end of the body, or a body over
/// [`Download::max_bytes`], fails the step and writes nothing, and so does
/// a body that ends before the size its `Content-Length` declares, or
/// before its chunked encoding's last chunk. A body framed only by the
/// server closing the connection cannot be told from a whole one when the
/// connection drops; `.checksum` is what catches that. A non-2xx
/// status fails the step naming the status and the URL. Redirects are
/// followed, up to ten, and a header given with [`Download::header_secret`]
/// is dropped when one leaves the scheme, host and port it was meant for
/// (the [module docs](super) have the policy). A URL's `user:pass@` is
/// masked in every message and diff.
/// `check` never opens the connection: a dry run reports what would be
/// fetched and why, and a step that would change has no output there
/// (vision 12).
///
/// **Limits.** None on the size of the body unless [`Download::max_bytes`]
/// sets one: it passes through the target a buffer at a time, as root or as
/// any account, so a JDK or a disk image takes disk, not memory. Fails if
/// `dest` exists and is not a regular file, or if its parent directory does
/// not exist (vision 6.7: create it with [`crate::file::Directory`]).
#[derive(Clone)]
pub struct Download {
    url: String,
    dest: PathBuf,
    checksum: Option<String>,
    mode: Option<u32>,
    owner: Option<Owner>,
    force: bool,
    backup: bool,
    timeout: Duration,
    max_bytes: Option<u64>,
    headers: Vec<HeaderSpec>,
}

/// `{:?}` masks the URL's userinfo and shows a secret header as its size.
impl std::fmt::Debug for Download {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Download")
            .field("url", &mask_url(&self.url))
            .field("dest", &self.dest)
            .field("checksum", &self.checksum)
            .field("mode", &self.mode)
            .field("owner", &self.owner)
            .field("force", &self.force)
            .field("backup", &self.backup)
            .field("timeout", &self.timeout)
            .field("max_bytes", &self.max_bytes)
            .field("headers", &self.headers)
            .finish()
    }
}

/// A `Download` with a URL but no destination yet; `.to(dest)` finishes it.
#[derive(Clone)]
pub struct DownloadBuilder {
    url: String,
}

impl std::fmt::Debug for DownloadBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadBuilder")
            .field("url", &mask_url(&self.url))
            .finish()
    }
}

/// Output of [`Download`]. Its `{:?}` masks the URL's userinfo.
#[derive(Clone, PartialEq, Eq)]
pub struct DownloadReport {
    /// The URL as given to [`Download::get`]. Redirects are followed, but the
    /// address they land on is not reported here.
    pub url: String,
    /// `dest` as given to [`DownloadBuilder::to`], not the temporary file the
    /// atomic write went through.
    pub path: PathBuf,
    /// Whether the URL was (or would be) fetched, as opposed to an
    /// attributes-only change or nothing at all.
    pub downloaded: bool,
    /// Size of the file now at `path`: after a download, the bytes the
    /// body had, counted as they were written.
    pub bytes: u64,
    /// SHA-256 of the file now at `path`, whatever `.checksum` asked for.
    ///
    /// `None` when the op did not have to read the file: no `.checksum`
    /// was configured and the file was already in place, so hashing it
    /// would have decided nothing and cost a full read of, say, a 500 MB
    /// tarball on every run, check mode included. Always `Some` after a
    /// download, computed from the body as it was written.
    pub sha256: Option<String>,
    /// Set only when `.backup(true)` and a previous version was saved.
    pub backup_path: Option<PathBuf>,
}

impl std::fmt::Debug for DownloadReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadReport")
            .field("url", &mask_url(&self.url))
            .field("path", &self.path)
            .field("downloaded", &self.downloaded)
            .field("bytes", &self.bytes)
            .field("sha256", &self.sha256)
            .field("backup_path", &self.backup_path)
            .finish()
    }
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

    /// Permission bits for `dest` (`0o644`, `0o755`). Unset by default: a new
    /// file gets the mode any new file gets (0666 less the umask), an
    /// existing one keeps its own. A download has it before it is renamed
    /// into place. Enforced on every run, so a file whose only difference is
    /// its mode is a `changed` step with an attribute diff and no download.
    pub fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Numeric owner (`chown uid:gid`). A download has it before it is
    /// renamed into place, and one this identity may not give fails the
    /// step with `dest` as it was. Changing the owner clears setuid, and
    /// setgid with group execute, as `chown` does; give `.mode(..)` too to
    /// keep them.
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
    /// (`<name>.~rustible.<unix-ts>`). A download that fails removes the
    /// copy it took, so one is left only beside a replacement.
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

    /// Largest response body to accept, in bytes. Unset by default: a
    /// download is as large as the URL it was given. A `Content-Length`
    /// above it fails before the body is read; a response without one, or
    /// one that lies, fails as soon as the read passes it. Either way
    /// nothing is written.
    pub fn max_bytes(mut self, n: u64) -> Self {
        self.max_bytes = Some(n);
        self
    }

    /// An extra request header, e.g. `("Accept", "application/octet-stream")`.
    /// For a token or a key, use [`Download::header_secret`].
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push(HeaderSpec {
            name: name.into(),
            value: Field::Plain(value.into()),
        });
        self
    }

    /// A request header whose value is secret, e.g. `("Authorization",
    /// &token)` with the token read by `ctx.local_secret` or from a file on
    /// the target into a [`Secret`]. A trailing newline is stripped. It
    /// shows as `<secret, N bytes>` in `Debug` and never in a diff or a
    /// message, and it is dropped when a redirect leaves the scheme, host
    /// and port it was sent to; a redirect from `https://` to `http://` is
    /// refused while it is attached.
    pub fn header_secret(mut self, name: impl Into<String>, value: &Secret) -> Self {
        self.headers.push(HeaderSpec {
            name: name.into(),
            value: Field::Secret(value.clone()),
        });
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
                // Under --check an earlier file::Directory may create it (vision 12).
                None if sys.check_mode() => {}
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
                let (sha256, actual) = hash_file(sys, &self.dest, c)?;
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

    /// The response body, unread, once the status says there is one to
    /// write.
    fn fetch(&self, url: &str) -> Result<client::Body> {
        let resp = client::send(Outgoing {
            method: Method::GET,
            url: url.to_string(),
            headers: self.headers.clone(),
            body: None,
            follow: true,
            timeout: Timeout::Head(self.timeout),
            secrets: vec![],
        })?;
        if !(200..300).contains(&resp.status) {
            let status = format!("{} {}", resp.status, resp.reason());
            bail!("GET {} returned {}", mask_url(url), status.trim_end());
        }
        resp.body(self.max_bytes)
            .map_err(|e| e.hinted(MAX_BYTES_HINT))
    }
}

/// SHA-256 of the file at `path`, and its digest by `checksum`'s algorithm,
/// read through `open_read` a buffer at a time.
fn hash_file(sys: &System, path: &Path, checksum: &Checksum) -> Result<(String, String)> {
    let mut file = sys.open_read(path)?;
    let mut digests = Digests::new(Some(checksum));
    let mut buf = vec![0; BUF];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return Ok(digests.finish()),
            Ok(n) => digests.update(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
}

/// The response body on its way to `dest`, as the reader `write_from`
/// streams into the staged file: counted and hashed as it is read, and at
/// its end checked against `.checksum`, failing there on a mismatch so the
/// staged file is never renamed into place. A body over `.max_bytes()`
/// fails as soon as the read passes it.
///
/// A failure is kept here as the step's message, word for word, and
/// `write_from` is handed an error carrying the same text; `apply` reports
/// the kept one, whatever a backend made of its copy.
struct Verified {
    body: client::Body,
    /// `None` once the body has ended and been checked.
    digests: Option<Digests>,
    checksum: Option<Checksum>,
    /// `GET <masked url>`, which the mismatch message opens with.
    what: String,
    dest: PathBuf,
    /// Bytes read so far: at the end, the body's size.
    bytes: u64,
    /// The body's SHA-256, set at its end.
    sha256: Option<String>,
    failure: Option<Error>,
}

impl Verified {
    fn new(body: client::Body, checksum: Option<Checksum>, url: &str, dest: &Path) -> Self {
        Verified {
            body,
            digests: Some(Digests::new(checksum.as_ref())),
            checksum,
            what: format!("GET {}", mask_url(url)),
            dest: dest.to_path_buf(),
            bytes: 0,
            sha256: None,
            failure: None,
        }
    }

    /// Keep `e` as the step's error, and hand `write_from` its text.
    fn fail(&mut self, e: Error) -> io::Error {
        let handed = io::Error::other(e.to_string());
        self.failure = Some(e);
        handed
    }
}

impl Read for Verified {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(e) = &self.failure {
            return Err(io::Error::other(e.to_string()));
        }
        if self.digests.is_none() {
            return Ok(0);
        }
        match self.body.read(buf) {
            Ok(0) => {
                let Some(digests) = self.digests.take() else {
                    return Ok(0);
                };
                let (sha256, actual) = digests.finish();
                if let Some(c) = &self.checksum
                    && actual != c.hex
                {
                    let e = Error::msg(format!(
                        "{}: {} checksum mismatch: got {actual}, want {}; nothing written to {}",
                        self.what,
                        c.algorithm.name(),
                        c.hex,
                        self.dest.display()
                    ));
                    return Err(self.fail(e));
                }
                self.sha256 = Some(sha256);
                Ok(0)
            }
            Ok(n) => {
                if let Some(digests) = &mut self.digests {
                    digests.update(&buf[..n]);
                }
                self.bytes += n as u64;
                Ok(n)
            }
            Err(e) => Err(self.fail(e.hinted(MAX_BYTES_HINT))),
        }
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
            max_bytes: None,
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

/// What [`Download`]'s `check` decided: fetch the URL and write it, or only
/// set the attributes of a file whose content is already right. The body is
/// fetched by `apply` and never held here; the headers stay on the op.
#[derive(Debug)]
pub struct DownloadIntent {
    dest: PathBuf,
    /// `Some` when the content is missing or stale.
    fetch: Option<Fetch>,
    attrs: AttrPlan,
}

struct Fetch {
    url: String,
    /// Why the file is fetched, for the report: `missing`, or what made the
    /// present one stale.
    reason: String,
    /// The mode and owner the downloaded file is given before it is renamed
    /// into place (decision 24 on #85).
    write: Option<WriteAttrs>,
}

impl std::fmt::Debug for Fetch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetch")
            .field("url", &mask_url(&self.url))
            .field("reason", &self.reason)
            .field("write", &self.write)
            .finish()
    }
}

impl Intent for DownloadIntent {
    fn diff(&self) -> Diff {
        match &self.fetch {
            // Size and digest are unknown until fetched; the diff says what
            // would be fetched and why.
            Some(Fetch { url, reason, .. }) => Diff::summary(format!(
                "GET {} -> {} ({reason})",
                mask_url(url),
                self.dest.display()
            )),
            None => Diff::attrs(self.dest.display().to_string(), self.attrs.changes()),
        }
    }
}

impl Op for Download {
    type Output = DownloadReport;
    type Intent = DownloadIntent;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable. http::Download is pure-Rust HTTP and TLS writing a file through `sys`; `ring` cross-compiles for Darwin.
        // The supported set is written out rather than left open, so a new
        // platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!("http::Download has no implementation for {}", other.name()),
        }
        validate_url(&self.url).map_err(Error::msg)?;
        for h in &self.headers {
            h.to_wire()
                .map_err(|e| Error::msg(format!("GET {}: {e}", mask_url(&self.url))))?;
        }
        let checksum = self.parsed_checksum()?;
        let (stat, state) = self.content_state(sys, checksum.as_ref())?;
        let attrs = plan_attrs(stat.as_ref(), self.mode, self.owner);
        let reason = match state {
            ContentState::Current { sha256 } => {
                if !attrs.differs() {
                    return Ok(Plan::Satisfied(DownloadReport {
                        url: self.url.clone(),
                        path: self.dest.clone(),
                        downloaded: false,
                        bytes: stat.as_ref().map(|s| s.size).unwrap_or_default(),
                        sha256,
                        backup_path: None,
                    }));
                }
                return Ok(Plan::Change(DownloadIntent {
                    dest: self.dest.clone(),
                    fetch: None,
                    attrs,
                }));
            }
            ContentState::Missing => "missing".to_string(),
            ContentState::Stale(why) => why,
        };
        Ok(Plan::Change(DownloadIntent {
            dest: self.dest.clone(),
            fetch: Some(Fetch {
                url: self.url.clone(),
                reason,
                write: attrs.write_attrs(stat.as_ref().map(|s| s.mode)),
            }),
            attrs,
        }))
    }

    fn apply(&self, sys: &System, intent: DownloadIntent) -> Result<DownloadReport> {
        let checksum = self.parsed_checksum()?;
        let DownloadIntent { dest, fetch, attrs } = intent;
        let Some(Fetch { url, write, .. }) = fetch else {
            // The content already matched: no fetch. The report reads the
            // file as it is rather than carrying a copy of what `check` saw.
            attrs.apply_differing(sys, &dest)?;
            let (stat, state) = self.content_state(sys, checksum.as_ref())?;
            let sha256 = match state {
                ContentState::Current { sha256 } => sha256,
                ContentState::Missing | ContentState::Stale(_) => None,
            };
            return Ok(DownloadReport {
                url: self.url.clone(),
                path: dest,
                downloaded: false,
                bytes: stat.as_ref().map(|s| s.size).unwrap_or_default(),
                sha256,
                backup_path: None,
            });
        };
        // The body streams into the file staged beside `dest`, with its
        // mode and owner, and is renamed over it only once it is whole and
        // verified: a mismatch, a body over `.max_bytes()`, a read that
        // fails (one short of its `Content-Length` or chunked framing) or
        // a refused owner leaves `dest` as it was, and removes the backup
        // taken for it (decision 26).
        let mut body = Verified::new(self.fetch(&url)?, checksum, &url, &dest);
        let written = write_from_with_backup(sys, &dest, self.backup, &mut body, write);
        if let Some(e) = body.failure.take() {
            return Err(e);
        }
        let (backup_path, _) = written?;
        Ok(DownloadReport {
            url,
            path: dest,
            downloaded: true,
            bytes: body.bytes,
            sha256: body.sha256,
            backup_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::super::test_server::Route;
    use super::*;
    use crate::file::testing::{Set, expect_change, fake_sys, sizeless_sys, staged};

    /// `http::Download` claims a mac and refuses a platform nobody claimed.
    /// The mac half is a real download from the test server with Darwin
    /// facts, because TLS from the target is exactly what the macOS spike
    /// proved and what a wrong gate would silently lose; the refusal half
    /// never opens a socket, since the gate comes first.
    #[test]
    fn runs_on_a_mac_and_refuses_an_unclaimed_platform() {
        let (base, hits) = serve(vec![("/hello.txt", 200, vec![], HELLO.to_vec())]);
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let base_sys = fake_sys(&fake);

        let mut mac = base_sys.facts().clone();
        mac.os = Os::Macos;
        let sys = base_sys.clone().with_facts(mac);
        let op = Download::get(format!("{base}/hello.txt")).to("/opt/hello.txt");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/opt/hello.txt").unwrap().as_bytes(), HELLO);
        let served = hits.load(Ordering::SeqCst);

        let mut bsd = base_sys.facts().clone();
        bsd.os = Os::Other("freebsd".into());
        let sys = base_sys.with_facts(bsd);
        let err = Download::get(format!("{base}/hello.txt"))
            .to("/opt/other.txt")
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("http::Download has no implementation for freebsd"),
            "{err}"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            served,
            "the refusal made no request"
        );
    }

    const HELLO: &[u8] = b"hello from rustible\n";
    const HELLO_SHA256: &str = "86a9660ed95754054a62f1dbc68e53ab443dd67c84fa77362a699dbf8604da3d";

    /// The shared loopback server, as its base URL and hit counter.
    fn serve(routes: Vec<Route>) -> (String, Arc<AtomicUsize>) {
        let s = super::super::test_server::serve(routes);
        (s.base, s.hits)
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
            c.diff().short(),
            "GET http://h/hello.txt -> /opt/hello.txt (force)"
        );
        // A download is an intent to fetch, which is what `apply` acts on.
        assert!(c.fetch.is_some());
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
            c.diff().short(),
            "GET http://h/hello.txt -> /opt/hello.txt (missing)"
        );
        let c = expect_change(
            &Download::get("http://h/hello.txt")
                .to("/opt/other.txt")
                .checksum(format!("sha256:{HELLO_SHA256}")),
            &sys,
        );
        assert_eq!(
            c.diff().short(),
            "GET http://h/hello.txt -> /opt/other.txt (sha256 is 2d711642b726..., want 86a9660ed957...)"
        );
    }

    #[test]
    fn attrs_only_change_applies_without_downloading() {
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
        assert_eq!(c.diff().short(), "mode=0755 owner=10:20");
        // An intent with no fetch is what `apply` branches on: no
        // fetch, and the host name above would fail one.
        assert!(c.fetch.is_none());
        let r = op.apply(&sys, c).unwrap();
        // No `.checksum`, so the destination is never read or hashed and
        // the report carries no digest: the whole point of the laziness.
        assert_eq!(
            r,
            DownloadReport {
                sha256: None,
                ..report("http://nowhere.invalid/hello.txt", false)
            }
        );
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
            err.contains("declares a 4096 byte body (Content-Length), over the 100 byte limit"),
            "{err}"
        );
        assert!(err.contains(".max_bytes()"), "{err}");
        assert!(fake.file("/opt/big.bin").is_none(), "nothing written");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_body_over_max_bytes_is_refused_without_a_content_length() {
        // No `Content-Length`, so the size is unknown until it is read and
        // only the count `client::Body` keeps as it reads can stop it: the
        // guard that holds whatever the server's header says.
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
        assert!(err.contains("larger than the 100 byte limit"), "{err}");
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
        assert_eq!(fake.reads(), vec![]);

        // With a checksum the read has to happen anyway, so the digest it
        // produces is reported. It streams through `open_read`, so a
        // destination of any size is hashed a buffer at a time.
        let op = op.checksum(format!("sha256:{HELLO_SHA256}"));
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected Satisfied");
        };
        assert_eq!(r.sha256.as_deref(), Some(HELLO_SHA256));
        let reads: Vec<_> = fake
            .reads()
            .into_iter()
            .map(|r| (r.path, r.bytes, r.streamed))
            .collect();
        assert_eq!(
            reads,
            [(PathBuf::from("/opt/hello.txt"), HELLO.len() as u64, true)]
        );
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
        // Under --check the missing parent is one an earlier file::Directory
        // may create (vision 12): would change, and no connection is opened
        // (the host does not resolve, so a fetch would have failed loudly).
        let dry = fake_sys(&fake).with_check_mode(true);
        let c = expect_change(&Download::get("http://h/x").to("/missing/x"), &dry);
        assert_eq!(c.diff().short(), "GET http://h/x -> /missing/x (missing)");
        assert!(fake.file("/missing").is_none() && fake.file("/missing/x").is_none());
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
        let planted = fake.attr_calls().len();
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r, report(&url, true));
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!((f.mode, f.bytes.as_slice()), (0o600, HELLO));
        // The mode is the staged file's, before the rename (decision 24),
        // and nothing is set on `dest` afterwards.
        assert_eq!(staged(&fake, planted, "/opt/hello.txt"), [Set::Mode(0o600)]);
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "check never fetched");
    }

    /// Without `.mode()`, a download over a setuid file keeps `4755`, and
    /// the next `check` is `Satisfied`: nothing in `apply` sets a mode, so
    /// this is the rewrite keeping the one the file had (issue #51).
    #[test]
    fn a_download_over_a_setuid_file_without_mode_keeps_the_bit() {
        let (base, _) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt").with_file_mode(
            "/opt/hello.txt",
            "old",
            0o4755,
        ));
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .checksum(format!("sha256:{HELLO_SHA256}"));
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!((f.mode, f.bytes.as_slice()), (0o4755, HELLO));
        assert!(fake.attr_calls().is_empty(), "no mode was asked for");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// The owner asked for is given to the new content whatever the file
    /// had by the time of `apply`: here a `chown` planted between `check`
    /// and `apply`, as an unprivileged rewrite used to leave it. It is set
    /// on the staged file, never on `dest` after the rename
    /// (`file::copy`'s `a_rewrite_gives_the_owner_asked_for_whatever_the_file_has_by_then`).
    #[test]
    fn a_download_gives_the_owner_asked_for_whatever_the_file_has_by_then() {
        let (base, _) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt").with_file_mode(
            "/opt/hello.txt",
            "old",
            0o640,
        ));
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .checksum(format!("sha256:{HELLO_SHA256}"))
            .owner(0, 0);
        let c = expect_change(&op, &sys);
        rustible_sdk::backend::Backend::set_owner(
            &*fake,
            std::path::Path::new("/opt/hello.txt"),
            1000,
            1000,
        )
        .unwrap();
        let planted = fake.attr_calls().len();
        op.apply(&sys, c).unwrap();
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o640, 0, 0));
        assert_eq!(
            staged(&fake, planted, "/opt/hello.txt"),
            [Set::Mode(0o640), Set::Owner(0, 0)]
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// With `.owner()` already right and no `.mode()`, a download over a
    /// setuid or setgid file keeps the bit: the owner is given to the
    /// staged file, whose `chown` clears it, and the write sets the mode it
    /// kept again after it (found by review of #74, when the `chown` came
    /// after the rename and nothing set the bit back).
    #[test]
    fn a_download_with_owner_already_right_keeps_setuid() {
        let (base, _) = hello_server();
        for mode in [0o4755, 0o2755] {
            let fake = Arc::new(Fake::new().with_dir("/opt").with_file_mode(
                "/opt/hello.txt",
                "old",
                mode,
            ));
            let planted = fake.attr_calls().len();
            let sys = fake_sys(&fake);
            let op = Download::get(format!("{base}/hello.txt"))
                .to("/opt/hello.txt")
                .checksum(format!("sha256:{HELLO_SHA256}"))
                .owner(0, 0);
            let c = expect_change(&op, &sys);
            op.apply(&sys, c).unwrap();
            let f = fake.file("/opt/hello.txt").unwrap();
            assert_eq!(
                (f.mode, f.uid, f.gid, f.bytes.as_slice()),
                (mode, 0, 0, HELLO)
            );
            assert_eq!(
                staged(&fake, planted, "/opt/hello.txt"),
                [Set::Mode(0o755), Set::Owner(0, 0), Set::Mode(mode)],
                "{mode:o}"
            );
            assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        }
    }

    /// A new file wanted at 0600 under another owner gets both before it is
    /// renamed into place (#79, decision 24 on #85), mode before owner, and
    /// no third call, since 0600 has no setuid for the `chown` to clear;
    /// nothing is set on the final path afterwards. A `chown` this identity
    /// may not make, as without `CAP_CHOWN`, fails the step with nothing
    /// created at all: not a file at 0644, nor one at 0600 under the wrong
    /// owner, nor a staged file beside it.
    #[test]
    fn a_download_gets_its_mode_and_owner_before_the_rename() {
        let (base, _) = hello_server();
        let op = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .checksum(format!("sha256:{HELLO_SHA256}"))
            .mode(0o600)
            .owner(5, 6);
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let planted = fake.attr_calls().len();
        let sys = fake_sys(&fake);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(
            staged(&fake, planted, "/opt/hello.txt"),
            [Set::Mode(0o600), Set::Owner(5, 6)]
        );
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!(
            (f.mode, f.uid, f.gid, f.bytes.as_slice()),
            (0o600, 5, 6, HELLO)
        );

        let fake = Arc::new(Fake::new().with_dir("/opt").with_chown_refused());
        let sys = fake_sys(&fake);
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("Operation not permitted"), "{err}");
        assert!(fake.file("/opt/hello.txt").is_none());
        assert_eq!(sys.read_dir("/opt").unwrap(), Vec::<PathBuf>::new());
    }

    /// A download over a setuid file, under a new owner with
    /// `.mode(0o4755)`: the staged content is 0755 for its `chown` and 4755
    /// after it, so a refused `chown` never leaves the new content setuid
    /// under the old owner, and the rename puts it in place finished.
    #[test]
    fn a_download_under_a_new_owner_clears_setuid_before_the_chown() {
        let (base, _) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt").with_file_mode(
            "/opt/hello.txt",
            "old",
            0o4755,
        ));
        let planted = fake.attr_calls().len();
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .checksum(format!("sha256:{HELLO_SHA256}"))
            .mode(0o4755)
            .owner(5, 6);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(
            staged(&fake, planted, "/opt/hello.txt"),
            [Set::Mode(0o755), Set::Owner(5, 6), Set::Mode(0o4755)]
        );
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!(
            (f.mode, f.uid, f.gid, f.bytes.as_slice()),
            (0o4755, 5, 6, HELLO)
        );
    }

    /// A download over a setuid file under a new owner, with no `.mode()`:
    /// the new content gets the old mode less what a `chown` clears, as a
    /// `chown` of the old file would have left it. When that `chown` is
    /// refused the step fails and the file is as it was, old content, mode
    /// and owner (`file::copy`'s
    /// `an_owner_only_rewrite_gives_the_mode_a_chown_would_leave`).
    #[test]
    fn an_owner_only_download_gives_the_mode_a_chown_would_leave() {
        let (base, _) = hello_server();
        let op = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .checksum(format!("sha256:{HELLO_SHA256}"))
            .owner(5, 6);
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file_mode("/opt/hello.txt", "old", 0o4755)
                .with_chown_refused(),
        );
        let sys = fake_sys(&fake);
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("Operation not permitted"), "{err}");
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!(
            (f.mode, f.uid, f.bytes.as_slice()),
            (0o4755, 0, &b"old"[..])
        );

        let fake = Arc::new(Fake::new().with_dir("/opt").with_file_mode(
            "/opt/hello.txt",
            "old",
            0o4755,
        ));
        let sys = fake_sys(&fake);
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        let f = fake.file("/opt/hello.txt").unwrap();
        assert_eq!(
            (f.mode, f.uid, f.gid, f.bytes.as_slice()),
            (0o755, 5, 6, HELLO)
        );
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

    /// `Download`'s timeout bounds connecting and the response head, not the
    /// body: a release tarball that streams slowly is slow, not stuck. (The
    /// opposite of `http::Request`, whose timeout covers everything.)
    #[test]
    fn a_slow_body_is_not_cut_by_the_timeout() {
        let (base, _) = serve(vec![(
            "/slow",
            200,
            vec![("X-Stall-Body", "700".into())],
            HELLO.to_vec(),
        )]);
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/slow"))
            .to("/opt/hello.txt")
            .timeout(Duration::from_millis(300));
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/opt/hello.txt").unwrap().as_bytes(), HELLO);
    }

    /// A header that cannot be sent is refused at `check`, before anything
    /// is fetched, and a secret one is not quoted.
    #[test]
    fn a_header_that_cannot_be_sent_is_refused_at_check() {
        let (base, hits) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let op = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .header_secret("Authorization", &Secret::new("Bearer a\nb"));
        let err = op.check(&sys).unwrap_err().chain();
        assert!(
            err.contains("the secret for header `Authorization` is not a valid header value"),
            "{err}"
        );
        assert!(!err.contains("Bearer a"), "{err}");
        let err = Download::get(format!("{base}/hello.txt"))
            .to("/opt/hello.txt")
            .header("Bad Name", "x")
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("`Bad Name` is not a valid header name"),
            "{err}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// `header_secret` shows as its size, the URL's userinfo is masked in
    /// `Debug` and the diff, and a redirect to another origin arrives
    /// without the secret (the policy `http::Request` follows too).
    #[test]
    fn a_secret_header_is_hidden_and_dropped_on_a_redirect_to_another_origin() {
        use super::super::test_server::serve as serve_full;
        let target = serve_full(vec![("/x", 200, vec![], HELLO.to_vec())]);
        let first = serve_full(vec![(
            "/away",
            302,
            vec![("Location", target.url("/x"))],
            vec![],
        )]);
        let url = first.url("/away").replace("http://", "http://bob:u5er-pw@");
        let op = Download::get(&url)
            .to("/opt/hello.txt")
            .header_secret("Authorization", &Secret::new("Bearer t0k3n\n"))
            .header("Accept", "*/*");
        let dbg = format!("{op:?}");
        assert!(!dbg.contains("t0k3n") && !dbg.contains("u5er-pw"), "{dbg}");
        assert!(dbg.contains("<secret, 13 bytes>"), "{dbg}");
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let c = expect_change(&op, &sys);
        let diff = c.diff().render();
        assert!(diff.contains("http://bob:********@127.0.0.1"), "{diff}");
        assert!(!format!("{c:?}").contains("u5er-pw"));
        let report = op.apply(&sys, c).unwrap();
        let dbg = format!("{report:?}");
        assert!(
            dbg.contains("bob:********@") && !dbg.contains("u5er-pw"),
            "{dbg}"
        );
        assert_eq!(report.url, url, "the output itself keeps the URL as given");
        assert_eq!(fake.content("/opt/hello.txt").unwrap().as_bytes(), HELLO);
        assert_eq!(
            first.seen()[0].header("authorization"),
            Some("Bearer t0k3n")
        );
        let arrived = &target.seen()[0];
        assert_eq!(arrived.header("authorization"), None, "{arrived:?}");
        assert_eq!(arrived.header("accept"), Some("*/*"));
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

    /// Decision 26 on #87: a download that fails after `.backup(true)` took
    /// its copy removes it, so a failed run leaves `dest` as it was and no
    /// `.~rustible.` file beside it. Each way a streamed download fails:
    /// a checksum mismatch at the body's end and a body over `.max_bytes()`
    /// without a `Content-Length`, both in today's words; a body that ends
    /// before its `Content-Length`; and an owner this identity may not give.
    #[test]
    fn a_failed_download_leaves_dest_as_it_was_and_no_backup() {
        let s = super::super::test_server::serve(vec![
            ("/hello.txt", 200, vec![], HELLO.to_vec()),
            (
                "/unsized",
                200,
                vec![("X-Omit-Length", String::new())],
                vec![b'x'; 4096],
            ),
            (
                "/cut",
                200,
                vec![("X-Declare-Length", "4096".into())],
                vec![b'x'; 100],
            ),
        ]);
        let want = "0".repeat(64);
        let to = |path: &str| {
            Download::get(s.url(path))
                .to("/opt/hello.txt")
                .force(true)
                .backup(true)
        };
        let cases = [
            (
                to("/hello.txt").checksum(format!("sha256:{want}")),
                format!(
                    "GET {}: sha256 checksum mismatch: got {HELLO_SHA256}, want {want}; nothing written to /opt/hello.txt",
                    s.url("/hello.txt")
                ),
                false,
            ),
            (
                to("/unsized").max_bytes(100),
                format!(
                    "GET {}: the body is larger than the 100 byte limit; raise it with .max_bytes()",
                    s.url("/unsized")
                ),
                false,
            ),
            (
                to("/cut"),
                format!("GET {}: reading the body: ", s.url("/cut")),
                false,
            ),
            (
                to("/hello.txt").owner(5, 6),
                "Operation not permitted".into(),
                true,
            ),
        ];
        for (op, message, refuse_chown) in cases {
            let fake = Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello.txt", "old");
            let fake = Arc::new(if refuse_chown {
                fake.with_chown_refused()
            } else {
                fake
            });
            let sys = fake_sys(&fake);
            let c = expect_change(&op, &sys);
            let err = op.apply(&sys, c).unwrap_err().chain();
            assert!(err.contains(&message), "{err}");
            assert_eq!(fake.content("/opt/hello.txt").unwrap(), "old", "{message}");
            assert_eq!(
                sys.read_dir("/opt").unwrap(),
                [PathBuf::from("/opt/hello.txt")],
                "{message}"
            );
        }
    }

    /// The two messages a streamed failure keeps word for word are the
    /// whole step error, not a part of one: what the step reports is the
    /// body's own failure, not a write failure of `dest` wrapping it.
    #[test]
    fn a_streamed_failure_is_the_whole_error_in_todays_words() {
        let s = super::super::test_server::serve(vec![
            ("/hello.txt", 200, vec![], HELLO.to_vec()),
            (
                "/unsized",
                200,
                vec![("X-Omit-Length", String::new())],
                vec![b'x'; 4096],
            ),
        ]);
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let want = "0".repeat(128);
        let op = Download::get(s.url("/hello.txt"))
            .to("/opt/x")
            .checksum(format!("sha512:{want}"));
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert_eq!(
            err,
            format!(
                "GET {}: sha512 checksum mismatch: got {}, want {want}; nothing written to /opt/x",
                s.url("/hello.txt"),
                digest(Algorithm::Sha512, HELLO)
            )
        );
        let op = Download::get(s.url("/unsized")).to("/opt/x").max_bytes(100);
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert_eq!(
            err,
            format!(
                "GET {}: the body is larger than the 100 byte limit; raise it with .max_bytes()",
                s.url("/unsized")
            )
        );
        assert_eq!(sys.read_dir("/opt").unwrap(), Vec::<PathBuf>::new());
    }

    /// No limit unless one is set (decision 16 on #87). A head declaring a
    /// body one byte over the gibibyte that was the default is not refused
    /// on its `Content-Length`: the body is read, and this one, which ends
    /// early, fails as a cut body does. And a body of several MiB, with no
    /// `.max_bytes()`, is written whole.
    #[test]
    fn a_body_over_the_old_default_is_not_refused() {
        let big: Vec<u8> = (0..(4 << 20) + 3).map(|i| (i % 251) as u8).collect();
        let s = super::super::test_server::serve(vec![
            (
                "/huge",
                200,
                vec![("X-Declare-Length", ((1u64 << 30) + 1).to_string())],
                b"the start of a very large body".to_vec(),
            ),
            ("/big", 200, vec![], big.clone()),
        ]);
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = fake_sys(&fake);
        let op = Download::get(s.url("/huge")).to("/opt/huge");
        assert!(format!("{op:?}").contains("max_bytes: None"), "{op:?}");
        let c = expect_change(&op, &sys);
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("reading the body"), "{err}");
        assert!(!err.contains("limit"), "{err}");
        assert!(fake.file("/opt/huge").is_none());

        let op = Download::get(s.url("/big")).to("/opt/big");
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.bytes, big.len() as u64);
        assert_eq!(fake.file("/opt/big").unwrap().bytes, big);
    }

    /// `bytes` and `sha256` come from the body as it streamed, not from
    /// reading `dest` back or from its `stat`: here the `stat` says 0 bytes,
    /// as a `/proc` file's does, and `dest` is never read.
    #[test]
    fn the_report_comes_from_the_stream() {
        let (base, _) = hello_server();
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = sizeless_sys(&fake, "/opt/hello.txt");
        let url = format!("{base}/hello.txt");
        let op = Download::get(&url).to("/opt/hello.txt");
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r, report(&url, true));
        assert_eq!(fake.reads(), vec![], "dest was not read back");

        // With a checksum in another algorithm, the report still carries
        // the SHA-256, and both come from the one pass.
        let fake = Arc::new(Fake::new().with_dir("/opt"));
        let sys = sizeless_sys(&fake, "/opt/hello.txt");
        let sha512 = digest(Algorithm::Sha512, HELLO);
        let op = Download::get(&url)
            .to("/opt/hello.txt")
            .checksum(format!("sha512:{sha512}"));
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r, report(&url, true));
        assert_eq!(fake.reads(), vec![], "dest was not read back");
    }

    /// A temporary directory for a test through the in-process helper,
    /// removed when the guard drops.
    struct Scratch<'a>(&'a System, PathBuf);

    impl Drop for Scratch<'_> {
        fn drop(&mut self) {
            let _ = self.0.remove_all(&self.1);
        }
    }

    fn scratch<'a>(sys: &'a System, name: &str) -> Scratch<'a> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rustible-download-{name}-{}-{nanos}",
            std::process::id()
        ));
        sys.mkdir_all(&dir).unwrap();
        Scratch(sys, dir)
    }

    /// A body of several helper chunks.
    fn multi_chunk() -> Vec<u8> {
        use rustible_sdk::protocol::CHUNK_SIZE;
        (0..3 * CHUNK_SIZE + 5).map(|i| (i % 253) as u8).collect()
    }

    /// Through a real escalation helper (in process, as this user, over a
    /// temporary directory): a body of several helper chunks is downloaded
    /// with a checksum and a mode, and the next `check` hashes it back
    /// through the helper and is satisfied.
    #[test]
    fn a_download_through_the_helper_streams_a_multi_chunk_body() {
        let content = multi_chunk();
        let s = super::super::test_server::serve(vec![("/big", 200, vec![], content.clone())]);
        let sys = System::in_process_helper(Arc::new(Collect::default())).unwrap();
        let dir = scratch(&sys, "checksum");
        let dest = dir.1.join("big");
        sys.write_atomic(&dest, b"old").unwrap();

        let sha256 = digest(Algorithm::Sha256, &content);
        let op = Download::get(s.url("/big"))
            .to(&dest)
            .checksum(format!("sha256:{sha256}"))
            .mode(0o600)
            .backup(true);
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(
            (r.bytes, r.sha256.as_deref()),
            (content.len() as u64, Some(sha256.as_str()))
        );
        assert_eq!(sys.read(&dest).unwrap(), content);
        assert_eq!(sys.stat(&dest).unwrap().unwrap().mode & 0o7777, 0o600);
        let backup = r.backup_path.expect("a backup");
        assert_eq!(sys.read(&backup).unwrap(), b"old");
        let Plan::Satisfied(again) = op.check(&sys).unwrap() else {
            panic!("expected satisfied");
        };
        assert_eq!(again.sha256, Some(sha256));
        assert_eq!(s.hits(), 1, "check never fetched");
        assert_eq!(
            sys.read_dir(&dir.1).unwrap().len(),
            2,
            "dest and its backup, nothing staged left beside them"
        );
    }

    /// The same without `.checksum` (#82 asks for both): the body streams
    /// through the helper into a new file, the report's digest is the
    /// stream's, and the next `check` is satisfied without reading it.
    /// `.force(true)` then streams it over the existing file. And a body
    /// over `.max_bytes()`, with no `Content-Length` to refuse it early,
    /// fails part way through the helper's write, leaving the file as it
    /// was and nothing staged beside it.
    #[test]
    fn a_download_without_a_checksum_through_the_helper() {
        let content = multi_chunk();
        let s = super::super::test_server::serve(vec![
            ("/big", 200, vec![], content.clone()),
            (
                "/unsized",
                200,
                vec![("X-Omit-Length", String::new())],
                content.clone(),
            ),
        ]);
        let sys = System::in_process_helper(Arc::new(Collect::default())).unwrap();
        let dir = scratch(&sys, "plain");
        let dest = dir.1.join("big");

        let op = Download::get(s.url("/big")).to(&dest).mode(0o640);
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        let sha256 = digest(Algorithm::Sha256, &content);
        assert_eq!(
            (r.bytes, r.sha256.as_deref()),
            (content.len() as u64, Some(sha256.as_str()))
        );
        assert_eq!(sys.read(&dest).unwrap(), content);
        assert_eq!(sys.stat(&dest).unwrap().unwrap().mode & 0o7777, 0o640);
        let Plan::Satisfied(again) = op.check(&sys).unwrap() else {
            panic!("expected satisfied");
        };
        assert_eq!((again.bytes, again.sha256), (content.len() as u64, None));

        let forced = op.clone().force(true);
        let c = expect_change(&forced, &sys);
        assert_eq!(forced.apply(&sys, c).unwrap().bytes, content.len() as u64);
        assert_eq!(s.hits(), 2);

        let over = Download::get(s.url("/unsized"))
            .to(&dest)
            .force(true)
            .max_bytes(content.len() as u64 - 1);
        let c = expect_change(&over, &sys);
        let err = over.apply(&sys, c).unwrap_err().chain();
        assert_eq!(
            err,
            format!(
                "GET {}: the body is larger than the {} byte limit; raise it with .max_bytes()",
                s.url("/unsized"),
                content.len() - 1
            )
        );
        assert_eq!(sys.read(&dest).unwrap(), content);
        assert_eq!(sys.read_dir(&dir.1).unwrap(), [dest]);
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
        assert!(r.changed && !r.is_available());
        assert!(fake.file("/opt/hello.txt").is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    /// Real TLS through `ring` against a public host. Not part of
    /// `cargo test`: run with `cargo test -p rustible-std https_ -- --ignored`.
    #[test]
    #[ignore = "needs network"]
    fn https_download_from_github_with_ring_tls() {
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
