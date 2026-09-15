//! The inventory data model (vision 10.1, 10.2.1, 10.3): hosts, groups,
//! typed connection parameters, and the untyped var bag.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// One inventory value. The file format has typed scalars, so the bag keeps
/// them typed (vision 10.3): no string round-trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    /// A KDL string, quoted or raw. The quotes are not kept.
    Str(String),
    /// A KDL integer, the usual case.
    Int(i64),
    /// Only for values above `i64::MAX`.
    UInt(u64),
    /// A KDL number with a decimal point or an exponent. `#inf` and `#nan`
    /// are rejected while parsing, so this is always finite.
    Float(f64),
    /// `#true` or `#false`.
    Bool(bool),
    /// The child-node form with two or more values, `ports 22 80`, or with
    /// none at all, `ports`, which is the empty list. `ports 22` is an
    /// [`Int`](Scalar::Int) instead, and only becomes a one-element list if
    /// the playbook's schema asks for a list. Lists do not nest: a var
    /// written with a block is a load error.
    List(Vec<Scalar>),
}

impl Scalar {
    /// The JSON the playbook's `Start` frame carries and the schema check
    /// runs against.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Scalar::Str(s) => serde_json::Value::String(s.clone()),
            Scalar::Int(i) => serde_json::Value::from(*i),
            Scalar::UInt(u) => serde_json::Value::from(*u),
            Scalar::Float(f) => serde_json::Value::from(*f),
            Scalar::Bool(b) => serde_json::Value::Bool(*b),
            Scalar::List(items) => {
                serde_json::Value::Array(items.iter().map(Scalar::to_json).collect())
            }
        }
    }
}

impl fmt::Display for Scalar {
    /// KDL 2.0 syntax, so what `inventory show` prints can be pasted back.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scalar::Str(s) => write!(f, "{s:?}"),
            Scalar::Int(i) => write!(f, "{i}"),
            Scalar::UInt(u) => write!(f, "{u}"),
            Scalar::Float(x) => write!(f, "{x:?}"),
            Scalar::Bool(true) => f.write_str("#true"),
            Scalar::Bool(false) => f.write_str("#false"),
            Scalar::List(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
        }
    }
}

/// Playbook vars declared at one level (all, a group, or a host). Sorted so
/// output is deterministic.
pub type VarBag = BTreeMap<String, Scalar>;

/// Convert a bag into the JSON object the schema check and `Start` want.
pub fn bag_to_json(bag: &VarBag) -> serde_json::Value {
    serde_json::Value::Object(bag.iter().map(|(k, v)| (k.clone(), v.to_json())).collect())
}

/// How the orchestrator reaches a host. Written `connection="ssh"` or
/// `connection="local"` on a host, a group, or `defaults`; the built-in
/// default is [`Connection::Ssh`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Connection {
    /// Over ssh, using `addr`, `port`, `ssh_user` and `ssh_args`. Because
    /// this is also the built-in default, a host that sets neither `addr`
    /// nor `connection` fails to load.
    Ssh,
    /// The orchestrator's own machine. No ssh parameter applies and `addr`
    /// may be left out.
    Local,
}

impl Connection {
    /// The two accepted spellings, in the order the error for an
    /// unrecognized `connection=` lists them.
    pub const NAMES: [&'static str; 2] = ["ssh", "local"];

    /// The connection a `connection=` value spells, or `None` for anything
    /// else, which the parser turns into an error quoting what was written.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ssh" => Some(Connection::Ssh),
            "local" => Some(Connection::Local),
            _ => None,
        }
    }

    /// The KDL spelling: what `inventory show` prints and what
    /// [`Connection::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            Connection::Ssh => "ssh",
            Connection::Local => "local",
        }
    }
}

/// How the playbook binary gains root on the target. (Ansible's `become`;
/// that word is a Rust keyword and never appears here.)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Escalate {
    /// Through `sudo`. The built-in default when nothing sets `escalate`.
    Sudo,
    /// Through `doas`, the OpenBSD-derived alternative.
    Doas,
    /// No escalation at all: the playbook binary keeps running as the user
    /// that connected, and an op that needs root fails on its own terms.
    None,
}

impl Escalate {
    /// The three accepted spellings, in the order the error for an
    /// unrecognized `escalate=` lists them.
    pub const NAMES: [&'static str; 3] = ["sudo", "doas", "none"];

    /// The mode an `escalate=` value spells, or `None` for anything else,
    /// which the parser turns into an error quoting what was written.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sudo" => Some(Escalate::Sudo),
            "doas" => Some(Escalate::Doas),
            "none" => Some(Escalate::None),
            _ => None,
        }
    }

    /// The KDL spelling: what `inventory show` prints and what
    /// [`Escalate::parse`] reads back.
    pub fn as_str(self) -> &'static str {
        match self {
            Escalate::Sudo => "sudo",
            Escalate::Doas => "doas",
            Escalate::None => "none",
        }
    }
}

