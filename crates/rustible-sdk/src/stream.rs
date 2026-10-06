//! File streaming over the channel (vision doc 5.6): the orchestrator-side
//! half. `WorkspaceFiles` serves `FileRequest`s from the workspace root and
//! writes `FetchChunk`s under it, denying anything that resolves outside; the
//! `chunks` iterator splits any reader into `CHUNK_SIZE` pieces. The binary's
//! half (`ctx.local_file`, `ctx.local_secret`, `ctx.fetch`) lives in `ctx`.
//!
//! This lives in the SDK rather than the CLI so a local run (no orchestrator)
//! serves files through exactly the same rules, and so the rules have one
//! set of tests.

use std::collections::{BTreeSet, HashMap};
use std::fs::{File, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use tempfile::NamedTempFile;
use zeroize::Zeroizing;

pub use crate::protocol::CHUNK_SIZE;

/// One piece of a streamed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Byte offset of `bytes` within the whole file. Chunks of one request
    /// arrive in order, so a consumer that keeps a running total can reject
    /// a gap; [`write_chunks`] does exactly that.
    pub offset: u64,
    /// Up to [`CHUNK_SIZE`] bytes, and fewer only in the last chunk. Wiped on
    /// drop: a chunk may be part of a secret (`ctx.local_secret`).
    pub bytes: Zeroizing<Vec<u8>>,
    /// The final chunk of this file. Exactly one chunk of a stream carries
    /// it, and a receiver uses it rather than a byte count to know the file
    /// is complete.
    pub last: bool,
}

/// Splits a reader into `CHUNK_SIZE` chunks. The stream always ends with a
/// chunk marked `last`: a short read is the last chunk; a source whose size
/// is a multiple of `CHUNK_SIZE` (including empty) ends with an empty one.
pub struct Chunks<R: Read> {
    r: R,
    offset: u64,
    done: bool,
}

/// The basename of a run's temp directory, `.rustible-<run id>`.
///
/// Both sides compute it from `Start.run_id`: the binary to create the
/// directory, the orchestrator to remove it after killing a binary that
/// ignored `Cancel`. A random name (what `tempfile` gives) is only ever
/// known to the binary, and SIGKILL runs no destructor, so a cancelled run
/// used to leave the directory and every streamed file in it behind with
/// nothing to collect it.
///
/// The id is reduced to characters that cannot escape the temp directory or
/// confuse a shell, because it arrives over the wire. An id that survives
/// nothing keeps a fixed name rather than a random one: two such runs then
/// collide loudly at `create_dir` instead of quietly sharing a directory.
///
/// That filtering is not injective: `a/b` and `ab` both give
/// `.rustible-ab`, and anything unsanitisable gives `.rustible-unnamed`.
/// Two colliding runs fail at `create_dir`, which refuses an existing path,
/// so a collision is loud rather than silent, and the orchestrator sends
/// hex ids so it is not reachable today. Feed this a structured id and that
/// stops being true.
pub fn run_dir_name(run_id: &str) -> String {
    let safe: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if safe.is_empty() {
        ".rustible-unnamed".to_string()
    } else {
        format!(".rustible-{safe}")
    }
}

/// Wrap any reader in the [`Chunks`] iterator. An interrupted read is
/// retried; any other read error is yielded once and ends the iteration
/// without a `last` chunk, so a truncated source can never be mistaken for
/// a complete file.
pub fn chunks<R: Read>(r: R) -> Chunks<R> {
    Chunks {
        r,
        offset: 0,
        done: false,
    }
}

impl<R: Read> Iterator for Chunks<R> {
    type Item = io::Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let mut buf = Zeroizing::new(vec![0u8; CHUNK_SIZE]);
        let mut filled = 0;
        while filled < CHUNK_SIZE {
            match self.r.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
        buf.truncate(filled);
        let last = filled < CHUNK_SIZE;
        let chunk = Chunk {
            offset: self.offset,
            bytes: buf,
            last,
        };
        self.offset += filled as u64;
        self.done = last;
        Some(Ok(chunk))
    }
}

/// The orchestrator's view of the workspace for streaming: files are served
/// from under `root` and fetched files are written under it. A request that
/// is absolute, contains `..`, or resolves (through a symlink) outside the
/// root is denied with a reason the binary reports verbatim.
#[derive(Debug, Clone)]
pub struct WorkspaceFiles {
    root: PathBuf,
}

