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
//!
//! In the user forms, [`Present`] owns `~/.ssh` as well as the file inside
//! it: it creates the directory when it is missing and holds both at the
//! mode sshd requires, owned by the account. The one thing it never creates
//! is the home directory itself. See [`Present`] for why, and for what it
//! still refuses.

use std::path::{Path, PathBuf};

use rustible_sdk::backend::{FileKind, Stat};
use rustible_sdk::prelude::*;

use crate::file::{Owner, apply_attrs, plan_attrs};

/// The mode `~/.ssh` is held at: what `sshd(8)` recommends, and what
/// `ansible.posix.authorized_key` sets.
///
/// The half that is not a preference is the *write* bits. sshd's
/// `StrictModes` is on by default, and its own manual says that if
/// `authorized_keys`, `~/.ssh` or the home directory "are writable by other
/// users ... sshd will not allow it to be used": the keys are then ignored
/// silently, so a group-writable `.ssh` is an account that cannot log in
/// while the run reports success. Ownership counts too — sshd accepts the
/// user or root and refuses a third party.
///
/// 0700 rather than 0755 is the recommendation rather than the rule, and is
/// what this op sets because it is also what Ansible sets.
const SSH_DIR_MODE: u32 = 0o700;

/// The mode `authorized_keys` is held at: `sshd(8)`'s recommended
/// "read/write for the user, and not accessible by others", and Ansible's
/// value. sshd itself only refuses a file writable by others — 0644 passes
/// its check — so unlike the directory this is hygiene rather than a login
/// failure: a world-readable `authorized_keys` tells every account on the
/// box which keys open this one.
const KEYS_FILE_MODE: u32 = 0o600;

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
                created_dir: None,
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
    /// The `~/.ssh` directory, when *this step* created it. `None` when it
    /// was already there, in the `in_file` form, and for [`Absent`], which
    /// creates nothing.
    pub created_dir: Option<PathBuf>,
}

/// Whose file: a user looked up in `/etc/passwd`, or an explicit path.
#[derive(Debug, Clone)]
enum Target {
    /// `~user/.ssh/authorized_keys`. The op owns the file *and* the `.ssh`
    /// directory holding it: both are created when missing and kept at the
    /// mode sshd requires, owned by the account.
    User(String),
    /// Same, but with home, uid, and gid already known (from `user::Present`
    /// or `user::Existing`), so no lookup happens.
    Account { home: PathBuf, uid: u32, gid: u32 },
    /// An explicit file. No ownership handling, and no directory: there is
    /// no account here, so nothing says who a created directory should
    /// belong to. The parent must exist.
    File(PathBuf),
}

/// What a target resolves to on the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Resolved {
    path: PathBuf,
    /// The user's `.ssh` directory, which the user forms own alongside the
    /// file in it. `None` in the `in_file` form, which owns neither.
    ssh_dir: Option<PathBuf>,
    /// uid and gid the directory and the file must belong to (user forms
    /// only). Its presence is what makes this op manage attributes at all.
    owner: Option<(u32, u32)>,
}