/// The closed set of parameters (vision 10.2.1) as written on one node.
/// `None` means "not set here"; resolution fills the gaps from the nearest
/// group outward, then `defaults`, then the built-in default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostParams {
    /// `addr="10.0.0.4"`: the hostname or address ssh connects to. Host
    /// only, and rejected on a group or on `defaults`, since an address
    /// names one machine and cannot be inherited. Required, at load time,
    /// of every host whose resolved `connection` is
    /// [`Ssh`](Connection::Ssh). The built-in default is none.
    pub addr: Option<String>,
    /// `connection="ssh"` or `connection="local"`. The built-in default is
    /// [`Connection::Ssh`].
    pub connection: Option<Connection>,
    /// `ssh_user="cadu"`: the account ssh logs in as. The built-in default
    /// is the orchestrator's own username, from
    /// [`local_username`](super::local_username).
    pub ssh_user: Option<String>,
    /// `port=2222`: the ssh port. An integer outside 0 to 65535 is a load
    /// error rather than a wrapped value. The built-in default is 22.
    pub port: Option<u16>,
    /// `escalate="sudo"`, `"doas"`, or `"none"`. The built-in default is
    /// [`Escalate::Sudo`].
    pub escalate: Option<Escalate>,
    /// `escalate_user="deploy"`: the account `sudo` or `doas` switches to.
    /// The built-in default is `root`.
    pub escalate_user: Option<String>,
    /// Extra arguments handed to `ssh`. Two spellings: the property
    /// `ssh_args="-4"` for a single argument, or the child node
    /// `ssh_args "-o" "StrictHostKeyChecking=no"` for several. Using both on
    /// one node is a load error. The list is not merged across levels: the
    /// nearest level that sets it wins whole, and the rest are reported as
    /// overridden. The built-in default is empty.
    pub ssh_args: Option<Vec<String>>,
}

impl HostParams {
    /// The seven parameter names, in the order `inventory show` prints them.
    pub const NAMES: [&'static str; 7] = [
        "addr",
        "connection",
        "ssh_user",
        "port",
        "escalate",
        "escalate_user",
        "ssh_args",
    ];

    /// Whether any parameter is set on this node.
    pub fn is_empty(&self) -> bool {
        *self == HostParams::default()
    }
}

/// A host as defined in the file: its own parameters and vars, and the group
/// it is nested in, if any. Group membership through `members` is not
/// stored here; ask [`super::Inventory::groups_of`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Host {
    /// The name in `host "web1"`, unique across hosts and groups.
    pub name: String,
    /// Only what this `host` node itself sets. Everything else is `None`
    /// here and is filled in by [`super::Inventory::resolve`].
    pub params: HostParams,
    /// The host's own `vars` block. The nearest level there is: it wins
    /// over every group and over the top-level `vars`.
    pub vars: VarBag,
    /// The group whose block this host is defined inside.
    pub group: Option<String>,
}

/// A group as defined in the file. `members` are the names (hosts or groups)
/// listed in `members`; `hosts` are the hosts nested inside the block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// The name in `group "web"`, unique across hosts and groups.
    pub name: String,
    /// What the `group` node itself sets, offered to every host that has
    /// this group in [`super::Inventory::groups_of`] and does not set the
    /// parameter nearer.
    pub params: HostParams,
    /// The group's own `vars` block, applied to its hosts unless the host,
    /// or a nearer group, sets the same key.
    pub vars: VarBag,
    /// The names in the `members` child node: hosts, or other groups, which
    /// is how groups nest (writing a `group` node inside a `group` node is
    /// a load error instead). A name that is neither a host nor a group is
    /// a load error, and so is a membership cycle.
    pub members: Vec<String>,
    /// Hosts written as `host` nodes inside this group's block. Equivalent
    /// to `members` for precedence: both put the group at distance 1 from
    /// the host. Not the full membership, which is transitive; for that ask
    /// [`super::Inventory::select`].
    pub hosts: Vec<String>,
}

/// A loaded, structurally valid inventory. Maps keep definition order out;
/// [`super::Inventory::host_names`] gives it back.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Inventory {
    /// Every host by name, whether written at top level or nested inside a
    /// group.
    pub hosts: BTreeMap<String, Host>,
    /// Every group by name.
    pub groups: BTreeMap<String, Group>,
    /// Workspace-wide parameters (`defaults ssh_user="cadu" ...`).
    pub defaults: HostParams,
    /// Workspace-wide vars: the "all" level.
    pub vars: VarBag,
    /// Host and group names in the order they appear in the file.
    pub(crate) order: Vec<String>,
}
