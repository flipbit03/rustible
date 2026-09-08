//! Unix user accounts. Ansible's `ansible.builtin.user`, split by desired
//! state (vision 6.3): [`Present`] ensures an account exists with the given
//! attributes, [`Absent`] ensures it does not, [`Existing`] looks one up
//! without changing anything (vision 13.1), and [`Membership`] adds a user
//! to one group (vision 6.6).
//!
//! Every op reads `/etc/passwd` and `/etc/group` through `sys` and changes
//! them only through the distro's own tools, chosen from `facts.distro`
//! (vision 7.4): shadow-utils `useradd`/`usermod`/`userdel` everywhere
//! except Alpine, whose BusyBox `adduser`/`deluser`/`addgroup`/`delgroup`
//! take different flags and cannot modify an existing account's attributes.
//! Nothing here creates a group (vision 6.7): a `.groups([..])` or `.gid(..)`
//! naming a missing group fails at `check`; use `group::Present` first.

use std::path::PathBuf;

use rustible_sdk::prelude::*;

use crate::group::{
    Group, Tools, group_by_gid, group_entry, groups_of, lookup_group, require_root, run_tool,
    validate_field, validate_name,
};

/// A user account as it stands on the machine. Output of [`Present`] and
/// [`Existing`]; what `file::Directory::owner` and
/// `ssh::authorized_keys::Present::for_user` will take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    /// Primary group id.
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
    /// Supplementary groups: every group in `/etc/group` whose member field
    /// lists the user, sorted by name. The primary group appears only if it
    /// also lists the user.
    pub groups: Vec<String>,
}

/// One well-formed line of `/etc/passwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// The GECOS field, Ansible's `comment`.
    pub comment: String,
    pub home: PathBuf,
    pub shell: PathBuf,
}

/// Pure: every well-formed line of `/etc/passwd` text
/// (`name:password:uid:gid:gecos:home:shell`). Lines with fewer than seven
/// fields, a non-numeric uid or gid, or an empty name are skipped; a missing
/// trailing newline does not matter.
pub fn parse_passwd(text: &str) -> Vec<PasswdEntry> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 7 || f[0].is_empty() {
                return None;
            }
            Some(PasswdEntry {
                name: f[0].to_string(),
                uid: f[2].parse().ok()?,
                gid: f[3].parse().ok()?,
                comment: f[4].to_string(),
                home: PathBuf::from(f[5]),
                shell: PathBuf::from(f[6]),
            })
        })
        .collect()
}

/// Pure: the account called `name`, if `/etc/passwd` text has it.
pub fn passwd_entry(text: &str, name: &str) -> Option<PasswdEntry> {
    parse_passwd(text).into_iter().find(|e| e.name == name)
}

/// Pure: like [`passwd_entry`], but a line that names the user and does not
/// parse is an error rather than "missing", so an op never tries to create
/// an account whose line is merely broken.
pub fn lookup_user(text: &str, name: &str) -> Result<Option<PasswdEntry>> {
    match passwd_entry(text, name) {
        Some(e) => Ok(Some(e)),
        None => match text.lines().find(|l| l.split(':').next() == Some(name)) {
            Some(line) => bail!("/etc/passwd has a malformed line for `{name}`: {line}"),
            None => Ok(None),
        },
    }
}

/// Pure: the account with this uid, if `/etc/passwd` text has it.
pub fn passwd_by_uid(text: &str, uid: u32) -> Option<PasswdEntry> {
    parse_passwd(text).into_iter().find(|e| e.uid == uid)
}

/// Pure: an [`Account`] from a passwd entry and `/etc/group` text.
pub fn account_of(entry: &PasswdEntry, group_text: &str) -> Account {
    Account {
        name: entry.name.clone(),
        uid: entry.uid,
        gid: entry.gid,
        home: entry.home.clone(),
        shell: entry.shell.clone(),
        groups: groups_of(group_text, &entry.name),
    }
}

/// Read both files and build the account, or `None` if the user is missing.
fn read_account(sys: &System, name: &str) -> Result<Option<Account>> {
    let passwd = sys.read_to_string("/etc/passwd")?;
    let Some(entry) = lookup_user(&passwd, name)? else {
        return Ok(None);
    };
    let group = sys.read_to_string("/etc/group")?;
    Ok(Some(account_of(&entry, &group)))
}

/// A primary group given by id or by name. `From` impls let `.gid(1000)`,
/// `.gid("docker")` and `.gid(&group)` all work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupId {
    Id(u32),
    Name(String),
}

impl From<u32> for GroupId {
    fn from(gid: u32) -> Self {
        GroupId::Id(gid)
    }
}

impl From<&str> for GroupId {
    fn from(name: &str) -> Self {
        GroupId::Name(name.to_string())
    }
}

impl From<String> for GroupId {
    fn from(name: String) -> Self {
        GroupId::Name(name)
    }
}

impl From<&Group> for GroupId {
    fn from(group: &Group) -> Self {
        GroupId::Id(group.gid)
    }
}

/// What [`Present`] asks for on an existing account, with the primary group
/// already resolved to a gid. Input of [`plan_modify`]. The default asks for
/// nothing: every attribute `None`, no groups, `append` on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub home: Option<PathBuf>,
    pub shell: Option<PathBuf>,
    pub comment: Option<String>,
    /// Supplementary groups to have (`append`) or to have exactly.
    pub groups: Vec<String>,
    pub append: bool,
}

impl Default for Desired {
    fn default() -> Self {
        Desired {
            uid: None,
            gid: None,
            home: None,
            shell: None,
            comment: None,
            groups: vec![],
            append: true,
        }
    }
}

/// What must change on an existing account. Output of [`plan_modify`]; each
/// `Some` is an attribute to set, and the group lists are the delta. The
/// `changes` are the same facts as a [`Diff::Attrs`], and
/// [`Delta::from_changes`] reads them back: `check` produces the diff and
/// `apply` executes it (vision 6.2) without inspecting the system again.
#[derive(Debug, Clone, Default)]
pub struct Delta {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub home: Option<PathBuf>,
    pub shell: Option<PathBuf>,
    pub comment: Option<String>,
    pub add_groups: Vec<String>,
    pub remove_groups: Vec<String>,
    /// The diff to report, one entry per attribute above.
    pub changes: Vec<AttrChange>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Pure: the delta back from the attribute changes [`plan_modify`]
    /// produced. Errors on changes this module did not write.
    pub fn from_changes(changes: &[AttrChange]) -> Result<Delta> {
        let mut delta = Delta::default();
        let bad = |c: &AttrChange| {
            Error::msg(format!(
                "user::Present::apply received a diff it did not produce: {}: {} -> {}",
                c.name, c.from, c.to
            ))
        };
        let split = |s: &str| -> Vec<String> {
            s.split(',')
                .filter(|g| !g.is_empty())
                .map(str::to_string)
                .collect()
        };
        for c in changes {
            match c.name.as_str() {
                "uid" => delta.uid = Some(c.to.parse().map_err(|_| bad(c))?),
                "gid" => delta.gid = Some(c.to.parse().map_err(|_| bad(c))?),
                "home" => delta.home = Some(PathBuf::from(&c.to)),
                "shell" => delta.shell = Some(PathBuf::from(&c.to)),
                "comment" => delta.comment = Some(c.to.clone()),
                "groups" => {
                    let (before, after) = (split(&c.from), split(&c.to));
                    delta.add_groups = after
                        .iter()
                        .filter(|g| !before.contains(g))
                        .cloned()
                        .collect();
                    delta.remove_groups = before
                        .iter()
                        .filter(|g| !after.contains(g))
                        .cloned()
                        .collect();
                }
                _ => return Err(bad(c)),
            }
        }
        delta.changes = changes.to_vec();
        Ok(delta)
    }

