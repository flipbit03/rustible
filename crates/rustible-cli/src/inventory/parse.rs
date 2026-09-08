//! `hosts.kdl` (KDL 2.0) into the model, collecting every error.
//!
//! Nodes: top-level `vars`, `defaults`, `host`, `group`; inside `group`:
//! `vars`, `members`, `host`; inside `host` and `group`: `vars`, `ssh_args`.
//! Slash-dash (`/-node`) is handled by the `kdl` crate: a disabled node never
//! reaches this code.

use std::collections::BTreeMap;

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use rustible_sdk::vars::did_you_mean;

use super::error::{LineIndex, LoadError, LoadErrors};
use super::model::{Connection, Escalate, Group, Host, HostParams, Inventory, Scalar, VarBag};
use super::resolve::Conflict;

/// Parse `src` (read from `file`, which is only used in messages).
pub fn parse(src: &str, file: &str) -> Result<Inventory, LoadErrors> {
    let index = LineIndex::new(src);
    let mut p = Parser {
        file,
        index: &index,
        errors: vec![],
        inv: Inventory::default(),
        defined_at: BTreeMap::new(),
        seen_top_vars: None,
        seen_defaults: None,
        bad_addr: vec![],
    };
    match KdlDocument::parse_v2(src) {
        Ok(doc) => {
            for node in doc.nodes() {
                p.top_level(node);
            }
            p.check_members();
            p.check_cycles();
            p.check_addr();
            p.check_conflicts();
        }
        Err(e) => {
            for d in &e.diagnostics {
                let mut msg = d
                    .message
                    .clone()
                    .unwrap_or_else(|| "invalid KDL".to_string());
                if let Some(label) = &d.label
                    && label != "here"
                {
                    msg = format!("{msg} ({label})");
                }
                if let Some(help) = &d.help {
                    msg = format!("{msg}; {help}");
                }
                p.err(d.span.offset(), format!("syntax: {msg}"));
            }
        }
    }
    p.finish()
}

struct Parser<'a> {
    file: &'a str,
    index: &'a LineIndex<'a>,
    /// `(offset, error)`, sorted before returning.
    errors: Vec<(usize, LoadError)>,
    inv: Inventory,
    /// Byte offset of every host and group definition, for "first at line".
    defined_at: BTreeMap<String, usize>,
    seen_top_vars: Option<usize>,
    seen_defaults: Option<usize>,
    /// Hosts whose `addr` was given but rejected; `check_addr` skips them.
    bad_addr: Vec<String>,
}

/// Where a `vars` block or a parameter lives, for messages.
#[derive(Clone, Copy)]
enum Owner<'a> {
    Host(&'a str),
    Group(&'a str),
    Defaults,
    All,
}

impl std::fmt::Display for Owner<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Owner::Host(n) => write!(f, "host `{n}`"),
            Owner::Group(n) => write!(f, "group `{n}`"),
            Owner::Defaults => f.write_str("`defaults`"),
            Owner::All => f.write_str("top-level `vars`"),
        }
    }
}

const TOP_LEVEL: [&str; 4] = ["vars", "defaults", "host", "group"];
const IN_HOST: [&str; 2] = ["vars", "ssh_args"];
const IN_GROUP: [&str; 4] = ["vars", "members", "host", "ssh_args"];

impl<'a> Parser<'a> {
    fn err(&mut self, offset: usize, message: impl Into<String>) {
        let (line, column) = self.index.position(offset);
        self.errors.push((
            offset,
            LoadError {
                file: self.file.to_string(),
                line,
                column,
                message: message.into(),
            },
        ));
    }

    fn line_of(&self, offset: usize) -> usize {
        self.index.position(offset).0
    }

    fn finish(mut self) -> Result<Inventory, LoadErrors> {
        if self.errors.is_empty() {
            Ok(self.inv)
        } else {
            self.errors.sort_by_key(|(o, _)| *o);
            Err(LoadErrors(
                self.errors.into_iter().map(|(_, e)| e).collect(),
            ))
        }
    }

