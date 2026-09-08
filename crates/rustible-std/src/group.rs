//! Unix groups. Ansible's `ansible.builtin.group`, split by desired state
//! (vision 6.3): [`Present`] ensures a group exists (with a given gid, if
//! asked), [`Absent`] ensures it does not.
//!
//! Both ops read `/etc/group` through `sys` and change it only through the
//! distro's own tools, chosen from `facts.distro` (vision 7.4): shadow-utils
//! `groupadd`/`groupmod`/`groupdel` everywhere except Alpine, whose BusyBox
//! `addgroup`/`delgroup` take different flags and have no `groupmod`. Both
//! ops need root.
//!
//! The pure parsers here ([`parse_group`], [`group_entry`], [`groups_of`])
//! are shared with the `user` module.

use rustible_sdk::prelude::*;

/// A group as it stands in `/etc/group`. Output of [`Present`], and what
/// `user::Membership::in_group` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub gid: u32,
    /// Users listed in the group's member field, in file order. A user's
    /// primary group does not list them here.
    pub members: Vec<String>,
}

/// Pure: every well-formed line of `/etc/group` text
/// (`name:password:gid:member,member`). Lines with fewer than four fields,
/// a non-numeric gid, or an empty name are skipped; a missing trailing
/// newline does not matter.
pub fn parse_group(text: &str) -> Vec<Group> {
    text.lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 4 || f[0].is_empty() {
                return None;
            }
            Some(Group {
                name: f[0].to_string(),
                gid: f[2].parse().ok()?,
                members: f[3]
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .collect(),
            })
        })
        .collect()
}

/// Pure: the group called `name`, if `/etc/group` text has it.
pub fn group_entry(text: &str, name: &str) -> Option<Group> {
    parse_group(text).into_iter().find(|g| g.name == name)
}

/// Pure: like [`group_entry`], but a line that names the group and does not
/// parse is an error rather than "missing", so an op never tries to create
/// a group whose line is merely broken.
pub fn lookup_group(text: &str, name: &str) -> Result<Option<Group>> {
    match group_entry(text, name) {
        Some(g) => Ok(Some(g)),
        None => match text.lines().find(|l| l.split(':').next() == Some(name)) {
            Some(line) => bail!("/etc/group has a malformed line for `{name}`: {line}"),
            None => Ok(None),
        },
    }
}

/// Pure: the group with this gid, if `/etc/group` text has it.
pub fn group_by_gid(text: &str, gid: u32) -> Option<Group> {
    parse_group(text).into_iter().find(|g| g.gid == gid)
}

/// Pure: the names of every group whose member field lists `user`, sorted.
/// This is the supplementary group list; the primary group is in
/// `/etc/passwd`.
pub fn groups_of(text: &str, user: &str) -> Vec<String> {
    let mut names: Vec<String> = parse_group(text)
        .into_iter()
        .filter(|g| g.members.iter().any(|m| m == user))
        .map(|g| g.name)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Which family of account tools the host has. Chosen from `facts.distro`;
/// `Other` distros get shadow-utils and a clear error if the binary is
/// missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tools {
    /// `useradd`, `usermod`, `userdel`, `groupadd`, `groupmod`, `groupdel`.
    Shadow,
    /// BusyBox `adduser`, `deluser`, `addgroup`, `delgroup`. No `usermod`,
    /// no `groupmod`.
    BusyBox,
}

impl Tools {
    /// Probe for the binaries first (so `apk add shadow` on Alpine, or a
    /// BusyBox-only image of another distro, is honoured), then fall back to
    /// the distro name.
    pub(crate) fn of(sys: &System) -> Tools {
        let has = |p: &str| sys.exists(p).unwrap_or(false);
        if has("/usr/sbin/usermod") || has("/usr/sbin/useradd") {
            return Tools::Shadow;
        }
        if has("/bin/busybox") || has("/usr/sbin/adduser") {
            return Tools::BusyBox;
        }
        match sys.facts().distro {
            Distro::Alpine => Tools::BusyBox,
            _ => Tools::Shadow,
        }
    }
}

