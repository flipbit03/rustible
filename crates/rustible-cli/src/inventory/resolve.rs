//! Precedence (vision 10.2.1 for parameters, 10.3 for vars) with the
//! source of every value kept for `inventory show`.
//!
//! Parameters: host, nearest group outward, `defaults`, built-in default.
//! Vars: top-level `vars`, groups outermost to innermost, host. Group
//! distance is the shortest path from the host: the group it is nested in
//! and every group that lists it in `members` are at distance 1; a group
//! listing a distance-d group is at d+1. Two groups at the same distance
//! that both set a key the host (or a nearer group) does not override is a
//! conflict and a load-time error.

use std::collections::BTreeMap;
use std::fmt;

use super::error::UnknownName;
use super::model::{Connection, Escalate, HostParams, Inventory, Scalar, VarBag};

/// Where a resolved value came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Top-level `vars`.
    All,
    Group(String),
    Host,
    Defaults,
    BuiltIn,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Source::All => f.write_str("all"),
            Source::Group(g) => write!(f, "group {g}"),
            Source::Host => f.write_str("host"),
            Source::Defaults => f.write_str("defaults"),
            Source::BuiltIn => f.write_str("built-in"),
        }
    }
}

/// The seven parameters with every gap filled.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ResolvedParams {
    /// `None` only when `connection` is `local`.
    pub addr: Option<String>,
    pub connection: Connection,
    pub ssh_user: String,
    pub port: u16,
    pub escalate: Escalate,
    pub escalate_user: String,
    pub ssh_args: Vec<String>,
}

impl ResolvedParams {
    /// Each parameter rendered the way `inventory show` prints it, in
    /// [`HostParams::NAMES`] order.
    pub fn rendered(&self) -> [(&'static str, String); 7] {
        [
            (
                "addr",
                self.addr
                    .as_ref()
                    .map(|a| format!("{a:?}"))
                    .unwrap_or_else(|| "-".into()),
            ),
            ("connection", self.connection.as_str().into()),
            ("ssh_user", format!("{:?}", self.ssh_user)),
            ("port", self.port.to_string()),
            ("escalate", self.escalate.as_str().into()),
            ("escalate_user", format!("{:?}", self.escalate_user)),
            ("ssh_args", format!("{:?}", self.ssh_args)),
        ]
    }
}

/// A value that lost to a nearer one.
#[derive(Debug, Clone, PartialEq)]
pub struct Overridden {
    pub key: String,
    /// Rendered, so parameters and vars share the type.
    pub value: String,
    pub source: Source,
}

/// Provenance of everything in a [`Resolved`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sources {
    pub params: BTreeMap<&'static str, Source>,
    pub vars: BTreeMap<String, Source>,
    pub overridden_params: Vec<Overridden>,
    pub overridden_vars: Vec<Overridden>,
}

/// One host, fully resolved.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    pub host: String,
    /// Every group the host belongs to, nearest first (what
    /// `HostInfo::groups` carries).
    pub groups: Vec<String>,
    pub params: ResolvedParams,
    pub vars: VarBag,
    pub sources: Sources,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    Var,
    Param,
}

impl fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ConflictKind::Var => "var",
            ConflictKind::Param => "parameter",
        })
    }
}

