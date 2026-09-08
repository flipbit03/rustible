//! Helper module for `cadu/mc.rs`. Not a playbook: no `#[rustible::playbook]`.

use rustible::sdk::Applied;
use rustible_std::apt::InstallReport;

pub fn describe(r: &Applied<InstallReport>) -> String {
    if r.changed && r.predicted {
        "would install".to_string()
    } else if r.changed {
        format!("installed {}", r.installed.iter().map(|p| format!("{} {}", p.name, p.version)).collect::<Vec<_>>().join(", "))
    } else {
        format!("already there: {}", r.already_present.iter().map(|p| format!("{} {}", p.name, p.version)).collect::<Vec<_>>().join(", "))
    }
}
