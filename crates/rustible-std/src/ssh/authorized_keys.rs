//! Keys in a user's `authorized_keys` file. Ansible's
//! `ansible.posix.authorized_key`, split by desired state (vision 6.3):
//! [`Present`] ensures keys are there (optionally exactly these, with
//! `exclusive`), [`Absent`] ensures they are not.
//!
//! Key identity is the pair (key type, base64 key). Comments and the
//! leading options field never take part in matching: an existing line that
//! carries the same key with a different comment counts as present and is
//! left exactly as it was. Blank lines, `#` comments, and lines that do not
//! parse as a key are preserved untouched, in place.

use std::path::{Path, PathBuf};

use rustible_sdk::prelude::*;

/// One line of an `authorized_keys` file, as sshd reads it:
/// `[options] key-type base64-key [comment]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicKey {
    /// The leading options field, e.g. `command="/bin/date",no-pty`. Kept
    /// verbatim, quotes included.
    pub options: Option<String>,
    /// `ssh-ed25519`, `ssh-rsa`, `ecdsa-sha2-nistp256`, `sk-...`, and their
    /// `-cert-v01@openssh.com` variants.
    pub key_type: String,
    /// The base64 body of the key.
    pub key: String,
    /// Everything after the key, trimmed. Usually `user@host`.
    pub comment: Option<String>,
}

impl PublicKey {
    /// Parse one line. `None` for blank lines, `#` comments, and anything
    /// that is not a public key line.
    pub fn parse(line: &str) -> Option<PublicKey> {
        parse_line(line)
    }

    /// The canonical line for this key: single spaces, no trailing newline.
    pub fn to_line(&self) -> String {
        let mut s = String::new();
        if let Some(o) = &self.options {
            s.push_str(o);
            s.push(' ');
        }
        s.push_str(&self.key_type);
        s.push(' ');
        s.push_str(&self.key);
        if let Some(c) = &self.comment {
            s.push(' ');
            s.push_str(c);
        }
        s
    }

    /// What makes two keys the same key: type and body only.
    pub fn same_key(&self, other: &PublicKey) -> bool {
        self.key_type == other.key_type && self.key == other.key
    }
}

/// True for the first token of a key line when it names a key type rather
/// than an options list. sshd's rule is the same: if the first field is not
/// a key type, it is the options field.
fn is_key_type(s: &str) -> bool {
    s.starts_with("ssh-") || s.starts_with("ecdsa-sha2-") || s.starts_with("sk-")
}

/// Split off the first whitespace-delimited token. The rest has its leading
/// whitespace removed.
fn take_token(s: &str) -> (&str, &str) {
    let end = s.find(|c: char| c.is_ascii_whitespace()).unwrap_or(s.len());
    (&s[..end], s[end..].trim_start())
}

/// Like [`take_token`] but whitespace inside double quotes does not end the
/// token, and a backslash escapes the next character inside quotes
/// (`command="echo \"hi\"",no-pty`).
fn take_options(s: &str) -> (&str, &str) {
    let mut in_quote = false;
    let mut escaped = false;
    let mut end = s.len();
    for (i, c) in s.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' if in_quote => escaped = true,
            '"' => in_quote = !in_quote,
            c if c.is_ascii_whitespace() && !in_quote => {
                end = i;
                break;
            }
            _ => {}
        }
    }
    (&s[..end], s[end..].trim_start())
}

fn is_base64(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=')
}

/// Pure: parse one `authorized_keys` line. `None` for blank lines, `#`
/// comments, and lines that are not a key.
pub fn parse_line(line: &str) -> Option<PublicKey> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (first, after_first) = take_options(line);
    let (options, rest) = if is_key_type(first) {
        (None, line)
    } else {
        (Some(first.to_string()), after_first)
    };
    let (key_type, rest) = take_token(rest);
    if !is_key_type(key_type) {
        return None;
    }
    let (key, rest) = take_token(rest);
    if !is_base64(key) {
        return None;
    }
    let comment = rest.trim();
    Some(PublicKey {
        options,
        key_type: key_type.to_string(),
        key: key.to_string(),
        comment: (!comment.is_empty()).then(|| comment.to_string()),
    })
}

/// One line of the file. Key lines keep their raw text so that untouched
/// lines are written back byte for byte.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Other(String),
    Key { raw: String, key: PublicKey },
}