    /// True when something other than group membership changes; BusyBox
    /// cannot do that.
    fn changes_attributes(&self) -> bool {
        self.uid.is_some()
            || self.gid.is_some()
            || self.home.is_some()
            || self.shell.is_some()
            || self.comment.is_some()
    }
}

fn sorted(mut names: Vec<String>) -> Vec<String> {
    names.sort();
    names.dedup();
    names
}

/// Pure: compare an existing account (and its sorted supplementary groups)
/// with what is desired. Attributes not asked for are left alone. With
/// `append`, groups the account already has stay; without it, the account
/// ends up in exactly `want.groups`.
pub fn plan_modify(current: &PasswdEntry, current_groups: &[String], want: &Desired) -> Delta {
    let mut delta = Delta::default();
    let mut changes = vec![];
    let mut change = |name: &str, from: String, to: String| {
        changes.push(AttrChange {
            name: name.into(),
            from,
            to,
        });
    };

    if let Some(uid) = want.uid
        && uid != current.uid
    {
        change("uid", current.uid.to_string(), uid.to_string());
        delta.uid = Some(uid);
    }
    if let Some(gid) = want.gid
        && gid != current.gid
    {
        change("gid", current.gid.to_string(), gid.to_string());
        delta.gid = Some(gid);
    }
    if let Some(home) = &want.home
        && home != &current.home
    {
        change(
            "home",
            current.home.display().to_string(),
            home.display().to_string(),
        );
        delta.home = Some(home.clone());
    }
    if let Some(shell) = &want.shell
        && shell != &current.shell
    {
        change(
            "shell",
            current.shell.display().to_string(),
            shell.display().to_string(),
        );
        delta.shell = Some(shell.clone());
    }
    if let Some(comment) = &want.comment
        && comment != &current.comment
    {
        change("comment", current.comment.clone(), comment.clone());
        delta.comment = Some(comment.clone());
    }

    let wanted = sorted(want.groups.clone());
    let add: Vec<String> = wanted
        .iter()
        .filter(|g| !current_groups.contains(g))
        .cloned()
        .collect();
    let remove: Vec<String> = if want.append {
        vec![]
    } else {
        current_groups
            .iter()
            .filter(|g| !wanted.contains(g))
            .cloned()
            .collect()
    };
    if !add.is_empty() || !remove.is_empty() {
        let after = if want.append {
            sorted([current_groups.to_vec(), add.clone()].concat())
        } else {
            wanted
        };
        change("groups", current_groups.join(","), after.join(","));
        delta.add_groups = add;
        delta.remove_groups = remove;
    }
    delta.changes = changes;
    delta
}

/// Login shell a new account gets when none is asked for: shadow-utils read
/// `SHELL=` from `/etc/default/useradd` (Debian and Ubuntu ship `/bin/sh`,
/// Fedora `/bin/bash`) and fall back to `/bin/sh`; BusyBox uses `/bin/sh`.
fn default_shell(sys: &System, tools: Tools) -> Result<PathBuf> {
    if tools == Tools::Shadow && sys.exists("/etc/default/useradd")? {
        let text = sys.read_to_string("/etc/default/useradd")?;
        if let Some(shell) = text.lines().find_map(|l| {
            l.trim()
                .strip_prefix("SHELL=")
                .map(|s| s.trim().trim_matches('"').to_string())
        }) && !shell.is_empty()
        {
            return Ok(PathBuf::from(shell));
        }
    }
    Ok(PathBuf::from("/bin/sh"))
}

/// What [`Present::check`] found and validated.
struct Inspection {
    /// The account as it is, if it exists.
    current: Option<PasswdEntry>,
    /// Its supplementary groups, sorted.
    current_groups: Vec<String>,
    /// Primary group resolved to (gid, name), when asked for.
    primary: Option<(u32, String)>,
    /// Attribute changes on an existing account.
    delta: Delta,
    group_text: String,
}

/// Ensure a user account exists with the given attributes.
/// `ansible.builtin.user` with `state: present`.
///
/// ```ignore
/// let account = ctx.step(
///     "Ensure rustible user exists",
///     user::Present::new("rustible")
///         .shell("/bin/bash")
///         .groups(["docker", "adm"])
///         .create_home(true),
/// )?;
/// ctx.step("Ensure ~/.ssh exists",
///     file::Directory::at(account.home.join(".ssh")).mode(0o700))?;
/// ```
///
/// Attributes not asked for are never touched on an existing account, and
/// `create_home`/`system` only matter on creation, as in Ansible. Groups
/// named in `groups` and the primary group from `gid` must already exist
/// (vision 6.7). On Alpine, where BusyBox has no `usermod`, changing the
/// uid, gid, home, shell or comment of an existing account fails clearly;
/// group membership still works through `addgroup`/`delgroup`.
///
/// Prediction (vision 12): the output is predicted whenever it is honest.
/// For an existing account everything is known. For a new account the uid
/// must be given (the tool allocates it otherwise) and the primary gid must
/// be given or follow from the tools' private-group rule (a group named
/// after the user with gid equal to the uid, when that gid is free);
/// otherwise the step returns a change without a prediction and a
/// check-mode run that chains from it stops there with a clear message.
#[derive(Debug, Clone)]
pub struct Present {
    name: String,
    uid: Option<u32>,
    gid: Option<GroupId>,
    home: Option<PathBuf>,
    shell: Option<PathBuf>,
    create_home: bool,
    system: bool,
    groups: Vec<String>,
    append: bool,
    comment: Option<String>,
}

impl Present {
    pub fn new(name: impl Into<String>) -> Self {
        Present {
            name: name.into(),
            uid: None,
            gid: None,
            home: None,
            shell: None,
            create_home: true,
            system: false,
            groups: vec![],
            append: true,
            comment: None,
        }
    }

    pub fn uid(mut self, uid: u32) -> Self {
        self.uid = Some(uid);
        self
    }

    /// Primary group, by gid, by name, or from a `group::Group`. Must exist.
    pub fn gid(mut self, gid: impl Into<GroupId>) -> Self {
        self.gid = Some(gid.into());
        self
    }

    /// Home directory path. Default `/home/<name>`. Changing it on an
    /// existing account does not move the old directory.
    pub fn home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    /// Login shell.
    pub fn shell(mut self, shell: impl Into<PathBuf>) -> Self {
        self.shell = Some(shell.into());
        self
    }

    /// Create the home directory when creating the account. Default true.
    pub fn create_home(mut self, on: bool) -> Self {
        self.create_home = on;
        self
    }

    /// Allocate the uid from the system range when creating (`useradd -r`,
    /// `adduser -S`).
    pub fn system(mut self, on: bool) -> Self {
        self.system = on;
        self
    }

    /// Supplementary groups. Each must exist. See [`append`](Self::append).
    pub fn groups<I, S>(mut self, groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.groups = groups.into_iter().map(Into::into).collect();
        self
    }

    /// With `true` (the default) the account is added to `groups` and keeps
    /// any others; with `false` it ends up in exactly `groups`. Ansible's
    /// `append`, with the default flipped because keeping memberships is the
    /// safe choice.
    pub fn append(mut self, on: bool) -> Self {
        self.append = on;
        self
    }

    /// The GECOS field. Ansible's `comment`.
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// The primary group as (gid, name), which must exist (vision 6.7).
    fn resolve_primary(&self, group_text: &str) -> Result<Option<(u32, String)>> {
        Ok(match &self.gid {
            None => None,
            Some(GroupId::Id(gid)) => match group_by_gid(group_text, *gid) {
                Some(g) => Some((g.gid, g.name)),
                None => bail!(
                    "no group has gid {gid}; user::Present does not create groups, use \
                     group::Present first"
                ),
            },
            Some(GroupId::Name(name)) => match lookup_group(group_text, name)? {
                Some(g) => Some((g.gid, g.name)),
                None => bail!(
                    "group `{name}` does not exist; user::Present does not create groups, use \
                     group::Present first"
                ),
            },
        })
    }

