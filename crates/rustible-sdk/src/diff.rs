//! What a step says it would change.
//!
//! An op never hands a [`Diff`] to anything. Its `check` returns an
//! [`Intent`], and `ctx.step` renders the step's `Diff` from that intent with
//! [`Intent::diff`]; the runtime puts it in the `StepFinished` event and the
//! orchestrator renders it. Nothing reads a `Diff` but the renderer: it is
//! the report, never an instruction, which is why it is opaque outside the
//! SDK. These strings are the only account of a change a user ever sees, so
//! they are written for a reader, not for a machine.
//!
//! [`Intent`]: crate::op::Intent
//! [`Intent::diff`]: crate::op::Intent::diff

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a step would change. Serialized over the channel, rendered by the
/// orchestrator.
///
/// Write-only outside the SDK: an op builds one with [`Diff::text`],
/// [`Diff::attrs`], [`Diff::summary`] or [`Diff::many`], and what it does
/// with it afterwards is [`Diff::render`] and [`Diff::short`]. There is no
/// variant to match and no field to read, because a `Diff` is the report
/// rendered from an op's [`Intent`](crate::op::Intent) and never the
/// instruction `apply` follows. An `apply` that decided what to do from
/// display strings would let rewording a report change what runs. Reading one
/// back now means parsing one of its string forms (`render()`, `{:?}` or its
/// JSON), which is plainly wrong and caught in review; the type cannot stop
/// it, only make it conspicuous.
///
/// The JSON is the private enum's, unchanged by the wrapper
/// (`#[serde(transparent)]`), so the wire format does not depend on this
/// type being opaque.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Diff(Repr);

/// The shapes a [`Diff`] takes. Private, so that nothing outside the SDK can
/// read a diff back; its variant and field names are the wire format.
#[derive(Debug, Clone, Serialize, Deserialize)]
enum Repr {
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
        /// One entry per differing attribute.
        changes: Vec<AttrChange>,
    },
    /// Something with no meaningful before/after (a restart, a command).
    Summary(String),
    /// Several changes that one step makes together, in the order they
    /// happen. Built only by [`Diff::many`], which makes a `Many` inside a
    /// `Many`, a `Many` of one and a `Many` of none unreachable.
    Many(Vec<Diff>),
}

/// One line of a [`Diff::attrs`]. Both sides are already rendered as text by
/// the op, so it decides how a mode, a gid or a boolean should read. Built
/// with [`AttrChange::new`]; like a `Diff`, it cannot be read back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttrChange {
    name: String,
    from: String,
    to: String,
}

impl AttrChange {
    /// One attribute: `name` as a user would recognize it (`mode`, `owner`,
    /// `enabled`, `exists`), the value now, and the value the op would leave
    /// behind. Spell out an absence rather than leaving `from` empty, e.g.
    /// `yes (dir)` against a `to` of `no`.
    pub fn new(name: impl Into<String>, from: impl Into<String>, to: impl Into<String>) -> Self {
        AttrChange {
            name: name.into(),
            from: from.into(),
            to: to.into(),
        }
    }
}

impl Diff {
    /// A whole-file change. Pass the complete before and after text; the
    /// unified diff is computed at render time, not here, so building this
    /// costs no diffing. `path` is rendered as the diff header only.
    pub fn text(
        path: impl Into<PathBuf>,
        before: impl Into<String>,
        after: impl Into<String>,
    ) -> Self {
        Diff(Repr::Text {
            path: path.into(),
            before: before.into(),
            after: after.into(),
        })
    }

    /// One or more attribute changes on `subject`: what the attributes
    /// belong to, as the reader knows it (a unit name, a path, a user name).
    /// One [`AttrChange`] per differing attribute; an op with none has
    /// nothing to change and returns [`Plan::Satisfied`] instead.
    ///
    /// [`Plan::Satisfied`]: crate::op::Plan::Satisfied
    pub fn attrs(subject: impl Into<String>, changes: Vec<AttrChange>) -> Self {
        Diff(Repr::Attrs {
            subject: subject.into(),
            changes,
        })
    }

    /// The fallback when there is no before and after worth showing (a
    /// restart, a command). Also what text-oriented ops fall back to when a
    /// file is binary or too large to diff.
    pub fn summary(s: impl Into<String>) -> Self {
        Diff(Repr::Summary(s.into()))
    }

    /// Compose parts that one step makes together into one diff, in the
    /// order they happen. For an op whose single resource spans more than
    /// one thing on disk: `ssh::authorized_keys` writing a file *and*
    /// creating the `.ssh` directory that holds it reports both, so the
    /// account of what changed stays complete.
    ///
    /// Not a way to bundle unrelated work: two things that can be wanted
    /// independently are two steps. Vision 6.7 is the rule that keeps them
    /// apart — an op changes one kind of resource, and its one exception is
    /// narrow and named.
    ///
    /// Returns the part itself when there is one, so the common case keeps
    /// the shape it always had and an op that composes conditionally does
    /// not have to special-case it; `None` when there are none, because a
    /// change with nothing in it is not a change and the caller should be
    /// returning [`Plan::Satisfied`] instead. A nested composition is
    /// flattened.
    ///
    /// [`Plan::Satisfied`]: crate::op::Plan::Satisfied
    pub fn many(parts: impl IntoIterator<Item = Diff>) -> Option<Diff> {
        let mut flat = Vec::new();
        for part in parts {
            match part.0 {
                Repr::Many(inner) => flat.extend(inner),
                one => flat.push(Diff(one)),
            }
        }
        match flat.len() {
            0 => None,
            1 => flat.pop(),
            _ => Some(Diff(Repr::Many(flat))),
        }
    }