/// The line terminator a file uses, so a rewrite keeps CRLF files CRLF.
fn eol_of(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

fn parse_text(text: &str) -> Vec<Entry> {
    text.lines()
        .map(|l| match parse_line(l) {
            Some(key) => Entry::Key {
                raw: l.to_string(),
                key,
            },
            None => Entry::Other(l.to_string()),
        })
        .collect()
}

/// Join entries back into file text. Always newline-terminated when there is
/// at least one line, so a file that lacked a trailing newline gets one when
/// it is rewritten (the same normalization `file::Line` does).
fn render(entries: &[Entry], eol: &str) -> String {
    let mut out = String::new();
    for e in entries {
        match e {
            Entry::Other(l) => out.push_str(l),
            Entry::Key { raw, .. } => out.push_str(raw),
        }
        out.push_str(eol);
    }
    out
}

/// Drop repeated keys from the request, keeping the first occurrence.
fn dedupe(keys: &[PublicKey]) -> Vec<PublicKey> {
    let mut out: Vec<PublicKey> = Vec::new();
    for k in keys {
        if !out.iter().any(|o| o.same_key(k)) {
            out.push(k.clone());
        }
    }
    out
}

/// Result of [`plan_present`] or [`plan_absent`]: the new file text when
/// something changes, and what moved. Becomes a [`KeysReport`] once the op
/// knows the path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Planned {
    /// The whole file after the change. `None` when already satisfied.
    pub text: Option<String>,
    /// Keys appended (as requested).
    pub added: Vec<PublicKey>,
    /// Keys deleted (as they were in the file).
    pub removed: Vec<PublicKey>,
    /// Requested keys that were already there (as they are in the file).
    pub already_present: Vec<PublicKey>,
    /// Keys asked to be absent that were not in the file (as requested).
    pub not_present: Vec<PublicKey>,
}

impl Planned {
    fn into_report(self, path: PathBuf) -> (Option<String>, KeysReport) {
        (
            self.text,
            KeysReport {
                path,
                added: self.added,
                removed: self.removed,
                already_present: self.already_present,
                not_present: self.not_present,
            },
        )
    }
}

/// Pure: given the current file text, ensure `keys` are present. With
/// `exclusive`, every key line not in `keys` is removed; comments, blank
/// lines, and unparseable lines stay. New keys are appended at the end in
/// canonical form. Existing lines that carry a requested key are kept as
/// they are, comment and options included.
pub fn plan_present(text: &str, keys: &[PublicKey], exclusive: bool) -> Planned {
    let requested = dedupe(keys);
    let mut entries = parse_text(text);
    let mut planned = Planned::default();

    if exclusive {
        entries.retain(|e| match e {
            Entry::Key { key, .. } if !requested.iter().any(|r| r.same_key(key)) => {
                planned.removed.push(key.clone());
                false
            }
            _ => true,
        });
    }

    for req in requested {
        let existing = entries.iter().find_map(|e| match e {
            Entry::Key { key, .. } if key.same_key(&req) => Some(key.clone()),
            _ => None,
        });
        match existing {
            Some(key) => planned.already_present.push(key),
            None => {
                entries.push(Entry::Key {
                    raw: req.to_line(),
                    key: req.clone(),
                });
                planned.added.push(req);
            }
        }
    }

    if !planned.added.is_empty() || !planned.removed.is_empty() {
        planned.text = Some(render(&entries, eol_of(text)));
    }
    planned
}

/// Pure: given the current file text, remove every line carrying one of
/// `keys`. Everything else stays byte for byte.
pub fn plan_absent(text: &str, keys: &[PublicKey]) -> Planned {
    let requested = dedupe(keys);
    let mut entries = parse_text(text);
    let mut planned = Planned::default();

    entries.retain(|e| match e {
        Entry::Key { key, .. } if requested.iter().any(|r| r.same_key(key)) => {
            planned.removed.push(key.clone());
            false
        }
        _ => true,
    });
    for req in requested {
        if !planned.removed.iter().any(|r| r.same_key(&req)) {
            planned.not_present.push(req);
        }
    }

    if !planned.removed.is_empty() {
        planned.text = Some(render(&entries, eol_of(text)));
    }
    planned
}

/// Output of [`Present`] and [`Absent`]. `Present` fills `added`, `removed`
/// (only with `exclusive`), and `already_present`; `Absent` fills `removed`
/// and `not_present`. The other lists are empty.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeysReport {
    /// The `authorized_keys` file that was (or would be) written.
    pub path: PathBuf,
    /// Keys this step appended, as requested.
    pub added: Vec<PublicKey>,
    /// Keys this step deleted, as they were in the file (options and comment
    /// included).
    pub removed: Vec<PublicKey>,
    /// Requested keys that were already there, as they are in the file.
    pub already_present: Vec<PublicKey>,
    /// Keys asked to be absent that were not in the file, as requested.
    pub not_present: Vec<PublicKey>,
}

/// Whose file: a user looked up in `/etc/passwd`, or an explicit path.
#[derive(Debug, Clone)]
enum Target {
    /// `~user/.ssh/authorized_keys`, created 0600 and owned by the user; the
    /// `.ssh` directory must already exist (vision 6.7).
    User(String),
    /// Same, but with home, uid, and gid already known (from `user::Present`
    /// or `user::Existing`), so no lookup happens.
    Account { home: PathBuf, uid: u32, gid: u32 },
    /// An explicit file. No ownership handling; the parent directory must
    /// exist.
    File(PathBuf),
}

/// What a target resolves to on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    path: PathBuf,
    /// The user's `.ssh` directory to create if missing (user form only).
    ssh_dir: Option<PathBuf>,
    /// uid and gid to give created files (user form only).
    owner: Option<(u32, u32)>,
}

