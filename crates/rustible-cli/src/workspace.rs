//! Workspace-level helpers shared by every subcommand: root discovery
//! (vision doc section 10.4), lexical path normalization, and file writing.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};

/// Walk up from `start` looking for `rustible.toml`.
pub(crate) fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|d| d.join("rustible.toml").is_file())
        .map(Path::to_path_buf)
}

/// Resolve `.` and `..` lexically, without touching the filesystem, so a path
/// that does not exist yet (or is a symlink) is handled the same way.
pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(out.components().next_back(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Absolute, normalized form of a user-supplied path.
pub(crate) fn absolute(path: &Path) -> Result<PathBuf> {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    Ok(normalize(&joined))
}

/// Write a file, creating parent directories, and say so.
pub(crate) fn write_file(path: &Path, contents: &str, shown_as: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    eprintln!("    wrote {shown_as}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_lexically() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d")),
            PathBuf::from("/a/c/d")
        );
        assert_eq!(normalize(Path::new("../x/../y")), PathBuf::from("../y"));
    }
}