    fn top_level(&mut self, node: &KdlNode) {
        let at = node.name().span().offset();
        match node.name().value() {
            "vars" => {
                if let Some(first) = self.seen_top_vars {
                    let line = self.line_of(first);
                    self.err(
                        at,
                        format!("top-level `vars` is defined twice (first at line {line})"),
                    );
                    return;
                }
                self.seen_top_vars = Some(at);
                let bag = self.vars_block(node, Owner::All);
                self.inv.vars = bag;
            }
            "defaults" => {
                if let Some(first) = self.seen_defaults {
                    let line = self.line_of(first);
                    self.err(
                        at,
                        format!("`defaults` is defined twice (first at line {line})"),
                    );
                    return;
                }
                self.seen_defaults = Some(at);
                let (params, _) = self.params_and_children(node, Owner::Defaults, &["ssh_args"]);
                self.inv.defaults = params;
            }
            "host" => self.host(node, None),
            "group" => self.group(node),
            other => self.unknown_node(at, other, "at top level", &TOP_LEVEL),
        }
    }

    fn unknown_node(&mut self, at: usize, name: &str, place: &str, allowed: &[&str]) {
        let msg = match did_you_mean(name, allowed.iter().copied()) {
            Some(s) => format!("unknown node `{name}` {place}; did you mean `{s}`?"),
            None => format!(
                "unknown node `{name}` {place}; expected one of {}",
                allowed.join(", ")
            ),
        };
        self.err(at, msg);
    }

    /// The single positional string argument that names a host or group.
    fn name_arg(&mut self, node: &KdlNode, kind: &str) -> Option<String> {
        let at = node.name().span().offset();
        let mut name = None;
        for e in node.entries().iter().filter(|e| e.name().is_none()) {
            match (&name, e.value()) {
                (None, KdlValue::String(s)) => name = Some(s.clone()),
                (None, other) => {
                    self.err(
                        e.span().offset(),
                        format!("{kind} name must be a string, got {}", render(other)),
                    );
                    return None;
                }
                (Some(_), other) => self.err(
                    e.span().offset(),
                    format!(
                        "{kind} takes one name; unexpected extra argument {}",
                        render(other)
                    ),
                ),
            }
        }
        if name.is_none() {
            self.err(at, format!("`{kind}` needs a name: {kind} \"name\" ..."));
        }
        name
    }

    /// Register a host or group name; false if it clashes.
    fn define(&mut self, kind: &str, name: &str, at: usize) -> bool {
        if let Some(&first) = self.defined_at.get(name) {
            let line = self.line_of(first);
            let first_kind = if self.inv.hosts.contains_key(name) {
                "host"
            } else {
                "group"
            };
            let msg = if first_kind == kind {
                format!("{kind} `{name}` is defined twice (first at line {line})")
            } else {
                format!(
                    "{kind} `{name}` clashes with the {first_kind} of the same name (line {line}); names are unique across hosts and groups"
                )
            };
            self.err(at, msg);
            return false;
        }
        self.defined_at.insert(name.to_string(), at);
        self.inv.order.push(name.to_string());
        true
    }

    fn host(&mut self, node: &KdlNode, group: Option<&str>) {
        let at = node.name().span().offset();
        let Some(name) = self.name_arg(node, "host") else {
            return;
        };
        let owner = Owner::Host(&name);
        let (params, vars) = self.params_and_children(node, owner, &IN_HOST);
        if !self.define("host", &name, at) {
            return;
        }
        if let Some(g) = group
            && let Some(grp) = self.inv.groups.get_mut(g)
        {
            grp.hosts.push(name.clone());
        }
        self.inv.hosts.insert(
            name.clone(),
            Host {
                name,
                params,
                vars,
                group: group.map(str::to_string),
            },
        );
    }