/// A relative or empty home (legal in `/etc/passwd`; login treats empty as
/// `/`) would resolve `.ssh` against the process cwd, which for an escalated
/// binary is root's home: a key would land in the wrong account's file.
fn ensure_absolute_home(home: &Path) -> Result<()> {
    if !home.is_absolute() {
        bail!(
            "home directory {:?} is not an absolute path; refusing to guess where authorized_keys is",
            home.display().to_string()
        );
    }
    Ok(())
}

impl Target {
    fn resolve(&self, sys: &System) -> Result<Resolved> {
        match self {
            Target::File(path) => Ok(Resolved {
                path: path.clone(),
                ssh_dir: None,
                owner: None,
            }),
            Target::Account { home, uid, gid } => {
                ensure_absolute_home(home)?;
                let ssh_dir = home.join(".ssh");
                Ok(Resolved {
                    path: ssh_dir.join("authorized_keys"),
                    ssh_dir: Some(ssh_dir),
                    owner: Some((*uid, *gid)),
                })
            }
            Target::User(name) => {
                // /etc/passwd only (vision 7.4 and 13 plan the user ops around
                // it); NSS-only accounts (LDAP, SSSD, homed) are a known limit,
                // recorded in DECISIONS.md.
                let passwd = sys.read_to_string("/etc/passwd")?;
                let Some(entry) = crate::user::lookup_user(&passwd, name)? else {
                    bail!(
                        "user `{name}` does not exist in /etc/passwd; \
                         ssh::authorized_keys does not create users, use user::Present first"
                    );
                };
                let (uid, gid, home) = (entry.uid, entry.gid, entry.home);
                ensure_absolute_home(&home)?;
                let ssh_dir = home.join(".ssh");
                Ok(Resolved {
                    path: ssh_dir.join("authorized_keys"),
                    ssh_dir: Some(ssh_dir),
                    owner: Some((uid, gid)),
                })
            }
        }
    }
}

/// Parse the requested key lines, refusing anything that is not a key.
fn parse_keys(lines: &[String]) -> Result<Vec<PublicKey>> {
    lines
        .iter()
        .map(|l| {
            // One requested key is one file line: an embedded newline would
            // smuggle a second, unrequested key into a 0600 file.
            if l.chars().any(char::is_control) {
                return Err(Error::msg(format!(
                    "requested key contains a control character (newline?); one key per entry: {}",
                    l.trim().replace(['\n', '\r'], "\\n")
                )));
            }
            parse_line(l).ok_or_else(|| Error::msg(format!("not a public key line: {}", l.trim())))
        })
        .collect()
}

/// Read the file if it exists, else `None`. Refuses a target that is a
/// directory.
fn read_existing(sys: &System, path: &Path) -> Result<Option<String>> {
    use rustible_sdk::backend::FileKind;
    match sys.stat(path)? {
        None => Ok(None),
        Some(s) if s.kind == FileKind::Dir => bail!("{} is a directory", path.display()),
        // An atomic rewrite would replace the link itself with a regular file
        // and leave the link's target stale; refuse rather than surprise.
        Some(s) if s.kind == FileKind::Symlink => bail!(
            "{} is a symlink; ssh::authorized_keys does not rewrite through symlinks, point the op at the real file with in_file()",
            path.display()
        ),
        Some(_) => Ok(Some(sys.read_to_string(path)?)),
    }
}

/// Vision 6.7: this op owns the `authorized_keys` file and nothing else.
/// The parent directory (`~/.ssh` in the user forms) must already exist;
/// the vision's playbook ensures it with `file::Directory` in its own step.
/// A symlinked parent is fine (`stat_follow`).
fn check_parent(sys: &System, resolved: &Resolved) -> Result<()> {
    let parent = match &resolved.ssh_dir {
        Some(d) => d.clone(),
        None => match resolved.path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => return Ok(()),
        },
    };
    match sys.stat_follow(&parent)? {
        Some(s) if s.kind == rustible_sdk::backend::FileKind::Dir => Ok(()),
        Some(_) => bail!("{} exists and is not a directory", parent.display()),
        None => bail!(
            "{} does not exist; ssh::authorized_keys does not create it (vision 6.7), \
             ensure it first with file::Directory::at(..).mode(0o700).owner(..)",
            parent.display()
        ),
    }
}

/// Write the planned text, giving a new file mode 0600 and, in the user
/// forms, the user's ownership. Existing files keep their attributes.
fn write_file(sys: &System, resolved: &Resolved, text: &str) -> Result<()> {
    let is_new = !sys.exists(&resolved.path)?;
    sys.write_atomic(&resolved.path, text.as_bytes())?;
    if is_new {
        sys.set_mode(&resolved.path, 0o600)?;
        if let Some((uid, gid)) = resolved.owner {
            sys.set_owner(&resolved.path, uid, gid)?;
        }
    }
    Ok(())
}

