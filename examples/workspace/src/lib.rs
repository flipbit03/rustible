//! Code shared across this workspace's playbooks. Playbooks reach it by the
//! package name: `workspace::greeting()`, not `crate::greeting()`, because
//! playbook files are modules of the bin crate (vision doc section 9).

/// Example shared helper.
pub fn greeting(hostname: &str) -> String {
    format!("hello from {hostname}")
}