impl WorkspaceFiles {
    /// `root` must exist; it is canonicalized so symlink escapes can be told.
    pub fn new(root: impl AsRef<Path>) -> io::Result<Self> {
        Ok(WorkspaceFiles {
            root: root.as_ref().canonicalize()?,
        })
    }

    /// The canonicalized root, which is what every confinement check
    /// compares against. It is the resolved path, not the one handed to
    /// [`WorkspaceFiles::new`], so a workspace reached through a symlink
    /// still matches the paths `canonicalize` returns for files inside it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Lexical check shared by reads and writes: relative, no `..`, no
    /// prefix or root components.
    fn relative(&self, requested: &str) -> Result<PathBuf, String> {
        let p = Path::new(requested);
        if requested.is_empty() {
            return Err("empty path".into());
        }
        if p.is_absolute() {
            return Err(format!(
                "`{requested}` is absolute; paths are relative to the workspace root"
            ));
        }
        for c in p.components() {
            match c {
                Component::Normal(_) | Component::CurDir => {}
                Component::ParentDir => {
                    return Err(format!(
                        "`{requested}` contains `..`; only paths inside the workspace are served"
                    ));
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!("`{requested}` is not a relative path"));
                }
            }
        }
        Ok(self.root.join(p))
    }

    /// Resolve a `FileRequest` path to a regular file inside the root.
    pub fn resolve(&self, requested: &str) -> Result<PathBuf, String> {
        let joined = self.relative(requested)?;
        let real = joined.canonicalize().map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                format!("`{requested}` not found in the workspace")
            } else {
                format!("`{requested}`: {e}")
            }
        })?;
        if !real.starts_with(&self.root) {
            return Err(format!(
                "`{requested}` resolves outside the workspace ({})",
                real.display()
            ));
        }
        if !real.is_file() {
            return Err(format!("`{requested}` is not a regular file"));
        }
        Ok(real)
    }

    /// Open a requested file for chunked reading.
    pub fn open(&self, requested: &str) -> Result<File, String> {
        let real = self.resolve(requested)?;
        File::open(&real).map_err(|e| format!("`{requested}`: {e}"))
    }

    /// Resolve a `FetchChunk` destination inside the root, creating parent
    /// directories.
    ///
    /// Confinement is decided before anything is created: the deepest
    /// ancestor that already exists is canonicalized and must be inside the
    /// root. Creating first and checking after still refused the write, but
    /// a denied fetch through an in-workspace symlink pointing outward had
    /// by then made `<outside>/deep/nested` on the orchestrator. The check
    /// is repeated after `create_dir_all` because the last component of the
    /// path may itself be a symlink planted in between.
    pub fn resolve_dest(&self, dest: &str) -> Result<PathBuf, String> {
        let joined = self.relative(dest)?;
        let Some(name) = joined.file_name() else {
            return Err(format!("`{dest}` has no file name"));
        };
        let parent = joined.parent().unwrap_or(&self.root);

        let mut existing = parent;
        loop {
            if !existing.starts_with(&self.root) {
                return Err(format!("`{dest}` resolves outside the workspace"));
            }
            match existing.canonicalize() {
                Ok(real) if real.starts_with(&self.root) => break,
                Ok(real) => {
                    return Err(format!(
                        "`{dest}` resolves outside the workspace ({})",
                        real.display()
                    ));
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => match existing.parent() {
                    Some(p) => existing = p,
                    None => return Err(format!("`{dest}` resolves outside the workspace")),
                },
                Err(e) => return Err(format!("{}: {e}", existing.display())),
            }
        }

        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        let real_parent = parent
            .canonicalize()
            .map_err(|e| format!("{}: {e}", parent.display()))?;
        if !real_parent.starts_with(&self.root) {
            return Err(format!(
                "`{dest}` resolves outside the workspace ({})",
                real_parent.display()
            ));
        }
        Ok(real_parent.join(name))
    }
}

/// The temporary files of every fetch this process is part way through, so
/// [`remove_unfinished_fetches`] can reach them when no destructor will run.
fn unfinished() -> MutexGuard<'static, BTreeSet<PathBuf>> {
    static UNFINISHED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
    UNFINISHED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Remove the temporary file of every fetch in this process that has not had
/// its last chunk. Dropping a [`FetchStaging`] already does this for its
/// own; this is for a process about to exit without running destructors (a
/// second ctrl-c, `std::process::exit`), so none is left beside its
/// destination.
pub fn remove_unfinished_fetches() {
    remove_unfinished_under(Path::new("/"));
}