    fn inspect(&self, sys: &System) -> Result<Inspection> {
        validate_name("user", &self.name)?;
        if let Some(home) = &self.home {
            validate_field("home", &home.display().to_string())?;
        }
        if let Some(shell) = &self.shell {
            validate_field("shell", &shell.display().to_string())?;
        }
        if let Some(comment) = &self.comment {
            validate_field("comment", comment)?;
        }
        for g in &self.groups {
            validate_name("group", g)?;
        }
        if let Some(GroupId::Name(name)) = &self.gid {
            validate_name("group", name)?;
        }
        let passwd = sys.read_to_string("/etc/passwd")?;
        let group_text = sys.read_to_string("/etc/group")?;

        for g in &self.groups {
            if lookup_group(&group_text, g)?.is_none() {
                bail!(
                    "group `{g}` does not exist; user::Present does not create groups, use \
                     group::Present first"
                );
            }
        }
        let primary = self.resolve_primary(&group_text)?;

        let current = lookup_user(&passwd, &self.name)?;
        if current.is_none()
            && let Some(uid) = self.uid
            && let Some(taken) = passwd_by_uid(&passwd, uid)
        {
            bail!(
                "uid {uid} is already used by user `{}`; user::Present does not renumber other \
                 users",
                taken.name
            );
        }
        let current_groups = groups_of(&group_text, &self.name);
        let delta = match &current {
            Some(entry) => plan_modify(
                entry,
                &current_groups,
                &Desired {
                    uid: self.uid,
                    gid: primary.as_ref().map(|p| p.0),
                    home: self.home.clone(),
                    shell: self.shell.clone(),
                    comment: self.comment.clone(),
                    groups: self.groups.clone(),
                    append: self.append,
                },
            ),
            None => Delta::default(),
        };
        if current.is_some() && delta.changes_attributes() && Tools::of(sys) == Tools::BusyBox {
            bail!(
                "user `{}` exists and BusyBox has no `usermod` to change its {}; install the \
                 `shadow` package or drop those builders",
                self.name,
                delta
                    .changes
                    .iter()
                    .filter(|c| c.name != "groups")
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        Ok(Inspection {
            current,
            current_groups,
            primary,
            delta,
            group_text,
        })
    }

    /// The plan for a missing account: the diff and, when honest, the
    /// predicted account.
    fn plan_create(
        &self,
        sys: &System,
        primary: Option<(u32, String)>,
        group_text: &str,
    ) -> Result<Plan<Account>> {
        let tools = Tools::of(sys);
        let mut changes = vec![AttrChange {
            name: "exists".into(),
            from: "no".into(),
            to: "yes".into(),
        }];
        let mut change = |name: &str, to: String| {
            changes.push(AttrChange {
                name: name.into(),
                from: "-".into(),
                to,
            });
        };
        if self.system {
            change("system", "yes".into());
        }
        if let Some(uid) = self.uid {
            change("uid", uid.to_string());
        }
        if let Some((gid, _)) = &primary {
            change("gid", gid.to_string());
        }
        let home = self
            .home
            .clone()
            .unwrap_or_else(|| PathBuf::from(format!("/home/{}", self.name)));
        change("home", home.display().to_string());
        if !self.create_home {
            change("create_home", "no".into());
        }
        // BusyBox gives system accounts other defaults; only an explicit
        // shell is predicted there.
        let shell = match &self.shell {
            Some(s) => Some(s.clone()),
            None if tools == Tools::BusyBox && self.system => None,
            None => Some(default_shell(sys, tools)?),
        };
        if let Some(s) = &shell {
            change("shell", s.display().to_string());
        }
        if let Some(c) = &self.comment {
            change("comment", c.clone());
        }
        let groups = sorted(self.groups.clone());
        if !groups.is_empty() {
            change("groups", groups.join(","));
        }
        let diff = Diff::Attrs {
            subject: format!("user {}", self.name),
            changes,
        };

        let Some(uid) = self.uid else {
            return Ok(Plan::change(diff));
        };
        let gid = match primary {
            Some((gid, _)) => Some(gid),
            // The private-group rule: both tool families name the new group
            // after the user and prefer gid == uid; predict only when that
            // gid is free and no group already carries the name.
            None if tools == Tools::BusyBox && self.system => None,
            None if group_by_gid(group_text, uid).is_none()
                && group_entry(group_text, &self.name).is_none() =>
            {
                Some(uid)
            }
            None => None,
        };
        Ok(match (gid, shell) {
            (Some(gid), Some(shell)) => Plan::change_predicting(
                diff,
                Account {
                    name: self.name.clone(),
                    uid,
                    gid,
                    home,
                    shell,
                    groups,
                },
            ),
            _ => Plan::change(diff),
        })
    }

    fn create(&self, sys: &System, tools: Tools, primary: Option<(u32, String)>) -> Result<()> {
        match tools {
            Tools::Shadow => {
                let mut cmd = sys.cmd("useradd");
                if self.system {
                    cmd = cmd.arg("-r");
                }
                if let Some(uid) = self.uid {
                    cmd = cmd.args(["-u", &uid.to_string()]);
                }
                if let Some((gid, _)) = &primary {
                    cmd = cmd.args(["-g", &gid.to_string()]);
                }
                if !self.groups.is_empty() {
                    cmd = cmd.args(["-G", &sorted(self.groups.clone()).join(",")]);
                }
                if let Some(home) = &self.home {
                    cmd = cmd.args(["-d", &home.display().to_string()]);
                }
                if let Some(shell) = &self.shell {
                    cmd = cmd.args(["-s", &shell.display().to_string()]);
                }
                if let Some(comment) = &self.comment {
                    cmd = cmd.args(["-c", comment]);
                }
                cmd = cmd.arg(if self.create_home { "-m" } else { "-M" });
                run_tool(cmd.arg(&self.name), tools, "useradd")
            }
            Tools::BusyBox => {
                let mut cmd = sys.cmd("adduser").arg("-D");
                if self.system {
                    cmd = cmd.arg("-S");
                }
                if let Some(uid) = self.uid {
                    cmd = cmd.args(["-u", &uid.to_string()]);
                }
                if let Some((_, name)) = &primary {
                    cmd = cmd.args(["-G", name]);
                }
                if let Some(home) = &self.home {
                    cmd = cmd.args(["-h", &home.display().to_string()]);
                }
                if let Some(shell) = &self.shell {
                    cmd = cmd.args(["-s", &shell.display().to_string()]);
                }
                if let Some(comment) = &self.comment {
                    cmd = cmd.args(["-g", comment]);
                }
                if !self.create_home {
                    cmd = cmd.arg("-H");
                }
                run_tool(cmd.arg(&self.name), tools, "adduser")?;
                for g in sorted(self.groups.clone()) {
                    run_tool(
                        sys.cmd("addgroup").args([&self.name, &g]),
                        tools,
                        "addgroup",
                    )?;
                }
                Ok(())
            }
        }
    }

    fn modify(&self, sys: &System, tools: Tools, delta: &Delta) -> Result<()> {
        match tools {
            Tools::Shadow => {
                let mut cmd = sys.cmd("usermod");
                if let Some(uid) = delta.uid {
                    cmd = cmd.args(["-u", &uid.to_string()]);
                }
                if let Some(gid) = delta.gid {
                    cmd = cmd.args(["-g", &gid.to_string()]);
                }
                if let Some(home) = &delta.home {
                    cmd = cmd.args(["-d", &home.display().to_string()]);
                }
                if let Some(shell) = &delta.shell {
                    cmd = cmd.args(["-s", &shell.display().to_string()]);
                }
                if let Some(comment) = &delta.comment {
                    cmd = cmd.args(["-c", comment]);
                }
                if !delta.add_groups.is_empty() || !delta.remove_groups.is_empty() {
                    cmd = if self.append {
                        cmd.args(["-aG", &delta.add_groups.join(",")])
                    } else {
                        cmd.args(["-G", &sorted(self.groups.clone()).join(",")])
                    };
                }
                run_tool(cmd.arg(&self.name), tools, "usermod")
            }
            Tools::BusyBox => {
                for g in &delta.add_groups {
                    run_tool(sys.cmd("addgroup").args([&self.name, g]), tools, "addgroup")?;
                }
                for g in &delta.remove_groups {
                    run_tool(sys.cmd("delgroup").args([&self.name, g]), tools, "delgroup")?;
                }
                Ok(())
            }
        }
    }
}

impl Op for Present {
    type Output = Account;

    fn check(&self, sys: &System) -> Result<Plan<Account>> {
        require_root(sys, "user::Present")?;
        let Inspection {
            current,
            current_groups,
            primary,
            delta,
            group_text,
        } = self.inspect(sys)?;
        let Some(entry) = current else {
            return self.plan_create(sys, primary, &group_text);
        };
        if delta.is_empty() {
            return Ok(Plan::Satisfied(account_of(&entry, &group_text)));
        }
        let groups = if self.append {
            sorted([current_groups, delta.add_groups.clone()].concat())
        } else {
            sorted(self.groups.clone())
        };
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: format!("user {}", self.name),
                changes: delta.changes.clone(),
            },
            Account {
                name: self.name.clone(),
                uid: delta.uid.unwrap_or(entry.uid),
                gid: delta.gid.unwrap_or(entry.gid),
                home: delta.home.clone().unwrap_or(entry.home),
                shell: delta.shell.clone().unwrap_or(entry.shell),
                groups,
            },
        ))
    }

    fn apply(&self, sys: &System, change: Change<Account>) -> Result<Account> {
        let Diff::Attrs { changes, .. } = &change.diff else {
            bail!("user::Present::apply received a diff it did not produce");
        };
        let tools = Tools::of(sys);
        if changes.iter().any(|c| c.name == "exists") {
            let primary = self.resolve_primary(&sys.read_to_string("/etc/group")?)?;
            self.create(sys, tools, primary)?;
        } else {
            self.modify(sys, tools, &Delta::from_changes(changes)?)?;
        }
        read_account(sys, &self.name)?.ok_or_else(|| {
            Error::msg(format!(
                "user `{}` is not in /etc/passwd after creating it",
                self.name
            ))
        })
    }
}

