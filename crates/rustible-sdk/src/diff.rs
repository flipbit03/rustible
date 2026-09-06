use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a step would change. Serialized over the channel, rendered by the
/// orchestrator. Kept deliberately small for the spike.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Diff {
    /// Whole-text change of a file.
    Text {
        path: PathBuf,
        before: String,
        after: String,
    },
    /// One or more attribute changes on a resource (mode, owner, enabled, ...).
    Attrs {
        subject: String,
        changes: Vec<AttrChange>,
    },
    /// Something with no meaningful before/after (a restart, a command).
    Summary(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttrChange {
    pub name: String,
    pub from: String,
    pub to: String,
}

impl Diff {
    pub fn text(
        path: impl Into<PathBuf>,
        before: impl Into<String>,
        after: impl Into<String>,
    ) -> Self {
        Diff::Text {
            path: path.into(),
            before: before.into(),
            after: after.into(),
        }
    }

    pub fn summary(s: impl Into<String>) -> Self {
        Diff::Summary(s.into())
    }

    /// Human-readable rendering. Unified diff for text.
    pub fn render(&self) -> String {
        match self {
            Diff::Text {
                path,
                before,
                after,
            } => {
                let d = similar::TextDiff::from_lines(before, after);
                d.unified_diff()
                    .context_radius(2)
                    .header(
                        &format!("{} (before)", path.display()),
                        &format!("{} (after)", path.display()),
                    )
                    .to_string()
            }
            Diff::Attrs { subject, changes } => {
                let mut s = format!("{subject}:\n");
                for c in changes {
                    s.push_str(&format!("  {}: {} -> {}\n", c.name, c.from, c.to));
                }
                s
            }
            Diff::Summary(s) => s.clone(),
        }
    }

    /// One-line hint for the step list, e.g. "+2 -1 lines".
    pub fn short(&self) -> String {
        match self {
            Diff::Text { before, after, .. } => {
                let d = similar::TextDiff::from_lines(before, after);
                let (mut ins, mut del) = (0, 0);
                for c in d.iter_all_changes() {
                    match c.tag() {
                        similar::ChangeTag::Insert => ins += 1,
                        similar::ChangeTag::Delete => del += 1,
                        similar::ChangeTag::Equal => {}
                    }
                }
                format!("+{ins} -{del} lines")
            }
            Diff::Attrs { changes, .. } => changes
                .iter()
                .map(|c| format!("{}={}", c.name, c.to))
                .collect::<Vec<_>>()
                .join(" "),
            Diff::Summary(s) => s.clone(),
        }
    }
}