    fn group(&mut self, node: &KdlNode) {
        let at = node.name().span().offset();
        let Some(name) = self.name_arg(node, "group") else {
            return;
        };
        let owner = Owner::Group(&name);
        let (params, vars) = self.params_and_children(node, owner, &IN_GROUP);
        if !self.define("group", &name, at) {
            return;
        }
        self.inv.groups.insert(
            name.clone(),
            Group {
                name: name.clone(),
                params,
                vars,
                members: vec![],
                hosts: vec![],
            },
        );
        // Second pass over children for what needs the group registered.
        for child in node.iter_children() {
            let cat = child.name().span().offset();
            match child.name().value() {
                "members" => {
                    let mut members = vec![];
                    for e in child.entries() {
                        match (e.name(), e.value()) {
                            (None, KdlValue::String(s)) => members.push(s.clone()),
                            (None, other) => self.err(
                                e.span().offset(),
                                format!("members of group `{name}` must be strings, got {}", render(other)),
                            ),
                            (Some(k), _) => self.err(
                                e.span().offset(),
                                format!(
                                    "`members` in group `{name}` takes names, not properties (`{}=`)",
                                    k.value()
                                ),
                            ),
                        }
                    }
                    if child.children().is_some() {
                        self.err(
                            cat,
                            format!("`members` in group `{name}` takes names, not a block"),
                        );
                    }
                    if let Some(g) = self.inv.groups.get_mut(&name) {
                        g.members.extend(members);
                    }
                }
                "host" => self.host(child, Some(&name)),
                "group" => {
                    let inner = child
                        .entries()
                        .iter()
                        .find(|e| e.name().is_none())
                        .and_then(|e| e.value().as_string())
                        .map(|s| format!("group `{s}`"))
                        .unwrap_or_else(|| "a group".to_string());
                    self.err(
                        cat,
                        format!(
                            "{inner} inside group `{name}`: groups nest through `members`, not by placing a group inside a group"
                        ),
                    );
                }
                _ => {} // vars, ssh_args, unknown: handled in params_and_children
            }
        }
    }

    /// Properties on the node become parameters; the `vars` child becomes
    /// the bag; `ssh_args` child is the list form of that parameter. Any
    /// child not in `allowed` is an error (`members`, `host`, `group` are
    /// handled by the caller and only checked here).
    fn params_and_children(
        &mut self,
        node: &KdlNode,
        owner: Owner<'_>,
        allowed: &[&str],
    ) -> (HostParams, VarBag) {
        let mut params = HostParams::default();
        let mut set: Vec<&str> = vec![];
        for e in node.entries() {
            let Some(key) = e.name() else {
                if matches!(owner, Owner::Defaults) {
                    self.err(
                        e.span().offset(),
                        format!(
                            "`defaults` takes parameters (key=value), not arguments; got {}",
                            render(e.value())
                        ),
                    );
                }
                continue; // host/group names handled by name_arg
            };
            let key = key.value();
            if set.contains(&key) {
                self.err(
                    e.span().offset(),
                    format!("parameter `{key}` is set twice on {owner}"),
                );
                continue;
            }
            if self.param(&mut params, owner, key, e) {
                set.push(key);
            }
        }
        let mut vars = VarBag::new();
        let mut seen_vars = false;
        for child in node.iter_children() {
            let at = child.name().span().offset();
            match child.name().value() {
                "vars" if allowed.contains(&"vars") => {
                    if seen_vars {
                        self.err(at, format!("`vars` is defined twice on {owner}"));
                        continue;
                    }
                    seen_vars = true;
                    vars = self.vars_block(child, owner);
                }
                "ssh_args" if allowed.contains(&"ssh_args") => {
                    if set.contains(&"ssh_args") {
                        self.err(at, format!("parameter `ssh_args` is set twice on {owner}"));
                        continue;
                    }
                    set.push("ssh_args");
                    let mut args = vec![];
                    for e in child.entries() {
                        match (e.name(), e.value()) {
                            (None, KdlValue::String(s)) => args.push(s.clone()),
                            _ => self.err(
                                e.span().offset(),
                                format!(
                                    "`ssh_args` on {owner} takes strings, got {}",
                                    render(e.value())
                                ),
                            ),
                        }
                    }
                    params.ssh_args = Some(args);
                }
                "members" | "host" if allowed.contains(&child.name().value()) => {}
                "group" if matches!(owner, Owner::Group(_)) => {} // reported by `group`
                other => {
                    let place = format!("in {owner}");
                    self.unknown_node(at, other, &place, allowed);
                }
            }
        }
        (params, vars)
    }