/// Root check shared by every mutating op in `user` and `group`.
pub(crate) fn require_root(sys: &System, op: &str) -> Result<()> {
    if !sys.is_root() {
        bail!(
            "{op} needs root but this step runs as `{}`; use `escalate = true` on the \
             playbook or `ctx.as_root()` on the step",
            sys.facts().user
        );
    }
    Ok(())
}

/// Run an account-tool command, naming the tool family when it cannot be
/// spawned so a missing `groupadd` on an unknown distro reads as such.
pub(crate) fn run_tool(cmd: rustible_sdk::Cmd, tools: Tools, program: &str) -> Result<()> {
    cmd.run().map_err(|e| {
        let family = match tools {
            Tools::Shadow => "shadow-utils",
            Tools::BusyBox => "BusyBox",
        };
        e.context(format!("running `{program}` ({family})"))
    })?;
    Ok(())
}

/// Reject names that would break the passwd/group format or the tools:
/// empty, a leading `-` (read as a flag), or any of `:`, `,`, whitespace
/// and control characters.
pub(crate) fn validate_name(what: &str, name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("{what} name is empty");
    }
    if name.starts_with('-')
        || name
            .chars()
            .any(|c| c == ':' || c == ',' || c.is_whitespace() || c.is_control())
    {
        bail!("{what} name `{}` is not valid", name.escape_default());
    }
    Ok(())
}

/// Reject a free-text field (home, shell, comment) that would corrupt a
/// passwd line: `:` and control characters.
pub(crate) fn validate_field(what: &str, value: &str) -> Result<()> {
    if value.chars().any(|c| c == ':' || c.is_control()) {
        bail!("{what} `{}` is not valid", value.escape_default());
    }
    Ok(())
}

/// What [`Present::check`] found: the current group, if any, and the
/// attribute changes.
struct Inspection {
    current: Option<Group>,
    changes: Vec<AttrChange>,
}

/// Ensure a group exists. `ansible.builtin.group` with `state: present`.
///
/// ```ignore
/// let docker = ctx.step("Ensure group docker", group::Present::new("docker"))?;
/// ctx.step("Add rustible to docker", user::Membership::of(&account).in_group(&docker))?;
/// ```
///
/// Without `gid`, an existing group is left alone whatever its gid, and a
/// new one gets the gid the tool allocates. With `gid`, an existing group
/// whose gid differs is changed with `groupmod -g` (not available on Alpine,
/// which fails clearly). `system(true)` allocates from the system range on
/// creation and is ignored for an existing group, as in Ansible.
///
/// Prediction (vision 12): the output is predicted when the gid is known,
/// that is when `gid` is given or the group already exists. Creating without
/// a gid returns a change without a prediction, so a check-mode run that
/// chains from this step stops there with a clear message rather than
/// carrying a made-up gid.
#[derive(Debug, Clone)]
pub struct Present {
    name: String,
    gid: Option<u32>,
    system: bool,
}

impl Present {
    pub fn new(name: impl Into<String>) -> Self {
        Present {
            name: name.into(),
            gid: None,
            system: false,
        }
    }

    /// The gid the group must have.
    pub fn gid(mut self, gid: u32) -> Self {
        self.gid = Some(gid);
        self
    }

    /// Allocate the gid from the system range when creating (`groupadd -r`,
    /// `addgroup -S`).
    pub fn system(mut self, on: bool) -> Self {
        self.system = on;
        self
    }

    fn inspect(&self, sys: &System) -> Result<Inspection> {
        validate_name("group", &self.name)?;
        let text = sys.read_to_string("/etc/group")?;
        let current = lookup_group(&text, &self.name)?;
        let mut changes = vec![];
        match &current {
            None => {
                changes.push(AttrChange {
                    name: "exists".into(),
                    from: "no".into(),
                    to: "yes".into(),
                });
                if let Some(gid) = self.gid {
                    if let Some(taken) = group_by_gid(&text, gid) {
                        bail!(
                            "gid {gid} is already used by group `{}`; group::Present does not \
                             renumber other groups",
                            taken.name
                        );
                    }
                    changes.push(AttrChange {
                        name: "gid".into(),
                        from: "-".into(),
                        to: gid.to_string(),
                    });
                }
            }
            Some(g) => {
                if let Some(gid) = self.gid
                    && gid != g.gid
                {
                    if let Some(taken) = group_by_gid(&text, gid) {
                        bail!(
                            "gid {gid} is already used by group `{}`; group::Present does not \
                             renumber other groups",
                            taken.name
                        );
                    }
                    if Tools::of(sys) == Tools::BusyBox {
                        bail!(
                            "group `{}` has gid {} but {gid} was asked for, and BusyBox has no \
                             `groupmod` to change it; on Alpine `apk add shadow` provides groupmod, or drop `.gid()`",
                            g.name,
                            g.gid
                        );
                    }
                    changes.push(AttrChange {
                        name: "gid".into(),
                        from: g.gid.to_string(),
                        to: gid.to_string(),
                    });
                }
            }
        }
        Ok(Inspection { current, changes })
    }
}