impl Resolved {
    /// The owner as `file`'s attribute planner wants it, so `~/.ssh` and
    /// `authorized_keys` are planned and rendered by the same code as
    /// `file::Directory` and `file::Attrs`.
    fn file_owner(&self) -> Option<Owner> {
        self.owner.map(|(uid, gid)| Owner { uid, gid })
    }
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
                // The only variant of this op that is not portable, and the
                // reason the refusal lives here rather than on `Present` and
                // `Absent`: `Target::File` and `Target::Account` are plain
                // file work and run on macOS today. Naming the working
                // alternative matters, because the caller has one.
                match sys.facts().os {
                    Os::Linux => {}
                    Os::Macos => bail!(
                        "ssh::authorized_keys cannot look `{name}` up in /etc/passwd on macOS, \
                         where that file lists only system services and the accounts live in \
                         Open Directory. Pass the account directly with \
                         `for_account(home, uid, gid)`, which works here"
                    ),
                    ref other => bail!(
                        "ssh::authorized_keys looks users up in /etc/passwd, which rustible \
                         only trusts on Linux; this host is {}. Pass the account directly \
                         with `for_account(home, uid, gid)`",
                        other.name()
                    ),
                }
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
/// Read the file if it exists, along with the `stat` its attributes are
/// planned from. Refuses a target that is a directory or a symlink.
fn read_existing(sys: &System, path: &Path) -> Result<Option<(String, Stat)>> {
    match sys.stat(path)? {
        None => Ok(None),
        Some(s) if s.kind == FileKind::Dir => bail!("{} is a directory", path.display()),
        // An atomic rewrite would replace the link itself with a regular file
        // and leave the link's target stale; refuse rather than surprise.
        Some(s) if s.kind == FileKind::Symlink => bail!(
            "{} is a symlink; ssh::authorized_keys does not rewrite through symlinks, point the op at the real file with in_file()",
            path.display()
        ),
        Some(s) => Ok(Some((sys.read_to_string(path)?, s))),
    }
}

/// What `check` decided about `~/.ssh`.
#[derive(Debug, Default)]
struct SshDir {
    /// The directory this step will create. `None` when it is already there,
    /// or when this form of the op owns no directory.
    create: Option<PathBuf>,
    /// Attribute differences to report and repair, `exists` included when
    /// the directory is being created. Empty when it is already right.
    changes: Vec<AttrChange>,
}

/// Plan `~/.ssh` in the user forms: create it when it is absent, and hold it
/// at 0700 owned by the account when it is not.
///
/// This is the one place the op reaches past its own file, and it is the
/// exception vision 6.7 names: a file belonging to a single account, so the
/// directory holding it belongs to that account alone and is not a shared
/// resource, and the op was handed the account, so nothing about the owner
/// or the mode is a guess. The two limits 6.7 puts on it are kept here —
/// the creation and the repair are lines in the diff, and the home
/// directory itself is never created. `ansible.posix.authorized_key`'s
/// `manage_dir` defaults to true and does the same work, though it reaches it
/// only on a run that is already rewriting the file; [`Present`] goes
/// further, and says why.
///
/// Two refusals survive, both "something unexpected is in the way": `.ssh`
/// is not a directory, or it is a symlink pointing at nothing.
fn plan_ssh_dir(sys: &System, resolved: &Resolved) -> Result<SshDir> {
    let Some(dir) = &resolved.ssh_dir else {
        check_given_parent(sys, &resolved.path)?;
        return Ok(SshDir::default());
    };

    // `stat_follow`, so a symlinked `.ssh` — a home on shared storage is
    // often arranged that way — counts as the directory it points at, and
    // the mode and owner planned here are the target's, which is what
    // `set_mode` and `set_owner` would go on to change.
    if let Some(s) = sys.stat_follow(dir)? {
        if s.kind != FileKind::Dir {
            bail!(
                "{} exists and is not a directory; ssh::authorized_keys will not \
                 remove what is in the way of the account's .ssh",
                dir.display()
            );
        }
        return Ok(SshDir {
            create: None,
            changes: plan_attrs(Some(&s), Some(SSH_DIR_MODE), resolved.file_owner()),
        });
    }

    // `stat_follow` reports a dangling symlink as absent, and `mkdir` would
    // then fail with a bare EEXIST naming a path that "does not exist".
    // An `lstat` tells the two apart while there is still something useful
    // to say about it.
    if sys.stat(dir)?.is_some() {
        bail!(
            "{} is a symlink pointing at something that does not exist; \
             ssh::authorized_keys will not create the target or replace the link. \
             Point it at a real directory, or remove it",
            dir.display()
        );
    }

    // Absent, so this step makes it — but only this one component. Reaching
    // for `mkdir_all` here would create the *home* directory too, root-owned
    // and 0755, which is an account that cannot log in and a repair nobody
    // asked this op for. Ansible's `authorized_key` uses `os.mkdir` for the
    // same reason and fails the same way.
    let home = dir.parent().unwrap_or(dir);
    if sys.stat_follow(home)?.is_none() && !sys.check_mode() {
        bail!(
            "home directory {} does not exist, so ssh::authorized_keys cannot create \
             {} inside it; ssh::authorized_keys creates the account's .ssh but never \
             its home. Create it with user::Present::new(..).create_home(true), or \
             with file::Directory",
            home.display(),
            dir.display()
        );
    }
    // Under --check nothing has run, so a home an earlier step would create
    // is still missing and refusing here would fail a dry run of a playbook
    // that converges in one real pass — the wart this op used to have one
    // level down. A dry run writes nothing, and `Ctx::step` returns at its
    // check-mode arm without ever calling `apply` (`ctx.rs:256`, the only
    // `apply` call site in the workspace), so a plan made under this
    // tolerance cannot reach the creation — and `Present::apply` verifies
    // the home anyway rather than resting on that. Ansible's check mode does
    // not look at the directory at all, for the same reason.

    // A run that can act always takes the refusal above, because it runs
    // `check` with `check_mode` off. Note the guarantee is about *which
    // branch a real run takes*, not about re-checking: `Ctx::step` runs
    // `check` once and applies the plan it produced.
    let mut changes = vec![AttrChange {
        name: "exists".into(),
        from: "no".into(),
        to: "yes".into(),
    }];
    changes.extend(plan_attrs(None, Some(SSH_DIR_MODE), resolved.file_owner()));
    Ok(SshDir {
        create: Some(dir.clone()),
        changes,
    })
}

/// The `in_file` form owns the file it was handed and nothing around it: it
/// has no account, so there is no uid for a created directory to belong to
/// and no honest mode to give one. A missing parent is a refusal, as it was
/// for every form before the user forms learned to manage `~/.ssh`.
///
/// The refusal is worded differently under `--check`. Nothing registers a
/// planned directory the way `group::Present` registers a planned group, so
/// this `stat_follow` sees the machine as it is and reports a directory an
/// earlier step would create as missing. Telling the author to "ensure it
/// first with `file::Directory`" is then advice they have already taken, and
/// sends them looking for a bug in a playbook that is correct.
fn check_given_parent(sys: &System, path: &Path) -> Result<()> {
    let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) else {
        return Ok(());
    };
    match sys.stat_follow(parent)? {
        Some(s) if s.kind == FileKind::Dir => Ok(()),
        Some(_) => bail!("{} exists and is not a directory", parent.display()),
        None if sys.check_mode() => bail!(
            "{} does not exist; the in_file form of ssh::authorized_keys does not create \
             it, having no account to own it. Under --check a directory an earlier step \
             would create is still reported missing, because this op stats the real \
             filesystem. If a step in this run creates it, the real run converges and \
             there is nothing to fix; if not, ensure it with file::Directory::at(..)",
            parent.display()
        ),
        None => bail!(
            "{} does not exist; the in_file form of ssh::authorized_keys does not create \
             it, having no account to own it. Ensure it first with \
             file::Directory::at(..), or name the account with for_user/for_account, \
             which create and own ~/.ssh themselves",
            parent.display()
        ),
    }
}

/// The attributes `check` put in the diff for `subject`, as `apply_attrs`
/// takes them. `apply` sets exactly these and no others, because the plan is
/// the instruction — `check` does the thinking, `apply` executes it: a `check` that
/// found the mode already right does not chmod, so an unescalated run
/// managing its own keys is never asked to chown a file it already owns.
fn planned_attrs(
    diff: &Diff,
    subject: &Path,
    mode: u32,
    owner: Option<Owner>,
) -> (Option<u32>, Option<Owner>) {
    let subject = subject.display().to_string();
    for part in diff.parts() {
        if let Diff::Attrs {
            subject: s,
            changes,
        } = part
            && *s == subject
        {
            return (
                changes.iter().any(|c| c.name == "mode").then_some(mode),
                changes
                    .iter()
                    .any(|c| c.name == "owner")
                    .then_some(owner)
                    .flatten(),
            );
        }
    }
    (None, None)
}

/// The attribute changes for an existing `~/.ssh`, and none when the op owns
/// no directory. Unlike [`plan_ssh_dir`] this never plans a creation: its
/// caller has already found the file, so the directory holding it is there.
fn plan_existing_dir_attrs(sys: &System, resolved: &Resolved) -> Result<Vec<AttrChange>> {
    let Some(dir) = &resolved.ssh_dir else {
        return Ok(vec![]);
    };
    Ok(match sys.stat_follow(dir)? {
        Some(s) if s.kind == FileKind::Dir => {
            plan_attrs(Some(&s), Some(SSH_DIR_MODE), resolved.file_owner())
        }
        _ => vec![],
    })
}

/// `apply` resolves the target a second time, and for [`Target::User`] that
/// is a second read of `/etc/passwd`. If the account moved in between, every
/// path in the plan is about a file the machine no longer has: `mkdir_all`
/// would make the old `~/.ssh`, the write would go to a directory `check`
/// never inspected, and the attribute parts would match no subject and be
/// skipped in silence. One comparison turns all of that into a refusal.
fn ensure_same_target(resolved: &Resolved, report: &KeysReport) -> Result<()> {
    if resolved.path != report.path {
        bail!(
            "the account moved between check and apply: this step planned {} and now \
             resolves to {}. Nothing was written. Re-run, and if it keeps happening \
             something else is editing /etc/passwd while the playbook runs",
            report.path.display(),
            resolved.path.display()
        );
    }
    Ok(())
}

/// Apply the attribute parts `check` planned for the directory and the file.
/// Shared by [`Present`] and [`Absent`], which differ in *when* they plan
/// them, not in how they are applied.
///
/// A subject that matches no part means "`check` found nothing to fix here",
/// which is the common case; it cannot mean "the plan was about another
/// path", because [`ensure_same_target`] has already refused that.
fn apply_planned_attrs(sys: &System, resolved: &Resolved, diff: &Diff) -> Result<()> {
    if let Some(dir) = &resolved.ssh_dir {
        let (mode, owner) = planned_attrs(diff, dir, SSH_DIR_MODE, resolved.file_owner());
        apply_attrs(sys, dir, mode, owner)?;
    }
    let (mode, owner) = planned_attrs(diff, &resolved.path, KEYS_FILE_MODE, resolved.file_owner());
    apply_attrs(sys, &resolved.path, mode, owner)
}

