//! What a step says it would change.
//!
//! An op builds a [`Diff`] in `check` and hands it to [`Plan::Change`]; the
//! runtime puts it in the `StepFinished` event and the orchestrator renders
//! it. These strings are the only account of a change a user ever sees, so
//! they are written for a reader, not for a machine.
//!
//! [`Plan::Change`]: crate::op::Plan::Change

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a step would change. Serialized over the channel, rendered by the
/// orchestrator. Kept deliberately small for the spike.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Diff {
    /// Whole-text change of a file.
    Text {
        /// The file the op is about. Rendered as the diff header only; the
        /// path is never opened again.
        path: PathBuf,
        /// The contents now. The empty string when the file does not exist,
        /// which renders as a diff that adds every line.
        before: String,
        /// The contents the op would write, in full: `render` computes the
        /// unified diff, so ops never build a patch themselves.
        after: String,
    },
    /// One or more attribute changes on a resource (mode, owner, enabled, ...).
    Attrs {
        /// What the attributes belong to, as the reader knows it: a unit
        /// name, a path, a user name.
        subject: String,
        /// One entry per differing attribute. An op with an empty list has
        /// nothing to change and returns [`Plan::Satisfied`] instead.
        ///
        /// [`Plan::Satisfied`]: crate::op::Plan::Satisfied
        changes: Vec<AttrChange>,
    },
    /// Something with no meaningful before/after (a restart, a command).
    Summary(String),
    /// Several changes that one step makes together, in the order they
    /// happen. For an op whose single resource spans more than one thing on
    /// disk: `ssh::authorized_keys` writing a file *and* creating the `.ssh`
    /// directory that holds it reports both, so the account of what changed
    /// stays complete.
    ///
    /// Not a way to bundle unrelated work. Two things that can be wanted
    /// independently are two steps, and vision 6.7 is the test.
    Many(Vec<Diff>),
}

/// One line of a [`Diff::Attrs`]. Both sides are already rendered as text by
/// the op, so it decides how a mode, a gid or a boolean should read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttrChange {
    /// The attribute under the name a user would recognize: `mode`, `owner`,
    /// `enabled`, `exists`.
    pub name: String,
    /// The value now. Ops spell out an absence rather than leaving this
    /// empty, e.g. `yes (dir)` against a `to` of `no`.
    pub from: String,
    /// The value the op would leave behind.
    pub to: String,
}

impl Diff {
    /// A whole-file [`Diff::Text`]. Pass the complete before and after text;
    /// the unified diff is computed at render time, not here, so building
    /// this costs no diffing.
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

    /// A [`Diff::Summary`]: the fallback when there is no before and after
    /// worth showing. Also what text-oriented ops fall back to when a file
    /// is binary or too large to diff.
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
            // Each part already ends in a newline (a unified diff does, and
            // the `Attrs` rendering above does), so joining needs no
            // separator; an empty list renders as nothing, which is what a
            // caller that built one deserves to see.
            Diff::Many(parts) => parts.iter().map(Diff::render).collect(),
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
            Diff::Many(parts) => parts
                .iter()
                .map(Diff::short)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// The parts of a [`Diff::Many`], or the diff itself as a single part.
    /// An op that composes a `Many` in `check` uses this in `apply` to find
    /// the part it needs without matching the two shapes separately.
    pub fn parts(&self) -> &[Diff] {
        match self {
            Diff::Many(parts) => parts,
            one => std::slice::from_ref(one),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(subject: &str, name: &str, to: &str) -> Diff {
        Diff::Attrs {
            subject: subject.into(),
            changes: vec![AttrChange {
                name: name.into(),
                from: "-".into(),
                to: to.into(),
            }],
        }
    }

    /// The parts render in order and each already ends in a newline, so a
    /// `Many` reads as one account of the change rather than as a run-on.
    #[test]
    fn many_renders_its_parts_in_order() {
        let d = Diff::Many(vec![
            attrs("/home/a/.ssh", "exists", "yes"),
            Diff::text("/home/a/.ssh/authorized_keys", "", "key\n"),
        ]);
        let rendered = d.render();
        assert!(
            rendered.starts_with("/home/a/.ssh:\n  exists: - -> yes\n"),
            "{rendered}"
        );
        assert!(rendered.contains("+key"), "{rendered}");
        assert_eq!(d.short(), "exists=yes +1 -0 lines");
    }

    /// `parts` is what an op's `apply` uses to find the piece it needs, and
    /// a diff that is not a `Many` is a single part, so `apply` never has to
    /// match the two shapes separately.
    #[test]
    fn parts_treats_a_lone_diff_as_one_part() {
        let one = Diff::summary("restarted");
        assert_eq!(one.parts().len(), 1);
        assert!(matches!(one.parts()[0], Diff::Summary(_)));
        assert_eq!(Diff::Many(vec![]).parts().len(), 0);
    }

    /// An empty part contributes nothing to the step line rather than a
    /// stray separator.
    #[test]
    fn short_skips_parts_with_nothing_to_say() {
        let d = Diff::Many(vec![Diff::summary(""), attrs("/tmp/x", "mode", "0600")]);
        assert_eq!(d.short(), "mode=0600");
    }
}
