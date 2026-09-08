//! File streaming over the channel (vision doc 5.6): the orchestrator-side
//! half. `WorkspaceFiles` serves `FileRequest`s from the workspace root and
//! writes `FetchChunk`s under it, denying anything that resolves outside; the
//! `chunks` iterator splits any reader into `CHUNK_SIZE` pieces. The binary's
//! half (`ctx.local_file`, `ctx.local_secret`, `ctx.fetch`) lives in `ctx`.
//!
//! This lives in the SDK rather than the CLI so a local run (no orchestrator)
//! serves files through exactly the same rules, and so the rules have one
//! set of tests.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

pub use crate::protocol::CHUNK_SIZE;

/// One piece of a streamed file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub offset: u64,
    pub bytes: Vec<u8>,
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
        let mut buf = vec![0u8; CHUNK_SIZE];
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
    /// directories. The parent is canonicalized after creation so a symlink
    /// inside the workspace cannot redirect the write outside it.
    pub fn resolve_dest(&self, dest: &str) -> Result<PathBuf, String> {
        let joined = self.relative(dest)?;
        let Some(name) = joined.file_name() else {
            return Err(format!("`{dest}` has no file name"));
        };
        let parent = joined.parent().unwrap_or(&self.root);
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

    /// Write one fetched chunk. Offset 0 creates or truncates the file; later
    /// chunks must arrive in order.
    pub fn write_chunk(&self, dest: &str, offset: u64, bytes: &[u8]) -> Result<PathBuf, String> {
        let path = self.resolve_dest(dest)?;
        let mut f = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(offset == 0)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        let len = f
            .metadata()
            .map(|m| m.len())
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if offset != len {
            return Err(format!(
                "`{dest}`: chunk at offset {offset} but file has {len} bytes"
            ));
        }
        f.seek(SeekFrom::Start(offset))
            .and_then(|_| f.write_all(bytes))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
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
            bytes: vec![],
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

        ws.write_chunk("out/host", 0, b"abc").unwrap();
        ws.write_chunk("out/host", 3, b"def").unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("out/host")).unwrap(),
            b"abcdef"
        );
        assert!(
            ws.write_chunk("out/host", 2, b"x")
                .unwrap_err()
                .contains("offset")
        );
        // Offset 0 starts over.
        ws.write_chunk("out/host", 0, b"z").unwrap();
        assert_eq!(std::fs::read(dir.path().join("out/host")).unwrap(), b"z");
    }
}