    /// One `key=value` property. Returns whether it was accepted.
    fn param(
        &mut self,
        params: &mut HostParams,
        owner: Owner<'_>,
        key: &str,
        e: &KdlEntry,
    ) -> bool {
        let at = e.span().offset();
        let v = e.value();
        macro_rules! want_string {
            ($what:expr) => {
                match v {
                    KdlValue::String(s) => s.clone(),
                    other => {
                        self.err(
                            at,
                            format!(
                                "parameter `{key}` on {owner} must be {}, got {}",
                                $what,
                                render(other)
                            ),
                        );
                        return false;
                    }
                }
            };
        }
        match key {
            "addr" => {
                match owner {
                    Owner::Group(_) => {
                        self.err(
                            at,
                            format!(
                                "`addr` is not allowed on {owner}; addr is a host-only parameter"
                            ),
                        );
                        return false;
                    }
                    Owner::Defaults => {
                        self.err(
                            at,
                            "`addr` is not allowed on `defaults`; addr is set on each host",
                        );
                        return false;
                    }
                    _ => {}
                }
                match v {
                    KdlValue::String(s) => params.addr = Some(s.clone()),
                    other => {
                        if let Owner::Host(h) = owner {
                            self.bad_addr.push(h.to_string());
                        }
                        self.err(
                            at,
                            format!(
                                "parameter `addr` on {owner} must be a string, got {}",
                                render(other)
                            ),
                        );
                        return false;
                    }
                }
            }
            "connection" => {
                let s = want_string!("\"ssh\" or \"local\"");
                match Connection::parse(&s) {
                    Some(c) => params.connection = Some(c),
                    None => {
                        self.err(at, format!("parameter `connection` on {owner} must be \"ssh\" or \"local\", got {s:?}"));
                        return false;
                    }
                }
            }
            "ssh_user" => params.ssh_user = Some(want_string!("a string")),
            "escalate_user" => params.escalate_user = Some(want_string!("a string")),
            "port" => match v {
                KdlValue::Integer(i) => match u16::try_from(*i) {
                    Ok(p) => params.port = Some(p),
                    Err(_) => {
                        self.err(at, format!("parameter `port` on {owner} must be an integer from 0 to 65535, got {i}"));
                        return false;
                    }
                },
                other => {
                    self.err(
                        at,
                        format!(
                            "parameter `port` on {owner} must be an integer, got {}",
                            render(other)
                        ),
                    );
                    return false;
                }
            },
            "escalate" => {
                let s = want_string!("\"sudo\", \"doas\", or \"none\"");
                match Escalate::parse(&s) {
                    Some(m) => params.escalate = Some(m),
                    None => {
                        self.err(at, format!("parameter `escalate` on {owner} must be \"sudo\", \"doas\", or \"none\", got {s:?}"));
                        return false;
                    }
                }
            }
            "ssh_args" => {
                // Property form holds one argument; the list form is the
                // `ssh_args "a" "b"` child node.
                let s = want_string!(
                    "a string (for several, use the child node `ssh_args \"-o\" \"...\"`)"
                );
                params.ssh_args = Some(vec![s]);
            }
            other => {
                let msg = match did_you_mean(other, HostParams::NAMES.iter().copied()) {
                    Some(s) => {
                        format!("unknown parameter `{other}` on {owner}; did you mean `{s}`?")
                    }
                    None => format!(
                        "unknown parameter `{other}` on {owner}; parameters are {}",
                        HostParams::NAMES.join(", ")
                    ),
                };
                self.err(at, msg);
                return false;
            }
        }
        true
    }