/// [`remove_unfinished_fetches`] for the temporary files under `dir` only,
/// so a test does not reach another's.
fn remove_unfinished_under(dir: &Path) {
    let mut unfinished = unfinished();
    let ours: Vec<PathBuf> = unfinished
        .iter()
        .filter(|p| p.starts_with(dir))
        .cloned()
        .collect();
    for tmp in ours {
        unfinished.remove(&tmp);
        let _ = std::fs::remove_file(tmp);
    }
}

/// The receiving half of `ctx.fetch`, for the orchestrator and for a local
/// run alike: each fetch's chunks are written to a `.rustible-*` file beside
/// its destination, which is renamed over the destination only when the
/// chunk marked `last` arrives. The destination is therefore always either
/// what it held before or the whole new file: a fetch that stops part way,
/// because the target's read failed or the run ended, leaves it as it was.
/// Dropping this removes the temporary file of every fetch still in it.
#[derive(Default)]
pub struct FetchStaging {
    pending: HashMap<u32, Staged>,
}

/// One fetch in flight.
struct Staged {
    /// Where it lands, resolved inside the workspace.
    path: PathBuf,
    /// `.rustible-<random>` beside `path`, removed when dropped; `None` once
    /// it has been renamed into place.
    tmp: Option<NamedTempFile>,
    /// Bytes written so far, which the next chunk's offset must equal.
    written: u64,
}

impl Drop for Staged {
    fn drop(&mut self) {
        // The file itself goes with the `NamedTempFile`, dropped after this.
        if let Some(tmp) = &self.tmp {
            unfinished().remove(tmp.path());
        }
    }
}

