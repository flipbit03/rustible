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
//! naming a missing group fails at `check`; use `group::Present` first. Under
//! `--check`, for an account that does not exist yet, it does not fail: an
//! earlier `group::Present` in the run may create the group, so the step
//! reports `would change` with the group named in its diff and the real run
//! refuses if it is really missing. An existing account's missing group is
//! refused in both modes. Both halves are what Ansible's `user` does (vision
//! 12).

use std::path::PathBuf;

use rustible_sdk::prelude::*;

#[allow(unused_imports)]
use crate::group::{
    Group, Tools, group_by_gid, group_entry, groups_of, lookup_group, require_passwd_db,
    require_root, run_tool, validate_field, validate_name,
};

/// A user account as it stands on the machine. Output of [`Present`] and
/// [`Existing`]; what `file::Directory::owner` and
/// `ssh::authorized_keys::Present::for_user` will take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Login name, the first field of the account's `/etc/passwd` line.
    pub name: String,
    /// The uid `/etc/passwd` records, not the one that was asked for: when
    /// [`Present`] left the choice to `useradd`, this is what it allocated.
    pub uid: u32,
    /// Primary group id.
    pub gid: u32,
    /// Home directory as `/etc/passwd` records it. The directory itself may
    /// not exist; nothing here repairs a missing home after creation.
    pub home: PathBuf,
    /// Login shell as `/etc/passwd` records it, the distro tool's default
    /// when [`Present`] was not given one.
    pub shell: PathBuf,
    /// Supplementary groups: every group in `/etc/group` whose member field
    /// lists the user, sorted by name. The primary group appears only if it
    /// also lists the user.
    pub groups: Vec<String>,
}

/// One well-formed line of `/etc/passwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdEntry {
    /// Field 1, never empty: [`parse_passwd`] drops a line without a name.
    pub name: String,
    /// Field 3, decimal. A line whose uid does not parse is dropped too.
    pub uid: u32,
    /// Field 4, the primary group's id. Turning it into a name needs
    /// `/etc/group`, which is a separate read.
    pub gid: u32,
    /// The GECOS field, Ansible's `comment`.
    pub comment: String,
    /// Field 6, verbatim. Whether the directory exists is not checked here.
    pub home: PathBuf,
    /// Field 7, verbatim; empty when the line ends on its colon.
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
    /// A numeric gid, from `.gid(1000)` or from a `group::Group` an earlier
    /// step returned. `check` still looks it up in `/etc/group` and fails
    /// when no group carries it.
    Id(u32),
    /// A group name, from `.gid("docker")`. Resolved against `/etc/group` at
    /// `check`; under `--check`, for an account that does not exist yet, a
    /// name not there yet is carried into the diff as `group=<name>`, since
    /// an earlier step may create it (vision 12).
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

/// The requested primary group as `check` resolved it: an existing group
/// (gid known) or, under `--check` for an account that does not exist yet,
/// one not on the machine yet that an earlier step may create (gid known
/// only when it was asked for by number).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Primary {
    name: String,
    gid: Option<u32>,
}

/// What [`Present`] asks for on an existing account, with the primary group
/// already resolved to a gid. Input of [`plan_modify`]. The default asks for
/// nothing: every attribute `None`, no groups, `append` on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Desired {
    /// Uid to hold the account at. `None` leaves whatever `/etc/passwd` says.
    pub uid: Option<u32>,
    /// Primary group, already resolved from a [`GroupId`] to a gid by
    /// `check`, so [`plan_modify`] never has to read `/etc/group` again.
    pub gid: Option<u32>,
    /// Home path to record. Only the `/etc/passwd` field is compared and
    /// changed; an existing directory is not moved.
    pub home: Option<PathBuf>,
    /// Login shell to record. Not checked against `/etc/shells`.
    pub shell: Option<PathBuf>,
    /// GECOS field to record, Ansible's `comment`.
    pub comment: Option<String>,
    /// Supplementary groups to have (`append`) or to have exactly. `None`
    /// leaves memberships alone whatever `append` says, like Ansible's
    /// omitted `groups`.
    pub groups: Option<Vec<String>>,
    /// `true` adds `groups` to the memberships the account already has;
    /// `false` makes `groups` the complete list, so anything else is left.
    /// Read only when `groups` is `Some`.
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
            groups: None,
            append: true,
        }
    }
}

/// What must change on an existing account. Output of [`plan_modify`]; each
/// `Some` is an attribute to set, and the group lists are the delta. It is
/// the `Modify` half of [`Present`]'s intent: `check` produces it, `apply`
/// executes it field by field (vision 6.2), and the report is rendered from
/// it by [`Delta::changes`].
#[derive(Debug, Clone, Default)]
pub struct Delta {
    /// Uid to set (`usermod -u`). Files the old uid owns are not chowned.
    pub uid: Option<u32>,
    /// Primary gid to set (`usermod -g`).
    pub gid: Option<u32>,
    /// Home path to set (`usermod -d`, never `-m`: the old directory stays
    /// where it is and keeps its contents).
    pub home: Option<PathBuf>,
    /// Login shell to set (`usermod -s`).
    pub shell: Option<PathBuf>,
    /// GECOS field to set (`usermod -c`).
    pub comment: Option<String>,
    /// Groups to join, already narrowed to the ones the account is not in.
    pub add_groups: Vec<String>,
    /// Groups to leave, only ever non-empty when an exact list was asked for
    /// with `append: false`. BusyBox runs one `delgroup` per entry; shadow's
    /// `usermod` is instead handed the whole new list as `-G`.
    pub remove_groups: Vec<String>,
    /// The account as `check` found it: the report's `from` side.
    was: Was,
    /// The complete, sorted supplementary list the account ends up with,
    /// when that changes.
    groups_after: Vec<String>,
    /// An exact list was asked for (`append: false`): shadow's `usermod` is
    /// handed `groups_after` whole rather than the additions.
    exact: bool,
}

/// The attributes of an existing account as `check` read them, kept so the
/// report can show what each change replaces.
#[derive(Debug, Clone, Default)]
struct Was {
    uid: u32,
    gid: u32,
    home: PathBuf,
    shell: PathBuf,
    comment: String,
    groups: Vec<String>,
}

impl Delta {
    /// True when the account already matches: [`Present`] reports the step
    /// satisfied and never runs a tool.
    pub fn is_empty(&self) -> bool {
        !self.changes_attributes() && !self.changes_groups()
    }