/// Two sibling groups set the same key for a host with no nearer override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub host: String,
    pub kind: ConflictKind,
    pub key: String,
    pub groups: (String, String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    UnknownHost(UnknownName),
    Conflicts(Vec<Conflict>),
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResolveError::UnknownHost(u) => write!(f, "{u}"),
            ResolveError::Conflicts(cs) => {
                for (i, c) in cs.iter().enumerate() {
                    if i > 0 {
                        f.write_str("\n")?;
                    }
                    f.write_str(&super::parse::conflict_message(c))?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ResolveError {}

impl Inventory {
    /// Host and group names in file order.
    pub fn names(&self) -> &[String] {
        &self.order
    }

    /// Host names in file order.
    pub fn host_names(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|n| self.hosts.contains_key(*n))
            .cloned()
            .collect()
    }

    /// Group names in file order.
    pub fn group_names(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|n| self.groups.contains_key(*n))
            .cloned()
            .collect()
    }

    fn file_order(&self, names: &mut [String]) {
        names.sort_by_key(|n| self.order.iter().position(|o| o == n));
    }

    /// Groups by distance: `levels[0]` is distance 1. Each level in file
    /// order. Empty for an unknown host.
    pub fn group_levels(&self, host: &str) -> Vec<Vec<String>> {
        let Some(h) = self.hosts.get(host) else {
            return vec![];
        };
        let mut seen: Vec<String> = vec![];
        let mut levels: Vec<Vec<String>> = vec![];
        let mut frontier: Vec<String> = h.group.iter().cloned().collect();
        frontier.extend(
            self.groups
                .values()
                .filter(|g| g.members.iter().any(|m| m == host))
                .map(|g| g.name.clone()),
        );
        loop {
            frontier.sort();
            frontier.dedup();
            frontier.retain(|g| !seen.contains(g));
            if frontier.is_empty() {
                break;
            }
            self.file_order(&mut frontier);
            seen.extend(frontier.iter().cloned());
            let next: Vec<String> = self
                .groups
                .values()
                .filter(|g| g.members.iter().any(|m| frontier.contains(m)))
                .map(|g| g.name.clone())
                .collect();
            levels.push(std::mem::replace(&mut frontier, next));
        }
        levels
    }

    /// Every group of a host, nearest first (transitive closure).
    pub fn groups_of(&self, host: &str) -> Vec<String> {
        self.group_levels(host).into_iter().flatten().collect()
    }

    /// The hosts a playbook's `hosts = "..."` names: those of a group (in
    /// file order), or the one host.
    pub fn select(&self, name: &str) -> Result<Vec<&super::model::Host>, UnknownName> {
        if let Some(h) = self.hosts.get(name) {
            return Ok(vec![h]);
        }
        if self.groups.contains_key(name) {
            return Ok(self
                .host_names()
                .iter()
                .filter(|h| self.groups_of(h).iter().any(|g| g == name))
                .map(|h| &self.hosts[h])
                .collect());
        }
        Err(UnknownName {
            name: name.to_string(),
            suggestion: rustible_sdk::vars::did_you_mean(
                name,
                self.order.iter().map(String::as_str),
            )
            .map(str::to_string),
        })
    }

    /// One parameter through the precedence chain, with its source. `None`
    /// when nothing sets it (caller applies the built-in default). Ties
    /// between sibling groups are not checked here; `resolve` does that.
    pub(crate) fn resolve_param<T: Clone>(
        &self,
        host: &str,
        get: impl Fn(&HostParams) -> Option<T>,
    ) -> Option<(T, Source)> {
        let h = self.hosts.get(host)?;
        if let Some(v) = get(&h.params) {
            return Some((v, Source::Host));
        }
        for level in self.group_levels(host) {
            for g in level {
                if let Some(v) = get(&self.groups[&g].params) {
                    return Some((v, Source::Group(g)));
                }
            }
        }
        get(&self.defaults).map(|v| (v, Source::Defaults))
    }

    /// Resolve a host. After a successful load conflicts cannot occur, so
    /// the only error is an unknown host name.
    pub fn resolve(&self, host: &str) -> Result<Resolved, ResolveError> {
        if !self.hosts.contains_key(host) {
            return Err(ResolveError::UnknownHost(UnknownName {
                name: host.to_string(),
                suggestion: rustible_sdk::vars::did_you_mean(
                    host,
                    self.hosts.keys().map(String::as_str),
                )
                .map(str::to_string),
            }));
        }
        self.resolve_checked(host).map_err(ResolveError::Conflicts)
    }

    /// Resolve a known host, reporting sibling conflicts.
    pub(crate) fn resolve_checked(&self, host: &str) -> Result<Resolved, Vec<Conflict>> {
        let h = &self.hosts[host];
        let levels = self.group_levels(host);
        let mut conflicts = vec![];
        let mut sources = Sources::default();

        // Vars: all, then outermost level to innermost, then host. Track
        // what each level contributed so the conflict check can ask "is
        // this key set nearer?".
        let mut vars = VarBag::new();
        let put =
            |key: &str, value: &Scalar, src: Source, vars: &mut VarBag, sources: &mut Sources| {
                if let Some(old) = vars.insert(key.to_string(), value.clone()) {
                    let old_src = sources
                        .vars
                        .insert(key.to_string(), src)
                        .expect("source tracked");
                    sources.overridden_vars.push(Overridden {
                        key: key.to_string(),
                        value: old.to_string(),
                        source: old_src,
                    });
                } else {
                    sources.vars.insert(key.to_string(), src);
                }
            };
        for (k, v) in &self.vars {
            put(k, v, Source::All, &mut vars, &mut sources);
        }
        for (i, level) in levels.iter().enumerate().rev() {
            let nearer: Vec<&str> = levels[..i].iter().flatten().map(String::as_str).collect();
            let set_nearer = |key: &str| {
                h.vars.contains_key(key)
                    || nearer
                        .iter()
                        .any(|g| self.groups[*g].vars.contains_key(key))
            };
            let set_nearer_param = |get: &dyn Fn(&HostParams) -> bool| {
                get(&h.params) || nearer.iter().any(|g| get(&self.groups[*g].params))
            };
            for (n, g) in level.iter().enumerate() {
                let grp = &self.groups[g];
                for (k, v) in &grp.vars {
                    // Same-distance sibling also sets it, nothing nearer does.
                    if let Some(other) = level[..n]
                        .iter()
                        .find(|o| self.groups[*o].vars.contains_key(k))
                    {
                        if !set_nearer(k) {
                            conflicts.push(Conflict {
                                host: host.to_string(),
                                kind: ConflictKind::Var,
                                key: k.clone(),
                                groups: (other.clone(), g.clone()),
                            });
                        }
                        continue;
                    }
                    put(k, v, Source::Group(g.clone()), &mut vars, &mut sources);
                }
                for name in HostParams::NAMES {
                    let is_set = param_is_set(name);
                    if !is_set(&grp.params) {
                        continue;
                    }
                    if let Some(other) = level[..n].iter().find(|o| is_set(&self.groups[*o].params))
                        && !set_nearer_param(&is_set)
                    {
                        conflicts.push(Conflict {
                            host: host.to_string(),
                            kind: ConflictKind::Param,
                            key: name.to_string(),
                            groups: (other.clone(), g.clone()),
                        });
                    }
                }
            }
        }
        for (k, v) in &h.vars {
            put(k, v, Source::Host, &mut vars, &mut sources);
        }
        if !conflicts.is_empty() {
            return Err(conflicts);
        }

        // Parameters: host, nearest group outward, defaults, built-in.
        let mut sources_params = BTreeMap::new();
        let mut overridden_params = vec![];
        macro_rules! param {
            ($name:literal, $get:expr, $builtin:expr, $render:expr) => {{
                let get = $get;
                let mut chain: Vec<(_, Source)> = vec![];
                if let Some(v) = get(&h.params) {
                    chain.push((v, Source::Host));
                }
                for level in &levels {
                    for g in level {
                        if let Some(v) = get(&self.groups[g].params) {
                            chain.push((v, Source::Group(g.clone())));
                        }
                    }
                }
                if let Some(v) = get(&self.defaults) {
                    chain.push((v, Source::Defaults));
                }
                let mut it = chain.into_iter();
                let (value, src) = it.next().unwrap_or_else(|| ($builtin, Source::BuiltIn));
                for (v, s) in it {
                    overridden_params.push(Overridden {
                        key: $name.to_string(),
                        value: ($render)(&v),
                        source: s,
                    });
                }
                sources_params.insert($name, src);
                value
            }};
        }
        let quoted = |s: &String| format!("{s:?}");
        let connection = param!(
            "connection",
            |p: &HostParams| p.connection,
            Connection::Ssh,
            |c: &Connection| c.as_str().to_string()
        );
        let addr = param!(
            "addr",
            |p: &HostParams| p.addr.clone().map(Some),
            None::<String>,
            |a: &Option<String>| a.as_deref().map(|s| format!("{s:?}")).unwrap_or_default()
        );
        let params = ResolvedParams {
            addr,
            connection,
            ssh_user: param!(
                "ssh_user",
                |p: &HostParams| p.ssh_user.clone(),
                local_username(),
                quoted
            ),
            port: param!("port", |p: &HostParams| p.port, 22, |p: &u16| p.to_string()),
            escalate: param!(
                "escalate",
                |p: &HostParams| p.escalate,
                Escalate::Sudo,
                |e: &Escalate| e.as_str().to_string()
            ),
            escalate_user: param!(
                "escalate_user",
                |p: &HostParams| p.escalate_user.clone(),
                "root".to_string(),
                quoted
            ),
            ssh_args: param!(
                "ssh_args",
                |p: &HostParams| p.ssh_args.clone(),
                vec![],
                |a: &Vec<String>| format!("{a:?}")
            ),
        };
        sources.params = sources_params;
        sources.overridden_params = overridden_params;

        Ok(Resolved {
            host: host.to_string(),
            groups: levels.into_iter().flatten().collect(),
            params,
            vars,
            sources,
        })
    }
}

fn param_is_set(name: &str) -> impl Fn(&HostParams) -> bool {
    move |p: &HostParams| match name {
        "addr" => p.addr.is_some(),
        "connection" => p.connection.is_some(),
        "ssh_user" => p.ssh_user.is_some(),
        "port" => p.port.is_some(),
        "escalate" => p.escalate.is_some(),
        "escalate_user" => p.escalate_user.is_some(),
        "ssh_args" => p.ssh_args.is_some(),
        _ => false,
    }
}

/// The built-in `ssh_user`: the local username (vision 10.2.1).
pub fn local_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".to_string())
}
