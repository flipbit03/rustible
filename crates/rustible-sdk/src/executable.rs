//! Writing a file that a test then executes, so that the `exec` cannot fail
//! with `ETXTBSY` ("Text file busy").
//!
//! The tests share one process and run on threads of it. A file written here
//! with `std::fs::write` is open for writing for the length of the write, and
//! a thread that forks in that window gives its child a copy of the
//! descriptor. `O_CLOEXEC` closes the copy when the child execs, not when it
//! forks, so until then the file has a writer. Linux refuses to `exec` a
//! file that has one (rust-lang/rust#114554). Renaming the file once it is
//! written does not help, because the writer holds the inode, not the name.
//!
//! So a child writes the file instead. `sh` opens it and `cat` fills it from
//! a pipe, and both have exited before [`write`] returns. This process never
//! holds the descriptor, so nothing it forks can inherit one, and the race is
//! closed rather than made rarer.
//!
//! `rustible-cli` has a copy, `write_executable` in `run.rs`'s tests, for its
//! own stand-in `sudo`. One helper for both crates would have to be a public
//! item of this crate, which is SDK surface, so each crate keeps its own.

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// Write `bytes` to `path` from a child process, then give it `mode`.
pub(crate) fn write(path: &Path, bytes: &[u8], mode: u32) {
    let mut child = Command::new("/bin/sh")
        .args(["-c", r#"exec cat > "$1""#, "sh"])
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "writing {}: {status}", path.display());
    // `chmod` takes a path, not a descriptor, so it opens no writer.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}
