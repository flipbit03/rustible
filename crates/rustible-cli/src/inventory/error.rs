//! Load-time errors. Every one names the file, line, and column (vision
//! 10.2.3); a load collects all of them instead of stopping at the first.

use std::fmt;

/// One problem found while loading an inventory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
    pub file: String,
    /// 1-based.
    pub line: usize,
    /// 1-based, in characters.
    pub column: usize,
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
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

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
    pub name: String,
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