/// Ensure keys are in a user's `authorized_keys`. Ansible's
/// `ansible.posix.authorized_key` with `state: present`.
///
/// ```no_run
/// # use rustible_sdk::prelude::*;
/// # use rustible_std::ssh::authorized_keys;
/// # fn playbook(ctx: &mut Ctx) -> Result<()> {
/// let keys = ["ssh-ed25519 AAAAC3...XYZ cadu@x86", "ssh-ed25519 AAAAC3...ABC cadu@arm"];
/// ctx.step("Install authorized keys",
///     authorized_keys::Present::for_user_name("cadu").keys(keys).exclusive(true))?;
/// # Ok(()) }
/// ```
///
/// The user-name form reads `/etc/passwd` for home, uid, and gid; the
/// `for_user(&account)` and `for_account(home, uid, gid)` forms take them
/// directly (from `user::Present` or `user::Existing`) with no lookup. Both create `authorized_keys` (0600,
/// owned by the user) when missing and never touch the attributes of one
/// that exists; `~/.ssh` must already exist (vision 6.7: ensure it with
/// `file::Directory` first). The `in_file` form writes an explicit path and
/// handles no ownership.
#[derive(Debug, Clone)]
pub struct Present {
    target: Target,
    keys: Vec<String>,
    exclusive: bool,
}

impl Present {
    /// Keys for the account named `name`, in `~name/.ssh/authorized_keys`.
    pub fn for_user_name(name: impl Into<String>) -> PresentBuilder {
        PresentBuilder {
            target: Target::User(name.into()),
            exclusive: false,
        }
    }

    /// Keys for an account whose home, uid, and gid are already known (the
    /// `user::Account` output of `user::Present`/`user::Existing`), in
    /// `<home>/.ssh/authorized_keys`. No lookup.
    pub fn for_account(home: impl Into<PathBuf>, uid: u32, gid: u32) -> PresentBuilder {
        PresentBuilder {
            target: Target::Account {
                home: home.into(),
                uid,
                gid,
            },
            exclusive: false,
        }
    }

    /// Keys for a `user::Account` (vision 6.1): `for_account` with the
    /// account's home, uid, and gid. No lookup.
    pub fn for_user(account: &crate::user::Account) -> PresentBuilder {
        Self::for_account(&account.home, account.uid, account.gid)
    }

    /// Keys in an explicit file. No ownership handling.
    pub fn in_file(path: impl Into<PathBuf>) -> PresentBuilder {
        PresentBuilder {
            target: Target::File(path.into()),
            exclusive: false,
        }
    }

    /// Remove every key that is not in the list (Ansible's `exclusive: true`).
    /// Comments and blank lines stay. Also available on the builder.
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }
}

/// Builder for [`Present`]; `keys` finishes it.
#[derive(Debug, Clone)]
pub struct PresentBuilder {
    target: Target,
    exclusive: bool,
}

impl PresentBuilder {
    /// Remove every key that is not in the list (Ansible's `exclusive: true`).
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }

    /// The keys that must be present, as full public key lines
    /// (`[options] type base64 [comment]`). Finishes the builder. Lines that
    /// are not keys make the step fail at `check`.
    pub fn keys<I, S>(self, keys: I) -> Present
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Present {
            target: self.target,
            keys: keys.into_iter().map(Into::into).collect(),
            exclusive: self.exclusive,
        }
    }
}

impl Op for Present {
    type Output = KeysReport;

    fn check(&self, sys: &System) -> Result<Plan<KeysReport>> {
        let keys = parse_keys(&self.keys)?;
        let resolved = self.target.resolve(sys)?;
        check_parent(sys, &resolved)?;
        let before = read_existing(sys, &resolved.path)?;
        let planned = plan_present(before.as_deref().unwrap_or(""), &keys, self.exclusive);
        let (text, report) = planned.into_report(resolved.path.clone());
        match text {
            None => Ok(Plan::Satisfied(report)),
            Some(after) => Ok(Plan::change_predicting(
                Diff::text(&resolved.path, before.unwrap_or_default(), after),
                report,
            )),
        }
    }

    fn apply(&self, sys: &System, change: Change<KeysReport>) -> Result<KeysReport> {
        let Diff::Text { after, .. } = &change.diff else {
            bail!("authorized_keys::Present::apply received a non-text diff");
        };
        let Some(report) = change.predicted else {
            bail!("authorized_keys::Present::apply received a change without its prediction");
        };
        let resolved = self.target.resolve(sys)?;
        write_file(sys, &resolved, after)?;
        Ok(report)
    }
}

/// Ensure keys are not in a user's `authorized_keys`. Ansible's
/// `ansible.posix.authorized_key` with `state: absent`.
///
/// ```no_run
/// # use rustible_sdk::prelude::*;
/// # use rustible_std::ssh::authorized_keys;
/// # fn playbook(ctx: &mut Ctx) -> Result<()> {
/// # let old_keys = ["ssh-ed25519 AAAAC3...OLD cadu@retired-laptop"];
/// let revoked = ctx.step("Revoke compromised keys",
///     authorized_keys::Absent::for_user_name("rustible").keys(old_keys))?;
/// ctx.log(format!("removed {} key(s)", revoked.removed.len()));
/// # Ok(()) }
/// ```
///
/// A missing file is already satisfied. Nothing is created and no
/// attributes are changed.
#[derive(Debug, Clone)]
pub struct Absent {
    target: Target,
    keys: Vec<String>,
}

