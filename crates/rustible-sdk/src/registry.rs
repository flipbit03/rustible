//! What a workspace's generated `src/main.rs` hands to the runtime: every
//! discovered playbook, by name, with the metadata the `#[playbook]` macro
//! produced (vision doc sections 5.5 and 9).

use serde_json::Value;

use crate::ctx::Ctx;
use crate::error::Result;

/// One playbook's metadata and entry point. The macro generates a
/// `pub static __RUSTIBLE_PLAYBOOK: Playbook` in each playbook module.
pub struct Playbook {
    /// Host or group name from the attribute.
    pub hosts: &'static str,
    /// Launch the binary escalated (Ansible's `become`).
    pub escalate: bool,
    /// JSON Schema of the vars struct, or `Value::Null` when the playbook
    /// takes no vars.
    pub schema: fn() -> Value,
    /// Deserializes the vars and runs the playbook's `main`.
    pub entry: fn(&mut Ctx, Value) -> Result<()>,
    /// Deserializes the vars into the typed struct and stops there: the
    /// orchestrator's pre-check (`--check-vars`) runs serde without running
    /// the playbook, so both sides validate with the same code.
    pub check_vars: fn(Value) -> Result<()>,
}

/// A playbook with the name the build script derived from its path
/// (`cadu/mc` for `playbooks/cadu/mc.rs`).
pub struct Named {
    /// The name the orchestrator selects with `Down::Start`'s `playbook`
    /// field and a hand run passes as `<binary> <name>`. Slash-separated,
    /// following the file's path under `playbooks/`.
    pub name: &'static str,
    /// The `__RUSTIBLE_PLAYBOOK` static the `#[playbook]` macro generated in
    /// that module.
    pub playbook: &'static Playbook,
}

impl Named {
    /// One entry of the `--describe` document: name, target hosts, whether
    /// the binary wants escalation, and the vars JSON Schema, obtained by
    /// calling [`Playbook::schema`]. The orchestrator reads this to know
    /// what to put in `Start` and to validate inventory vars before it
    /// bothers uploading the binary.
    pub fn describe(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "hosts": self.playbook.hosts,
            "escalate": self.playbook.escalate,
            "vars_schema": (self.playbook.schema)(),
        })
    }
}

/// The `--describe` document for a whole binary.
pub fn describe_all(playbooks: &[Named]) -> Value {
    serde_json::json!({
        "protocol": crate::protocol::PROTOCOL_VERSION,
        "playbooks": playbooks.iter().map(Named::describe).collect::<Vec<_>>(),
    })
}