    /// Human-readable rendering. Unified diff for text.
    pub fn render(&self) -> String {
        match &self.0 {
            Repr::Text {
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
            Repr::Attrs { subject, changes } => {
                let mut s = format!("{subject}:\n");
                for c in changes {
                    s.push_str(&format!("  {}: {} -> {}\n", c.name, c.from, c.to));
                }
                s
            }
            Repr::Summary(s) => s.clone(),
            // Each part already ends in a newline (a unified diff does, and
            // the `Attrs` rendering above does), so joining needs no
            // separator; an empty list renders as nothing, which is what a
            // caller that built one deserves to see.
            Repr::Many(parts) => parts.iter().map(Diff::render).collect(),
        }
    }

    /// One-line hint for the step list, e.g. "+2 -1 lines".
    ///
    /// Always one line: a [`Diff::summary`] contributes its first line only,
    /// so a summary can carry detail on the lines after it (a request's body,
    /// what a step waits for) that [`Diff::render`] shows at `-v` and the
    /// step line, printed in every run, does not.
    pub fn short(&self) -> String {
        match &self.0 {
            Repr::Text { before, after, .. } => {
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
            Repr::Attrs { changes, .. } => changes
                .iter()
                .map(|c| format!("{}={}", c.name, c.to))
                .collect::<Vec<_>>()
                .join(" "),
            Repr::Summary(s) => s.lines().next().unwrap_or_default().to_string(),
            Repr::Many(parts) => parts
                .iter()
                .map(Diff::short)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" "),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(subject: &str, name: &str, to: &str) -> Diff {
        Diff::attrs(subject, vec![AttrChange::new(name, "-", to)])
    }

    /// The parts render in order and each already ends in a newline, so a
    /// `Many` reads as one account of the change rather than as a run-on.
    #[test]
    fn many_renders_its_parts_in_order() {
        let d = Diff::many([
            attrs("/home/a/.ssh", "exists", "yes"),
            Diff::text("/home/a/.ssh/authorized_keys", "", "key\n"),
        ])
        .unwrap();
        let rendered = d.render();
        assert!(
            rendered.starts_with("/home/a/.ssh:\n  exists: - -> yes\n"),
            "{rendered}"
        );
        assert!(rendered.contains("+key"), "{rendered}");
        assert_eq!(d.short(), "exists=yes +1 -0 lines");
    }

    /// An empty part contributes nothing to the step line rather than a
    /// stray separator.
    #[test]
    fn short_skips_parts_with_nothing_to_say() {
        let d = Diff::many([Diff::summary(""), attrs("/tmp/x", "mode", "0600")]).unwrap();
        assert_eq!(d.short(), "mode=0600");
    }

    /// The step line is one line whatever a summary holds: the lines after
    /// the first are for `render`, which `-v` prints, and a multi-line
    /// `short` would break the step line in every run.
    #[test]
    fn short_of_a_summary_is_its_first_line() {
        let d = Diff::summary("PATCH http://h/x\n{\n  \"a\": 1\n}");
        assert_eq!(d.short(), "PATCH http://h/x");
        assert_eq!(d.render(), "PATCH http://h/x\n{\n  \"a\": 1\n}");
        assert_eq!(Diff::summary("one").short(), "one");
        assert_eq!(Diff::summary("").short(), "");
        let many = Diff::many([
            attrs("/tmp/x", "mode", "0600"),
            Diff::summary("waits for x\nmore"),
        ])
        .unwrap();
        assert_eq!(many.short(), "mode=0600 waits for x");
    }

    /// The constructor is what keeps the recursive variant from carrying
    /// shapes that mean nothing: no `Many` of none, no `Many` of one, no
    /// nesting. The variant is recursive because `Vec<Diff>` is the only
    /// composition that does not duplicate every other variant into a second
    /// enum; this is the price of that, and it is paid in one place.
    #[test]
    fn many_collapses_the_shapes_that_would_mean_nothing() {
        assert!(Diff::many([]).is_none());

        let one = Diff::many([attrs("/tmp/x", "mode", "0600")]).unwrap();
        assert!(matches!(one.0, Repr::Attrs { .. }), "one part stays itself");

        let nested = Diff::many([
            Diff::many([attrs("/a", "mode", "1"), attrs("/b", "mode", "2")]).unwrap(),
            attrs("/c", "mode", "3"),
        ])
        .unwrap();
        let Repr::Many(parts) = &nested.0 else {
            panic!("expected many")
        };
        assert_eq!(parts.len(), 3, "flattened, not nested");
        assert!(parts.iter().all(|p| matches!(p.0, Repr::Attrs { .. })));
    }
}
