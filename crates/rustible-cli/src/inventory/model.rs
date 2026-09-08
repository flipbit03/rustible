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
    Str(String),
    Int(i64),
    /// Only for values above `i64::MAX`.
    UInt(u64),
    Float(f64),
    Bool(bool),
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

/// How the orchestrator reaches a host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Connection {
    Ssh,
    Local,
}

impl Connection {
    pub const NAMES: [&'static str; 2] = ["ssh", "local"];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ssh" => Some(Connection::Ssh),
            "local" => Some(Connection::Local),
            _ => None,
        }
    }

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
    Sudo,
    Doas,
    None,
}

impl Escalate {
    pub const NAMES: [&'static str; 3] = ["sudo", "doas", "none"];

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sudo" => Some(Escalate::Sudo),
            "doas" => Some(Escalate::Doas),
            "none" => Some(Escalate::None),
            _ => None,
        }
    }

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
    pub addr: Option<String>,
    pub connection: Option<Connection>,
    pub ssh_user: Option<String>,
    pub port: Option<u16>,
    pub escalate: Option<Escalate>,
    pub escalate_user: Option<String>,
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
    pub name: String,
    pub params: HostParams,
    pub vars: VarBag,
    /// The group whose block this host is defined inside.
    pub group: Option<String>,
}

/// A group as defined in the file. `members` are the names (hosts or groups)
/// listed in `members`; `hosts` are the hosts nested inside the block.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub name: String,
    pub params: HostParams,
    pub vars: VarBag,
    pub members: Vec<String>,
    pub hosts: Vec<String>,
}

/// A loaded, structurally valid inventory. Maps keep definition order out;
/// [`super::Inventory::host_names`] gives it back.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Inventory {
    pub hosts: BTreeMap<String, Host>,
    pub groups: BTreeMap<String, Group>,
    /// Workspace-wide parameters (`defaults ssh_user="cadu" ...`).
    pub defaults: HostParams,
    /// Workspace-wide vars: the "all" level.
    pub vars: VarBag,
    /// Host and group names in the order they appear in the file.
    pub(crate) order: Vec<String>,
}