impl Op for Present {
    type Output = Group;

    fn check(&self, sys: &System) -> Result<Plan<Group>> {
        require_root(sys, "group::Present")?;
        let Inspection { current, changes } = self.inspect(sys)?;
        if changes.is_empty() {
            return Ok(Plan::Satisfied(
                current.expect("no changes means it exists"),
            ));
        }
        if current.is_none() {
            // Check mode: let a later `user::Present`/`user::Membership` in
            // this run accept the group this step would create (vision 6.7
            // still holds: nothing is created here).
            sys.note_would_create("group", &self.name, self.gid);
        }
        let diff = Diff::Attrs {
            subject: format!("group {}", self.name),
            changes,
        };
        let gid = self.gid.or(current.as_ref().map(|g| g.gid));
        Ok(match gid {
            Some(gid) => Plan::change_predicting(
                diff,
                Group {
                    name: self.name.clone(),
                    gid,
                    members: current.map(|g| g.members).unwrap_or_default(),
                },
            ),
            None => Plan::change(diff),
        })
    }

    fn apply(&self, sys: &System, change: Change<Group>) -> Result<Group> {
        // `check` produced the diff; execute it (vision 6.2). An `exists`
        // change means create, anything else is the gid change.
        let Diff::Attrs { changes, .. } = &change.diff else {
            bail!("group::Present::apply received a diff it did not produce");
        };
        let creating = changes.iter().any(|c| c.name == "exists");
        let tools = Tools::of(sys);
        match (creating, tools) {
            (true, Tools::Shadow) => {
                let mut cmd = sys.cmd("groupadd");
                if self.system {
                    cmd = cmd.arg("-r");
                }
                if let Some(gid) = self.gid {
                    cmd = cmd.args(["-g", &gid.to_string()]);
                }
                run_tool(cmd.arg(&self.name), tools, "groupadd")?;
            }
            (true, Tools::BusyBox) => {
                let mut cmd = sys.cmd("addgroup");
                if self.system {
                    cmd = cmd.arg("-S");
                }
                if let Some(gid) = self.gid {
                    cmd = cmd.args(["-g", &gid.to_string()]);
                }
                run_tool(cmd.arg(&self.name), tools, "addgroup")?;
            }
            (false, Tools::Shadow) => {
                let gid = self.gid.expect("a modify plan always carries a gid");
                run_tool(
                    sys.cmd("groupmod")
                        .args(["-g", &gid.to_string(), &self.name]),
                    tools,
                    "groupmod",
                )?;
            }
            (false, Tools::BusyBox) => bail!("BusyBox has no `groupmod`"),
        }
        let text = sys.read_to_string("/etc/group")?;
        group_entry(&text, &self.name).ok_or_else(|| {
            Error::msg(format!(
                "group `{}` is not in /etc/group after creating it",
                self.name
            ))
        })
    }
}

/// Output of [`Absent`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub name: String,
    /// The gid the group had; `None` when it did not exist.
    pub gid: Option<u32>,
}

/// Ensure a group does not exist. `ansible.builtin.group` with
/// `state: absent`.
///
/// ```ignore
/// ctx.step("Remove group games", group::Absent::new("games"))?;
/// ```
///
/// A group that is some user's primary group cannot be removed (`groupdel`
/// refuses); the step fails at `check` naming that user rather than in
/// `apply`, per vision 6.7.
#[derive(Debug, Clone)]
pub struct Absent {
    name: String,
}

