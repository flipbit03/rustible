//! Vars validation against a playbook's schema (vision 10.3): the
//! orchestrator-side pre-check, run for every resolved host before any
//! cross-compile. The binary re-validates at `Start`.
//!
//! The schema is what `--describe` prints as `vars_schema` for one
//! playbook: schemars 1.x JSON Schema (`properties`, `required`, `type`,
//! `format`, `default`, `$defs`), or `null` for a playbook without vars.

use std::fmt;

use rustible_sdk::vars::{
    coerce_scalars_to_lists, missing_required, non_flat_vars, type_mismatches, unknown_keys,
};
use serde_json::Value;

use super::model::{VarBag, bag_to_json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    /// An undeclared var: the bag is shared by every playbook targeting the
    /// host, so this is not a failure.
    Warning,
}

/// One problem with one var on one host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarError {
    pub var: String,
    pub severity: Severity,
    pub message: String,
}

impl VarError {
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for VarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

/// Check one host's resolved vars against one playbook's schema. Errors:
/// a schema property that is an object (vars are flat), a required var
/// missing, a var of the wrong type. Warning: a var the playbook does not
/// declare. A `null` schema means the playbook takes no vars; nothing is
/// checked.
pub fn validate(vars: &VarBag, schema: &Value) -> Vec<VarError> {
    if schema.is_null() {
        return vec![];
    }
    // Same coercion the binary applies at `Start` (a scalar where the
    // schema wants a list becomes a one-element list), so both sides agree.
    let raw = coerce_scalars_to_lists(schema, bag_to_json(vars));
    let mut out = vec![];
    for name in non_flat_vars(schema) {
        out.push(VarError {
            message: format!("var `{name}` is an object; vars are flat scalars, lists, or enums"),
            var: name,
            severity: Severity::Error,
        });
    }
    for name in missing_required(schema, &raw) {
        out.push(VarError {
            message: format!("missing required var `{name}`"),
            var: name,
            severity: Severity::Error,
        });
    }
    for m in type_mismatches(schema, &raw) {
        out.push(VarError {
            message: m.to_string(),
            var: m.var,
            severity: Severity::Error,
        });
    }
    for u in unknown_keys(schema, &raw) {
        out.push(VarError {
            message: u.to_string(),
            var: u.key,
            severity: Severity::Warning,
        });
    }
    out
}

/// Per-host results for one playbook, in the order the hosts were selected.
pub type HostResults = Vec<(String, Vec<VarError>)>;

/// The vision 10.3 report for one playbook: `None` when every host is fine
/// (warnings do not count). `target` is what the playbook's `hosts = ".."`
/// names; `is_group` says whether that is a group or a single host.
pub fn format_vars_report(
    target: &str,
    is_group: bool,
    playbook: &str,
    inventory_file: &str,
    results: &HostResults,
) -> Option<String> {
    let failing: Vec<&(String, Vec<VarError>)> = results
        .iter()
        .filter(|(_, errs)| errs.iter().any(VarError::is_error))
        .collect();
    if failing.is_empty() {
        return None;
    }
    let mut out = String::new();
    if is_group {
        out.push_str(&format!(
            "error: {} of {} hosts in group `{target}` do not satisfy the vars of {playbook}\n\n",
            failing.len(),
            results.len()
        ));
    } else {
        out.push_str(&format!(
            "error: host `{target}` does not satisfy the vars of {playbook}\n\n"
        ));
    }
    let width = results.iter().map(|(h, _)| h.len()).max().unwrap_or(0);
    for (host, errs) in results {
        let label = format!("host `{host}`");
        let pad = " ".repeat(width - host.len());
        let errors: Vec<&VarError> = errs.iter().filter(|e| e.is_error()).collect();
        if errors.is_empty() {
            out.push_str(&format!("  {label}{pad}   ok\n"));
            continue;
        }
        for (i, e) in errors.iter().enumerate() {
            if i == 0 {
                out.push_str(&format!("  {label}{pad}   {e}\n"));
            } else {
                out.push_str(&format!(
                    "  {}   {e}\n",
                    " ".repeat(label.len() + pad.len())
                ));
            }
        }
    }
    out.push_str(&format!(
        "\n  Vars are resolved from {inventory_file} as: vars -> group vars -> host vars.\n"
    ));
    // One hint per missing required var, naming the hosts that lack it.
    let mut missing: Vec<(String, Vec<String>)> = vec![];
    for (host, errs) in failing.iter() {
        for e in errs
            .iter()
            .filter(|e| e.is_error() && e.message.starts_with("missing required var"))
        {
            match missing.iter_mut().find(|(v, _)| *v == e.var) {
                Some((_, hosts)) => hosts.push(host.clone()),
                None => missing.push((e.var.clone(), vec![host.clone()])),
            }
        }
    }
    for (var, hosts) in missing {
        let where_ = match hosts.len() {
            1 => format!("host {}", hosts[0]),
            _ => format!("hosts {}", join_and(&hosts)),
        };
        if is_group {
            out.push_str(&format!(
                "  Add `{var}` to {where_}, or to group \"{target}\" vars if it is shared.\n"
            ));
        } else {
            out.push_str(&format!("  Add `{var}` to {where_}.\n"));
        }
    }
    Some(out)
}

fn join_and(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}