/// Output of [`Absent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub name: String,
    /// The home directory this step deleted (`remove_home`); `None` when the
    /// home was kept or the account did not exist.
    pub home: Option<PathBuf>,
}

/// Ensure a user account does not exist. `ansible.builtin.user` with
/// `state: absent`.
///
/// ```ignore
/// ctx.step("Remove old deploy user", user::Absent::new("deploy").remove_home(true))?;
/// ```
///
/// `remove_home` is Ansible's `remove: yes`: `userdel -r` (or BusyBox
/// `deluser --remove-home`) deletes the home directory and mail spool with
/// the account. The user's private group goes with the account when the
/// tool removes it, as it does by default; other groups are never touched.
#[derive(Debug, Clone)]
pub struct Absent {
    name: String,
    remove_home: bool,
}

impl Absent {
    pub fn new(name: impl Into<String>) -> Self {
        Absent {
            name: name.into(),
            remove_home: false,
        }
    }

    /// Delete the home directory with the account. Default false.
    pub fn remove_home(mut self, on: bool) -> Self {
        self.remove_home = on;
        self
    }
}

impl Op for Absent {
    type Output = Removed;

    fn check(&self, sys: &System) -> Result<Plan<Removed>> {
        require_root(sys, "user::Absent")?;
        validate_name("user", &self.name)?;
        let passwd = sys.read_to_string("/etc/passwd")?;
        let Some(entry) = lookup_user(&passwd, &self.name)? else {
            return Ok(Plan::Satisfied(Removed {
                name: self.name.clone(),
                home: None,
            }));
        };
        let mut changes = vec![AttrChange {
            name: "exists".into(),
            from: "yes".into(),
            to: "no".into(),
        }];
        if self.remove_home {
            changes.push(AttrChange {
                name: "home".into(),
                from: entry.home.display().to_string(),
                to: "removed".into(),
            });
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: format!("user {}", self.name),
                changes,
            },
            Removed {
                name: self.name.clone(),
                home: self.remove_home.then_some(entry.home),
            },
        ))
    }

    fn apply(&self, sys: &System, change: Change<Removed>) -> Result<Removed> {
        let tools = Tools::of(sys);
        match tools {
            Tools::Shadow => {
                let mut cmd = sys.cmd("userdel");
                if self.remove_home {
                    cmd = cmd.arg("-r");
                }
                run_tool(cmd.arg(&self.name), tools, "userdel")?;
            }
            Tools::BusyBox => {
                let mut cmd = sys.cmd("deluser");
                if self.remove_home {
                    cmd = cmd.arg("--remove-home");
                }
                run_tool(cmd.arg(&self.name), tools, "deluser")?;
            }
        }
        change.predicted.ok_or_else(|| {
            Error::msg("user::Absent::apply received a change without its prediction")
        })
    }
}

/// Look up an account that must already exist. Read-only (vision 13.1):
/// never reports `changed`, fails the run if the user is missing. Ansible's
/// `getent` plus `register`.
///
/// ```ignore
/// let account = ctx.step("Look up rustible user", user::Existing::named("rustible"))?;
/// ```
///
/// Needs no root: it only reads `/etc/passwd` and `/etc/group`.
#[derive(Debug, Clone)]
pub struct Existing {
    name: String,
}

impl Existing {
    pub fn named(name: impl Into<String>) -> Self {
        Existing { name: name.into() }
    }
}

impl Op for Existing {
    type Output = Account;

    fn check(&self, sys: &System) -> Result<Plan<Account>> {
        validate_name("user", &self.name)?;
        match read_account(sys, &self.name)? {
            Some(account) => Ok(Plan::Satisfied(account)),
            None => bail!(
                "user `{}` does not exist; user::Existing only looks up, use user::Present to \
                 create it",
                self.name
            ),
        }
    }

    fn apply(&self, _: &System, _: Change<Account>) -> Result<Account> {
        bail!("user::Existing never changes anything; apply must not be called")
    }
}

/// Output of [`Membership`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub user: String,
    pub group: String,
}

/// Ensure a user is a member of one group (vision 6.6). The user and the
/// group must both exist (vision 6.7): this op creates neither. Ansible's
/// `user` with `groups: [g]` and `append: yes`, one group per step so the
/// report says which membership changed.
///
/// ```ignore
/// for name in ["docker", "adm"] {
///     let grp = ctx.step(format!("Ensure group {name} exists"), group::Present::new(name))?;
///     ctx.step(format!("Add rustible to {name}"), user::Membership::of(&account).in_group(&grp))?;
/// }
/// ```
///
/// Satisfied when the group's member field lists the user or the group is
/// the user's primary group. Adds with `usermod -aG` (BusyBox: `addgroup
/// user group`), which never removes other memberships.
#[derive(Debug, Clone)]
pub struct Membership {
    user: String,
    group: String,
}

impl Membership {
    /// Membership of this account. Finish with [`MembershipBuilder::in_group`].
    pub fn of(account: &Account) -> MembershipBuilder {
        MembershipBuilder {
            user: account.name.clone(),
        }
    }