    /// The report's rows, one per attribute that changes, in the order
    /// `usermod` takes them.
    pub fn changes(&self) -> Vec<AttrChange> {
        let mut changes = vec![];
        if let Some(uid) = self.uid {
            changes.push(AttrChange::new(
                "uid",
                self.was.uid.to_string(),
                uid.to_string(),
            ));
        }
        if let Some(gid) = self.gid {
            changes.push(AttrChange::new(
                "gid",
                self.was.gid.to_string(),
                gid.to_string(),
            ));
        }
        if let Some(home) = &self.home {
            changes.push(AttrChange::new(
                "home",
                self.was.home.display().to_string(),
                home.display().to_string(),
            ));
        }
        if let Some(shell) = &self.shell {
            changes.push(AttrChange::new(
                "shell",
                self.was.shell.display().to_string(),
                shell.display().to_string(),
            ));
        }
        if let Some(comment) = &self.comment {
            changes.push(AttrChange::new(
                "comment",
                self.was.comment.as_str(),
                comment.as_str(),
            ));
        }
        if self.changes_groups() {
            changes.push(AttrChange::new(
                "groups",
                self.was.groups.join(","),
                self.groups_after.join(","),
            ));
        }
        changes
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

    fn changes_groups(&self) -> bool {
        !self.add_groups.is_empty() || !self.remove_groups.is_empty()
    }

    /// The names of the attributes other than membership that change, for
    /// the BusyBox refusal.
    fn attribute_names(&self) -> Vec<&'static str> {
        [
            ("uid", self.uid.is_some()),
            ("gid", self.gid.is_some()),
            ("home", self.home.is_some()),
            ("shell", self.shell.is_some()),
            ("comment", self.comment.is_some()),
        ]
        .into_iter()
        .filter_map(|(name, set)| set.then_some(name))
        .collect()
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
    let mut delta = Delta {
        was: Was {
            uid: current.uid,
            gid: current.gid,
            home: current.home.clone(),
            shell: current.shell.clone(),
            comment: current.comment.clone(),
            groups: current_groups.to_vec(),
        },
        ..Delta::default()
    };
    delta.uid = want.uid.filter(|uid| *uid != current.uid);
    delta.gid = want.gid.filter(|gid| *gid != current.gid);
    delta.home = want.home.clone().filter(|h| h != &current.home);
    delta.shell = want.shell.clone().filter(|s| s != &current.shell);
    delta.comment = want.comment.clone().filter(|c| c != &current.comment);

    // No `groups` asked for: memberships are not touched, whatever `append`
    // says (an `append(false)` alone must never strip a user's groups).
    let Some(wanted) = want.groups.clone().map(sorted) else {
        return delta;
    };
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
        delta.groups_after = if want.append {
            sorted([current_groups.to_vec(), add.clone()].concat())
        } else {
            wanted
        };
        delta.add_groups = add;
        delta.remove_groups = remove;
        delta.exact = !want.append;
    }
    delta
}

/// What a new account gets when nothing is asked for: shadow-utils read
/// `SHELL=` and `HOME=` (the base directory) from `/etc/default/useradd`
/// (Debian and Ubuntu ship `/bin/sh` and `/home`, Fedora `/bin/bash`) and
/// fall back to `/bin/sh` and `/home`. BusyBox `adduser` uses `/home` but
/// takes the shell from the invoking environment (`$SHELL`, else the
/// invoking user's own shell in `/etc/passwd`, `/bin/ash` for root on
/// Alpine), which this op cannot see through `sys`; the shell field here is
/// not used for BusyBox.
struct UseraddDefaults {
    shell: PathBuf,
    home_base: PathBuf,
}

fn useradd_defaults(sys: &System, tools: Tools) -> Result<UseraddDefaults> {
    let mut d = UseraddDefaults {
        shell: PathBuf::from("/bin/sh"),
        home_base: PathBuf::from("/home"),
    };
    if tools == Tools::Shadow && sys.exists("/etc/default/useradd")? {
        let text = sys.read_to_string("/etc/default/useradd")?;
        let value = |key: &str| {
            text.lines().find_map(|l| {
                l.trim()
                    .strip_prefix(key)
                    .map(|v| v.trim().trim_matches('"').to_string())
                    .filter(|v| !v.is_empty())
            })
        };
        if let Some(shell) = value("SHELL=") {
            d.shell = PathBuf::from(shell);
        }
        if let Some(home) = value("HOME=") {
            d.home_base = PathBuf::from(home);
        }
    }
    Ok(d)
}

/// What [`Present::check`] found and validated.
struct Inspection {
    /// The account as it is, if it exists.
    current: Option<PasswdEntry>,
    /// Primary group, when asked for.
    primary: Option<Primary>,
    /// Attribute changes on an existing account.
    delta: Delta,
    group_text: String,
}

/// Ensure a user account exists with the given attributes.
/// `ansible.builtin.user` with `state: present`.
///
/// ```no_run
/// # use rustible_sdk::prelude::*;
/// # use rustible_std::{file, user};
/// # fn playbook(ctx: &mut Ctx) -> Result<()> {
/// let account = ctx.step(
///     "Ensure rustible user exists",
///     user::Present::new("rustible")
///         .shell("/bin/bash")
///         .groups(["docker", "adm"])
///         .create_home(true),
/// )?;
/// ctx.step("Ensure the app directory",
///     file::Directory::at("/srv/app")
///         .owner(account.uid, account.gid)
///         .mode(0o750))?;
/// # Ok(()) }
/// ```
///
/// Attributes not asked for are never touched on an existing account, and
/// `create_home`/`system` only matter on creation, as in Ansible. Groups
/// named in `groups` and the primary group from `gid` must already exist
/// (vision 6.7). Without `gid`, a new account whose name is already a
/// group's name gets that group as its primary group (`useradd -g`), where
/// the tools would otherwise refuse to create the private group; Ansible
/// passes `-N` instead and lands the account in the default group. On Alpine, where BusyBox has no `usermod`, changing the
/// uid, gid, home, shell or comment of an existing account fails clearly;
/// group membership still works through `addgroup`/`delgroup`.
///
/// Check mode (vision 12): a step that would change has no output, so a
/// dry run that chains from this step stops at the first read with a clear
/// message. For an account that does not exist yet, a group named in
/// `groups` or `gid` that is not on the machine yet is not refused there —
/// an earlier `group::Present` in the run may create it — and appears in
/// the diff by name; the real run refuses it. For an existing account a
/// missing group is refused in both modes. This is Ansible's `user`
/// behaviour in both cases.
#[derive(Debug, Clone)]
pub struct Present {
    name: String,
    uid: Option<u32>,
    gid: Option<GroupId>,
    home: Option<PathBuf>,
    shell: Option<PathBuf>,
    create_home: bool,
    system: bool,
    groups: Option<Vec<String>>,
    append: bool,
    comment: Option<String>,
}

impl Present {
    /// Ensure the account `name` exists. Nothing else is asked for yet: the
    /// distro tool picks the uid, the primary group and the shell, and only
    /// [`Present::create_home`] starts out on. An account that already exists
    /// is left exactly as it is until a builder method names an attribute.
    pub fn new(name: impl Into<String>) -> Self {
        Present {
            name: name.into(),
            uid: None,
            gid: None,
            home: None,
            shell: None,
            create_home: true,
            system: false,
            groups: None,
            append: true,
            comment: None,
        }
    }

    /// Numeric uid. Unset by default, which lets the tool allocate one.
    /// Enforced on an existing account too, and that only rewrites
    /// `/etc/passwd`: files owned by the old uid are not chowned.
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