impl FetchStaging {
    /// Write one chunk of request `req` toward `dest`, a path relative to
    /// `files`' root. The first chunk, at offset 0, resolves `dest` (refusing
    /// one that is a directory or anything else but a file or a symlink,
    /// before a byte is staged) and creates the temporary file beside it.
    /// On `last` the file is synced and renamed over the destination, and
    /// its path and size come back; until then, `None`. Any refusal or
    /// failure drops the fetch, its temporary file with it, and says why.
    pub fn chunk(
        &mut self,
        files: &WorkspaceFiles,
        req: u32,
        dest: &str,
        offset: u64,
        bytes: &[u8],
        last: bool,
    ) -> Result<Option<(PathBuf, u64)>, String> {
        let mut staged = match self.pending.remove(&req) {
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
        let Some(tmp) = staged.tmp.as_mut() else {
            unreachable!("a pending fetch has its temporary file")
        };
        tmp.write_all(bytes)
            .map_err(|e| format!("{}: {e}", tmp.path().display()))?;
        staged.written += bytes.len() as u64;
        if !last {
            self.pending.insert(req, staged);
            return Ok(None);
        }
        let Some(tmp) = staged.tmp.take() else {
            unreachable!("a pending fetch has its temporary file")
        };
        unfinished().remove(tmp.path());
        tmp.as_file()
            .sync_all()
            .map_err(|e| format!("{}: {e}", tmp.path().display()))?;
        tmp.persist(&staged.path)
            .map_err(|e| format!("{}: {}", staged.path.display(), e.error))?;
        Ok(Some((staged.path.clone(), staged.written)))
    }

    /// A temporary file beside `dest`'s resolved path, with the mode of the
    /// file it replaces, or 0666 less the umask for a new one, which is what
    /// a fetched file was created with when chunks were written in place.
    fn begin(files: &WorkspaceFiles, dest: &str) -> Result<Staged, String> {
        let path = files.resolve_dest(dest)?;
        let mode = match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => Some(m.permissions().mode() & 0o777),
            Ok(m) if m.file_type().is_symlink() => None,
            Ok(m) => {
                let what = if m.is_dir() {
                    "a directory"
                } else {
                    "not a regular file"
                };
                return Err(format!(
                    "`{dest}` is {what} in the workspace; a fetch only replaces a file, \
                     so move it away or fetch to another path"
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let dir = path.parent().unwrap_or(files.root());
        let tmp = tempfile::Builder::new()
            .prefix(".rustible-")
            .permissions(Permissions::from_mode(0o666))
            .tempfile_in(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        unfinished().insert(tmp.path().to_path_buf());
        let staged = Staged {
            path,
            tmp: Some(tmp),
            written: 0,
        };
        if let (Some(mode), Some(tmp)) = (mode, &staged.tmp) {
            tmp.as_file()
                .set_permissions(Permissions::from_mode(mode))
                .map_err(|e| format!("{}: {e}", tmp.path().display()))?;
        }
        Ok(staged)
    }
}

/// Collect a chunk stream into a writer (the binary's side of `local_file`).
pub fn write_chunks<W: Write>(w: &mut W, chunk: &Chunk, expected_offset: u64) -> io::Result<u64> {
    if chunk.offset != expected_offset {
        return Err(io::Error::other(format!(
            "chunk at offset {} but {} bytes received so far",
            chunk.offset, expected_offset
        )));
    }
    w.write_all(&chunk.bytes)?;
    Ok(expected_offset + chunk.bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(bytes: &[u8]) -> Vec<Chunk> {
        chunks(bytes).map(|c| c.unwrap()).collect()
    }

    #[test]
    fn run_dir_names_cannot_escape_the_temp_directory() {
        assert_eq!(run_dir_name("1a2b3c"), ".rustible-1a2b3c");
        // A run id is orchestrator-supplied and arrives over the wire.
        assert_eq!(run_dir_name("../../etc/cron.d/x"), ".rustible-etccrondx");
        assert_eq!(run_dir_name("a/../b"), ".rustible-ab");
        assert_eq!(run_dir_name("; rm -rf /"), ".rustible-rm-rf");
        assert_eq!(run_dir_name(""), ".rustible-unnamed");
        assert_eq!(run_dir_name("/////"), ".rustible-unnamed");
        assert!(run_dir_name(&"x".repeat(500)).len() <= 80);
    }

    #[test]
    fn chunking_always_ends_with_last() {
        let empty = collect(b"");
        assert_eq!(empty.len(), 1);
        assert!(empty[0].last && empty[0].bytes.is_empty() && empty[0].offset == 0);

        let small = collect(b"abc");
        assert_eq!(small.len(), 1);
        assert_eq!(
            (small[0].offset, small[0].last, &small[0].bytes[..]),
            (0, true, &b"abc"[..])
        );

        let exact = collect(&vec![1u8; CHUNK_SIZE]);
        assert_eq!(exact.len(), 2);
        assert!(!exact[0].last && exact[0].bytes.len() == CHUNK_SIZE);
        assert!(exact[1].last && exact[1].bytes.is_empty() && exact[1].offset == CHUNK_SIZE as u64);

        let big = collect(&vec![2u8; 2 * CHUNK_SIZE + 5]);
        assert_eq!(big.len(), 3);
        assert_eq!(
            big.iter()
                .map(|c| (c.offset, c.bytes.len(), c.last))
                .collect::<Vec<_>>(),
            vec![
                (0, CHUNK_SIZE, false),
                (CHUNK_SIZE as u64, CHUNK_SIZE, false),
                (2 * CHUNK_SIZE as u64, 5, true)
            ]
        );
    }

    #[test]
    fn chunks_reassemble_through_write_chunks() {
        let data: Vec<u8> = (0..CHUNK_SIZE as u32 * 2 + 17).map(|i| i as u8).collect();
        let mut out = Vec::new();
        let mut off = 0;
        for c in chunks(data.as_slice()) {
            off = write_chunks(&mut out, &c.unwrap(), off).unwrap();
        }
        assert_eq!(out, data);
        let bad = Chunk {
            offset: 999,
            bytes: Vec::new().into(),
            last: true,
        };
        assert!(write_chunks(&mut out, &bad, 0).is_err());
    }

    #[test]
    fn requests_outside_the_workspace_are_denied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("files")).unwrap();
        std::fs::write(root.join("files/a.txt"), b"hello").unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), b"nope").unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret"), root.join("files/link")).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("files/dirlink")).unwrap();

        let ws = WorkspaceFiles::new(root).unwrap();
        assert!(ws.resolve("files/a.txt").is_ok());
        assert!(ws.resolve("./files/a.txt").is_ok());
        assert!(ws.resolve("/etc/passwd").unwrap_err().contains("absolute"));
        assert!(ws.resolve("../x").unwrap_err().contains(".."));
        assert!(ws.resolve("files/../../x").unwrap_err().contains(".."));
        assert!(
            ws.resolve("files/missing")
                .unwrap_err()
                .contains("not found")
        );
        assert!(
            ws.resolve("files")
                .unwrap_err()
                .contains("not a regular file")
        );
        assert!(ws.resolve("files/link").unwrap_err().contains("outside"));
        assert!(
            ws.resolve("files/dirlink/secret")
                .unwrap_err()
                .contains("outside")
        );
        assert!(ws.resolve("").is_err());
    }

    #[test]
    fn fetch_destinations_are_confined_and_written_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let ws = WorkspaceFiles::new(dir.path()).unwrap();
        assert!(ws.resolve_dest("/tmp/x").unwrap_err().contains("absolute"));
        assert!(ws.resolve_dest("out/../../x").unwrap_err().contains(".."));

        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
        assert!(ws.resolve_dest("escape/x").unwrap_err().contains("outside"));

        // A denial creates nothing outside the workspace on the way to
        // saying no, however deep the refused destination is.
        assert!(
            ws.resolve_dest("escape/deep/nested/x")
                .unwrap_err()
                .contains("outside")
        );
        assert!(
            !outside.path().join("deep").exists(),
            "a refused fetch created directories outside the workspace"
        );
    }

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
        let mut fetches = FetchStaging::default();

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
        assert!(fetches.pending.is_empty());
    }