    /// Membership of the account with this name.
    pub fn of_name(name: impl Into<String>) -> MembershipBuilder {
        MembershipBuilder { user: name.into() }
    }
}

/// Builder for [`Membership`]; `in_group` finishes it.
#[derive(Debug, Clone)]
pub struct MembershipBuilder {
    user: String,
}

impl MembershipBuilder {
    /// The group to be a member of. Finishes the builder.
    pub fn in_group(self, group: &Group) -> Membership {
        Membership {
            user: self.user,
            group: group.name.clone(),
        }
    }

    /// The group, by name. Finishes the builder.
    pub fn in_group_named(self, name: impl Into<String>) -> Membership {
        Membership {
            user: self.user,
            group: name.into(),
        }
    }
}

impl Op for Membership {
    type Output = Member;

    fn check(&self, sys: &System) -> Result<Plan<Member>> {
        require_root(sys, "user::Membership")?;
        validate_name("user", &self.user)?;
        validate_name("group", &self.group)?;
        let passwd = sys.read_to_string("/etc/passwd")?;
        let Some(entry) = lookup_user(&passwd, &self.user)? else {
            bail!(
                "user `{}` does not exist; user::Membership does not create users, use \
                 user::Present first",
                self.user
            );
        };
        let group_text = sys.read_to_string("/etc/group")?;
        let Some(group) = lookup_group(&group_text, &self.group)? else {
            bail!(
                "group `{}` does not exist; user::Membership does not create groups, use \
                 group::Present first",
                self.group
            );
        };
        let member = Member {
            user: self.user.clone(),
            group: self.group.clone(),
        };
        if group.members.contains(&self.user) || group.gid == entry.gid {
            return Ok(Plan::Satisfied(member));
        }
        let before = groups_of(&group_text, &self.user);
        let after = sorted([before.clone(), vec![self.group.clone()]].concat());
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: format!("user {}", self.user),
                changes: vec![AttrChange {
                    name: "groups".into(),
                    from: before.join(","),
                    to: after.join(","),
                }],
            },
            member,
        ))
    }

    fn apply(&self, sys: &System, change: Change<Member>) -> Result<Member> {
        let tools = Tools::of(sys);
        match tools {
            Tools::Shadow => run_tool(
                sys.cmd("usermod").args(["-aG", &self.group, &self.user]),
                tools,
                "usermod",
            )?,
            Tools::BusyBox => run_tool(
                sys.cmd("addgroup").args([&self.user, &self.group]),
                tools,
                "addgroup",
            )?,
        }
        change.predicted.ok_or_else(|| {
            Error::msg("user::Membership::apply received a change without its prediction")
        })
    }
}