    /// Create the home directory when creating the account (`useradd -m`).
    /// Default true. Only matters on creation: an existing account whose
    /// home is missing is not repaired here (unlike Ansible); use
    /// `file::Directory` for that.
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
        self.groups = Some(groups.into_iter().map(Into::into).collect());
        self
    }

    /// With `true` (the default) the account is added to `groups` and keeps
    /// any others; with `false` it ends up in exactly `groups`. Has no effect
    /// unless `groups(..)` was given. Ansible's `append`, with the default
    /// flipped because keeping memberships is the safe choice.
    pub fn append(mut self, on: bool) -> Self {
        self.append = on;
        self
    }

    /// The GECOS field. Ansible's `comment`.
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// The primary group, which must exist (vision 6.7). Under `--check`,
    /// for an account that does not exist yet, a missing one is named in the
    /// diff and not refused: an earlier `group::Present` in the run may
    /// create it, and Ansible's `user` validates nothing before it reports
    /// a new account `changed` in check mode. For an existing account it is
    /// refused in both modes, as Ansible refuses it (vision 12).
    fn resolve_primary(
        &self,
        sys: &System,
        group_text: &str,
        is_new: bool,
    ) -> Result<Option<Primary>> {
        let deferred = is_new && sys.check_mode();
        Ok(match &self.gid {
            None => None,
            Some(GroupId::Id(gid)) => match group_by_gid(group_text, *gid) {
                Some(g) => Some(Primary {
                    name: g.name,
                    gid: Some(g.gid),
                }),
                None if deferred => Some(Primary {
                    name: gid.to_string(),
                    gid: Some(*gid),
                }),
                None => bail!(
                    "no group has gid {gid}; user::Present does not create groups, use \
                     group::Present first"
                ),
            },
            Some(GroupId::Name(name)) => match lookup_group(group_text, name)? {
                Some(g) => Some(Primary {
                    name: g.name,
                    gid: Some(g.gid),
                }),
                None if deferred => Some(Primary {
                    name: name.clone(),
                    gid: None,
                }),
                None => bail!(
                    "group `{name}` does not exist; user::Present does not create groups, \
                     use group::Present first"
                ),
            },
        })
    }

    /// The primary group for a new account when `gid` was not given: the
    /// group named after the user, if one exists, because `useradd`/`adduser`
    /// would otherwise fail trying to create the private group. `None` lets
    /// the tool create it.
    fn same_named_group(&self, group_text: &str) -> Result<Option<Primary>> {
        Ok(lookup_group(group_text, &self.name)?.map(|g| Primary {
            name: g.name,
            gid: Some(g.gid),
        }))
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
        for g in self.groups.iter().flatten() {
            validate_name("group", g)?;
        }
        if let Some(home) = &self.home
            && !home.is_absolute()
        {
            bail!("home {} must be an absolute path", home.display());
        }
        if let Some(GroupId::Name(name)) = &self.gid {
            validate_name("group", name)?;
        }
        let passwd = sys.read_to_string("/etc/passwd")?;
        let group_text = sys.read_to_string("/etc/group")?;

        let current = lookup_user(&passwd, &self.name)?;
        // A missing supplementary group is refused, except under --check for
        // an account that does not exist yet, where an earlier step may
        // create it; an existing account's is refused in both modes, as
        // Ansible refuses it (vision 12).
        let deferred = sys.check_mode() && current.is_none();
        for g in self.groups.iter().flatten() {
            if lookup_group(&group_text, g)?.is_none() && !deferred {
                bail!(
                    "group `{g}` does not exist; user::Present does not create groups, use \
                     group::Present first"
                );
            }
        }
        let mut primary = self.resolve_primary(sys, &group_text, current.is_none())?;
        if primary.is_none() && current.is_none() {
            primary = self.same_named_group(&group_text)?;
        }
        if let Some(uid) = self.uid
            && let Some(taken) = passwd_by_uid(&passwd, uid)
            && taken.name != self.name
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
                    gid: primary.as_ref().and_then(|p| p.gid),
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
                "user `{}` exists and only BusyBox account tools were found (no `usermod`) to \
                 change its {}; on Alpine `apk add shadow` provides usermod, or drop those builders",
                self.name,
                delta.attribute_names().join(", ")
            );
        }
        Ok(Inspection {
            current,
            primary,
            delta,
            group_text,
        })
    }

    /// What creating the missing account means, every field resolved:
    /// the primary group, and the home and shell the tool would default to.
    fn plan_create(
        &self,
        sys: &System,
        tools: Tools,
        primary: Option<Primary>,
    ) -> Result<Creation> {
        let defaults = useradd_defaults(sys, tools)?;
        Ok(Creation {
            system: self.system,
            uid: self.uid,
            primary,
            home: self.home.clone(),
            default_home: defaults.home_base.join(&self.name),
            create_home: self.create_home,
            shell: self.shell.clone(),
            // BusyBox `adduser` takes the shell from `$SHELL` or the invoking
            // user's passwd entry, which this op cannot see through `sys`:
            // only an explicit shell is shown there.
            default_shell: (tools == Tools::Shadow).then_some(defaults.shell),
            comment: self.comment.clone(),
            groups: sorted(self.groups.clone().unwrap_or_default()),
        })
    }

    fn create(&self, sys: &System, tools: Tools, c: Creation) -> Result<()> {
        // A real run's `check` refused a group it could not find, so the gid
        // is known here; the check-mode tolerance never reaches `apply`.
        let primary = match c.primary {
            Some(Primary {
                name,
                gid: Some(gid),
            }) => Some((gid, name)),
            Some(Primary { name, gid: None }) => bail!(
                "group `{name}` does not exist; user::Present does not create groups, use \
                 group::Present first"
            ),
            None => None,
        };
        match tools {
            Tools::Shadow => {
                let mut cmd = sys.cmd("useradd");
                if c.system {
                    cmd = cmd.arg("-r");
                }
                if let Some(uid) = c.uid {
                    cmd = cmd.args(["-u", &uid.to_string()]);
                }
                if let Some((gid, _)) = &primary {
                    cmd = cmd.args(["-g", &gid.to_string()]);
                }
                if !c.groups.is_empty() {
                    cmd = cmd.args(["-G", &c.groups.join(",")]);
                }
                if let Some(home) = &c.home {
                    cmd = cmd.args(["-d", &home.display().to_string()]);
                }
                if let Some(shell) = &c.shell {
                    cmd = cmd.args(["-s", &shell.display().to_string()]);
                }
                if let Some(comment) = &c.comment {
                    cmd = cmd.args(["-c", comment]);
                }
                cmd = cmd.arg(if c.create_home { "-m" } else { "-M" });
                run_tool(cmd.arg(&self.name), tools, "useradd")
            }
            Tools::BusyBox => {
                let mut cmd = sys.cmd("adduser").arg("-D");
                if c.system {
                    cmd = cmd.arg("-S");
                }
                if let Some(uid) = c.uid {
                    cmd = cmd.args(["-u", &uid.to_string()]);
                }
                if let Some((_, name)) = &primary {
                    cmd = cmd.args(["-G", name]);
                }
                if let Some(home) = &c.home {
                    cmd = cmd.args(["-h", &home.display().to_string()]);
                }
                if let Some(shell) = &c.shell {
                    cmd = cmd.args(["-s", &shell.display().to_string()]);
                }
                if let Some(comment) = &c.comment {
                    cmd = cmd.args(["-g", comment]);
                }
                if !c.create_home {
                    cmd = cmd.arg("-H");
                }
                run_tool(cmd.arg(&self.name), tools, "adduser")?;
                for g in &c.groups {
                    run_tool(sys.cmd("addgroup").args([&self.name, g]), tools, "addgroup")?;
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
                if delta.changes_groups() {
                    // An exact list is handed over whole; otherwise only the
                    // additions are appended.
                    cmd = if delta.exact {
                        cmd.args(["-G", &delta.groups_after.join(",")])
                    } else {
                        cmd.args(["-aG", &delta.add_groups.join(",")])
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

/// What [`Present`]'s `check` decided: create the account, or modify the
/// one it found, with the tool family it found to do so.
#[derive(Debug)]
pub struct UserIntent {
    name: String,
    tools: Tools,
    change: UserChange,
}

#[derive(Debug)]
enum UserChange {
    Create(Creation),
    Modify(Delta),
}

/// A new account, every field resolved by `check`.
#[derive(Debug)]
struct Creation {
    system: bool,
    uid: Option<u32>,
    /// The primary group as `check` resolved it. Under `--check` it may be
    /// a group not on the machine yet, named and without a gid.
    primary: Option<Primary>,
    /// The home asked for, passed to the tool; `None` leaves it to the
    /// tool, which picks `default_home`.
    home: Option<PathBuf>,
    default_home: PathBuf,
    create_home: bool,
    /// The shell asked for, passed to the tool.
    shell: Option<PathBuf>,
    /// The shell `useradd` picks when none is asked for; `None` on
    /// BusyBox, whose choice this op cannot see.
    default_shell: Option<PathBuf>,
    comment: Option<String>,
    /// Supplementary groups, sorted.
    groups: Vec<String>,
}

impl Creation {
    fn changes(&self) -> Vec<AttrChange> {
        let mut changes = vec![AttrChange::new("exists", "no", "yes")];
        let mut set = |name: &str, to: String| changes.push(AttrChange::new(name, "-", to));
        if self.system {
            set("system", "yes".into());
        }
        if let Some(uid) = self.uid {
            set("uid", uid.to_string());
        }
        match &self.primary {
            Some(Primary { gid: Some(gid), .. }) => set("gid", gid.to_string()),
            // Under --check, a group an earlier step may create: named, gid unknown.
            Some(Primary { name, gid: None }) => set("group", name.clone()),
            None => {}
        }
        let home = self.home.as_ref().unwrap_or(&self.default_home);
        set("home", home.display().to_string());
        if !self.create_home {
            set("create_home", "no".into());
        }
        if let Some(s) = self.shell.as_ref().or(self.default_shell.as_ref()) {
            set("shell", s.display().to_string());
        }
        if let Some(c) = &self.comment {
            set("comment", c.clone());
        }
        if !self.groups.is_empty() {
            set("groups", self.groups.join(","));
        }
        changes
    }
}

impl Intent for UserIntent {
    fn diff(&self) -> Diff {
        let changes = match &self.change {
            UserChange::Create(c) => c.changes(),
            UserChange::Modify(delta) => delta.changes(),
        };
        Diff::attrs(format!("user {}", self.name), changes)
    }
}

impl Op for Present {
    type Output = Account;
    type Intent = UserIntent;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        require_passwd_db(sys, "user::Present")?;
        require_root(sys, "user::Present")?;
        let Inspection {
            current,
            primary,
            delta,
            group_text,
        } = self.inspect(sys)?;
        let tools = Tools::of(sys);
        let change = match current {
            None => UserChange::Create(self.plan_create(sys, tools, primary)?),
            Some(entry) if delta.is_empty() => {
                return Ok(Plan::Satisfied(account_of(&entry, &group_text)));
            }
            Some(_) => UserChange::Modify(delta),
        };
        Ok(Plan::Change(UserIntent {
            name: self.name.clone(),
            tools,
            change,
        }))
    }

    fn apply(&self, sys: &System, intent: UserIntent) -> Result<Account> {
        match intent.change {
            UserChange::Create(c) => self.create(sys, intent.tools, c)?,
            UserChange::Modify(delta) => self.modify(sys, intent.tools, &delta)?,
        }
        // The uid and gid a new account got are the tool's answer: read
        // them back.
        read_account(sys, &intent.name)?.ok_or_else(|| {
            Error::msg(format!(
                "user `{}` is not in /etc/passwd after creating it",
                intent.name
            ))
        })
    }
}

/// Output of [`Absent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    /// The account, as named to [`Absent::new`]. Reported whether the step
    /// deleted it or found it already gone.
    pub name: String,
    /// The home directory this step deleted (`remove_home`); `None` when the
    /// home was kept or the account did not exist.
    pub home: Option<PathBuf>,
}

/// Ensure a user account does not exist. `ansible.builtin.user` with
/// `state: absent`.
///
/// ```no_run
/// # use rustible_sdk::prelude::*;
/// # use rustible_std::user;
/// # fn playbook(ctx: &mut Ctx) -> Result<()> {
/// ctx.step("Remove old deploy user", user::Absent::new("deploy").remove_home(true))?;
/// # Ok(()) }
/// ```
///
/// `remove_home` is Ansible's `remove: yes`: `userdel -r` (or BusyBox
/// `deluser --remove-home`) deletes the home directory and mail spool with
/// the account. Other groups are never touched.
///
/// # A group named after the account can go with it
///
/// `userdel` removes the account's primary group when that group has the
/// account's name and no other members (`USERGROUPS_ENAB`, the Debian and
/// Ubuntu default). Normally that is the private group `useradd` made, and
/// nobody misses it. But [`Present`] deliberately adopts an existing
/// same-named group as the primary group rather than failing, so this
/// sequence removes a group no step asked to remove:
///
/// ```no_run
/// # use rustible_sdk::prelude::*;
/// # use rustible_std::{group, user};
/// # fn playbook(ctx: &mut Ctx) -> Result<()> {
/// ctx.step("group", group::Present::new("app"))?;   // creates group `app`
/// ctx.step("user", user::Present::new("app"))?;     // adopts it as primary
/// ctx.step("gone", user::Absent::new("app"))?;      // takes group `app` too
/// # Ok(()) }
/// ```
///
/// Give the group a member other than the account, or a name of its own, if
/// it must outlive the account. The alternative was for [`Present`] to pass
/// `useradd -N` as Ansible does, which leaves the account in the system
/// default group (`users`, gid 100) that nobody asked for; see
/// `docs/plan/DECISIONS.md`.
#[derive(Debug, Clone)]
pub struct Absent {
    name: String,
    remove_home: bool,
}

impl Absent {
    /// Remove the account `name`, keeping its home directory;
    /// [`Absent::remove_home`] deletes that too. An account that is not there
    /// is satisfied, not an error.
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

/// What [`Absent`]'s `check` decided: delete the account, and its home when
/// asked, with the tool family it found to do so.
#[derive(Debug)]
pub struct RemoveUser {
    name: String,
    /// The home `check` read from the account's entry, when it goes too
    /// (`userdel -r`); `None` keeps it. The output reports it.
    home: Option<PathBuf>,
    tools: Tools,
}

impl Intent for RemoveUser {
    fn diff(&self) -> Diff {
        let mut changes = vec![AttrChange::new("exists", "yes", "no")];
        if let Some(home) = &self.home {
            changes.push(AttrChange::new(
                "home",
                home.display().to_string(),
                "removed",
            ));
        }
        Diff::attrs(format!("user {}", self.name), changes)
    }
}

impl Op for Absent {
    type Output = Removed;
    type Intent = RemoveUser;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        require_passwd_db(sys, "user::Absent")?;
        require_root(sys, "user::Absent")?;
        validate_name("user", &self.name)?;
        let passwd = sys.read_to_string("/etc/passwd")?;
        let Some(entry) = lookup_user(&passwd, &self.name)? else {
            return Ok(Plan::Satisfied(Removed {
                name: self.name.clone(),
                home: None,
            }));
        };
        Ok(Plan::Change(RemoveUser {
            name: self.name.clone(),
            home: self.remove_home.then_some(entry.home),
            tools: Tools::of(sys),
        }))
    }

    fn apply(&self, sys: &System, intent: RemoveUser) -> Result<Removed> {
        let RemoveUser { name, home, tools } = intent;
        match tools {
            Tools::Shadow => {
                let mut cmd = sys.cmd("userdel");
                if home.is_some() {
                    cmd = cmd.arg("-r");
                }
                run_tool(cmd.arg(&name), tools, "userdel")?;
            }
            Tools::BusyBox => {
                let mut cmd = sys.cmd("deluser");
                if home.is_some() {
                    cmd = cmd.arg("--remove-home");
                }
                run_tool(cmd.arg(&name), tools, "deluser")?;
            }
        }
        Ok(Removed { name, home })
    }
}

/// Look up an account that must already exist. Read-only (vision 13.1):
/// never reports `changed`, fails the run if the user is missing. Ansible's
/// `getent` plus `register`.
///
/// ```no_run
/// # use rustible_sdk::prelude::*;
/// # use rustible_std::user;
/// # fn playbook(ctx: &mut Ctx) -> Result<()> {
/// let account = ctx.step("Look up rustible user", user::Existing::named("rustible"))?;
/// # let _ = account;
/// # Ok(()) }
/// ```
///
/// Needs no root: it only reads `/etc/passwd` and `/etc/group`.
#[derive(Debug, Clone)]
pub struct Existing {
    name: String,
}

impl Existing {
    /// Read the account `name` out of `/etc/passwd` and `/etc/group`. There
    /// is no "missing is fine" variant: a step that hands its [`Account`] to
    /// later steps has nothing to hand them if the user is not there, so a
    /// missing account fails the run.
    pub fn named(name: impl Into<String>) -> Self {
        Existing { name: name.into() }
    }
}

impl Op for Existing {
    type Output = Account;
    type Intent = std::convert::Infallible;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        require_passwd_db(sys, "user::Existing")?;
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

    fn apply(&self, _: &System, intent: Self::Intent) -> Result<Account> {
        match intent {}
    }
}

/// Output of [`Membership`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// The account, as [`Membership::of`] named it.
    pub user: String,
    /// The group it belongs to now, whether this step added it or found it
    /// there (a primary group counts as membership).
    pub group: String,
}

/// Ensure a user is a member of one group (vision 6.6). The user and the
/// group must both exist (vision 6.7): this op creates neither. Ansible's
/// `user` with `groups: [g]` and `append: yes`, one group per step so the
/// report says which membership changed.
///
/// ```no_run
/// # use rustible_sdk::prelude::*;
/// # use rustible_std::{group, user};
/// # fn playbook(ctx: &mut Ctx) -> Result<()> {
/// # let account = ctx.step("Look up rustible", user::Existing::named("rustible"))?;
/// for name in ["docker", "adm"] {
///     let grp = ctx.step(format!("Ensure group {name} exists"), group::Present::new(name))?;
///     ctx.step(format!("Add rustible to {name}"), user::Membership::of(&account).in_group(&grp))?;
/// }
/// # Ok(()) }
/// ```
///
/// Satisfied when the group's member field lists the user or the group is
/// the user's primary group. Adds with `usermod -aG` (BusyBox: `addgroup
/// user group`), which never removes other memberships. Under `--check`, a
/// user that does not exist yet is not refused, whatever its group's state,
/// and the step reports `would change` (Ansible reports a new account
/// `changed` without validating its groups); an existing user's missing
/// group is refused in both modes, as Ansible refuses it (vision 12).
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
    type Intent = Join;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        require_passwd_db(sys, "user::Membership")?;
        require_root(sys, "user::Membership")?;
        validate_name("user", &self.user)?;
        validate_name("group", &self.group)?;
        // Both the user and the group are refused when missing, except under
        // --check for a user that does not exist yet: an earlier step may
        // create the user, and its group with it, and Ansible's `user`
        // validates neither before it reports a new account `changed`. An
        // existing user's missing group is refused in both modes, as Ansible
        // refuses it (vision 12).
        let passwd = sys.read_to_string("/etc/passwd")?;
        let entry = match lookup_user(&passwd, &self.user)? {
            Some(e) => Some(e),
            None if sys.check_mode() => None,
            None => bail!(
                "user `{}` does not exist; user::Membership does not create users, use \
                 user::Present first",
                self.user
            ),
        };
        let group_text = sys.read_to_string("/etc/group")?;
        let group = match lookup_group(&group_text, &self.group)? {
            Some(g) => Some(g),
            None if sys.check_mode() && entry.is_none() => None,
            None => bail!(
                "group `{}` does not exist; user::Membership does not create groups, use \
                 group::Present first",
                self.group
            ),
        };
        let member = Member {
            user: self.user.clone(),
            group: self.group.clone(),
        };
        if let (Some(entry), Some(group)) = (&entry, &group)
            && (group.members.contains(&self.user) || group.gid == entry.gid)
        {
            return Ok(Plan::Satisfied(member));
        }
        let before = groups_of(&group_text, &self.user);
        Ok(Plan::Change(Join {
            user: self.user.clone(),
            group: self.group.clone(),
            before,
            tools: Tools::of(sys),
        }))
    }

    fn apply(&self, sys: &System, intent: Join) -> Result<Member> {
        let Join {
            user, group, tools, ..
        } = intent;
        match tools {
            Tools::Shadow => run_tool(
                sys.cmd("usermod").args(["-aG", &group, &user]),
                tools,
                "usermod",
            )?,
            Tools::BusyBox => {
                run_tool(sys.cmd("addgroup").args([&user, &group]), tools, "addgroup")?
            }
        }
        Ok(Member { user, group })
    }
}

/// What [`Membership`]'s `check` decided: add the user to the group.
#[derive(Debug)]
pub struct Join {
    user: String,
    group: String,
    /// The supplementary groups `check` found the user in, for the report.
    before: Vec<String>,
    tools: Tools,
}

impl Intent for Join {
    fn diff(&self) -> Diff {
        let after = sorted([self.before.clone(), vec![self.group.clone()]].concat());
        Diff::attrs(
            format!("user {}", self.user),
            vec![AttrChange::new(
                "groups",
                self.before.join(","),
                after.join(","),
            )],
        )
    }
}

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
                groups: Some(vec!["adm".into()]),
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
                groups: Some(vec!["docker".into(), "adm".into()]),
                append: true,
                ..Desired::default()
            },
        );
        assert_eq!(d.shell.as_deref(), Some(Path::new("/bin/bash")));
        assert_eq!(d.add_groups, vec!["docker"]);
        assert!(d.remove_groups.is_empty());
        assert_eq!(
            Diff::attrs("user cadu", d.changes()).render(),
            "user cadu:\n  shell: /bin/zsh -> /bin/bash\n  groups: adm,sudo -> adm,docker,sudo\n"
        );
    }

    #[test]
    fn modify_exact_groups_adds_and_removes() {
        let d = plan_modify(
            &cadu(),
            &groups_of(GROUP, "cadu"),
            &Desired {
                groups: Some(vec!["docker".into(), "adm".into()]),
                append: false,
                ..Desired::default()
            },
        );
        assert_eq!(d.add_groups, vec!["docker"]);
        assert_eq!(d.remove_groups, vec!["sudo"]);
        assert_eq!(Diff::attrs("u", d.changes()).short(), "groups=adm,docker");
        let d = plan_modify(
            &cadu(),
            &groups_of(GROUP, "cadu"),
            &Desired {
                groups: Some(vec![]),
                append: false,
                ..Desired::default()
            },
        );
        assert_eq!(d.remove_groups, vec!["adm", "sudo"]);
        assert_eq!(Diff::attrs("u", d.changes()).short(), "groups=");
    }

    #[test]
    fn modify_without_groups_leaves_memberships_alone() {
        for append in [true, false] {
            let d = plan_modify(
                &cadu(),
                &groups_of(GROUP, "cadu"),
                &Desired {
                    groups: None,
                    append,
                    ..Desired::default()
                },
            );
            assert!(d.add_groups.is_empty() && d.remove_groups.is_empty());
            assert!(d.is_empty(), "append={append}: {d:?}");
        }
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
        assert_eq!(d.changes().len(), 4);
        assert!(d.changes_attributes());
    }

    /// The delta is what `apply` executes and what the report is rendered
    /// from: every field it sets is one row, with what `check` read as the
    /// `from` side. Before the intent, `apply` parsed these rows back into a
    /// delta, splitting the group lists on `,`.
    #[test]
    fn delta_renders_every_field_it_sets() {
        let want = Desired {
            uid: Some(1001),
            gid: Some(4),
            home: Some("/srv/cadu".into()),
            shell: Some("/bin/bash".into()),
            comment: Some("Carlos E.".into()),
            groups: Some(vec!["docker".into()]),
            append: false,
        };
        let d = plan_modify(&cadu(), &groups_of(GROUP, "cadu"), &want);
        assert_eq!(d.add_groups, vec!["docker"]);
        assert_eq!(d.remove_groups, vec!["adm", "sudo"]);
        assert_eq!(
            Diff::attrs("user cadu", d.changes()).render(),
            "user cadu:\n  uid: 1000 -> 1001\n  gid: 1000 -> 4\n  home: /home/cadu -> /srv/cadu\n  \
             shell: /bin/zsh -> /bin/bash\n  comment: Cadu -> Carlos E.\n  \
             groups: adm,sudo -> docker\n"
        );
    }

    // ---- Fake backend ----

    fn fake_sys(fake: &Arc<Fake>) -> System {
        System::fake(fake.clone(), Arc::new(Collect::default()))
    }

    fn alpine(sys: System) -> System {
        let mut facts = sys.facts().clone();
        facts.distro = Distro::Alpine;
        facts.package_managers = [Pm::Apk].into_iter().collect();
        sys.with_facts(facts)
    }

    /// A mac. `/etc/passwd` exists there and describes only system
    /// services, so every op in this module has to refuse before reading it.
    fn macos(sys: System) -> System {
        let mut facts = sys.facts().clone();
        facts.os = Os::Macos;
        facts.distro = Distro::Macos;
        facts.package_managers = [Pm::Brew].into_iter().collect();
        sys.with_facts(facts)
    }

    /// Measured on macOS 26.3 before this gate existed: `user::Absent`
    /// reported `ok` for the logged-in account and `user::Existing` reported
    /// that it did not exist. Every one of the four refuses now, and the
    /// message says where the accounts actually live.
    #[test]
    fn every_user_op_refuses_a_mac_naming_open_directory() {
        let fake = Arc::new(base());
        let s = macos(fake_sys(&fake));

        let errs = [
            Present::new("cadu").check(&s).unwrap_err().chain(),
            Absent::new("cadu").check(&s).unwrap_err().chain(),
            Existing::named("cadu").check(&s).unwrap_err().chain(),
            Membership::of_name("cadu")
                .in_group_named("staff")
                .check(&s)
                .unwrap_err()
                .chain(),
        ];
        for (op, err) in ["Present", "Absent", "Existing", "Membership"]
            .iter()
            .zip(&errs)
        {
            assert!(err.contains(&format!("user::{op}")), "{op}: {err}");
            assert!(err.contains("Open Directory"), "{op}: {err}");
            assert!(err.contains("/etc/passwd"), "{op}: {err}");
        }
    }

    /// A platform rustible has never heard of is refused too, and told which
    /// assumption it failed rather than being handed a Darwin story.
    #[test]
    fn an_unknown_platform_is_refused_by_name() {
        let fake = Arc::new(base());
        let mut facts = fake_sys(&fake).facts().clone();
        facts.os = Os::Other("freebsd".into());
        let s = fake_sys(&fake).with_facts(facts);
        let err = Present::new("cadu").check(&s).unwrap_err().chain();
        assert!(err.contains("freebsd"), "{err}");
        assert!(err.contains("only knows to be true on Linux"), "{err}");
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

    fn change<O: Op>(plan: Plan<O>) -> O::Intent {
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
    fn present_creates_with_useradd_and_rereads() {
        let fake = Arc::new(base().with_cmd("useradd", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Present::new("rustible")
            .uid(1002)
            .gid("adm")
            .shell("/bin/bash")
            .groups(["sudo", "docker"])
            .comment("Rustible");
        let c = change(op.check(&sys).unwrap());
        assert_eq!(
            c.diff().short(),
            "exists=yes uid=1002 gid=4 home=/home/rustible shell=/bin/bash comment=Rustible \
             groups=docker,sudo"
        );
        assert!(fake.commands().is_empty(), "check runs nothing");

        // Stand in for useradd's effect on the files.
        write(
            &fake,
            "/etc/passwd",
            &format!("{PASSWD}rustible:x:1002:4:Rustible:/home/rustible:/bin/bash\n"),
        );
        write(
            &fake,
            "/etc/group",
            &GROUP
                .replace("docker:x:998:", "docker:x:998:rustible")
                .replace("sudo:x:27:cadu", "sudo:x:27:cadu,rustible"),
        );
        let account = op.apply(&sys, c).unwrap();
        assert_eq!(
            account,
            Account {
                name: "rustible".into(),
                uid: 1002,
                gid: 4,
                home: "/home/rustible".into(),
                shell: "/bin/bash".into(),
                groups: vec!["docker".into(), "sudo".into()],
            },
            "apply reports the account as the files now have it"
        );
        assert_eq!(
            fake.argvs(),
            vec![vec![
                "useradd",
                "-u",
                "1002",
                "-g",
                "4",
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
    fn present_create_without_uid_plans_from_the_defaults() {
        let fake = Arc::new(base());
        let c = change(Present::new("rustible").check(&fake_sys(&fake)).unwrap());
        // No uid and no gid in the diff: useradd allocates both, and the op
        // does not guess what it will pick.
        assert_eq!(
            c.diff().short(),
            "exists=yes home=/home/rustible shell=/bin/sh"
        );
    }

    #[test]
    fn present_create_takes_a_same_named_group_as_primary() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake);
        // The private group's gid comes from login.defs ranges the op does
        // not model, so a uid alone puts no gid in the diff.
        let c = change(Present::new("rustible").uid(1500).check(&sys).unwrap());
        assert_eq!(
            c.diff().short(),
            "exists=yes uid=1500 home=/home/rustible shell=/bin/sh"
        );
        // A group named after the new user becomes its primary group (the
        // tool would refuse to create the private group), so the gid is known.
        let c = change(Present::new("docker").uid(1500).check(&sys).unwrap());
        assert!(c.diff().short().contains("gid=998"), "{}", c.diff().short());
        // With an explicit primary group the gid is known.
        let c = change(
            Present::new("rustible")
                .uid(4)
                .gid("adm")
                .check(&sys)
                .unwrap(),
        );
        assert!(c.diff().short().contains("gid=4 "), "{}", c.diff().short());
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
            assert!(c.diff().short().contains("gid=998"), "{}", c.diff().short());
        }
        let op = Present::new("svc")
            .gid("docker")
            .system(true)
            .create_home(false)
            .home("/var/lib/svc");
        let c = change(op.check(&sys).unwrap());
        assert_eq!(
            c.diff().short(),
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
        assert_eq!(c.diff().short(), "shell=/bin/bash groups=adm,docker,sudo");

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
        assert_eq!(a.shell, Path::new("/bin/bash"));
        assert_eq!(a.groups, vec!["adm", "docker", "sudo"]);
        assert_eq!(a.uid, 1000);
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
        assert_eq!(c.diff().short(), "groups=docker");
        write(
            &fake,
            "/etc/group",
            "root:x:0:\nadm:x:4:\ncadu:x:1000:\ndocker:x:998:cadu\nsudo:x:27:\n",
        );
        let a = op.apply(&sys, c).unwrap();
        assert_eq!(a.groups, vec!["docker"]);
        assert_eq!(fake.argvs(), vec![vec!["usermod", "-G", "docker", "cadu"]]);
    }

    /// An exact list that overlaps what the account has: `usermod -G` gets
    /// the whole new list, kept group included, not only the additions.
    #[test]
    fn present_exact_groups_hands_usermod_the_whole_list() {
        let fake = Arc::new(base().with_cmd("usermod", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Present::new("cadu").groups(["adm", "docker"]).append(false);
        let c = change(op.check(&sys).unwrap());
        assert_eq!(c.diff().short(), "groups=adm,docker");
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![vec!["usermod", "-G", "adm,docker", "cadu"]]
        );
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
            c.diff().short(),
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
        assert!(
            c.diff().short().contains("uid=1002 gid=998"),
            "{}",
            c.diff().short()
        );
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
        assert_eq!((a.uid, a.gid), (1002, 998));
        assert_eq!(a.groups, vec!["adm", "sudo"]);
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
    fn present_on_alpine_system_user_shows_only_an_explicit_shell() {
        let fake = Arc::new(base().with_cmd("adduser", None, 0, ""));
        let sys = alpine(fake_sys(&fake));
        let c = change(
            Present::new("svc")
                .uid(100)
                .system(true)
                .check(&sys)
                .unwrap(),
        );
        assert!(!c.diff().short().contains("shell="), "{}", c.diff().short());
        let op = Present::new("svc")
            .uid(100)
            .system(true)
            .gid("docker")
            .shell("/sbin/nologin");
        let c = change(op.check(&sys).unwrap());
        assert!(
            c.diff().short().contains("shell=/sbin/nologin"),
            "{}",
            c.diff().short()
        );
        // And the system flag the intent carries reaches `adduser` as `-S`.
        write(
            &fake,
            "/etc/passwd",
            &format!("{PASSWD}svc:x:100:998::/home/svc:/sbin/nologin\n"),
        );
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![vec![
                "adduser",
                "-D",
                "-S",
                "-u",
                "100",
                "-G",
                "docker",
                "-s",
                "/sbin/nologin",
                "svc"
            ]]
        );
    }

    #[test]
    fn present_on_busybox_shows_no_shell_it_did_not_ask_for() {
        // BusyBox `adduser` takes the shell from `$SHELL` or the invoking
        // user's passwd entry (`/bin/ash` for root on Alpine), not `/bin/sh`,
        // and the op cannot see that through `sys`: the diff names no shell.
        let fake = Arc::new(base());
        let sys = alpine(fake_sys(&fake));
        let c = change(
            Present::new("rustible")
                .uid(1002)
                .gid("docker")
                .check(&sys)
                .unwrap(),
        );
        assert_eq!(
            c.diff().short(),
            "exists=yes uid=1002 gid=998 home=/home/rustible"
        );
        // An explicit shell is shown.
        let c = change(
            Present::new("rustible")
                .uid(1002)
                .gid("docker")
                .shell("/bin/ash")
                .check(&sys)
                .unwrap(),
        );
        assert!(
            c.diff().short().ends_with("shell=/bin/ash"),
            "{}",
            c.diff().short()
        );
        // shadow-utils read the default from /etc/default/useradd.
        let c = change(
            Present::new("rustible")
                .uid(1002)
                .gid("docker")
                .check(&fake_sys(&fake))
                .unwrap(),
        );
        assert!(
            c.diff().short().ends_with("shell=/bin/sh"),
            "{}",
            c.diff().short()
        );
    }

    #[test]
    fn a_group_named_after_a_new_user_becomes_its_primary_group() {
        // `group::Present::new("svc")` then `user::Present::new("svc")`:
        // useradd would refuse to create the private group `svc`.
        let group_text = format!("{GROUP}svc:x:4000:\n");
        let fake = Arc::new(
            base()
                .with_file("/etc/group", &group_text)
                .with_cmd("useradd", None, 0, ""),
        );
        let sys = fake_sys(&fake);
        let op = Present::new("svc").uid(4000).shell("/bin/sh");
        let c = change(op.check(&sys).unwrap());
        assert_eq!(
            c.diff().short(),
            "exists=yes uid=4000 gid=4000 home=/home/svc shell=/bin/sh"
        );
        write(
            &fake,
            "/etc/passwd",
            &format!("{PASSWD}svc:x:4000:4000::/home/svc:/bin/sh\n"),
        );
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![vec![
                "useradd", "-u", "4000", "-g", "4000", "-s", "/bin/sh", "-m", "svc"
            ]]
        );

        // BusyBox: `-G svc`.
        let fake = Arc::new(
            base()
                .with_file("/etc/group", &group_text)
                .with_cmd("adduser", None, 0, ""),
        );
        let sys = alpine(fake_sys(&fake));
        let op = Present::new("svc").uid(4000).shell("/bin/ash");
        let c = change(op.check(&sys).unwrap());
        write(
            &fake,
            "/etc/passwd",
            &format!("{PASSWD}svc:x:4000:4000::/home/svc:/bin/ash\n"),
        );
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![vec![
                "adduser", "-D", "-u", "4000", "-G", "svc", "-s", "/bin/ash", "svc"
            ]]
        );

        // An explicit `gid` wins over the same-named group.
        let fake = Arc::new(base().with_file("/etc/group", &group_text));
        let c = change(
            Present::new("svc")
                .uid(4000)
                .gid("docker")
                .check(&fake_sys(&fake))
                .unwrap(),
        );
        assert!(c.diff().short().contains("gid=998"), "{}", c.diff().short());
    }

    // ---- check mode: a prerequisite another step could create (vision 12) ----

    /// The tolerance is for an account that does not exist yet. An existing
    /// account's missing group is refused under `--check` exactly as in a
    /// real run, which is what Ansible's `user` does (`modify_user_usermod`
    /// validates the group before anything that respects check mode), and
    /// which also keeps the BusyBox "no `usermod`" refusal from being hidden
    /// behind a deferred gid.
    #[test]
    fn an_existing_accounts_missing_group_is_refused_under_check_too() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_file("/etc/group", GROUP)
                .with_cmd("adduser", None, 0, ""),
        );
        for sys in [
            fake_sys(&fake).with_check_mode(true),
            alpine(fake_sys(&fake)).with_check_mode(true),
            fake_sys(&fake),
        ] {
            let err = Present::new("cadu")
                .gid("web")
                .check(&sys)
                .unwrap_err()
                .chain();
            assert!(err.contains("group `web` does not exist"), "{err}");
            let err = Present::new("cadu")
                .groups(["web"])
                .check(&sys)
                .unwrap_err()
                .chain();
            assert!(err.contains("group `web` does not exist"), "{err}");
        }
        assert!(fake.commands().is_empty());
    }

    /// Vision 12: under `--check` an earlier `group::Present` in the run may
    /// create the primary group, so the step reports the account it would
    /// create and names the group it cannot number. A real run refuses,
    /// with the same message as before.
    #[test]
    fn check_mode_defers_a_missing_primary_group_by_name() {
        let fake = Arc::new(base());
        let dry = fake_sys(&fake).with_check_mode(true);
        let c = change(Present::new("app").gid("app").check(&dry).unwrap());
        assert_eq!(
            c.diff().short(),
            "exists=yes group=app home=/home/app shell=/bin/sh"
        );
        assert!(fake.commands().is_empty(), "check mode runs nothing");

        let err = Present::new("app")
            .gid("app")
            .check(&fake_sys(&fake))
            .unwrap_err()
            .chain();
        assert!(
            err.contains(
                "group `app` does not exist; user::Present does not create groups, use \
                 group::Present first"
            ),
            "{err}"
        );
    }

    #[test]
    fn check_mode_defers_a_missing_primary_group_by_id() {
        let fake = Arc::new(base());
        let dry = fake_sys(&fake).with_check_mode(true);
        let c = change(Present::new("app").gid(3000).check(&dry).unwrap());
        assert_eq!(
            c.diff().short(),
            "exists=yes gid=3000 home=/home/app shell=/bin/sh"
        );

        let err = Present::new("app")
            .gid(3000)
            .check(&fake_sys(&fake))
            .unwrap_err()
            .chain();
        assert!(
            err.contains(
                "no group has gid 3000; user::Present does not create groups, use \
                 group::Present first"
            ),
            "{err}"
        );
    }

    #[test]
    fn check_mode_defers_a_missing_supplementary_group() {
        let fake = Arc::new(base());
        let dry = fake_sys(&fake).with_check_mode(true);
        // A new account.
        let c = change(
            Present::new("app")
                .groups(["app", "adm"])
                .check(&dry)
                .unwrap(),
        );
        assert_eq!(
            c.diff().short(),
            "exists=yes home=/home/app shell=/bin/sh groups=adm,app"
        );
        // An existing one joining it is refused even here: Ansible's `user`
        // validates an existing account's groups under check mode.
        let err = Present::new("cadu")
            .groups(["app"])
            .check(&dry)
            .unwrap_err()
            .chain();
        assert!(err.contains("group `app` does not exist"), "{err}");
        assert!(fake.commands().is_empty(), "check mode runs nothing");

        let err = Present::new("app")
            .groups(["app"])
            .check(&fake_sys(&fake))
            .unwrap_err()
            .chain();
        assert!(
            err.contains(
                "group `app` does not exist; user::Present does not create groups, use \
                 group::Present first"
            ),
            "{err}"
        );
    }

    /// `Membership` under `--check`: a user that does not exist yet is
    /// deferred whatever the group's state (Ansible reports a new account
    /// `changed` without validating its groups); an existing user's missing
    /// group is refused in both modes, as Ansible refuses it.
    #[test]
    fn membership_defers_only_a_user_not_there_yet_in_check_mode() {
        let fake = Arc::new(base());
        let dry = fake_sys(&fake).with_check_mode(true);
        let c = change(
            Membership::of_name("ghost")
                .in_group_named("docker")
                .check(&dry)
                .unwrap(),
        );
        assert_eq!(c.diff().short(), "groups=docker");
        let c = change(
            Membership::of_name("ghost")
                .in_group_named("app")
                .check(&dry)
                .unwrap(),
        );
        assert_eq!(c.diff().short(), "groups=app");
        assert!(fake.commands().is_empty(), "check mode runs nothing");

        for sys in [dry, fake_sys(&fake)] {
            let err = Membership::of_name("cadu")
                .in_group_named("app")
                .check(&sys)
                .unwrap_err()
                .chain();
            assert!(
                err.contains(
                    "group `app` does not exist; user::Membership does not create groups, use \
                     group::Present first"
                ),
                "{err}"
            );
        }
        let real = fake_sys(&fake);
        let err = Membership::of_name("ghost")
            .in_group_named("docker")
            .check(&real)
            .unwrap_err()
            .chain();
        assert!(
            err.contains(
                "user `ghost` does not exist; user::Membership does not create users, use \
                 user::Present first"
            ),
            "{err}"
        );
    }

    /// The dry run of a first provision, through `Ctx`: group, user in it,
    /// membership. Every step reports `would change`, none has an output,
    /// nothing runs (vision 12).
    #[test]
    fn check_mode_through_ctx_walks_a_first_provision_with_no_outputs() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());

        let g = ctx
            .step("group", crate::group::Present::new("app"))
            .unwrap();
        assert!(g.changed && !g.is_available());
        let u = ctx
            .step("user", Present::new("app").gid("app").groups(["app"]))
            .unwrap();
        assert!(u.changed && !u.is_available());
        assert_eq!(
            u.diff.as_ref().unwrap().short(),
            "exists=yes group=app home=/home/app shell=/bin/sh groups=app"
        );
        let m = ctx
            .step("member", Membership::of_name("app").in_group_named("app"))
            .unwrap();
        assert!(m.changed && !m.is_available());

        // An existing account changing its shell: would change, and the
        // account is unavailable until `apply` has run.
        let r = ctx
            .step("shell", Present::new("cadu").shell("/bin/bash"))
            .unwrap();
        assert!(r.changed && !r.is_available());
        let err = r.output().unwrap_err().to_string();
        assert!(err.contains("would have changed"), "{err}");

        assert!(fake.commands().is_empty(), "check mode runs nothing");
        assert_eq!(fake.content("/etc/passwd").unwrap(), PASSWD);
        assert_eq!(fake.content("/etc/group").unwrap(), GROUP);
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
        assert!(
            err.contains("only BusyBox account tools were found (no `usermod`)"),
            "{err}"
        );
        assert!(err.contains("apk add shadow"), "{err}");
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
    fn present_reads_defaults_from_etc_default_useradd() {
        let fake = Arc::new(base().with_file(
            "/etc/default/useradd",
            "SHELL=/bin/bash\nHOME=\"/srv/home\"\n",
        ));
        let c = change(Present::new("x").check(&fake_sys(&fake)).unwrap());
        assert_eq!(
            c.diff().short(),
            "exists=yes home=/srv/home/x shell=/bin/bash"
        );

        // The defaults show alongside the ids that were given.
        let c = change(
            Present::new("x")
                .uid(1500)
                .gid(4)
                .check(&fake_sys(&fake))
                .unwrap(),
        );
        assert_eq!(
            c.diff().short(),
            "exists=yes uid=1500 gid=4 home=/srv/home/x shell=/bin/bash"
        );

        let fake = Arc::new(
            Fake::new()
                .with_file("/etc/passwd", PASSWD)
                .with_file("/etc/group", GROUP),
        );
        let c = change(Present::new("x").check(&fake_sys(&fake)).unwrap());
        assert_eq!(c.diff().short(), "exists=yes home=/home/x shell=/bin/sh");
    }

    #[test]
    fn present_refuses_a_relative_home() {
        let fake = Arc::new(base());
        let err = Present::new("x")
            .home("rustible")
            .check(&fake_sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("absolute"), "{err}");
    }

    #[test]
    fn present_refuses_a_uid_owned_by_another_account_even_when_modifying() {
        let fake = Arc::new(base());
        let err = Present::new("cadu")
            .uid(65534)
            .check(&fake_sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("65534"), "{err}");
        assert!(err.contains("nobody"), "{err}");
        // The account's own uid is not "taken".
        let plan = Present::new("cadu")
            .uid(1000)
            .check(&fake_sys(&fake))
            .unwrap();
        assert!(matches!(plan, Plan::Satisfied(_)));
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
        assert_eq!(c.diff().short(), "exists=no home=removed");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.home.as_deref(), Some(Path::new("/home/cadu")));
        assert_eq!(fake.argvs(), vec![vec!["userdel", "-r", "cadu"]]);

        let fake = Arc::new(base().with_cmd("userdel", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Absent::new("cadu");
        let c = change(op.check(&sys).unwrap());
        assert_eq!(c.diff().short(), "exists=no");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.home, None);
        assert_eq!(fake.argvs(), vec![vec!["userdel", "cadu"]]);
    }

    /// The home reported is the one `check` read, carried in the intent:
    /// `/etc/passwd` changes between `check` and `apply` here, and an `apply`
    /// that read it again would report the new home.
    #[test]
    fn absent_reports_the_home_check_read() {
        let fake = Arc::new(base().with_cmd("userdel", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Absent::new("cadu").remove_home(true);
        let c = change(op.check(&sys).unwrap());
        write(
            &fake,
            "/etc/passwd",
            &PASSWD.replace("/home/cadu", "/srv/moved"),
        );
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.home.as_deref(), Some(Path::new("/home/cadu")));
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
        // `Existing`'s intent is `Infallible`, so `Satisfied` is the only plan it has.
        let Plan::Satisfied(a) = Existing::named("cadu").check(&sys).unwrap();
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
        assert_eq!(c.diff().short(), "groups=adm,docker,sudo");
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
    fn check_mode_through_ctx_runs_nothing_and_no_would_change_step_has_an_output() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());

        // Every id given or none: a would-change step has no output either
        // way (vision 12), the diff is what the dry run shows.
        let account = ctx
            .step(
                "user",
                Present::new("rustible").uid(1002).gid(4).shell("/bin/bash"),
            )
            .unwrap();
        assert!(account.changed && !account.is_available());
        assert_eq!(
            account.diff.as_ref().unwrap().short(),
            "exists=yes uid=1002 gid=4 home=/home/rustible shell=/bin/bash"
        );
        let nouid = ctx.step("user", Present::new("nouid")).unwrap();
        assert!(nouid.changed && !nouid.is_available());

        let removed = ctx
            .step("gone", Absent::new("cadu").remove_home(true))
            .unwrap();
        assert!(removed.changed && !removed.is_available());
        assert_eq!(
            removed.diff.as_ref().unwrap().short(),
            "exists=no home=removed"
        );

        let member = ctx
            .step(
                "member",
                Membership::of_name("cadu").in_group_named("docker"),
            )
            .unwrap();
        assert!(member.changed && !member.is_available());

        // Satisfied steps keep their output.
        let existing = ctx.step("lookup", Existing::named("cadu")).unwrap();
        assert!(!existing.changed && existing.is_available());
        assert_eq!(existing.home, Path::new("/home/cadu"));

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
        assert!(r.changed && r.is_available());
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