    /// A `vars` node in either form: `vars a=1 b="x"` and/or
    /// `vars { a 1; ports 22 80 }`.
    fn vars_block(&mut self, node: &KdlNode, owner: Owner<'_>) -> VarBag {
        let mut bag = VarBag::new();
        for e in node.entries() {
            let at = e.span().offset();
            let Some(key) = e.name() else {
                self.err(
                    at,
                    format!(
                        "`vars` on {owner} takes key=value properties or a block, got bare {}",
                        render(e.value())
                    ),
                );
                continue;
            };
            let key = key.value().to_string();
            let Some(value) = self.scalar(at, &key, owner, e.value()) else {
                continue;
            };
            self.put_var(&mut bag, owner, key, value, at);
        }
        for child in node.iter_children() {
            let at = child.name().span().offset();
            let key = child.name().value().to_string();
            if child.children().is_some() {
                self.err(
                    at,
                    format!(
                        "var `{key}` on {owner} has a block; vars are scalars or lists, not nested"
                    ),
                );
                continue;
            }
            let mut items = vec![];
            let mut ok = true;
            for e in child.entries() {
                if let Some(prop) = e.name() {
                    self.err(
                        e.span().offset(),
                        format!("var `{key}` on {owner} takes values, not properties (`{}=`); write `{key} 1 2 3`", prop.value()),
                    );
                    ok = false;
                    continue;
                }
                match self.scalar(e.span().offset(), &key, owner, e.value()) {
                    Some(v) => items.push(v),
                    None => ok = false,
                }
            }
            if !ok {
                continue;
            }
            let value = match items.len() {
                0 => {
                    self.err(at, format!("var `{key}` on {owner} has no value"));
                    continue;
                }
                1 => items.pop().expect("one item"),
                _ => Scalar::List(items),
            };
            self.put_var(&mut bag, owner, key, value, at);
        }
        bag
    }