impl Absent {
    /// Keys for the account named `name`, in `~name/.ssh/authorized_keys`.
    pub fn for_user_name(name: impl Into<String>) -> AbsentBuilder {
        AbsentBuilder {
            target: Target::User(name.into()),
        }
    }

    /// Keys for an account whose home, uid, and gid are already known. No lookup.
    pub fn for_account(home: impl Into<PathBuf>, uid: u32, gid: u32) -> AbsentBuilder {
        AbsentBuilder {
            target: Target::Account {
                home: home.into(),
                uid,
                gid,
            },
        }
    }

    /// Keys for a `user::Account`: `for_account` with the account's home,
    /// uid, and gid. No lookup.
    pub fn for_user(account: &crate::user::Account) -> AbsentBuilder {
        Self::for_account(&account.home, account.uid, account.gid)
    }

    /// Keys in an explicit file.
    pub fn in_file(path: impl Into<PathBuf>) -> AbsentBuilder {
        AbsentBuilder {
            target: Target::File(path.into()),
        }
    }
}

/// Builder for [`Absent`]; `keys` finishes it.
#[derive(Debug, Clone)]
pub struct AbsentBuilder {
    target: Target,
}

impl AbsentBuilder {
    /// The keys that must be absent, as full public key lines. Only the key
    /// type and body are matched. Finishes the builder.
    pub fn keys<I, S>(self, keys: I) -> Absent
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Absent {
            target: self.target,
            keys: keys.into_iter().map(Into::into).collect(),
        }
    }
}

impl Op for Absent {
    type Output = KeysReport;

    fn check(&self, sys: &System) -> Result<Plan<KeysReport>> {
        let keys = parse_keys(&self.keys)?;
        let resolved = self.target.resolve(sys)?;
        let before = read_existing(sys, &resolved.path)?;
        let planned = plan_absent(before.as_deref().unwrap_or(""), &keys);
        let (text, report) = planned.into_report(resolved.path.clone());
        match text {
            None => Ok(Plan::Satisfied(report)),
            Some(after) => Ok(Plan::change_predicting(
                Diff::text(&resolved.path, before.unwrap_or_default(), after),
                report,
            )),
        }
    }

