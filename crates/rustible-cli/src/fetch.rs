//! The orchestrator's half of `ctx.fetch`: `FetchChunk`s are written to a
//! temporary file beside their destination, which is renamed over it only
//! when the chunk marked `last` arrives (decision 27 on #86). The
//! destination is therefore always either what it held before or the whole
//! new file: a fetch that stops part way, because the target's read failed
//! or the run ended, leaves it as it was, and its temporary file is removed
//! when the host's run ends, however it ends.

use std::collections::HashMap;
use std::fs::{File, Permissions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use rustible_sdk::stream::WorkspaceFiles;
use tempfile::NamedTempFile;

/// The fetches of one host's run that have not had their last chunk, by
/// request id. Dropping it removes every temporary file still in it.
#[derive(Default)]
pub(crate) struct Fetches(HashMap<u32, Staged>);

/// One fetch in flight.
struct Staged {
    /// Where it lands, resolved inside the workspace.
    path: PathBuf,
    /// `.rustible-<random>` beside `path`, removed when dropped.
    tmp: NamedTempFile,
    /// Bytes written so far, which the next chunk's offset must equal.
    written: u64,
}

impl Fetches {
    /// Write one chunk of request `req` toward `dest`, a path relative to
    /// the workspace. The first chunk, at offset 0, resolves `dest` and
    /// creates the temporary file beside it. On `last` the file is renamed
    /// over the destination, and its path and size come back; until then,
    /// `None`. Any refusal or failure drops the fetch, its temporary file
    /// with it, and says why.
    pub(crate) fn chunk(
        &mut self,
        files: &WorkspaceFiles,
        req: u32,
        dest: &str,
        offset: u64,
        bytes: &[u8],
        last: bool,
    ) -> Result<Option<(PathBuf, u64)>, String> {
        let mut staged = match self.0.remove(&req) {
            Some(s) => s,
            None if offset == 0 => Self::begin(files, dest)?,
            None => {
                return Err(format!(
                    "`{dest}`: chunk at offset {offset} but nothing received before it"
                ));
            }
        };
        if offset != staged.written {
            return Err(format!(
                "`{dest}`: chunk at offset {offset} but {} bytes received so far",
                staged.written
            ));
        }
        let shown = staged.tmp.path().display().to_string();
        staged
            .tmp
            .write_all(bytes)
            .map_err(|e| format!("{shown}: {e}"))?;
        staged.written += bytes.len() as u64;
        if !last {
            self.0.insert(req, staged);
            return Ok(None);
        }
        let Staged { path, tmp, written } = staged;
        tmp.persist(&path)
            .map_err(|e| format!("{}: {}", path.display(), e.error))?;
        Ok(Some((path, written)))
    }

    /// A temporary file beside `dest`'s resolved path, with the mode the
    /// file it replaces has, or 0666 less the umask for a new one, which is
    /// what a fetched file was created with when chunks went straight in.
    fn begin(files: &WorkspaceFiles, dest: &str) -> Result<Staged, String> {
        let path = files.resolve_dest(dest)?;
        let dir = path.parent().unwrap_or(files.root());
        let tmp = tempfile::Builder::new()
            .prefix(".rustible-")
            .permissions(Permissions::from_mode(0o666))
            .tempfile_in(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        if let Ok(old) = std::fs::metadata(&path)
            && old.is_file()
        {
            let mode = old.permissions().mode() & 0o777;
            File::set_permissions(tmp.as_file(), Permissions::from_mode(mode))
                .map_err(|e| format!("{}: {e}", tmp.path().display()))?;
        }
        Ok(Staged {
            path,
            tmp,
            written: 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// Everything in `dir`, by name, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A fetch replaces its destination only on its last chunk: before it,
    /// the old file is whole and the new content sits in a `.rustible-*`
    /// file beside it; after it, the new file is whole and nothing is
    /// beside it.
    #[test]
    fn a_complete_fetch_replaces_the_destination_on_its_last_chunk() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("out/f"), b"old").unwrap();
        let files = WorkspaceFiles::new(dir.path()).unwrap();
        let mut fetches = Fetches::default();

        assert_eq!(
            fetches.chunk(&files, 1, "out/f", 0, b"new ", false),
            Ok(None)
        );
        assert_eq!(
            fetches.chunk(&files, 1, "out/f", 4, b"cont", false),
            Ok(None)
        );
        assert_eq!(std::fs::read(dir.path().join("out/f")).unwrap(), b"old");
        let out = names(&dir.path().join("out"));
        assert!(
            out.len() == 2 && out[0].starts_with(".rustible-") && out[1] == "f",
            "{out:?}"
        );

        let (path, n) = fetches
            .chunk(&files, 1, "out/f", 8, b"ent", true)
            .unwrap()
            .unwrap();
        assert_eq!((path.file_name().unwrap(), n), ("f".as_ref(), 11));
        assert_eq!(std::fs::read(&path).unwrap(), b"new content");
        assert_eq!(names(&dir.path().join("out")), ["f"]);
        assert!(fetches.0.is_empty());
    }

    /// A fetch that never gets its last chunk leaves the destination as it
    /// was, and its temporary file goes when the run's `Fetches` is dropped;
    /// one that never had a destination leaves nothing at all.
    #[test]
    fn an_unfinished_fetch_leaves_the_destination_and_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("out/f"), b"old").unwrap();
        let files = WorkspaceFiles::new(dir.path()).unwrap();
        let mut fetches = Fetches::default();
        fetches
            .chunk(&files, 1, "out/f", 0, b"partial", false)
            .unwrap();
        fetches
            .chunk(&files, 2, "out/g", 0, b"partial", false)
            .unwrap();
        assert_eq!(names(&dir.path().join("out")).len(), 3);
        drop(fetches);
        assert_eq!(names(&dir.path().join("out")), ["f"]);
        assert_eq!(std::fs::read(dir.path().join("out/f")).unwrap(), b"old");
    }

    /// A chunk out of order is refused and ends its fetch, temporary file
    /// and all; so is a first chunk that does not start at 0, and a
    /// destination outside the workspace.
    #[test]
    fn a_chunk_out_of_order_ends_its_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let files = WorkspaceFiles::new(dir.path()).unwrap();
        let mut fetches = Fetches::default();
        fetches.chunk(&files, 1, "f", 0, b"abc", false).unwrap();
        let err = fetches.chunk(&files, 1, "f", 5, b"x", true).unwrap_err();
        assert_eq!(err, "`f`: chunk at offset 5 but 3 bytes received so far");
        assert_eq!(names(dir.path()), Vec::<String>::new());
        let err = fetches.chunk(&files, 1, "f", 3, b"x", true).unwrap_err();
        assert!(err.contains("nothing received before it"), "{err}");
        let err = fetches.chunk(&files, 2, "../f", 0, b"x", true).unwrap_err();
        assert!(err.contains("`..`"), "{err}");
        assert!(fetches.0.is_empty());
    }

    /// The new file keeps the mode of the one it replaces, as the file
    /// written in place used to.
    #[test]
    fn a_fetch_keeps_the_mode_of_the_file_it_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"old").unwrap();
        std::fs::set_permissions(&f, Permissions::from_mode(0o640)).unwrap();
        let files = WorkspaceFiles::new(dir.path()).unwrap();
        let mut fetches = Fetches::default();
        fetches.chunk(&files, 1, "f", 0, b"new", true).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read(&f).unwrap(), b"new");
    }
}
