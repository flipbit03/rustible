//! Load-time errors. Every one names the file, line, and column (vision
//! 10.2.3); a load collects all of them instead of stopping at the first.

use std::fmt;

/// One problem found while loading an inventory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    /// The path as given to [`Inventory::load`](super::Inventory::load), or
    /// the label passed to [`Inventory::parse`](super::Inventory::parse).
    /// Never canonicalized, so it stays what the user typed and an editor
    /// can jump to it.
    pub file: String,
    /// 1-based.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
    /// The problem alone, with no position and no `error:` prefix; the
    /// [`Display`](fmt::Display) of this type adds both.
    pub message: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: error: {}",
            self.file, self.line, self.column, self.message
        )
    }
}

impl std::error::Error for LoadError {}

/// Everything wrong with one file, in source order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadErrors(pub Vec<LoadError>);

impl LoadErrors {
    /// True only for a `LoadErrors` built by hand: a load with no problems
    /// returns the inventory, so one that returns this always carries at
    /// least one error.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many problems the file has. A load reports all of them, so this
    /// is the real count and not a truncated one.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// The problems in source order, earliest position first.
    pub fn iter(&self) -> impl Iterator<Item = &LoadError> {
        self.0.iter()
    }
}

impl fmt::Display for LoadErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, e) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str("\n")?;
            }
            write!(f, "{e}")?;
        }
        Ok(())
    }
}

impl std::error::Error for LoadErrors {}

/// A name that is neither a host nor a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownName {
    /// The name as it was written, on the command line or in a playbook's
    /// `hosts = "..."`.
    pub name: String,
    /// The closest defined name, when one is close enough to be worth
    /// suggesting; `None` when nothing is. Candidates are hosts and groups
    /// for [`Inventory::select`](super::Inventory::select), hosts only for
    /// [`Inventory::resolve`](super::Inventory::resolve).
    pub suggestion: Option<String>,
}

impl fmt::Display for UnknownName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "no host or group named `{}`", self.name)?;
        if let Some(s) = &self.suggestion {
            write!(f, "; did you mean `{s}`?")?;
        }
        Ok(())
    }
}

impl std::error::Error for UnknownName {}

/// Maps byte offsets in a source text to 1-based line and column.
pub(crate) struct LineIndex<'a> {
    src: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(src: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        LineIndex { src, line_starts }
    }

    /// `(line, column)` of a byte offset; the column counts characters.
    pub fn position(&self, offset: usize) -> (usize, usize) {
        let offset = offset.min(self.src.len());
        let line = self.line_starts.partition_point(|&s| s <= offset);
        let start = self.line_starts[line - 1];
        let column = self.src[start..offset].chars().count() + 1;
        (line, column)
    }
}