    fn apply(&self, sys: &System, change: Change<KeysReport>) -> Result<KeysReport> {
        let Diff::Text { after, .. } = &change.diff else {
            bail!("authorized_keys::Absent::apply received a non-text diff");
        };
        let Some(report) = change.predicted else {
            bail!("authorized_keys::Absent::apply received a change without its prediction");
        };
        let resolved = self.target.resolve(sys)?;
        // The file exists (check found keys in it), so this never creates.
        sys.write_atomic(&resolved.path, after.as_bytes())?;
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::*;

    const K1: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE cadu@x86";
    const K2: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITWO cadu@arm";
    const K3: &str = "ssh-rsa AAAAB3NzaC1yc2EAAAADAQABTHREE ci@jenkins";

    fn key(s: &str) -> PublicKey {
        parse_line(s).unwrap()
    }

    // ---- parsing ----

    #[test]
    fn parses_plain_key_with_comment() {
        let k = key(K1);
        assert_eq!(k.options, None);
        assert_eq!(k.key_type, "ssh-ed25519");
        assert_eq!(k.key, "AAAAC3NzaC1lZDI1NTE5AAAAIONE");
        assert_eq!(k.comment.as_deref(), Some("cadu@x86"));
        assert_eq!(k.to_line(), K1);
    }

    #[test]
    fn parses_key_without_comment_and_with_extra_spaces() {
        let k = key("  ssh-rsa   AAAAB3   ");
        assert_eq!(k.key_type, "ssh-rsa");
        assert_eq!(k.key, "AAAAB3");
        assert_eq!(k.comment, None);
        assert_eq!(k.to_line(), "ssh-rsa AAAAB3");
    }

    #[test]
    fn parses_options_field_with_quoted_spaces() {
        let line =
            r#"command="echo \"hi there\"",no-pty,from="10.0.0.0/8" ssh-ed25519 AAAAC3 deploy"#;
        let k = key(line);
        assert_eq!(
            k.options.as_deref(),
            Some(r#"command="echo \"hi there\"",no-pty,from="10.0.0.0/8""#)
        );
        assert_eq!(k.key_type, "ssh-ed25519");
        assert_eq!(k.key, "AAAAC3");
        assert_eq!(k.comment.as_deref(), Some("deploy"));
        assert_eq!(k.to_line(), line);
    }

    #[test]
    fn parses_cert_and_sk_types() {
        assert_eq!(
            key("sk-ssh-ed25519@openssh.com AAAA yubikey").key_type,
            "sk-ssh-ed25519@openssh.com"
        );
        assert_eq!(
            key("ssh-ed25519-cert-v01@openssh.com AAAA").key_type,
            "ssh-ed25519-cert-v01@openssh.com"
        );
        assert_eq!(
            key("ecdsa-sha2-nistp256 AAAA").key_type,
            "ecdsa-sha2-nistp256"
        );
    }

    #[test]
    fn non_keys_do_not_parse() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   "), None);
        assert_eq!(parse_line("# a comment"), None);
        assert_eq!(parse_line("garbage line here"), None);
        assert_eq!(parse_line("ssh-ed25519"), None);
        assert_eq!(parse_line("ssh-ed25519 not*base64"), None);
        assert_eq!(parse_line("no-pty ssh-ed25519"), None);
    }

    #[test]
    fn comment_and_options_do_not_affect_identity() {
        let a = key("ssh-ed25519 AAAA laptop");
        let b = key("no-pty ssh-ed25519 AAAA desktop");
        let c = key("ssh-ed25519 BBBB laptop");
        assert!(a.same_key(&b));
        assert!(!a.same_key(&c));
    }

    // ---- plan_present ----

    #[test]
    fn present_on_empty_file_appends() {
        let p = plan_present("", &[key(K1), key(K2)], false);
        assert_eq!(p.text.as_deref(), Some(&*format!("{K1}\n{K2}\n")));
        assert_eq!(p.added, vec![key(K1), key(K2)]);
        assert!(p.removed.is_empty() && p.already_present.is_empty());
    }

    #[test]
    fn present_is_satisfied_when_all_there() {
        let text = format!("{K1}\n{K2}\n");
        let p = plan_present(&text, &[key(K2), key(K1)], false);
        assert_eq!(p.text, None);
        assert_eq!(p.already_present, vec![key(K2), key(K1)]);
        assert!(p.added.is_empty());
    }

    #[test]
    fn present_matches_ignoring_comment_and_keeps_existing_line() {
        let text = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE   old-comment\n";
        let p = plan_present(text, &[key(K1)], false);
        assert_eq!(p.text, None, "same key with another comment is present");
        assert_eq!(p.already_present[0].comment.as_deref(), Some("old-comment"));
    }

    #[test]
    fn present_adds_missing_trailing_newline_when_writing() {
        let p = plan_present(K1, &[key(K2)], false);
        assert_eq!(p.text.as_deref(), Some(&*format!("{K1}\n{K2}\n")));
    }

    #[test]
    fn present_lacking_trailing_newline_alone_is_satisfied() {
        let p = plan_present(K1, &[key(K1)], false);
        assert_eq!(p.text, None);
    }

    #[test]
    fn present_preserves_comments_blank_lines_and_options() {
        let text = format!(
            "# managed keys\n\n{K1}\n\ncommand=\"/bin/date\",no-pty {K3}\nnot a key at all\n"
        );
        let p = plan_present(&text, &[key(K2)], false);
        assert_eq!(p.text.as_deref(), Some(&*format!("{text}{K2}\n")));
    }

    #[test]
    fn present_dedupes_requested_keys() {
        let p = plan_present(
            "",
            &[key(K1), key("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIONE dup")],
            false,
        );
        assert_eq!(p.text.as_deref(), Some(&*format!("{K1}\n")));
        assert_eq!(p.added.len(), 1);
    }

    #[test]
    fn present_exclusive_removes_strangers_and_keeps_comments() {
        let text = format!("# header\n{K1}\n{K3}\n\n");
        let p = plan_present(&text, &[key(K1), key(K2)], true);
        assert_eq!(
            p.text.as_deref(),
            Some(&*format!("# header\n{K1}\n\n{K2}\n"))
        );
        assert_eq!(p.removed, vec![key(K3)]);
        assert_eq!(p.added, vec![key(K2)]);
        assert_eq!(p.already_present, vec![key(K1)]);
    }

    #[test]
    fn present_exclusive_is_satisfied_when_exact() {
        let text = format!("{K1}\n# note\n{K2}\n");
        let p = plan_present(&text, &[key(K1), key(K2)], true);
        assert_eq!(p.text, None);
    }

    #[test]
    fn present_non_exclusive_leaves_strangers() {
        let text = format!("{K3}\n");
        let p = plan_present(&text, &[key(K1)], false);
        assert_eq!(p.text.as_deref(), Some(&*format!("{K3}\n{K1}\n")));
        assert!(p.removed.is_empty());
    }

    // ---- plan_absent ----

    #[test]
    fn absent_removes_matching_lines_only() {
        let text = format!("# keep\n{K1}\nno-pty {K2}\n{K3}\n");
        let p = plan_absent(
            &text,
            &[key("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAITWO other")],
        );
        assert_eq!(p.text.as_deref(), Some(&*format!("# keep\n{K1}\n{K3}\n")));
        assert_eq!(p.removed.len(), 1);
        assert_eq!(p.removed[0].options.as_deref(), Some("no-pty"));
        assert!(p.not_present.is_empty());
    }

    #[test]
    fn absent_of_missing_key_is_satisfied() {
        let text = format!("{K1}\n");
        let p = plan_absent(&text, &[key(K2)]);
        assert_eq!(p.text, None);
        assert_eq!(p.not_present, vec![key(K2)]);
        assert!(p.removed.is_empty());
    }

    #[test]
    fn absent_on_empty_text_is_satisfied() {
        let p = plan_absent("", &[key(K1)]);
        assert_eq!(p.text, None);
        assert_eq!(p.not_present, vec![key(K1)]);
    }

    #[test]
    fn absent_removes_every_duplicate_line_of_a_key() {
        let text = format!("{K1}\n{K2}\n{K1}\n");
        let p = plan_absent(&text, &[key(K1), key(K1)]);
        assert_eq!(p.text.as_deref(), Some(&*format!("{K2}\n")));
        assert_eq!(p.removed.len(), 2);
    }

    #[test]
    fn requested_key_with_embedded_newline_is_refused() {
        let fake = fake_with_user_and_ssh_dir();
        let sys = fake_sys(&fake);
        let smuggled = format!("{K1}\n{K2}");
        let e = Present::for_user_name("cadu")
            .keys([smuggled])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(e.contains("control character"), "{e}");
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        let text = format!("# c\r\n{K1}\r\n");
        let planned = plan_present(&text, &[key(K2)], false);
        assert_eq!(planned.text.unwrap(), format!("# c\r\n{K1}\r\n{K2}\r\n"));
    }

    #[test]
    fn for_account_form_does_no_lookup() {
        let fake = Arc::new(Fake::new().with_dir("/srv/home/.ssh"));
        let sys = fake_sys(&fake);
        let op = Present::for_account("/srv/home", 42, 43).keys([K1]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!()
        };
        op.apply(&sys, c).unwrap();
        let f = fake.file("/srv/home/.ssh/authorized_keys").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o600, 42, 43));
    }

    #[test]
    fn for_user_chains_from_user_existing() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_file("/etc/group", "root:x:0:\ncadu:x:1001:\n")
                .with_dir("/home/cadu")
                .with_dir("/home/cadu/.ssh"),
        );
        let sys = fake_sys(&fake);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let account = ctx
            .step("lookup", crate::user::Existing::named("cadu"))
            .unwrap();
        let r = ctx
            .step("keys", Present::for_user(&account).keys([K1]))
            .unwrap();
        assert!(r.changed);
        assert_eq!(r.path, PathBuf::from("/home/cadu/.ssh/authorized_keys"));
        let f = fake.file("/home/cadu/.ssh/authorized_keys").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o600, 1000, 1001));
        let r = ctx
            .step("revoke", Absent::for_user(&account).keys([K1]))
            .unwrap();
        assert!(r.changed);
        assert_eq!(fake.content("/home/cadu/.ssh/authorized_keys").unwrap(), "");
    }

    // ---- Fake backend ----

    const PASSWD: &str =
        "root:x:0:0:root:/root:/bin/bash\ncadu:x:1000:1001:Cadu:/home/cadu:/bin/zsh\n";

    fn fake_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    fn fake_with_user() -> Arc<Fake> {
        Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu"),
        )
    }

    fn fake_with_user_and_ssh_dir() -> Arc<Fake> {
        Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/home/cadu/.ssh"),
        )
    }

    #[test]
    fn user_form_creates_file_with_mode_and_owner_inside_existing_ssh_dir() {
        let fake = fake_with_user_and_ssh_dir();
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K1, K2]);

        let Plan::Change(change) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(change.diff.short(), "+2 -0 lines");
        let predicted = change.predicted.clone().unwrap();
        assert_eq!(
            predicted.path,
            PathBuf::from("/home/cadu/.ssh/authorized_keys")
        );
        assert_eq!(predicted.added, vec![key(K1), key(K2)]);
        assert!(
            fake.file("/home/cadu/.ssh/authorized_keys").is_none(),
            "check must not create"
        );

        let report = op.apply(&sys, change).unwrap();
        assert_eq!(report, predicted);
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K1}\n{K2}\n")
        );
        let file = fake.file("/home/cadu/.ssh/authorized_keys").unwrap();
        assert_eq!((file.mode, file.uid, file.gid), (0o600, 1000, 1001));

        // Second check is satisfied and reports what is there.
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.already_present.len(), 2);
        assert!(r.added.is_empty());
    }

    #[test]
    fn existing_file_and_dir_keep_their_attributes() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/home/cadu/.ssh")
                .with_file_mode("/home/cadu/.ssh/authorized_keys", format!("{K1}\n"), 0o644),
        );
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K2]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!()
        };
        op.apply(&sys, c).unwrap();
        let file = fake.file("/home/cadu/.ssh/authorized_keys").unwrap();
        assert_eq!(file.mode, 0o644, "existing file mode untouched");
        assert_eq!(fake.file("/home/cadu/.ssh").unwrap().mode, 0o755);
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K1}\n{K2}\n")
        );
    }

    #[test]
    fn exclusive_through_ctx_is_changed_then_ok() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/home/cadu/.ssh")
                .with_file("/home/cadu/.ssh/authorized_keys", format!("{K3}\n{K1}\n")),
        );
        let sys = fake_sys(&fake);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        // Vision 6.1 order: `.keys(..).exclusive(true)`.
        let op = Present::for_user_name("cadu")
            .keys([K1, K2])
            .exclusive(true);

        let r = ctx.step("keys", op.clone()).unwrap();
        assert!(r.changed);
        assert_eq!(r.removed, vec![key(K3)]);
        assert_eq!(r.added, vec![key(K2)]);
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K1}\n{K2}\n")
        );

        let r = ctx.step("keys again", op).unwrap();
        assert!(!r.changed);
    }

    #[test]
    fn check_mode_predicts_and_writes_nothing() {
        let fake = fake_with_user_and_ssh_dir();
        let sink = Arc::new(Collect::default());
        let sys = System::fake(fake.clone(), sink).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step("keys", Present::for_user_name("cadu").keys([K1]))
            .unwrap();
        assert!(r.changed && r.predicted && r.is_available());
        assert_eq!(r.added, vec![key(K1)]);
        assert!(fake.file("/home/cadu/.ssh/authorized_keys").is_none());
    }

    #[test]
    fn in_file_form_writes_the_given_path_without_owner() {
        let fake = Arc::new(Fake::new().with_dir("/etc/ssh/keys"));
        let sys = fake_sys(&fake);
        let op = Present::in_file("/etc/ssh/keys/root").keys([K1]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!()
        };
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.path, PathBuf::from("/etc/ssh/keys/root"));
        let f = fake.file("/etc/ssh/keys/root").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o600, 0, 0));
        assert!(fake.commands().is_empty(), "no commands are run");
    }

    #[test]
    fn in_file_form_refuses_missing_parent() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake);
        let err = Present::in_file("/nope/authorized_keys")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("/nope does not exist"), "{err}");
    }

    #[test]
    fn unknown_user_is_an_error() {
        let fake = fake_with_user();
        let sys = fake_sys(&fake);
        let err = Present::for_user_name("ghost")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("user `ghost` does not exist"), "{err}");
        let err = Absent::for_user_name("ghost")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("user `ghost` does not exist"), "{err}");
    }

    #[test]
    fn user_form_refuses_missing_ssh_dir_naming_file_directory() {
        // Vision 6.7: the op owns the file, not the directory.
        let fake = fake_with_user();
        let sys = fake_sys(&fake);
        let err = Present::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("/home/cadu/.ssh does not exist") && err.contains("file::Directory"),
            "{err}"
        );
        assert!(fake.file("/home/cadu/.ssh").is_none());
    }

    #[test]
    fn create_path_through_ctx_passes_the_mutation_guard() {
        // The step driver runs `check` under the guard that refuses file
        // mutations (vision 7.3). Creating the file is the most write-prone
        // path, so drive it end to end: a `check` that touched the
        // filesystem would fail the step here.
        let fake = fake_with_user_and_ssh_dir();
        let sys = fake_sys(&fake);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step("keys", Present::for_user_name("cadu").keys([K1]))
            .unwrap();
        assert!(r.changed && !r.predicted);
        assert_eq!(r.added, vec![key(K1)]);
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K1}\n")
        );
    }

    #[test]
    fn invalid_key_line_is_an_error() {
        let fake = fake_with_user();
        let sys = fake_sys(&fake);
        let err = Present::for_user_name("cadu")
            .keys(["this is not a key"])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not a public key line"), "{err}");
    }

    #[test]
    fn absent_removes_exactly_one_of_three() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/home/cadu/.ssh")
                .with_file(
                    "/home/cadu/.ssh/authorized_keys",
                    format!("# keys\n{K1}\n{K2}\n{K3}\n"),
                ),
        );
        let sys = fake_sys(&fake);
        let op = Absent::for_user_name("cadu").keys([K2]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "+0 -1 lines");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.removed, vec![key(K2)]);
        assert!(r.not_present.is_empty());
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("# keys\n{K1}\n{K3}\n")
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn absent_on_missing_file_is_satisfied_and_creates_nothing() {
        let fake = fake_with_user();
        let sys = fake_sys(&fake);
        let Plan::Satisfied(r) = Absent::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap()
        else {
            panic!("expected satisfied")
        };
        assert_eq!(r.not_present, vec![key(K1)]);
        assert!(fake.file("/home/cadu/.ssh").is_none());
    }

    #[test]
    fn relative_or_empty_home_is_refused() {
        let fake = Arc::new(Fake::new().with_dir("/root/.ssh"));
        let sys = fake_sys(&fake);
        let e = Present::for_account("", 900, 900)
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not an absolute path"), "{e}");
        let fake =
            Arc::new(Fake::new().with_file("/etc/passwd", "svc:x:900:900:::/usr/sbin/nologin\n"));
        let sys = fake_sys(&fake);
        let e = Present::for_user_name("svc")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not an absolute path"), "{e}");
    }
}