/// Context for a failed `chmod`/`chown`. Without it the step fails with a
/// bare errno against a path — `/home/app/.ssh: Operation not permitted (os
/// error 1)` — which says nothing about what wanted the change or what to do.
/// Ownership is the half that needs root, and an unescalated run against a
/// directory root already owns is the way most people will meet this.
fn attr_context(path: &Path, owner: Option<Owner>) -> String {
    match owner {
        Some(o) => format!(
            "setting {} to {}:{} and the mode sshd wants, which ssh::authorized_keys does \
             so the account's keys are actually read; changing an owner needs root, so a \
             step that is not escalated fails here on a path somebody else owns",
            path.display(),
            o.uid,
            o.gid
        ),
        None => format!(
            "setting the mode on {}, which ssh::authorized_keys does so the keys it wrote \
             are actually read",
            path.display()
        ),
    }
}

/// The new file text in a plan, or `None` when only attributes were wrong
/// and the contents are already what they should be.
fn planned_text(diff: &Diff) -> Option<&str> {
    diff.parts().iter().find_map(|p| match p {
        Diff::Text { after, .. } => Some(after.as_str()),
        _ => None,
    })
}

/// Write the planned text, and nothing else. The mode and the owner come
/// from the attributes `check` planned, so this never asks the machine a
/// question whose answer could have changed since.
fn write_file(sys: &System, resolved: &Resolved, text: &str) -> Result<()> {
    sys.write_atomic(&resolved.path, text.as_bytes())
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
/// directly (from `user::Present` or `user::Existing`) with no lookup.
///
/// **The user forms own `~/.ssh` as well as the file in it.** Both are
/// created when missing, and both are held at 0700 and 0600 respectively,
/// owned by the account, on every run — the modes `sshd(8)` recommends and
/// the ones Ansible sets.
///
/// Repairing them matters because of what sshd does when they are wrong:
/// `StrictModes` is on by default, and a `~/.ssh` writable by anyone but its
/// owner makes sshd ignore the keys **silently**. Install keys into one of
/// those and the step reports a clean `changed` over an account that still
/// cannot log in. So the attributes are checked every run and the repair is
/// reported as its own block in the diff.
///
/// That last part goes **further than Ansible**, deliberately.
/// `ansible.posix.authorized_key` does the same work — `manage_dir` defaults
/// to true — but gates it on `do_write` (`authorized_key.py:672-673`), so on
/// a host whose keys are already correct it never looks at the mode and
/// leaves a group-writable `~/.ssh` in place. The keys being right is exactly
/// the case where a wrong mode is invisible and fatal, so this op checks it
/// on every run and reports the repair as its own block in the diff.
/// [`Absent`] keeps Ansible's gate, because a revocation is not a claim
/// about the account's keys as a whole.
///
/// The home directory itself is *not* created: that belongs to
/// `user::Present::create_home`, and creating it here would leave it
/// root-owned.
///
/// The `in_file` form writes an explicit path, handles no ownership, and
/// refuses a missing parent directory: with no account named there is no
/// owner for a directory it might create.
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
        let dir = plan_ssh_dir(sys, &resolved)?;
        let existing = read_existing(sys, &resolved.path)?;
        let (before, before_stat) = match existing {
            Some((text, stat)) => (Some(text), Some(stat)),
            None => (None, None),
        };
        let planned = plan_present(before.as_deref().unwrap_or(""), &keys, self.exclusive);
        let (text, mut report) = planned.into_report(resolved.path.clone());

        // `~/.ssh` is created to hold a file, never for its own sake: with
        // no key to write and no file to protect, a missing directory stays
        // missing and `keys([])` leaves the machine alone.
        //
        // Note what this does *not* skip. If the directory is already there
        // it is still checked, even on a run with no key to write — a
        // group-writable `~/.ssh` lets anyone drop a key into the account,
        // and that is true whether or not this particular step had one to
        // add. Gating the repair on "does the file happen to exist", which
        // an earlier cut of this did, made the promise in `Present`'s doc
        // depend on something unrelated to it. `rustible_github`'s helper
        // reaches exactly that case: it warns when a GitHub login has no
        // public keys and then runs this op with an empty list.
        if text.is_none() && before_stat.is_none() && dir.create.is_some() {
            return Ok(Plan::Satisfied(report));
        }

        // The file's mode and owner, planned whether or not it exists yet, so
        // that `apply` never has to ask the machine anything: an earlier cut
        // left a created file's attributes to an `is_new` check inside
        // `apply`, and a file that appeared between `check` and `apply` then
        // kept whatever mode it was made with while the step reported
        // success.
        //
        // `in_file` is the exception in the other direction. It was handed a
        // path and told nothing about who should own it, so it sets the mode
        // on a file it creates and leaves one that already exists alone.
        let file_attrs = match (resolved.file_owner(), &before_stat) {
            // A file that is already there: repaired.
            (Some(o), Some(stat)) => plan_attrs(Some(stat), Some(KEYS_FILE_MODE), Some(o)),
            // One this step will create: planned up front, but only when
            // there is text to write. With nothing to write no file appears,
            // and planning its mode would have `apply` chmod a path that is
            // not there.
            (Some(o), None) if text.is_some() => plan_attrs(None, Some(KEYS_FILE_MODE), Some(o)),
            (None, None) if text.is_some() => plan_attrs(None, Some(KEYS_FILE_MODE), None),
            _ => vec![],
        };

        // Filesystem order: the directory, then the file's contents, then
        // the file's attributes.
        let mut parts = Vec::new();
        if !dir.changes.is_empty()
            && let Some(d) = &resolved.ssh_dir
        {
            parts.push(Diff::Attrs {
                subject: d.display().to_string(),
                changes: dir.changes,
            });
        }
        if let Some(after) = text {
            parts.push(Diff::text(
                &resolved.path,
                before.unwrap_or_default(),
                after,
            ));
        }
        if !file_attrs.is_empty() {
            parts.push(Diff::Attrs {
                subject: resolved.path.display().to_string(),
                changes: file_attrs,
            });
        }
        let Some(diff) = Diff::many(parts) else {
            return Ok(Plan::Satisfied(report));
        };
        report.created_dir = dir.create;
        Ok(Plan::change_predicting(diff, report))
    }

    fn apply(&self, sys: &System, change: Change<KeysReport>) -> Result<KeysReport> {
        let Some(report) = change.predicted else {
            bail!("authorized_keys::Present::apply received a change without its prediction");
        };
        let resolved = self.target.resolve(sys)?;
        ensure_same_target(&resolved, &report)?;

        // The directory first: the file goes inside it.
        if let Some(dir) = &report.created_dir {
            // `check` refuses a missing home, and only a dry run — which
            // never reaches `apply` (`Ctx::step` returns at the check-mode
            // arm) — is allowed past that. Verify rather than trust: the only
            // tool available is `mkdir_all`, which would invent the home
            // directory root-owned and 0755, and that is the one outcome
            // this op promises never to produce. One `stat` is a cheap price
            // for making the promise structural.
            let home = dir.parent().unwrap_or(dir.as_path());
            if sys.stat_follow(home)?.is_none() {
                bail!(
                    "home directory {} disappeared between check and apply; refusing to \
                     create {} because doing so would create the home too, root-owned",
                    home.display(),
                    dir.display()
                );
            }
            sys.mkdir_all(dir)?;
        }
        if let Some(dir) = &resolved.ssh_dir {
            let (mode, owner) =
                planned_attrs(&change.diff, dir, SSH_DIR_MODE, resolved.file_owner());
            apply_attrs(sys, dir, mode, owner).with_context(|| attr_context(dir, owner))?;
        }

        // Absent when only the attributes were wrong: the keys in the file
        // are already the ones asked for.
        if let Some(after) = planned_text(&change.diff) {
            write_file(sys, &resolved, after)?;
        }
        let (mode, owner) = planned_attrs(
            &change.diff,
            &resolved.path,
            KEYS_FILE_MODE,
            resolved.file_owner(),
        );
        apply_attrs(sys, &resolved.path, mode, owner)
            .with_context(|| attr_context(&resolved.path, owner))?;
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
/// A missing file, or one that does not carry the key, is already satisfied:
/// nothing is written, nothing is created, and no attribute is touched.
///
/// **On a run that does remove a key**, the account's `~/.ssh` and the file
/// are brought to the mode sshd requires and to the account's ownership,
/// exactly as [`Present`] does. This is `ansible.posix.authorized_key`'s own
/// rule, which gates the whole directory-and-ownership pass on `do_write`
/// (`authorized_key.py:672-673`): a revocation that rewrites the file takes
/// that pass with it, and one that finds nothing to revoke does not.
///
/// `Absent` never *creates* `~/.ssh`. It cannot need to: the directory is
/// missing only when the file is, and then there is no key to remove.
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
        let existing = read_existing(sys, &resolved.path)?;
        let (before, before_stat) = match existing {
            Some((text, stat)) => (Some(text), Some(stat)),
            None => (None, None),
        };
        let planned = plan_absent(before.as_deref().unwrap_or(""), &keys);
        let (text, report) = planned.into_report(resolved.path.clone());

        // Ansible's `do_write` gate. No key to remove means no write, and a
        // run that writes nothing takes no directory or ownership pass with
        // it — so a revocation that finds nothing to revoke is `ok` and
        // leaves even a wrong mode alone. `Present` is the op that asserts
        // what the account's keys should be, and the op to reach for when
        // the attributes are what need fixing.
        let Some(after) = text else {
            return Ok(Plan::Satisfied(report));
        };

        // It is rewriting the file, so the pass comes along. `before_stat`
        // is `Some` here: `plan_absent` only removes what it read.
        let dir_changes = plan_existing_dir_attrs(sys, &resolved)?;
        let file_changes = match (resolved.file_owner(), &before_stat) {
            (Some(o), Some(stat)) => plan_attrs(Some(stat), Some(KEYS_FILE_MODE), Some(o)),
            _ => vec![],
        };

        let mut parts = Vec::new();
        if !dir_changes.is_empty()
            && let Some(dir) = &resolved.ssh_dir
        {
            parts.push(Diff::Attrs {
                subject: dir.display().to_string(),
                changes: dir_changes,
            });
        }
        parts.push(Diff::text(
            &resolved.path,
            before.unwrap_or_default(),
            after,
        ));
        if !file_changes.is_empty() {
            parts.push(Diff::Attrs {
                subject: resolved.path.display().to_string(),
                changes: file_changes,
            });
        }
        let diff = Diff::many(parts).expect("the text part is always there");
        Ok(Plan::change_predicting(diff, report))
    }

    fn apply(&self, sys: &System, change: Change<KeysReport>) -> Result<KeysReport> {
        let Some(after) = planned_text(&change.diff) else {
            bail!("authorized_keys::Absent::apply received a change with no file text");
        };
        let Some(report) = change.predicted else {
            bail!("authorized_keys::Absent::apply received a change without its prediction");
        };
        let resolved = self.target.resolve(sys)?;
        ensure_same_target(&resolved, &report)?;
        // Attributes first, and the directory's before the file's, the same
        // order `Present` uses: `write_atomic` makes its temporary file
        // inside the directory, so a `~/.ssh` this identity cannot write to
        // fails the write — even when the `mode: 0500 -> 0700` this step
        // just planned is exactly what would have made it writable.
        apply_planned_attrs(sys, &resolved, &change.diff)?;
        // The file exists: `check` found keys in it, so this never creates.
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

    /// Facts for a mac.
    fn macos(sys: System) -> System {
        let mut facts = sys.facts().clone();
        facts.os = Os::Macos;
        facts.distro = Distro::Macos;
        facts.package_managers = [Pm::Brew].into_iter().collect();
        facts.init = Init::Launchd;
        sys.with_facts(facts)
    }

    /// The refusal is on the *variant*, not the op: only the name lookup
    /// reads `/etc/passwd`, and a mac's copy of that file describes only
    /// system services. The message has to name the form that does work,
    /// because the caller has one.
    #[test]
    fn the_name_lookup_refuses_a_mac_and_names_for_account() {
        let fake = fake_with_user();
        let s = macos(fake_sys(&fake));
        let err = Present::for_user_name("cadu")
            .keys([K1])
            .check(&s)
            .unwrap_err()
            .chain();
        assert!(err.contains("Open Directory"), "{err}");
        assert!(err.contains("for_account(home, uid, gid)"), "{err}");
    }

    /// And the other half: `for_account` is plain file work and runs on a mac
    /// today. Measured against a real one — it wrote and removed a key in
    /// `/Users/cadu/.ssh/authorized_keys`. Gating the whole op on the OS
    /// would have deleted this.
    #[test]
    fn for_account_still_works_on_a_mac() {
        // The home has to exist — this op creates `.ssh`, never the home
        // above it — and on the real mac both were already there. `.ssh` is
        // planted 0755 root-owned, so this also covers the repair running
        // on a platform whose accounts came from Open Directory.
        let fake = Arc::new(
            Fake::new()
                .with_dir("/Users/cadu")
                .with_dir("/Users/cadu/.ssh"),
        );
        let s = macos(fake_sys(&fake));
        let op = Present::for_account("/Users/cadu", 501, 20).keys([K1]);

        let Plan::Change(c) = op.check(&s).unwrap() else {
            panic!("expected change")
        };
        op.apply(&s, c).unwrap();
        assert!(
            fake.content("/Users/cadu/.ssh/authorized_keys")
                .unwrap()
                .contains(K1)
        );
        // Idempotent on the second pass, on the same platform.
        assert!(matches!(op.check(&s).unwrap(), Plan::Satisfied(_)));
    }

    fn fake_with_user() -> Arc<Fake> {
        Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu"),
        )
    }

    /// Set a planted path's mode and owner. The `Fake`'s builders take
    /// neither for a directory and consume `self`, so this goes through the
    /// `Backend` trait, the way `sysctl.rs` drives a second read.
    fn set_attrs(fake: &Arc<Fake>, path: &str, mode: u32, uid: u32, gid: u32) {
        use rustible_sdk::backend::Backend;
        Backend::set_mode(&**fake, Path::new(path), mode).unwrap();
        Backend::set_owner(&**fake, Path::new(path), uid, gid).unwrap();
    }

    /// A `.ssh` that is already right: 0700, owned by cadu. `with_dir`
    /// plants 0755 root-owned, which this op now repairs, so a test about
    /// keys says so here rather than reporting an attribute change it did
    /// not mean to exercise.
    fn fake_with_user_and_ssh_dir() -> Arc<Fake> {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/home/cadu/.ssh"),
        );
        set_attrs(&fake, "/home/cadu/.ssh", 0o700, 1000, 1001);
        fake
    }

    #[test]
    fn user_form_creates_file_with_mode_and_owner_inside_existing_ssh_dir() {
        let fake = fake_with_user_and_ssh_dir();
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K1, K2]);

        let Plan::Change(change) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(
            change.diff.short(),
            "+2 -0 lines mode=0600 owner=1000:1001",
            "a created file's mode and owner are planned, not left to apply"
        );
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

    /// The planted `.ssh` is 0755 root-owned. sshd would actually *accept*
    /// that one — it refuses only a directory writable by others, and root
    /// ownership is allowed — which is the point: the op holds both at the
    /// recommended modes rather than at the minimum sshd tolerates, the way
    /// Ansible does. `wrong_modes_and_ownership_are_repaired` at tier 3 uses
    /// 0775, which sshd does refuse.
    ///
    /// A key *is* being added here, so Ansible would repair too;
    /// `a_wrong_mode_alone_is_a_change_with_no_text_diff` is the case its
    /// `do_write` gate misses and this op does not.
    #[test]
    fn an_existing_file_and_dir_are_repaired_to_what_sshd_needs() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/home/cadu/.ssh") // 0755, root-owned
                .with_file_mode("/home/cadu/.ssh/authorized_keys", format!("{K1}\n"), 0o644),
        );
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K2]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!()
        };
        assert_eq!(
            c.diff.short(),
            "mode=0700 owner=1000:1001 +1 -0 lines mode=0600 owner=1000:1001"
        );
        op.apply(&sys, c).unwrap();

        let file = fake.file("/home/cadu/.ssh/authorized_keys").unwrap();
        assert_eq!((file.mode, file.uid, file.gid), (0o600, 1000, 1001));
        let dir = fake.file("/home/cadu/.ssh").unwrap();
        assert_eq!((dir.mode, dir.uid, dir.gid), (0o700, 1000, 1001));
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K1}\n{K2}\n")
        );
        // And the repair converges: nothing left to fix on the next pass.
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// The attributes are a change in their own right: the keys can be
    /// exactly as asked for while the file is still readable by everyone.
    ///
    /// The rendered diff is asserted whole because of what is *missing* from
    /// it. The file is already owned by the account, so no `owner` line is
    /// planned — and `apply` sets only what the diff names, so no `chown`
    /// happens. That is what keeps an unescalated run working: `chown` needs
    /// root, and a run managing its own keys must not be asked to give away
    /// a file it already owns just because the mode was wrong.
    #[test]
    fn a_wrong_mode_alone_is_a_change_with_no_text_diff() {
        let fake = fake_with_user_and_ssh_dir();
        rustible_sdk::backend::Backend::write(
            &*fake,
            Path::new("/home/cadu/.ssh/authorized_keys"),
            format!("{K1}\n").as_bytes(),
        )
        .unwrap();
        set_attrs(&fake, "/home/cadu/.ssh/authorized_keys", 0o644, 1000, 1001);
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K1]);

        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("a mode sshd rejects is a change")
        };
        assert_eq!(
            c.diff.render(),
            "/home/cadu/.ssh/authorized_keys:\n  mode: 0644 -> 0600\n",
            "no text part: the keys are already right"
        );
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.file("/home/cadu/.ssh/authorized_keys").unwrap().mode,
            0o600
        );
        // The contents were never rewritten, only the mode.
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K1}\n")
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
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
        // The *reason*, not just the fact: `in_file` names no account, so
        // there is no owner for a directory it might create. Asserting only
        // on "does not exist" would pass on the message the user forms used
        // to give, which now create the directory instead.
        assert!(err.contains("having no account to own it"), "{err}");
        assert!(
            err.contains("for_user/for_account"),
            "names the forms that do create it: {err}"
        );
        assert!(fake.file("/nope").is_none());
    }

    /// `exclusive` and the directory work are independent, and a first
    /// provision with `.exclusive(true)` is a real shape — it is what
    /// `rustible_github`'s exclusive mode does on a fresh account.
    #[test]
    fn exclusive_on_a_fresh_account_creates_the_directory_and_writes_only_ours() {
        let fake = fake_with_user(); // no /home/cadu/.ssh
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu")
            .exclusive(true)
            .keys([K1, K2]);

        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.file("/home/cadu/.ssh").unwrap().mode,
            0o700,
            "exclusive does not skip the directory work"
        );
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K1}\n{K2}\n")
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// `apply` will not create a home directory even when handed a plan that
    /// says to create `.ssh` inside a home that is not there.
    ///
    /// `Ctx::step` cannot produce that pairing — it returns at the check-mode
    /// arm without calling `apply` — so this reaches past it and calls the
    /// two halves directly, which is the only way to exercise the guard. The
    /// outcome it prevents is the one `[M6] 2026-09-08` refused and this op's
    /// own message promises: a root-owned home, an account that cannot log
    /// in, and a repair nobody asked for.
    #[test]
    fn apply_refuses_to_create_a_home_even_when_the_plan_says_to() {
        let fake = Arc::new(Fake::new().with_file("/etc/passwd", PASSWD));
        let op = Present::for_user_name("cadu").keys([K1]);

        // A dry check tolerates the missing home and plans the directory.
        let dry = fake_sys(&fake).with_check_mode(true);
        let Plan::Change(c) = op.check(&dry).unwrap() else {
            panic!("check mode tolerates a missing home")
        };
        assert_eq!(
            c.predicted.as_ref().unwrap().created_dir,
            Some(PathBuf::from("/home/cadu/.ssh"))
        );

        // Handing that plan to a real `apply` must not make the home.
        let err = op.apply(&fake_sys(&fake), c).unwrap_err().chain();
        assert!(err.contains("home directory /home/cadu"), "{err}");
        assert!(fake.file("/home/cadu").is_none(), "no root-owned home");
        assert!(fake.file("/home/cadu/.ssh").is_none());
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

    /// The headline of issue #40: the step that used to refuse now does the
    /// work, in one step, and says so.
    #[test]
    fn user_form_creates_ssh_dir_and_reports_it() {
        let fake = fake_with_user(); // /home/cadu exists, /home/cadu/.ssh does not
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K1]);

        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        // Three parts, in filesystem order. The directory block renders
        // exactly as the `file::Directory` step it replaces would have; the
        // file's is planned too, even though the file is new, so `apply`
        // never has to ask the machine whether it created it.
        let parts = c.diff.parts();
        assert_eq!(parts.len(), 3, "{}", c.diff.render());
        assert_eq!(
            parts[0].render(),
            "/home/cadu/.ssh:\n  exists: no -> yes\n  mode: - -> 0700\n  owner: - -> 1000:1001\n"
        );
        assert!(matches!(parts[1], Diff::Text { .. }));
        assert_eq!(
            parts[2].render(),
            "/home/cadu/.ssh/authorized_keys:\n  mode: - -> 0600\n  owner: - -> 1000:1001\n"
        );
        assert_eq!(
            c.diff.short(),
            "exists=yes mode=0700 owner=1000:1001 +1 -0 lines mode=0600 owner=1000:1001"
        );
        let predicted = c.predicted.clone().unwrap();
        assert_eq!(
            predicted.created_dir,
            Some(PathBuf::from("/home/cadu/.ssh"))
        );
        assert!(
            fake.file("/home/cadu/.ssh").is_none(),
            "check must not create"
        );

        let report = op.apply(&sys, c).unwrap();
        assert_eq!(report.created_dir, Some(PathBuf::from("/home/cadu/.ssh")));
        let dir = fake.file("/home/cadu/.ssh").unwrap();
        assert_eq!(
            (dir.kind, dir.mode, dir.uid, dir.gid),
            (FileKind::Dir, 0o700, 1000, 1001)
        );
        let file = fake.file("/home/cadu/.ssh/authorized_keys").unwrap();
        assert_eq!((file.mode, file.uid, file.gid), (0o600, 1000, 1001));

        // Idempotent, and the second pass reports no directory work.
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.created_dir, None);
    }

    /// `~/.ssh` is created to hold a file, never for its own sake.
    #[test]
    fn no_key_to_write_means_no_directory_is_created() {
        let fake = fake_with_user();
        let sys = fake_sys(&fake);
        let keys: [&str; 0] = [];
        assert!(matches!(
            Present::for_user_name("cadu")
                .keys(keys)
                .check(&sys)
                .unwrap(),
            Plan::Satisfied(_)
        ));
        assert!(fake.file("/home/cadu/.ssh").is_none());
    }

    /// The line this op will not cross. `mkdir_all` would have made
    /// `/home/cadu` too — root-owned and 0755, an account that cannot log
    /// in. Ansible's `authorized_key` fails here too, using `os.mkdir`
    /// rather than `os.makedirs` for the same reason.
    #[test]
    fn a_missing_home_is_refused_and_never_created() {
        let fake = Arc::new(Fake::new().with_file("/etc/passwd", PASSWD));
        let sys = fake_sys(&fake);
        let err = Present::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("home directory /home/cadu does not exist"),
            "{err}"
        );
        assert!(err.contains("create_home(true)"), "names the fix: {err}");
        assert!(fake.file("/home/cadu").is_none(), "nothing was created");
        assert!(fake.file("/home/cadu/.ssh").is_none());
    }

    /// ...but not under `--check`, where the home an earlier `user::Present`
    /// would have created is still missing and refusing would fail the dry
    /// run of a playbook that converges in one real pass. Nothing is written
    /// either way, and a run that can act takes the refusal above, because
    /// it runs `check` with check mode off.
    #[test]
    fn under_check_a_missing_home_does_not_fail_the_dry_run() {
        let fake = Arc::new(Fake::new().with_file("/etc/passwd", PASSWD));
        let sys = fake_sys(&fake).with_check_mode(true);
        let Plan::Change(c) = Present::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap()
        else {
            panic!("a dry run of a first provision must not fail")
        };
        assert_eq!(
            c.predicted.unwrap().created_dir,
            Some(PathBuf::from("/home/cadu/.ssh"))
        );
        assert!(fake.file("/home/cadu").is_none());
    }

    /// `stat_follow` reports a dangling symlink as absent, so without an
    /// `lstat` beside it this would reach `mkdir` and fail with a bare
    /// EEXIST on a path the message just called missing.
    #[test]
    fn a_dangling_ssh_symlink_is_refused_by_name() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_symlink("/home/cadu/.ssh", "/mnt/gone/ssh"),
        );
        let sys = fake_sys(&fake);
        let err = Present::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("symlink pointing at something that does not exist"),
            "{err}"
        );
        assert!(fake.file("/mnt/gone/ssh").is_none());
    }

    /// A symlink that goes somewhere real is fine, and the attributes
    /// planned are the target's, since that is what `set_mode` would change.
    #[test]
    fn a_symlinked_ssh_dir_is_used_and_repaired_through_the_link() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/srv/keys/cadu")
                .with_symlink("/home/cadu/.ssh", "/srv/keys/cadu"),
        );
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K1]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.predicted.as_ref().unwrap().created_dir, None);
        op.apply(&sys, c).unwrap();
        let target = fake.file("/srv/keys/cadu").unwrap();
        assert_eq!((target.mode, target.uid, target.gid), (0o700, 1000, 1001));
        assert_eq!(
            fake.file("/home/cadu/.ssh").unwrap().kind,
            FileKind::Symlink,
            "the link itself is left alone"
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        // Where the *file* lands is the one thing this tier cannot answer.
        // `Fake::resolve` follows only the final component of a path, so the
        // fake puts it at `/home/cadu/.ssh/authorized_keys` while a real
        // kernel resolves the directory and puts it under `/srv/keys/cadu`.
        // `symlinked_ssh_dir_is_followed_to_the_real_directory` in
        // `it_authorized_keys.rs` is where that is real; asserting a path
        // here would pin the fake's shortcut instead.
    }

    /// `Absent` reaches the directory through the same `stat_follow`, and
    /// nothing else covered it: swapping that for `stat` plans the *link's*
    /// attributes and chmods the link, which is the bug the `Fake::set_mode`
    /// fix on this branch exists to make visible.
    #[test]
    fn absent_repairs_through_a_symlinked_ssh_dir() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/srv/keys/cadu")
                .with_symlink("/home/cadu/.ssh", "/srv/keys/cadu")
                .with_file_mode(
                    "/home/cadu/.ssh/authorized_keys",
                    format!("{K1}\n{K2}\n"),
                    0o644,
                ),
        );
        let sys = fake_sys(&fake);
        let op = Absent::for_user_name("cadu").keys([K1]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        op.apply(&sys, c).unwrap();
        let target = fake.file("/srv/keys/cadu").unwrap();
        assert_eq!(
            (target.mode, target.uid, target.gid),
            (0o700, 1000, 1001),
            "the target, not the link"
        );
        assert_eq!(
            fake.file("/home/cadu/.ssh").unwrap().kind,
            FileKind::Symlink
        );
    }

    /// Something unexpected in the way still needs a human.
    #[test]
    fn a_regular_file_where_ssh_should_be_is_refused() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_file("/home/cadu/.ssh", "not a directory\n"),
        );
        let sys = fake_sys(&fake);
        let err = Present::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("/home/cadu/.ssh exists and is not a directory"),
            "{err}"
        );
        assert!(err.contains("remove what is in the way"), "{err}");
    }

    /// The `in_file` form kept the old contract and the old check-mode
    /// wording with it: it has no account, so no uid for a directory it
    /// might create. A `file::Directory` one step earlier reports `would
    /// change` but creates nothing and no registry records it, so this op
    /// still sees the parent as missing; telling the author to add the step
    /// they already wrote sends them hunting for a bug in a correct
    /// playbook.
    #[test]
    fn under_check_the_in_file_missing_parent_refusal_does_not_blame_the_author() {
        let fake = Arc::new(Fake::new());
        let sys = fake_sys(&fake).with_check_mode(true);
        let err = Present::in_file("/etc/ssh/keys/root")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .to_string();

        assert!(err.contains("/etc/ssh/keys does not exist"), "{err}");
        assert!(err.contains("--check"), "names the dry run: {err}");
        assert!(
            err.contains("the real run converges"),
            "says the playbook may be fine: {err}"
        );
        // The real-run imperative must not be the advice offered here.
        assert!(
            !err.contains("Ensure it first with"),
            "must not give advice the author has already taken: {err}"
        );
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
        // Attributes already right, so the diff is about keys and nothing
        // else; `absent_repairs_attributes_on_a_run_that_removes_a_key`
        // covers the case where they are not.
        let fake = fake_with_user_and_ssh_dir();
        rustible_sdk::backend::Backend::write(
            &*fake,
            Path::new("/home/cadu/.ssh/authorized_keys"),
            format!("# keys\n{K1}\n{K2}\n{K3}\n").as_bytes(),
        )
        .unwrap();
        set_attrs(&fake, "/home/cadu/.ssh/authorized_keys", 0o600, 1000, 1001);
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

    /// A `.ssh` and a key file left wrong by a careless hand, carrying two
    /// keys.
    fn fake_with_wrong_attributes() -> Arc<Fake> {
        Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_dir("/home/cadu")
                .with_dir("/home/cadu/.ssh") // 0755, root-owned
                .with_file_mode(
                    "/home/cadu/.ssh/authorized_keys",
                    format!("{K1}\n{K2}\n"),
                    0o644,
                ),
        )
    }

    /// Ansible gates the whole directory-and-ownership pass on `do_write`
    /// (`authorized_key.py:672-673`), so a `state: absent` that removes a key
    /// takes it along. This is that half.
    #[test]
    fn absent_repairs_attributes_on_a_run_that_removes_a_key() {
        let fake = fake_with_wrong_attributes();
        let sys = fake_sys(&fake);
        let op = Absent::for_user_name("cadu").keys([K1]);

        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(
            c.diff.short(),
            "mode=0700 owner=1000:1001 +0 -1 lines mode=0600 owner=1000:1001"
        );
        op.apply(&sys, c).unwrap();

        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K2}\n")
        );
        let file = fake.file("/home/cadu/.ssh/authorized_keys").unwrap();
        assert_eq!((file.mode, file.uid, file.gid), (0o600, 1000, 1001));
        let dir = fake.file("/home/cadu/.ssh").unwrap();
        assert_eq!((dir.mode, dir.uid, dir.gid), (0o700, 1000, 1001));
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// And the other half, which is what keeps `Absent` honest: with no key
    /// to remove there is no write, so Ansible's `do_write` stays false and
    /// nothing is touched. A revocation that finds nothing to revoke must not
    /// report `changed` for a mode it decided to fix.
    #[test]
    fn absent_with_nothing_to_remove_repairs_nothing() {
        let fake = fake_with_wrong_attributes();
        let sys = fake_sys(&fake);
        let op = Absent::for_user_name("cadu").keys([K3]); // not in the file

        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        assert_eq!(fake.file("/home/cadu/.ssh").unwrap().mode, 0o755);
        assert_eq!(
            fake.file("/home/cadu/.ssh/authorized_keys").unwrap().mode,
            0o644
        );
    }

    /// The state the early return used to skip: `.ssh` is there and wrong,
    /// there is no file, and no key was asked for. A group-writable `~/.ssh`
    /// lets anyone drop a key into the account whether or not this step had
    /// one to add, so it is repaired. `rustible_github`'s helper reaches
    /// exactly this call — it warns when a GitHub login has no public keys
    /// and then runs the op with an empty list.
    #[test]
    fn an_existing_broken_ssh_dir_is_repaired_even_with_no_keys_to_write() {
        let fake = fake_with_user(); // /home/cadu exists
        rustible_sdk::backend::Backend::mkdir_all(&*fake, Path::new("/home/cadu/.ssh")).unwrap();
        let sys = fake_sys(&fake);
        let keys: [&str; 0] = [];
        let op = Present::for_user_name("cadu").keys(keys);

        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("a group-writable .ssh is a change even with nothing to write")
        };
        assert_eq!(
            c.diff.render(),
            "/home/cadu/.ssh:\n  mode: 0755 -> 0700\n  owner: 0:0 -> 1000:1001\n",
            "the directory alone: no file was written and none exists"
        );
        assert_eq!(c.predicted.as_ref().unwrap().created_dir, None);
        op.apply(&sys, c).unwrap();
        let dir = fake.file("/home/cadu/.ssh").unwrap();
        assert_eq!((dir.mode, dir.uid, dir.gid), (0o700, 1000, 1001));
        assert!(
            fake.file("/home/cadu/.ssh/authorized_keys").is_none(),
            "repairing the directory must not invent a file"
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// The headline divergence from Ansible, at the tier that can express
    /// it: keys right, file right, directory still wrong. Ansible's
    /// `do_write` gate reports `ok` here and leaves it; this op repairs it.
    #[test]
    fn the_directory_alone_being_wrong_is_a_change() {
        let fake = fake_with_user();
        rustible_sdk::backend::Backend::mkdir_all(&*fake, Path::new("/home/cadu/.ssh")).unwrap();
        rustible_sdk::backend::Backend::write(
            &*fake,
            Path::new("/home/cadu/.ssh/authorized_keys"),
            format!("{K1}\n").as_bytes(),
        )
        .unwrap();
        set_attrs(&fake, "/home/cadu/.ssh/authorized_keys", 0o600, 1000, 1001);
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K1]);

        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("a 0755 root-owned .ssh is a change on its own")
        };
        assert_eq!(
            c.diff.render(),
            "/home/cadu/.ssh:\n  mode: 0755 -> 0700\n  owner: 0:0 -> 1000:1001\n",
            "only the directory: the keys and the file are already right"
        );
        op.apply(&sys, c).unwrap();
        let dir = fake.file("/home/cadu/.ssh").unwrap();
        assert_eq!((dir.mode, dir.uid, dir.gid), (0o700, 1000, 1001));
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    /// `apply` sets the attributes the diff names and no others. The
    /// `in_file` form is where that is observable: it plans nothing for a
    /// file that already exists, so an `apply` that chmodded from its own
    /// desired state instead of from the plan would change this mode.
    ///
    /// The same property is what keeps an unescalated run working — a
    /// `check` that finds the owner already right plans no `owner` line, so
    /// `apply` issues no `chown`, which needs root.
    #[test]
    fn apply_touches_no_attribute_the_plan_did_not_name() {
        let fake = Arc::new(Fake::new().with_dir("/etc/ssh/keys").with_file_mode(
            "/etc/ssh/keys/root",
            format!("{K1}\n"),
            0o644,
        ));
        let sys = fake_sys(&fake);
        let op = Present::in_file("/etc/ssh/keys/root").keys([K2]);

        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert!(
            matches!(c.diff, Diff::Text { .. }),
            "in_file plans no attributes for a file that is already there"
        );
        op.apply(&sys, c).unwrap();
        let f = fake.file("/etc/ssh/keys/root").unwrap();
        assert_eq!(f.mode, 0o644, "the mode it had, not the one this op likes");
        assert_eq!((f.uid, f.gid), (0, 0));
    }

    /// `read_existing`'s two refusals, both rewritten on this branch and
    /// neither covered before. The symlink one became more reachable with
    /// it: the op now teaches that a symlinked `~/.ssh` works, so a
    /// symlinked `authorized_keys` is the next thing somebody tries.
    #[test]
    fn a_directory_or_a_symlink_where_the_file_should_be_is_refused() {
        let fake = fake_with_user_and_ssh_dir();
        rustible_sdk::backend::Backend::mkdir_all(
            &*fake,
            Path::new("/home/cadu/.ssh/authorized_keys"),
        )
        .unwrap();
        let sys = fake_sys(&fake);
        let err = Present::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("/home/cadu/.ssh/authorized_keys is a directory"),
            "{err}"
        );

        let fake = fake_with_user_and_ssh_dir();
        rustible_sdk::backend::Backend::symlink(
            &*fake,
            Path::new("/srv/real_keys"),
            Path::new("/home/cadu/.ssh/authorized_keys"),
        )
        .unwrap();
        let sys = fake_sys(&fake);
        let err = Present::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(err.contains("is a symlink"), "{err}");
        assert!(
            err.contains("in_file()"),
            "names the way to write the real file: {err}"
        );
    }

    /// `exclusive` with an empty list truncates the file, which is a foot
    /// gun worth pinning as deliberate: `rustible_github` refuses this exact
    /// combination, and the refusal only makes sense because the op itself
    /// does not.
    #[test]
    fn exclusive_with_no_keys_empties_the_file() {
        let fake = fake_with_user_and_ssh_dir();
        rustible_sdk::backend::Backend::write(
            &*fake,
            Path::new("/home/cadu/.ssh/authorized_keys"),
            format!("{K1}\n{K2}\n").as_bytes(),
        )
        .unwrap();
        set_attrs(&fake, "/home/cadu/.ssh/authorized_keys", 0o600, 1000, 1001);
        let sys = fake_sys(&fake);
        let keys: [&str; 0] = [];
        let op = Present::for_user_name("cadu").exclusive(true).keys(keys);

        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.removed.len(), 2);
        assert_eq!(fake.content("/home/cadu/.ssh/authorized_keys").unwrap(), "");
    }

    /// `apply` resolves the target a second time, so `check` and `apply` can
    /// disagree about where the account lives. Acting on both answers at
    /// once was the worst outcome available: `mkdir_all` would make the old
    /// `~/.ssh`, the keys would land somewhere `check` never inspected, and
    /// the attribute parts would match no subject and be skipped in silence.
    #[test]
    fn a_target_that_moves_between_check_and_apply_is_refused() {
        let fake = fake_with_user_and_ssh_dir();
        let sys = fake_sys(&fake);
        let op = Present::for_user_name("cadu").keys([K1]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };

        // The account moves house between the two halves of the step.
        rustible_sdk::backend::Backend::write(
            &*fake,
            Path::new("/etc/passwd"),
            b"root:x:0:0:root:/root:/bin/bash\ncadu:x:1000:1001:Cadu:/home/cadu2:/bin/zsh\n",
        )
        .unwrap();

        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("moved between check and apply"), "{err}");
        assert!(err.contains("/home/cadu/.ssh/authorized_keys"), "{err}");
        assert!(err.contains("/home/cadu2/.ssh/authorized_keys"), "{err}");
        assert!(
            fake.file("/home/cadu/.ssh/authorized_keys").is_none()
                && fake.file("/home/cadu2/.ssh").is_none(),
            "nothing was written to either home"
        );
    }

    /// The same guard on `Absent`, which resolves twice for the same reason.
    #[test]
    fn absent_also_refuses_a_target_that_moved() {
        let fake = fake_with_user_and_ssh_dir();
        rustible_sdk::backend::Backend::write(
            &*fake,
            Path::new("/home/cadu/.ssh/authorized_keys"),
            format!("{K1}\n").as_bytes(),
        )
        .unwrap();
        set_attrs(&fake, "/home/cadu/.ssh/authorized_keys", 0o600, 1000, 1001);
        let sys = fake_sys(&fake);
        let op = Absent::for_user_name("cadu").keys([K1]);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        rustible_sdk::backend::Backend::write(
            &*fake,
            Path::new("/etc/passwd"),
            b"root:x:0:0:root:/root:/bin/bash\ncadu:x:1000:1001:Cadu:/home/cadu2:/bin/zsh\n",
        )
        .unwrap();
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("moved between check and apply"), "{err}");
        assert_eq!(
            fake.content("/home/cadu/.ssh/authorized_keys").unwrap(),
            format!("{K1}\n"),
            "the key it was about to remove is still there"
        );
    }

    /// `Absent` never creates the directory, and never needs to: `.ssh` is
    /// missing only when the file is, and then there is no key to remove.
    #[test]
    fn absent_never_creates_the_directory() {
        let fake = fake_with_user(); // no /home/cadu/.ssh
        let sys = fake_sys(&fake);
        let Plan::Satisfied(r) = Absent::for_user_name("cadu")
            .keys([K1])
            .check(&sys)
            .unwrap()
        else {
            panic!("nothing to remove")
        };
        assert_eq!(r.created_dir, None);
        assert!(fake.file("/home/cadu/.ssh").is_none());
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