impl Absent {
    pub fn new(name: impl Into<String>) -> Self {
        Absent { name: name.into() }
    }
}

impl Op for Absent {
    type Output = Removed;

    fn check(&self, sys: &System) -> Result<Plan<Removed>> {
        require_root(sys, "group::Absent")?;
        validate_name("group", &self.name)?;
        let text = sys.read_to_string("/etc/group")?;
        let Some(group) = lookup_group(&text, &self.name)? else {
            return Ok(Plan::Satisfied(Removed {
                name: self.name.clone(),
                gid: None,
            }));
        };
        let passwd = sys.read_to_string("/etc/passwd")?;
        if let Some(owner) = crate::user::parse_passwd(&passwd)
            .into_iter()
            .find(|u| u.gid == group.gid)
        {
            bail!(
                "group `{}` (gid {}) is the primary group of user `{}`; change or remove that \
                 user first",
                group.name,
                group.gid,
                owner.name
            );
        }
        Ok(Plan::change_predicting(
            Diff::Attrs {
                subject: format!("group {}", self.name),
                changes: vec![AttrChange {
                    name: "exists".into(),
                    from: "yes".into(),
                    to: "no".into(),
                }],
            },
            Removed {
                name: self.name.clone(),
                gid: Some(group.gid),
            },
        ))
    }