// TODO(M6 harness): once `rustible_sdk::testing::integration` and
// `#[rustible::integration_test(images = [..])]` from branch m6-harness are
// on main, add the container tests on debian:12 and ubuntu:24.04:
// `Present::new("rustible-test").shell("/bin/bash")` twice (changed, then
// ok, home exists), `group::Present` plus `Membership` twice, and `Absent`
// with `remove_home` (home gone).

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rustible_sdk::backend::{Backend, Fake};
    use rustible_sdk::event::Collect;

    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
                          cadu:x:1000:1000:Cadu:/home/cadu:/bin/zsh\n\
                          nobody:x:65534:65534:nobody:/nonexistent:/usr/sbin/nologin\n";
    const GROUP: &str = "root:x:0:\n\
                         adm:x:4:cadu\n\
                         cadu:x:1000:\n\
                         docker:x:998:\n\
                         sudo:x:27:cadu\n";

    fn cadu() -> PasswdEntry {
        passwd_entry(PASSWD, "cadu").unwrap()
    }

    // ---- parsing ----

    #[test]
    fn parses_entries_and_skips_malformed_lines() {
        let text = "root:x:0:0:root:/root:/bin/bash\nbroken:x:1\n:x:1:1:::/bin/sh\n\
                    bad:x:abc:1:c:/h:/bin/sh\nlast:x:7:8:Last:/home/last:/bin/sh";
        let entries = parse_passwd(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "root");
        assert_eq!(entries[1].name, "last", "no trailing newline is fine");
        assert_eq!(
            entries[1],
            PasswdEntry {
                name: "last".into(),
                uid: 7,
                gid: 8,
                comment: "Last".into(),
                home: "/home/last".into(),
                shell: "/bin/sh".into(),
            }
        );
    }

    #[test]
    fn malformed_line_for_the_name_is_an_error_not_missing() {
        let text = format!("{PASSWD}broken:x:abc:1:b:/home/broken:/bin/sh\n");
        assert_eq!(lookup_user(&text, "cadu").unwrap().unwrap().uid, 1000);
        assert_eq!(lookup_user(&text, "ghost").unwrap(), None);
        let err = lookup_user(&text, "broken").unwrap_err().to_string();
        assert!(err.contains("malformed line for `broken`"), "{err}");
    }

    #[test]
    fn lookups_and_account_assembly() {
        assert_eq!(cadu().uid, 1000);
        assert_eq!(passwd_entry(PASSWD, "ghost"), None);
        assert_eq!(passwd_by_uid(PASSWD, 65534).unwrap().name, "nobody");
        assert_eq!(
            account_of(&cadu(), GROUP),
            Account {
                name: "cadu".into(),
                uid: 1000,
                gid: 1000,
                home: "/home/cadu".into(),
                shell: "/bin/zsh".into(),
                groups: vec!["adm".into(), "sudo".into()],
            }
        );
    }

    // ---- plan_modify ----

    #[test]
    fn modify_nothing_asked_is_empty() {
        let d = plan_modify(&cadu(), &groups_of(GROUP, "cadu"), &Desired::default());
        assert!(d.is_empty());
        let d = plan_modify(
            &cadu(),
            &groups_of(GROUP, "cadu"),
            &Desired {
                shell: Some("/bin/zsh".into()),
                groups: vec!["adm".into()],
                append: true,
                ..Desired::default()
            },
        );
        assert!(d.is_empty(), "{d:?}");
    }

    #[test]
    fn modify_shell_and_append_groups() {
        let d = plan_modify(
            &cadu(),
            &groups_of(GROUP, "cadu"),
            &Desired {
                shell: Some("/bin/bash".into()),
                groups: vec!["docker".into(), "adm".into()],
                append: true,
                ..Desired::default()
            },
        );
        assert_eq!(d.shell.as_deref(), Some(Path::new("/bin/bash")));
        assert_eq!(d.add_groups, vec!["docker"]);
        assert!(d.remove_groups.is_empty());
        let rendered: Vec<String> = d
            .changes
            .iter()
            .map(|c| format!("{}: {} -> {}", c.name, c.from, c.to))
            .collect();
        assert_eq!(
            rendered,
            vec![
                "shell: /bin/zsh -> /bin/bash",
                "groups: adm,sudo -> adm,docker,sudo"
            ]
        );
    }

    #[test]
    fn modify_exact_groups_adds_and_removes() {
        let d = plan_modify(
            &cadu(),
            &groups_of(GROUP, "cadu"),
            &Desired {
                groups: vec!["docker".into(), "adm".into()],
                append: false,
                ..Desired::default()
            },
        );
        assert_eq!(d.add_groups, vec!["docker"]);
        assert_eq!(d.remove_groups, vec!["sudo"]);
        assert_eq!(d.changes[0].to, "adm,docker");
        let d = plan_modify(
            &cadu(),
            &groups_of(GROUP, "cadu"),
            &Desired {
                groups: vec![],
                append: false,
                ..Desired::default()
            },
        );
        assert_eq!(d.remove_groups, vec!["adm", "sudo"]);
        assert_eq!(d.changes[0].to, "");
    }

    #[test]
    fn modify_ids_home_and_comment() {
        let d = plan_modify(
            &cadu(),
            &[],
            &Desired {
                uid: Some(1001),
                gid: Some(4),
                home: Some("/srv/cadu".into()),
                comment: Some("Carlos".into()),
                ..Desired::default()
            },
        );
        assert_eq!((d.uid, d.gid), (Some(1001), Some(4)));
        assert_eq!(d.home.as_deref(), Some(Path::new("/srv/cadu")));
        assert_eq!(d.comment.as_deref(), Some("Carlos"));
        assert_eq!(d.changes.len(), 4);
        assert!(d.changes_attributes());
    }

    #[test]
    fn delta_round_trips_through_its_changes() {
        let want = Desired {
            uid: Some(1001),
            gid: Some(4),
            home: Some("/srv/cadu".into()),
            shell: Some("/bin/bash".into()),
            comment: Some("Carlos E.".into()),
            groups: vec!["docker".into()],
            append: false,
        };
        let d = plan_modify(&cadu(), &groups_of(GROUP, "cadu"), &want);
        let back = Delta::from_changes(&d.changes).unwrap();
        assert_eq!((back.uid, back.gid), (d.uid, d.gid));
        assert_eq!(
            (back.home, back.shell, back.comment),
            (d.home, d.shell, d.comment)
        );
        assert_eq!(back.add_groups, vec!["docker"]);
        assert_eq!(back.remove_groups, vec!["adm", "sudo"]);

        let foreign = [AttrChange {
            name: "mode".into(),
            from: "0644".into(),
            to: "0600".into(),
        }];
        let err = Delta::from_changes(&foreign).unwrap_err().to_string();
        assert!(err.contains("did not produce"), "{err}");
    }

    // ---- Fake backend ----

    fn fake_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    fn alpine(sys: System) -> System {
        let mut facts = sys.facts().clone();
        facts.distro = Distro::Alpine;
        facts.package_manager = Pm::Apk;
        sys.with_facts(facts)
    }

    fn not_root(sys: System) -> System {
        let mut facts = sys.facts().clone();
        facts.is_root = false;
        facts.user = "cadu".into();
        sys.with_facts(facts)
    }

    fn base() -> Fake {
        Fake::new()
            .with_file("/etc/passwd", PASSWD)
            .with_file("/etc/group", GROUP)
            .with_file("/etc/default/useradd", "# defaults\nSHELL=/bin/sh\n")
    }

    fn write(fake: &Fake, path: &str, text: &str) {
        fake.write(Path::new(path), text.as_bytes()).unwrap();
    }

    fn change<T>(plan: Plan<T>) -> Change<T> {
        match plan {
            Plan::Change(c) => c,
            Plan::Satisfied(_) => panic!("expected a change"),
        }
    }

    #[test]
    fn present_is_satisfied_when_attributes_match() {
        let fake = Arc::new(base());
        let op = Present::new("cadu").shell("/bin/zsh").groups(["adm"]);
        let Plan::Satisfied(a) = op.check(&fake_sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(a.uid, 1000);
        assert_eq!(a.groups, vec!["adm", "sudo"]);
        assert!(fake.commands().is_empty());
    }

    #[test]
    fn present_creates_with_useradd_predicts_and_rereads() {
        let fake = Arc::new(base().with_cmd("useradd", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Present::new("rustible")
            .uid(1002)
            .shell("/bin/bash")
            .groups(["sudo", "docker"])
            .comment("Rustible");
        let c = change(op.check(&sys).unwrap());
        assert_eq!(
            c.diff.short(),
            "exists=yes uid=1002 home=/home/rustible shell=/bin/bash comment=Rustible \
             groups=docker,sudo"
        );
        let predicted = c.predicted.clone().unwrap();
        assert_eq!(
            predicted,
            Account {
                name: "rustible".into(),
                uid: 1002,
                gid: 1002,
                home: "/home/rustible".into(),
                shell: "/bin/bash".into(),
                groups: vec!["docker".into(), "sudo".into()],
            }
        );
        assert!(fake.commands().is_empty(), "check runs nothing");

        // Stand in for useradd's effect on the files.
        write(
            &fake,
            "/etc/passwd",
            &format!("{PASSWD}rustible:x:1002:1002:Rustible:/home/rustible:/bin/bash\n"),
        );
        write(
            &fake,
            "/etc/group",
            &format!(
                "{}rustible:x:1002:\n",
                GROUP
                    .replace("docker:x:998:", "docker:x:998:rustible")
                    .replace("sudo:x:27:cadu", "sudo:x:27:cadu,rustible")
            ),
        );
        let account = op.apply(&sys, c).unwrap();
        assert_eq!(account, predicted, "the prediction was honest");
        assert_eq!(
            fake.argvs(),
            vec![vec![
                "useradd",
                "-u",
                "1002",
                "-G",
                "docker,sudo",
                "-s",
                "/bin/bash",
                "-c",
                "Rustible",
                "-m",
                "rustible"
            ]]
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn present_create_without_uid_does_not_predict() {
        let fake = Arc::new(base());
        let c = change(Present::new("rustible").check(&fake_sys(&fake)).unwrap());
        assert_eq!(
            c.diff.short(),
            "exists=yes home=/home/rustible shell=/bin/sh"
        );
        assert!(c.predicted.is_none(), "uid is unknown until useradd runs");
    }

    #[test]
    fn present_create_does_not_predict_when_private_gid_is_taken() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake);
        // gid 4 is adm's, so the private group cannot get it.
        let c = change(Present::new("rustible").uid(4).check(&sys).unwrap());
        assert!(c.predicted.is_none());
        // A group already named after the user: the tool will not create
        // one and the outcome depends on login.defs.
        let c = change(Present::new("docker").uid(1500).check(&sys).unwrap());
        assert!(c.predicted.is_none());
        // With an explicit primary group the gid is known again.
        let c = change(
            Present::new("rustible")
                .uid(4)
                .gid("adm")
                .check(&sys)
                .unwrap(),
        );
        assert_eq!(c.predicted.unwrap().gid, 4);
    }

    #[test]
    fn present_create_takes_gid_by_name_id_or_group_and_system_flags() {
        let fake = Arc::new(base().with_cmd("useradd", None, 0, ""));
        let sys = fake_sys(&fake);
        let docker = group_entry(GROUP, "docker").unwrap();
        for op in [
            Present::new("svc").gid("docker"),
            Present::new("svc").gid(998),
            Present::new("svc").gid(&docker),
        ] {
            let c = change(op.check(&sys).unwrap());
            assert!(c.diff.short().contains("gid=998"), "{}", c.diff.short());
        }
        let op = Present::new("svc")
            .gid("docker")
            .system(true)
            .create_home(false)
            .home("/var/lib/svc");
        let c = change(op.check(&sys).unwrap());
        assert_eq!(
            c.diff.short(),
            "exists=yes system=yes gid=998 home=/var/lib/svc create_home=no shell=/bin/sh"
        );
        write(
            &fake,
            "/etc/passwd",
            &format!("{PASSWD}svc:x:999:998::/var/lib/svc:/bin/sh\n"),
        );
        let a = op.apply(&sys, c).unwrap();
        assert_eq!((a.uid, a.gid), (999, 998));
        assert_eq!(
            fake.argvs(),
            vec![vec![
                "useradd",
                "-r",
                "-g",
                "998",
                "-d",
                "/var/lib/svc",
                "-M",
                "svc"
            ]]
        );
    }

    #[test]
    fn present_modifies_with_usermod_append() {
        let fake = Arc::new(base().with_cmd("usermod", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Present::new("cadu").shell("/bin/bash").groups(["docker"]);
        let c = change(op.check(&sys).unwrap());
        assert_eq!(c.diff.short(), "shell=/bin/bash groups=adm,docker,sudo");
        let predicted = c.predicted.clone().unwrap();
        assert_eq!(predicted.shell, Path::new("/bin/bash"));
        assert_eq!(predicted.groups, vec!["adm", "docker", "sudo"]);
        assert_eq!(predicted.uid, 1000);

        write(
            &fake,
            "/etc/passwd",
            &PASSWD.replace("/bin/zsh", "/bin/bash"),
        );
        write(
            &fake,
            "/etc/group",
            &GROUP.replace("docker:x:998:", "docker:x:998:cadu"),
        );
        let a = op.apply(&sys, c).unwrap();
        assert_eq!(a, predicted);
        assert_eq!(
            fake.argvs(),
            vec![vec!["usermod", "-s", "/bin/bash", "-aG", "docker", "cadu"]]
        );
    }

    #[test]
    fn present_exact_groups_uses_usermod_capital_g() {
        let fake = Arc::new(base().with_cmd("usermod", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Present::new("cadu").groups(["docker"]).append(false);
        let c = change(op.check(&sys).unwrap());
        assert_eq!(c.diff.short(), "groups=docker");
        assert_eq!(c.predicted.as_ref().unwrap().groups, vec!["docker"]);
        write(
            &fake,
            "/etc/group",
            "root:x:0:\nadm:x:4:\ncadu:x:1000:\ndocker:x:998:cadu\nsudo:x:27:\n",
        );
        let a = op.apply(&sys, c).unwrap();
        assert_eq!(a.groups, vec!["docker"]);
        assert_eq!(fake.argvs(), vec![vec!["usermod", "-G", "docker", "cadu"]]);
    }

    #[test]
    fn present_modifies_ids_home_and_comment_in_one_usermod() {
        let fake = Arc::new(base().with_cmd("usermod", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Present::new("cadu")
            .uid(1001)
            .gid("adm")
            .home("/srv/cadu")
            .comment("Carlos");
        let c = change(op.check(&sys).unwrap());
        assert_eq!(
            c.diff.short(),
            "uid=1001 gid=4 home=/srv/cadu comment=Carlos"
        );
        write(
            &fake,
            "/etc/passwd",
            &PASSWD.replace(
                "cadu:x:1000:1000:Cadu:/home/cadu",
                "cadu:x:1001:4:Carlos:/srv/cadu",
            ),
        );
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![vec![
                "usermod",
                "-u",
                "1001",
                "-g",
                "4",
                "-d",
                "/srv/cadu",
                "-c",
                "Carlos",
                "cadu"
            ]]
        );
    }

    #[test]
    fn present_refuses_missing_groups_and_taken_uid() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake);
        let err = Present::new("cadu")
            .groups(["ghosts"])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("group `ghosts` does not exist"), "{err}");
        assert!(err.contains("group::Present first"), "{err}");
        let err = Present::new("x")
            .gid("ghosts")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("group `ghosts` does not exist"), "{err}");
        let err = Present::new("x")
            .gid(4242)
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no group has gid 4242"), "{err}");
        let err = Present::new("x")
            .uid(1000)
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("uid 1000 is already used by user `cadu`"),
            "{err}"
        );
        assert!(fake.commands().is_empty());
    }

    #[test]
    fn present_on_alpine_uses_adduser_and_addgroup() {
        let fake = Arc::new(
            base()
                .with_cmd("adduser", None, 0, "")
                .with_cmd("addgroup", None, 0, ""),
        );
        let sys = alpine(fake_sys(&fake));
        let op = Present::new("rustible")
            .uid(1002)
            .gid("docker")
            .shell("/bin/ash")
            .home("/srv/rustible")
            .comment("Rustible")
            .groups(["sudo", "adm"])
            .create_home(false);
        let c = change(op.check(&sys).unwrap());
        let predicted = c.predicted.clone().unwrap();
        assert_eq!((predicted.uid, predicted.gid), (1002, 998));
        write(
            &fake,
            "/etc/passwd",
            &format!("{PASSWD}rustible:x:1002:998:Rustible:/srv/rustible:/bin/ash\n"),
        );
        write(
            &fake,
            "/etc/group",
            &GROUP
                .replace("adm:x:4:cadu", "adm:x:4:cadu,rustible")
                .replace("sudo:x:27:cadu", "sudo:x:27:cadu,rustible"),
        );
        let a = op.apply(&sys, c).unwrap();
        assert_eq!(a, predicted);
        assert_eq!(
            fake.argvs(),
            vec![
                vec![
                    "adduser",
                    "-D",
                    "-u",
                    "1002",
                    "-G",
                    "docker",
                    "-h",
                    "/srv/rustible",
                    "-s",
                    "/bin/ash",
                    "-g",
                    "Rustible",
                    "-H",
                    "rustible"
                ],
                vec!["addgroup", "rustible", "adm"],
                vec!["addgroup", "rustible", "sudo"],
            ]
        );
    }

    #[test]
    fn present_on_alpine_system_user_without_shell_does_not_predict() {
        let fake = Arc::new(base());
        let sys = alpine(fake_sys(&fake));
        let c = change(
            Present::new("svc")
                .uid(100)
                .system(true)
                .check(&sys)
                .unwrap(),
        );
        assert!(c.predicted.is_none());
        let c = change(
            Present::new("svc")
                .uid(100)
                .system(true)
                .gid("docker")
                .shell("/sbin/nologin")
                .check(&sys)
                .unwrap(),
        );
        assert_eq!(c.predicted.unwrap().shell, Path::new("/sbin/nologin"));
    }

    #[test]
    fn present_on_alpine_changes_groups_but_not_attributes() {
        let fake = Arc::new(
            base()
                .with_cmd("addgroup", None, 0, "")
                .with_cmd("delgroup", None, 0, ""),
        );
        let sys = alpine(fake_sys(&fake));
        let err = Present::new("cadu")
            .shell("/bin/ash")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("BusyBox has no `usermod`"), "{err}");
        assert!(err.contains("change its shell"), "{err}");

        let op = Present::new("cadu").groups(["docker"]).append(false);
        let c = change(op.check(&sys).unwrap());
        write(
            &fake,
            "/etc/group",
            "root:x:0:\nadm:x:4:\ncadu:x:1000:\ndocker:x:998:cadu\nsudo:x:27:\n",
        );
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![
                vec!["addgroup", "cadu", "docker"],
                vec!["delgroup", "cadu", "adm"],
                vec!["delgroup", "cadu", "sudo"],
            ]
        );
    }

    #[test]
    fn present_needs_root_and_a_valid_name() {
        let fake = Arc::new(base());
        let err = Present::new("cadu")
            .check(&not_root(fake_sys(&fake)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("user::Present needs root"), "{err}");
        assert!(err.contains("runs as `cadu`"), "{err}");
        let sys = fake_sys(&fake);
        for bad in ["a:b", "a b", "-x", "a\nb", ""] {
            assert!(Present::new(bad).check(&sys).is_err(), "{bad:?}");
        }
        let err = Present::new("x")
            .comment("a:b")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("comment `a:b` is not valid"), "{err}");
        let err = Present::new("x")
            .shell("/bin/sh\n")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("shell `/bin/sh\\n` is not valid"), "{err}");
        let err = Present::new("cadu")
            .groups(["a b"])
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("group name `a b` is not valid"), "{err}");
        assert!(fake.commands().is_empty());
    }

    #[test]
    fn malformed_passwd_line_for_the_user_fails_instead_of_creating() {
        let fake = Arc::new(base().with_file(
            "/etc/passwd",
            format!("{PASSWD}svc:x:abc:1:b:/home/svc:/bin/sh\n"),
        ));
        let sys = fake_sys(&fake);
        let err = Present::new("svc").check(&sys).unwrap_err().to_string();
        assert!(err.contains("malformed line for `svc`"), "{err}");
        let err = Absent::new("svc").check(&sys).unwrap_err().to_string();
        assert!(err.contains("malformed line for `svc`"), "{err}");
        let err = Existing::named("svc").check(&sys).unwrap_err().to_string();
        assert!(err.contains("malformed line for `svc`"), "{err}");
    }

    #[test]
    fn present_missing_binary_names_the_tool_family() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake);
        let op = Present::new("rustible");
        let c = change(op.check(&sys).unwrap());
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("running `useradd` (shadow-utils)"), "{err}");
        assert!(err.contains("could not spawn"), "{err}");
    }

    #[test]
    fn present_reads_default_shell_from_useradd_defaults() {
        let fake =
            Arc::new(base().with_file("/etc/default/useradd", "SHELL=/bin/bash\nHOME=/home\n"));
        let c = change(Present::new("x").uid(1500).check(&fake_sys(&fake)).unwrap());
        assert_eq!(c.predicted.unwrap().shell, Path::new("/bin/bash"));
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_file("/etc/group", GROUP),
        );
        let c = change(Present::new("x").uid(1500).check(&fake_sys(&fake)).unwrap());
        assert_eq!(c.predicted.unwrap().shell, Path::new("/bin/sh"));
    }

    #[test]
    fn absent_is_satisfied_when_missing() {
        let fake = Arc::new(base());
        let Plan::Satisfied(r) = Absent::new("ghost").check(&fake_sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.home, None);
        assert!(fake.commands().is_empty());
    }

    #[test]
    fn absent_removes_with_userdel_and_reports_home() {
        let fake = Arc::new(base().with_cmd("userdel", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Absent::new("cadu").remove_home(true);
        let c = change(op.check(&sys).unwrap());
        assert_eq!(c.diff.short(), "exists=no home=removed");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.home.as_deref(), Some(Path::new("/home/cadu")));
        assert_eq!(fake.argvs(), vec![vec!["userdel", "-r", "cadu"]]);

        let fake = Arc::new(base().with_cmd("userdel", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Absent::new("cadu");
        let c = change(op.check(&sys).unwrap());
        assert_eq!(c.diff.short(), "exists=no");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.home, None);
        assert_eq!(fake.argvs(), vec![vec!["userdel", "cadu"]]);
    }

    #[test]
    fn absent_on_alpine_uses_deluser() {
        let fake = Arc::new(base().with_cmd("deluser", None, 0, ""));
        let sys = alpine(fake_sys(&fake));
        let op = Absent::new("cadu").remove_home(true);
        let c = change(op.check(&sys).unwrap());
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.argvs(), vec![vec!["deluser", "--remove-home", "cadu"]]);
    }

    #[test]
    fn absent_needs_root() {
        let fake = Arc::new(base());
        let err = Absent::new("cadu")
            .check(&not_root(fake_sys(&fake)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("user::Absent needs root"), "{err}");
    }

    #[test]
    fn existing_looks_up_without_root_and_fails_when_missing() {
        let fake = Arc::new(base());
        let sys = not_root(fake_sys(&fake));
        let Plan::Satisfied(a) = Existing::named("cadu").check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(a.home, Path::new("/home/cadu"));
        assert_eq!(a.groups, vec!["adm", "sudo"]);
        let err = Existing::named("ghost")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("user `ghost` does not exist"), "{err}");
        assert!(fake.commands().is_empty());

        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx.step("lookup", Existing::named("cadu")).unwrap();
        assert!(!r.changed);
        assert_eq!(r.uid, 1000);
    }

    #[test]
    fn membership_is_satisfied_for_members_and_primary_group() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake);
        let account = account_of(&cadu(), GROUP);
        let adm = group_entry(GROUP, "adm").unwrap();
        let op = Membership::of(&account).in_group(&adm);
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
        let op = Membership::of_name("cadu").in_group_named("cadu");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn membership_adds_with_usermod_ag() {
        let fake = Arc::new(base().with_cmd("usermod", None, 0, ""));
        let sys = fake_sys(&fake);
        let account = account_of(&cadu(), GROUP);
        let docker = group_entry(GROUP, "docker").unwrap();
        let op = Membership::of(&account).in_group(&docker);
        let c = change(op.check(&sys).unwrap());
        assert_eq!(c.diff.short(), "groups=adm,docker,sudo");
        let m = op.apply(&sys, c).unwrap();
        assert_eq!(
            m,
            Member {
                user: "cadu".into(),
                group: "docker".into()
            }
        );
        assert_eq!(fake.argvs(), vec![vec!["usermod", "-aG", "docker", "cadu"]]);
    }

    #[test]
    fn membership_on_alpine_uses_addgroup() {
        let fake = Arc::new(base().with_cmd("addgroup", None, 0, ""));
        let sys = alpine(fake_sys(&fake));
        let op = Membership::of_name("cadu").in_group_named("docker");
        let c = change(op.check(&sys).unwrap());
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.argvs(), vec![vec!["addgroup", "cadu", "docker"]]);
    }

    #[test]
    fn membership_refuses_missing_user_group_and_non_root() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake);
        let err = Membership::of_name("cadu")
            .in_group_named("ghosts")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("group `ghosts` does not exist"), "{err}");
        assert!(err.contains("group::Present first"), "{err}");
        let err = Membership::of_name("ghost")
            .in_group_named("docker")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("user `ghost` does not exist"), "{err}");
        let err = Membership::of_name("cadu")
            .in_group_named("docker")
            .check(&not_root(sys))
            .unwrap_err()
            .to_string();
        assert!(err.contains("user::Membership needs root"), "{err}");
    }

    #[test]
    fn check_mode_through_ctx_runs_nothing_and_chains_from_predictions() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());

        let account = ctx
            .step(
                "user",
                Present::new("rustible").uid(1002).shell("/bin/bash"),
            )
            .unwrap();
        assert!(account.changed && account.predicted && account.is_available());
        assert_eq!(account.home, Path::new("/home/rustible"));

        let unpredicted = ctx.step("user", Present::new("nouid")).unwrap();
        assert!(unpredicted.changed && !unpredicted.is_available());

        let removed = ctx
            .step("gone", Absent::new("cadu").remove_home(true))
            .unwrap();
        assert!(removed.changed && removed.is_available());

        let member = ctx
            .step(
                "member",
                Membership::of_name("cadu").in_group_named("docker"),
            )
            .unwrap();
        assert!(member.changed && member.is_available());

        assert!(fake.commands().is_empty(), "check mode runs nothing");
        assert_eq!(fake.content("/etc/passwd").unwrap(), PASSWD);
        assert_eq!(fake.content("/etc/group").unwrap(), GROUP);
    }

    #[test]
    fn membership_through_ctx_is_changed_then_ok() {
        let fake = Arc::new(base().with_cmd("usermod", None, 0, ""));
        let sys = fake_sys(&fake);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let op = Membership::of_name("cadu").in_group_named("docker");
        let r = ctx.step("member", op.clone()).unwrap();
        assert!(r.changed && !r.predicted);
        assert_eq!(r.group, "docker");
        assert_eq!(fake.argvs(), vec![vec!["usermod", "-aG", "docker", "cadu"]]);
        // The fake cannot run usermod: plant its effect for the second leg.
        write(
            &fake,
            "/etc/group",
            &GROUP.replace("docker:x:998:", "docker:x:998:cadu"),
        );
        let r = ctx.step("member again", op).unwrap();
        assert!(!r.changed);
        assert_eq!(fake.argvs().len(), 1, "nothing ran the second time");
    }
}