    /// A fetch that never gets its last chunk leaves the destination as it
    /// was, and its temporary file goes when the `FetchStaging` is dropped,
    /// or, for a process that exits without dropping it, with
    /// `remove_unfinished_fetches` (here limited to this test's directory,
    /// since the tests share the process). One that never had a destination leaves
    /// nothing at all.
    #[test]
    fn an_unfinished_fetch_leaves_the_destination_and_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("out/f"), b"old").unwrap();
        let files = WorkspaceFiles::new(dir.path()).unwrap();
        let mut fetches = FetchStaging::default();
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

        let mut fetches = FetchStaging::default();
        fetches
            .chunk(&files, 3, "out/g", 0, b"partial", false)
            .unwrap();
        remove_unfinished_under(files.root());
        assert_eq!(names(&dir.path().join("out")), ["f"]);
        std::mem::forget(fetches);
    }

    /// A chunk out of order is refused and ends its fetch, temporary file
    /// and all; so is a first chunk that does not start at 0, a destination
    /// outside the workspace, and, at the first chunk, before anything is
    /// staged, a destination that is a directory.
    #[test]
    fn a_chunk_out_of_order_or_onto_a_directory_ends_its_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let files = WorkspaceFiles::new(dir.path()).unwrap();
        let mut fetches = FetchStaging::default();
        fetches.chunk(&files, 1, "f", 0, b"abc", false).unwrap();
        let err = fetches.chunk(&files, 1, "f", 5, b"x", true).unwrap_err();
        assert_eq!(err, "`f`: chunk at offset 5 but 3 bytes received so far");
        assert_eq!(names(dir.path()), Vec::<String>::new());
        let err = fetches.chunk(&files, 1, "f", 3, b"x", true).unwrap_err();
        assert!(err.contains("nothing received before it"), "{err}");
        let err = fetches.chunk(&files, 2, "../f", 0, b"x", true).unwrap_err();
        assert!(err.contains("`..`"), "{err}");

        std::fs::create_dir(dir.path().join("d")).unwrap();
        let err = fetches.chunk(&files, 3, "d", 0, b"x", false).unwrap_err();
        assert!(err.contains("`d` is a directory in the workspace"), "{err}");
        assert_eq!(names(dir.path()), ["d"]);
        assert!(fetches.pending.is_empty());
    }

    /// The new file keeps the mode of the one it replaces, as the file
    /// written in place used to, and a symlink planted at the destination
    /// is replaced, not written through.
    #[test]
    fn a_fetch_keeps_the_mode_of_the_file_it_replaces_and_replaces_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("f");
        std::fs::write(&f, b"old").unwrap();
        std::fs::set_permissions(&f, Permissions::from_mode(0o640)).unwrap();
        let files = WorkspaceFiles::new(dir.path()).unwrap();
        let mut fetches = FetchStaging::default();
        fetches.chunk(&files, 1, "f", 0, b"new", true).unwrap();
        let meta = std::fs::metadata(&f).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o640);
        assert_eq!(std::fs::read(&f).unwrap(), b"new");

        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim");
        std::fs::write(&target, b"untouched").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("link")).unwrap();
        fetches
            .chunk(&files, 2, "link", 0, b"fetched", true)
            .unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"untouched");
        let link = dir.path().join("link");
        assert!(std::fs::symlink_metadata(&link).unwrap().is_file());
        assert_eq!(std::fs::read(&link).unwrap(), b"fetched");
    }
}