    fn apply(&self, sys: &System, change: Change<Removed>) -> Result<Removed> {
        let tools = Tools::of(sys);
        match tools {
            Tools::Shadow => run_tool(sys.cmd("groupdel").arg(&self.name), tools, "groupdel")?,
            Tools::BusyBox => run_tool(sys.cmd("delgroup").arg(&self.name), tools, "delgroup")?,
        }
        change.predicted.ok_or_else(|| {
            Error::msg("group::Absent::apply received a change without its prediction")
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use rustible_sdk::backend::{Backend, Fake};
    use rustible_sdk::event::Collect;

    use super::*;

    const GROUP: &str = "root:x:0:\nadm:x:4:cadu,syslog\ndocker:x:998:cadu\nnogroup:x:65534:\n";
    const PASSWD: &str =
        "root:x:0:0:root:/root:/bin/bash\ncadu:x:1000:1000:Cadu:/home/cadu:/bin/zsh\n";

    // ---- parsing ----

    #[test]
    fn parses_members_and_skips_malformed_lines() {
        let text = "root:x:0:\nadm:x:4:cadu,syslog\nbroken\n:x:5:\nbad:x:notanumber:\nlast:x:7:a";
        let groups = parse_group(text);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].name, "root");
        assert!(groups[0].members.is_empty());
        assert_eq!(groups[1].members, vec!["cadu", "syslog"]);
        assert_eq!(groups[2].name, "last", "no trailing newline is fine");
        assert_eq!(groups[2].members, vec!["a"]);
    }

    #[test]
    fn lookups_by_name_and_gid() {
        assert_eq!(group_entry(GROUP, "docker").unwrap().gid, 998);
        assert_eq!(group_entry(GROUP, "ghost"), None);
        assert_eq!(group_by_gid(GROUP, 4).unwrap().name, "adm");
        assert_eq!(group_by_gid(GROUP, 5), None);
    }

    #[test]
    fn malformed_line_for_the_name_is_an_error_not_missing() {
        let text = format!("{GROUP}docker2:x:notanumber:\n");
        assert_eq!(lookup_group(&text, "docker").unwrap().unwrap().gid, 998);
        assert_eq!(lookup_group(&text, "ghost").unwrap(), None);
        let err = lookup_group(&text, "docker2").unwrap_err().to_string();
        assert!(err.contains("malformed line for `docker2`"), "{err}");
    }

    #[test]
    fn groups_of_user_are_sorted_supplementary_groups() {
        assert_eq!(groups_of(GROUP, "cadu"), vec!["adm", "docker"]);
        assert_eq!(groups_of(GROUP, "syslog"), vec!["adm"]);
        assert!(groups_of(GROUP, "root").is_empty());
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
            .with_file("/etc/group", GROUP)
            .with_file("/etc/passwd", PASSWD)
    }

    #[test]
    fn tools_probe_binaries_before_the_distro_name() {
        // Nothing on disk: the distro decides.
        let fake = Arc::new(base());
        assert_eq!(Tools::of(&fake_sys(&fake)), Tools::Shadow);
        assert_eq!(Tools::of(&alpine(fake_sys(&fake))), Tools::BusyBox);
        // `apk add shadow` on Alpine: shadow tools win.
        let fake = Arc::new(base().with_file("/usr/sbin/usermod", ""));
        assert_eq!(Tools::of(&alpine(fake_sys(&fake))), Tools::Shadow);
        // A BusyBox-only image of a non-Alpine distro.
        let fake = Arc::new(base().with_file("/bin/busybox", ""));
        assert_eq!(Tools::of(&fake_sys(&fake)), Tools::BusyBox);
    }

    #[test]
    fn present_is_satisfied_when_group_exists() {
        let fake = Arc::new(base());
        let Plan::Satisfied(g) = Present::new("docker").check(&fake_sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(
            g,
            Group {
                name: "docker".into(),
                gid: 998,
                members: vec!["cadu".into()]
            }
        );
        assert!(fake.commands().is_empty());
    }

    #[test]
    fn present_creates_with_groupadd_and_rereads() {
        let fake = Arc::new(base().with_cmd("groupadd", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Present::new("rustible").gid(1500).system(true);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "exists=yes gid=1500");
        assert_eq!(
            c.predicted,
            Some(Group {
                name: "rustible".into(),
                gid: 1500,
                members: vec![]
            })
        );
        assert!(fake.commands().is_empty(), "check runs nothing");

        // The fake cannot run groupadd; stand in for its effect on the file.
        fake.write(
            Path::new("/etc/group"),
            format!("{GROUP}rustible:x:1500:\n").as_bytes(),
        )
        .unwrap();
        let g = op.apply(&sys, c).unwrap();
        assert_eq!(g.gid, 1500);
        assert_eq!(
            fake.argvs(),
            vec![vec!["groupadd", "-r", "-g", "1500", "rustible"]]
        );
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn present_without_gid_does_not_predict() {
        let fake = Arc::new(base());
        let Plan::Change(c) = Present::new("rustible").check(&fake_sys(&fake)).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "exists=yes");
        assert!(c.predicted.is_none(), "gid is unknown until groupadd runs");
    }

    #[test]
    fn present_changes_gid_with_groupmod() {
        let fake = Arc::new(base().with_cmd("groupmod", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Present::new("docker").gid(2000);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "gid=2000");
        assert_eq!(c.predicted.as_ref().unwrap().members, vec!["cadu"]);
        fake.write(
            Path::new("/etc/group"),
            GROUP.replace("docker:x:998:", "docker:x:2000:").as_bytes(),
        )
        .unwrap();
        let g = op.apply(&sys, c).unwrap();
        assert_eq!(g.gid, 2000);
        assert_eq!(fake.argvs(), vec![vec!["groupmod", "-g", "2000", "docker"]]);
    }

    #[test]
    fn present_refuses_a_taken_gid() {
        let fake = Arc::new(base());
        let err = Present::new("rustible")
            .gid(4)
            .check(&fake_sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("gid 4 is already used by group `adm`"),
            "{err}"
        );
    }

    #[test]
    fn present_on_alpine_uses_addgroup_flags() {
        let fake = Arc::new(base().with_cmd("addgroup", None, 0, ""));
        let sys = alpine(fake_sys(&fake));
        let op = Present::new("rustible").gid(1500).system(true);
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!()
        };
        fake.write(
            Path::new("/etc/group"),
            format!("{GROUP}rustible:x:1500:\n").as_bytes(),
        )
        .unwrap();
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.argvs(),
            vec![vec!["addgroup", "-S", "-g", "1500", "rustible"]]
        );
    }

    #[test]
    fn present_on_alpine_cannot_change_gid() {
        let fake = Arc::new(base());
        let sys = alpine(fake_sys(&fake));
        let err = Present::new("docker")
            .gid(2000)
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(err.contains("BusyBox has no `groupmod`"), "{err}");
    }

    #[test]
    fn present_needs_root() {
        let fake = Arc::new(base());
        let err = Present::new("docker")
            .check(&not_root(fake_sys(&fake)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("group::Present needs root"), "{err}");
        assert!(err.contains("`cadu`"), "{err}");
    }

    #[test]
    fn present_missing_binary_names_the_tool_family() {
        // No canned command: the fake refuses to spawn, like a missing binary.
        let fake = Arc::new(base());
        let sys = fake_sys(&fake);
        let op = Present::new("rustible");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!()
        };
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("running `groupadd` (shadow-utils)"), "{err}");
    }

    #[test]
    fn present_rejects_bad_names() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake);
        let err = Present::new("").check(&sys).unwrap_err().to_string();
        assert!(err.contains("name is empty"), "{err}");
        for bad in ["a:b", "a b", "a,b", "-x", "a\nb"] {
            let err = Present::new(bad).check(&sys).unwrap_err().to_string();
            assert!(err.contains("not valid"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn absent_is_satisfied_when_missing() {
        let fake = Arc::new(base());
        let Plan::Satisfied(r) = Absent::new("ghost").check(&fake_sys(&fake)).unwrap() else {
            panic!("expected satisfied")
        };
        assert_eq!(r.gid, None);
    }

    #[test]
    fn absent_removes_with_groupdel() {
        let fake = Arc::new(base().with_cmd("groupdel", None, 0, ""));
        let sys = fake_sys(&fake);
        let op = Absent::new("docker");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!("expected change")
        };
        assert_eq!(c.diff.short(), "exists=no");
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(r.gid, Some(998));
        assert_eq!(fake.argvs(), vec![vec!["groupdel", "docker"]]);
    }

    #[test]
    fn absent_on_alpine_uses_delgroup() {
        let fake = Arc::new(base().with_cmd("delgroup", None, 0, ""));
        let sys = alpine(fake_sys(&fake));
        let op = Absent::new("docker");
        let Plan::Change(c) = op.check(&sys).unwrap() else {
            panic!()
        };
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.argvs(), vec![vec!["delgroup", "docker"]]);
    }

    #[test]
    fn absent_refuses_a_primary_group() {
        let fake = Arc::new(base().with_file("/etc/group", format!("{GROUP}cadu:x:1000:\n")));
        let err = Absent::new("cadu")
            .check(&fake_sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(err.contains("is the primary group of user `cadu`"), "{err}");
    }

    #[test]
    fn absent_needs_root() {
        let fake = Arc::new(base());
        let err = Absent::new("docker")
            .check(&not_root(fake_sys(&fake)))
            .unwrap_err()
            .to_string();
        assert!(err.contains("group::Absent needs root"), "{err}");
    }

    #[test]
    fn a_planned_creation_is_noted_for_later_steps_in_check_mode_only() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake).with_check_mode(true);
        assert!(!sys.would_create("group", "rustible"));
        assert!(Present::new("rustible").check(&sys).unwrap().is_change());
        assert!(sys.would_create("group", "rustible"));
        assert!(sys.would_create_id("group", 5000).is_none());
        assert!(
            Present::new("fixed")
                .gid(5000)
                .check(&sys)
                .unwrap()
                .is_change()
        );
        assert_eq!(sys.would_create_id("group", 5000).unwrap().name, "fixed");
        // An existing group is not "planned".
        assert!(matches!(
            Present::new("docker").check(&sys).unwrap(),
            Plan::Satisfied(_)
        ));
        assert!(!sys.would_create("group", "docker"));
        // Outside check mode the note is inert: a real run never accepts a
        // group that is not on the machine.
        let real = fake_sys(&fake);
        assert!(Present::new("rustible").check(&real).unwrap().is_change());
        assert!(!real.would_create("group", "rustible"));
    }

    #[test]
    fn check_mode_through_ctx_runs_nothing_and_predicts() {
        let fake = Arc::new(base());
        let sys = fake_sys(&fake).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step("group", Present::new("rustible").gid(1500))
            .unwrap();
        assert!(r.changed && r.predicted && r.is_available());
        assert_eq!(r.gid, 1500);
        let r = ctx.step("group", Present::new("nogid")).unwrap();
        assert!(r.changed && !r.is_available());
        assert!(fake.commands().is_empty());
        assert_eq!(fake.content("/etc/group").unwrap(), GROUP);
    }
}