    fn put_var(
        &mut self,
        bag: &mut VarBag,
        owner: Owner<'_>,
        key: String,
        value: Scalar,
        at: usize,
    ) {
        match bag.entry(key) {
            std::collections::btree_map::Entry::Occupied(e) => {
                let key = e.key().clone();
                self.err(at, format!("var `{key}` is set twice on {owner}"));
            }
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(value);
            }
        }
    }

    fn scalar(&mut self, at: usize, key: &str, owner: Owner<'_>, v: &KdlValue) -> Option<Scalar> {
        match v {
            KdlValue::String(s) => Some(Scalar::Str(s.clone())),
            KdlValue::Integer(i) => match i64::try_from(*i) {
                Ok(i) => Some(Scalar::Int(i)),
                Err(_) => {
                    self.err(
                        at,
                        format!("var `{key}` on {owner}: integer {i} does not fit in 64 bits"),
                    );
                    None
                }
            },
            KdlValue::Float(f) => Some(Scalar::Float(*f)),
            KdlValue::Bool(b) => Some(Scalar::Bool(*b)),
            KdlValue::Null => {
                self.err(at, format!("var `{key}` on {owner} is #null; leave a var out instead of setting it to null"));
                None
            }
        }
    }

    // --- post-parse checks -------------------------------------------------

    fn check_members(&mut self) {
        let names: Vec<String> = self.defined_at.keys().cloned().collect();
        let mut errs = vec![];
        for g in self.inv.groups.values() {
            let at = self.defined_at[&g.name];
            for m in &g.members {
                if m == &g.name {
                    errs.push((at, format!("group `{}` lists itself in `members`", g.name)));
                } else if !self.defined_at.contains_key(m) {
                    let msg = match did_you_mean(m, names.iter().map(String::as_str)) {
                        Some(s) => format!(
                            "group `{}`: member `{m}` is not a host or group; did you mean `{s}`?",
                            g.name
                        ),
                        None => format!("group `{}`: member `{m}` is not a host or group", g.name),
                    };
                    errs.push((at, msg));
                }
            }
        }
        for (at, msg) in errs {
            self.err(at, msg);
        }
    }

    /// Membership cycles through `members` (group -> group). Each cycle is
    /// reported once, on the first group of the cycle in file order.
    fn check_cycles(&mut self) {
        let mut reported: Vec<Vec<String>> = vec![];
        let mut errs = vec![];
        for start in self.inv.order.clone() {
            if !self.inv.groups.contains_key(&start) {
                continue;
            }
            let mut stack = vec![start.clone()];
            if let Some(cycle) = self.find_cycle(&start, &mut stack) {
                let mut key = cycle.clone();
                key.sort();
                key.dedup();
                if reported.contains(&key) {
                    continue;
                }
                reported.push(key);
                let at = self.defined_at[&start];
                errs.push((
                    at,
                    format!(
                        "group `{start}` is a member of itself through {}",
                        cycle.join(" -> ")
                    ),
                ));
            }
        }
        for (at, msg) in errs {
            self.err(at, msg);
        }
    }

    fn find_cycle(&self, node: &str, stack: &mut Vec<String>) -> Option<Vec<String>> {
        let g = self.inv.groups.get(node)?;
        for m in &g.members {
            if !self.inv.groups.contains_key(m) || m == node {
                continue; // a group listing itself is reported by check_members
            }
            if stack.contains(m) {
                if m == &stack[0] {
                    let mut cycle = stack.clone();
                    cycle.push(m.clone());
                    return Some(cycle);
                }
                continue; // a cycle not through `start`; reported from its own start
            }
            stack.push(m.clone());
            if let Some(c) = self.find_cycle(m, stack) {
                return Some(c);
            }
            stack.pop();
        }
        None
    }

    /// A host reached over ssh needs an `addr`. Connection may come from a
    /// group or `defaults`, so this runs after everything is parsed.
    fn check_addr(&mut self) {
        let mut errs = vec![];
        for h in self.inv.hosts.values() {
            if h.params.addr.is_some() || self.bad_addr.contains(&h.name) {
                continue;
            }
            let connection = self
                .inv
                .resolve_param(&h.name, |p| p.connection)
                .map(|(c, _)| c)
                .unwrap_or(Connection::Ssh);
            if connection == Connection::Ssh {
                errs.push((
                    self.defined_at[&h.name],
                    format!(
                        "host `{}` has no `addr` and its connection is ssh; add addr=\"...\" or connection=\"local\"",
                        h.name
                    ),
                ));
            }
        }
        for (at, msg) in errs {
            self.err(at, msg);
        }
    }

    /// Sibling-group conflicts (vision 10.3), for every host.
    fn check_conflicts(&mut self) {
        if !self.errors.is_empty() {
            // Unknown members or cycles make distances meaningless.
            return;
        }
        let mut errs = vec![];
        for name in self.inv.host_names() {
            if let Err(conflicts) = self.inv.resolve_checked(&name) {
                for c in conflicts {
                    errs.push((self.defined_at[&name], conflict_message(&c)));
                }
            }
        }
        for (at, msg) in errs {
            self.err(at, msg);
        }
    }
}

pub(crate) fn conflict_message(c: &Conflict) -> String {
    format!(
        "host `{}`: {} `{}` is defined by both group `{}` and group `{}` at the same distance; set it on host `{}` or on a common parent group",
        c.host, c.kind, c.key, c.groups.0, c.groups.1, c.host
    )
}

/// A KDL value as the user wrote it, for messages.
fn render(v: &KdlValue) -> String {
    match v {
        KdlValue::String(s) => format!("{s:?}"),
        KdlValue::Integer(i) => i.to_string(),
        KdlValue::Float(f) => format!("{f:?}"),
        KdlValue::Bool(true) => "#true".into(),
        KdlValue::Bool(false) => "#false".into(),
        KdlValue::Null => "#null".into(),
    }
}
