//! Archive extraction. Ansible's `ansible.builtin.unarchive` (with
//! `remote_src: yes`; the archive is a file on the target).
//!
//! [`Extracted`] unpacks a tar archive, plain or compressed with gzip, xz
//! or zstd, into an existing directory. Everything is pure Rust (vision
//! 5.3): `tar`, `flate2` (miniz_oxide), `lzma-rust2` and `ruzstd`. bzip2
//! and zip are not supported. The format is detected from the file's magic
//! bytes, never from its name, so `release.tgz` and `blob.bin` work alike
//! and a mislabelled file is refused rather than misread.
//!
//! The archive is read and its members are written a chunk at a time, so
//! an archive or a member of any size is extracted in a few MiB of memory,
//! whatever the format and whether or not the step is escalated
//! ([`Extracted`] has the details under **Memory**).
//!
//! An archive that is not a plain tarball (a URL, a zip) is somebody
//! else's job: fetch with [`crate::http::Download`] first (vision 6.7).

use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use rustible_sdk::backend::{FileKind, WriteAttrs};
use rustible_sdk::prelude::*;

use crate::file::{Owner, set_mode_and_owner};

/// A supported archive format, as detected from the first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// No compression: a `ustar` header block at offset 257 of the file
    /// itself.
    Tar,
    /// gzip (magic `1f 8b`), read with `flate2`'s multi-member decoder, so a
    /// tarball concatenated by `pigz` is read to its end and not just to the
    /// first member.
    TarGz,
    /// xz (magic `fd 37 7a 58 5a 00`), read with `lzma-rust2`.
    TarXz,
    /// zstd (magic `28 b5 2f fd`), read frame by frame with `ruzstd` so
    /// several frames, such as `cat` of several `.zst` files, are all
    /// decoded, skippable frames between them are skipped, and each frame's
    /// content checksum is verified when it has been read to its end. Decoded as it is read, like the others: memory is the
    /// frame's window, which the compressor chose (8 MiB at `zstd -19`;
    /// `ruzstd` refuses a window over 100 MiB), plus 1 MiB decoded ahead.
    TarZst,
}

impl Format {
    /// The word used in diffs and error messages (`tar`, `tar.gz`, `tar.xz`,
    /// `tar.zst`). It names what the magic bytes said, which need not match
    /// what the file is called.
    pub fn name(self) -> &'static str {
        match self {
            Format::Tar => "tar",
            Format::TarGz => "tar.gz",
            Format::TarXz => "tar.xz",
            Format::TarZst => "tar.zst",
        }
    }
}

const SUPPORTED: &str = "supported formats: tar, tar.gz, tar.xz, tar.zst (detected by magic bytes)";

/// The format of an archive from its first bytes (at least 512 for a plain
/// tar). Pure. Names the format when it recognises one it does not
/// support (zip, bzip2, 7z, rar) and lists the supported ones otherwise.
pub fn detect_format(head: &[u8]) -> std::result::Result<Format, String> {
    if head.starts_with(&[0x1f, 0x8b]) {
        return Ok(Format::TarGz);
    }
    if head.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0x00]) {
        return Ok(Format::TarXz);
    }
    if head.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
        return Ok(Format::TarZst);
    }
    if is_tar_header(head) {
        return Ok(Format::Tar);
    }
    let known = if head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") {
        Some("zip")
    } else if head.starts_with(b"BZh") {
        Some("bzip2")
    } else if head.starts_with(&[b'7', b'z', 0xbc, 0xaf, 0x27, 0x1c]) {
        Some("7z")
    } else if head.starts_with(b"Rar!") {
        Some("rar")
    } else {
        None
    };
    match known {
        Some(name) => Err(format!("{name} archives are not supported; {SUPPORTED}")),
        None => Err(format!("not a recognised archive; {SUPPORTED}")),
    }
}

/// A ustar/GNU tar header block: `ustar` at offset 257.
fn is_tar_header(head: &[u8]) -> bool {
    head.len() >= 512 && &head[257..262] == b"ustar"
}

/// Normalise an archive member path and refuse what could escape the
/// destination (the tar-slip guard). Pure. Absolute paths and `..`
/// components are errors; `.` components are dropped, so `./a/b` is `a/b`
/// and `./` is empty (the caller skips an empty path).
pub fn validate_entry_path(raw: &Path) -> std::result::Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for c in raw.components() {
        match c {
            Component::Normal(n) => out.push(n),
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("`{}` is an absolute path", raw.display()));
            }
            Component::ParentDir => {
                return Err(format!("`{}` contains `..`", raw.display()));
            }
        }
    }
    Ok(out)
}

/// Drop the first `n` components (`tar --strip-components=N`). `None` when
/// nothing is left, which means the entry is skipped. Pure.
pub fn strip_components(rel: &Path, n: usize) -> Option<PathBuf> {
    let rest: PathBuf = rel.components().skip(n).collect();
    (!rest.as_os_str().is_empty()).then_some(rest)
}

/// Refuse a symlink target that points outside the destination: absolute
/// targets, and relative ones whose `..` climb above the root. Pure.
/// `link` is the (normalised, stripped) path of the symlink entry.
pub fn validate_link_target(link: &Path, target: &Path) -> std::result::Result<(), String> {
    if target.is_absolute() {
        return Err(format!(
            "symlink `{}` -> `{}` has an absolute target",
            link.display(),
            target.display()
        ));
    }
    let mut depth = link.components().count().saturating_sub(1) as isize;
    for c in target.components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!(
                        "symlink `{}` -> `{}` points outside the destination",
                        link.display(),
                        target.display()
                    ));
                }
            }
            Component::CurDir => {}
            Component::RootDir | Component::Prefix(_) => unreachable!("relative target"),
        }
    }
    Ok(())
}

/// What one archive member is, after validation and stripping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// A regular file, which for tar also covers the contiguous and GNU
    /// sparse entry types. `apply` streams it with `write_from`.
    File,
    /// A directory the archive names in its own right. `apply` creates it and
    /// any missing parent with `mkdir_all`.
    Dir,
    /// Target as written in the archive (relative, validated).
    Symlink(PathBuf),
    /// Path of the earlier regular file this entry duplicates (normalised,
    /// stripped).
    Hardlink(PathBuf),
}

/// One archive member the op will create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Relative to the destination.
    pub path: PathBuf,
    /// What to create there and, for a link, where it points.
    pub kind: Kind,
    /// Permission bits (`& 0o7777`).
    pub mode: u32,
    /// Content length as the archive declares it (the header's `size`,
    /// overridden by a pax `size` record or a GNU sparse map's real size),
    /// `0` for directories, symlinks and hard links. Attacker-controlled and
    /// unverified, so it is only counted in the report: nothing is reserved
    /// from it, and `apply` checks the stream against the length the
    /// archive declares where it reads the member.
    pub size: u64,
}

/// Output of [`Extracted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
    /// The archive, as given to [`Extracted::from_path`].
    pub src: PathBuf,
    /// The directory it was (or would be) unpacked into, as given to
    /// [`ExtractedBuilder::to`].
    pub dest: PathBuf,
    /// `None` when the `creates` marker made the step `ok` without opening
    /// the archive.
    pub format: Option<Format>,
    /// Regular files written, hard links included: each one lands as a full
    /// copy of the file it points at.
    pub files: usize,
    /// Directory members in the archive. A parent directory `apply` has to
    /// create for a file whose own directory the archive never names is not
    /// counted.
    pub dirs: usize,
    /// Symlink members. Hard links are counted in `files`, not here.
    pub symlinks: usize,
    /// Bytes of regular-file content, as the archive declares it: the
    /// headers' sizes, or a pax `size` record or a GNU sparse map's real
    /// size where the archive gives one.
    /// A hard link's header carries `size == 0`, so a hard link raises
    /// `files` but adds nothing here, even though `apply` writes it as a
    /// full copy of its target: this counts what the archive holds, not
    /// what lands on disk. Saturating, since the headers are untrusted.
    pub bytes: u64,
    /// Members dropped by `.strip_components` (fewer components than
    /// stripped) or with an empty path such as `./`.
    pub skipped: usize,
    /// Whether the archive was (or would be) extracted.
    pub extracted: bool,
}

/// Extract a tar archive on the target into a directory. Ansible's
/// `unarchive` with `remote_src: yes`, `src`, `dest`, `creates`,
/// `extra_opts: [--strip-components=N]`, `owner`/`group`.
///
/// ```no_run
/// # use rustible_std::archive;
/// let op = archive::Extracted::from_path("/tmp/tool-1.2.tar.gz")
///     .to("/opt/tool")
///     .strip_components(1)
///     .creates("bin/tool");
/// ```
///
/// **Idempotence is the `creates` marker**, as in Ansible: when
/// `.creates(path)` exists the step is `ok` and the archive is not even
/// opened. Without it the op extracts on every run and every run is
/// `changed`, so it flags itself with `always_changes` and the report marks
/// the step. (Ansible additionally runs `tar --diff`; this op does not.) A
/// relative `creates` is taken under `dest`.
///
/// **What `check` does.** Reads the archive through `sys`, detects the
/// format from its magic bytes, decompresses and walks every member
/// without writing, and refuses the whole archive when any member:
///
/// - has an absolute path or a `..` component (tar-slip), naming it;
/// - is a symlink to an absolute target or one climbing outside `dest`;
/// - sits under an earlier symlink member (a write through it would land
///   elsewhere);
/// - is a hard link to something not extracted before it;
/// - is a device, fifo or other special file;
/// - is a pax sparse member (GNU tar's `--format=posix -S`, or any sparse
///   file in an archive from macOS's `tar`), which it cannot expand. An
///   old-GNU sparse member (`--format=gnu -S`) extracts.
///
/// It also refuses when `dest` is missing (vision 6.7: create it with
/// [`crate::file::Directory`]) or not a directory, when a member's path is
/// already a directory where a file goes (or a file or symlink where a
/// directory goes), and when any directory on a member's path exists as a
/// symlink on disk. The counts in the diff come from the same walk, so a
/// dry run shows what an extraction would write; the report itself only
/// exists once `apply` has run (vision 12).
///
/// **What `apply` does.** Writes each member `check` planned, reading its
/// data from the archive once more (an archive changed since `check` is
/// refused at the first member that differs), through `sys`: files
/// streamed with `write_from` and the archive's permission bits,
/// directories with `mkdir_all`, symlinks made under a temporary name
/// beside their path and renamed over the file or link already there (a
/// directory there is refused at `check`), hard links as copies of the
/// already-extracted file, streamed the same way. Ownership from the
/// archive is ignored; `.owner(uid, gid)` sets one owner on every file and
/// directory (not on symlinks). Modification times are not restored.
///
/// **Mode and owner before the rename.** A file member is staged beside
/// its path, given its mode and the `.owner(..)` there, and only then
/// renamed into place, so its content is never readable at a wider mode
/// than the archive gives it, nor setuid under another owner, even for a
/// moment. An owner the identity cannot give (no root) fails the step with
/// nothing at that member's path. A directory gets its mode and owner once
/// made.
///
/// **Atomicity: each member, not the archive.** A file member is written
/// whole or not at all: one whose data ends before the length the archive
/// declares for it (the archive was truncated or replaced since `check`),
/// or whose compressed stream is corrupt, fails the step with nothing
/// written at its path, no parent directory left that was made for it, and
/// a file already there as it was. Members extracted before it in the same
/// run stay; the step is not rolled back, as with GNU tar and Ansible's
/// `unarchive`. The next run extracts the whole archive again, unless the
/// `.creates` marker is among the members already written, so make the
/// marker a member the archive lists last, or a path outside it.
///
/// **Memory.** Constant, about one chunk plus the decoder's state, for a
/// plain tar and for gzip, xz and zstd alike, escalated or not: the archive
/// is read a chunk at a time and each member is streamed to disk, never
/// held whole. zstd's state is the frame's window (8 MiB at `zstd -19`;
/// frames asking for over 100 MiB are refused), xz's its dictionary.
///
/// **Escalation.** Under `ctx.as_root()` or `ctx.as_user(..)` the archive is
/// read, and its members are written, through that identity's one helper,
/// interleaved a chunk at a time, so no size limit applies there either.
///
/// **Limits.** A sparse member's holes are written as data, not kept as
/// holes, so it takes its full real size on disk; an old-GNU sparse member
/// (`--format=gnu -S`) extracts that way, and a pax sparse one is refused
/// at `check` (above). Files already in `dest` that the archive does not
/// mention are left alone.
#[derive(Debug, Clone)]
pub struct Extracted {
    src: PathBuf,
    dest: PathBuf,
    creates: Option<PathBuf>,
    strip: usize,
    owner: Option<Owner>,
}

/// An `Extracted` with a source but no destination yet; `.to(dest)`
/// finishes it.
#[derive(Debug, Clone)]
pub struct ExtractedBuilder {
    src: PathBuf,
}

impl Extracted {
    /// The archive, a file on the target.
    pub fn from_path(src: impl Into<PathBuf>) -> ExtractedBuilder {
        ExtractedBuilder { src: src.into() }
    }

    /// Skip (report `ok`) when this path exists; relative to `dest`.
    /// Ansible's `creates`.
    pub fn creates(mut self, p: impl Into<PathBuf>) -> Self {
        self.creates = Some(p.into());
        self
    }

    /// Drop this many leading path components from every member
    /// (`--strip-components=N`); members with nothing left are skipped.
    pub fn strip_components(mut self, n: usize) -> Self {
        self.strip = n;
        self
    }

    /// Numeric owner (`chown uid:gid`) for every extracted file and
    /// directory.
    pub fn owner(mut self, uid: u32, gid: u32) -> Self {
        self.owner = Some(Owner { uid, gid });
        self
    }

    fn marker(&self) -> Option<PathBuf> {
        self.creates.as_ref().map(|c| {
            if c.is_absolute() {
                c.clone()
            } else {
                self.dest.join(c)
            }
        })
    }

    /// The archive's tar stream, read through `sys` a few chunks at a time:
    /// the first block is read to detect the format, then handed to the
    /// decoder ahead of the rest of the file, so nothing is read twice and
    /// nothing is held whole.
    fn open<'s>(&self, sys: &'s System) -> Result<(Format, tar::Archive<Box<dyn Read + 's>>)> {
        let mut file = sys
            .open_read(&self.src)
            .with_context(|| format!("reading archive {}", self.src.display()))?;
        let head = read_head(&mut file)
            .with_context(|| format!("reading archive {}", self.src.display()))?;
        let format =
            detect_format(&head).map_err(|e| Error::msg(format!("{}: {e}", self.src.display())))?;
        let mut reader = decompress(format, Box::new(Cursor::new(head).chain(file)));
        if format != Format::Tar {
            // Look at the first block before handing over to `tar`, so a
            // gzipped text file gets a plain answer.
            let head = match read_head(&mut reader) {
                Ok(head) => head,
                Err(e) => bail!("{}: decompressing: {e}", self.src.display()),
            };
            if !is_tar_header(&head) {
                bail!(
                    "{}: the {} stream does not contain a tar archive",
                    self.src.display(),
                    format.name()
                );
            }
            reader = Box::new(Cursor::new(head).chain(reader));
        }
        Ok((format, tar::Archive::new(reader)))
    }

    /// Read a zstd stream on to its end once `tar` has stopped at the
    /// archive's end marker, so the last frame's checksum is verified and
    /// anything after the last frame is refused, as they were when the
    /// stream was decoded whole. What is left is the tar's padding to its
    /// record size, a few KiB at most. gzip and xz streams are left where
    /// `tar` stopped, as before.
    fn finish<R: Read>(&self, format: Format, archive: tar::Archive<R>) -> Result<()> {
        if format == Format::TarZst {
            std::io::copy(&mut archive.into_inner(), &mut std::io::sink())
                .with_context(|| format!("{}: decompressing", self.src.display()))?;
        }
        Ok(())
    }

    /// Everything the archive would create, entry by entry in archive order,
    /// or why it is refused.
    fn plan(&self, sys: &System) -> Result<(Format, Vec<PlannedEntry>)> {
        let (format, mut archive) = self.open(sys)?;
        let entries = walk(&mut archive, self.strip)
            .with_context(|| format!("{}: refusing to extract", self.src.display()))?;
        self.finish(format, archive)?;
        let members: Vec<Member> = entries.iter().filter_map(|e| e.member.clone()).collect();
        self.check_destination(sys, &members)?;
        Ok((format, entries))
    }

    /// The on-disk side of the guard: `dest` is a directory, nothing on a
    /// member's path is a symlink, and no member lands on the wrong kind.
    fn check_destination(&self, sys: &System, members: &[Member]) -> Result<()> {
        match sys.stat_follow(&self.dest)? {
            Some(s) if s.kind == FileKind::Dir => {}
            Some(s) => bail!("{} is not a directory ({:?})", self.dest.display(), s.kind),
            // Under --check an earlier file::Directory may create it (vision 12).
            None if sys.check_mode() => {}
            None => bail!(
                "{} does not exist; create it first with file::Directory",
                self.dest.display()
            ),
        }
        let mut ancestors = BTreeSet::new();
        for m in members {
            let mut p = m.path.parent();
            while let Some(a) = p.filter(|a| !a.as_os_str().is_empty()) {
                ancestors.insert(a.to_path_buf());
                p = a.parent();
            }
        }
        for a in &ancestors {
            let full = self.dest.join(a);
            match sys.stat(&full)? {
                None => {}
                Some(s) if s.kind == FileKind::Dir => {}
                Some(s) if s.kind == FileKind::Symlink => bail!(
                    "{} is a symlink; refusing to extract through it",
                    full.display()
                ),
                Some(_) => bail!(
                    "{} is not a directory but the archive puts entries under it",
                    full.display()
                ),
            }
        }
        for m in members {
            let full = self.dest.join(&m.path);
            let Some(existing) = sys.stat(&full)? else {
                continue;
            };
            match (&m.kind, existing.kind) {
                (Kind::Dir, FileKind::Dir) => {}
                (Kind::Dir, other) => bail!(
                    "{} exists and is not a directory ({other:?}); the archive has a directory there",
                    full.display()
                ),
                (_, FileKind::Dir) => bail!(
                    "{} is a directory; the archive has a {} there",
                    full.display(),
                    match m.kind {
                        Kind::Symlink(_) => "symlink",
                        _ => "file",
                    }
                ),
                (_, FileKind::Other) => {
                    bail!("{} exists and is not a regular file", full.display())
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Write one planned member. `data` is the member's stream from the
    /// archive and `declared` the length the archive gives it there (its
    /// header, a pax `size` record, or a sparse map's real size): a file
    /// member is streamed into a staged file beside its path, which is
    /// renamed over the path only once the stream has come to exactly that
    /// length, so a member cut short writes nothing at its path.
    fn write_member(
        &self,
        sys: &System,
        dest: &Path,
        m: &Member,
        declared: u64,
        data: &mut dyn Read,
    ) -> Result<()> {
        let full = dest.join(&m.path);
        // A file's mode and owner go to the staged file before the rename
        // (decision 24 on #85), so its content is never readable wider than
        // the member's mode, nor setuid under another owner, even for a
        // moment. `write_from` sets them in `set_mode_and_owner`'s order.
        let attrs = WriteAttrs {
            mode: Some(m.mode),
            owner: self.owner.map(|o| (o.uid, o.gid)),
        };
        match &m.kind {
            Kind::File => {
                // The parents the archive implies for the member, made for
                // the staged file and taken away again, if still empty, when
                // the member is refused: a member that never lands leaves
                // nothing behind (issue #52).
                let made = self.missing_parents(sys, dest, &full)?;
                if let Some(parent) = made.first() {
                    sys.mkdir_all(parent)?;
                }
                let mut member = Declared {
                    data,
                    declared,
                    read: 0,
                    short: false,
                    failed: false,
                };
                let written = sys.write_from(&full, &mut member, Some(attrs));
                if let Err(e) = written {
                    for d in &made {
                        let _ = sys.remove(d);
                    }
                    // The member's reader stops where the archive does,
                    // without an error, so a stream that ends inside the
                    // member reads as a short file. Only the archive's own
                    // length for it says the data is all there; until it
                    // does, the staged file is never renamed over `full`.
                    ensure!(
                        !member.short,
                        "member `{}` ends after {} of the {declared} bytes the archive declares \
                         for it; the archive is truncated, or changed between check and apply, \
                         and nothing was written at {}; run the step again once the archive is \
                         whole",
                        m.path.display(),
                        member.read,
                        full.display()
                    );
                    if member.failed {
                        return Err(e.context(format!(
                            "reading member `{}` from the archive failed part way (truncated or \
                             corrupt), and nothing was written at {}",
                            m.path.display(),
                            full.display()
                        )));
                    }
                    return Err(e);
                }
                return Ok(());
            }
            Kind::Hardlink(target) => {
                if let Some(parent) = full.parent() {
                    sys.mkdir_all(parent)?;
                }
                // A copy of the file extracted earlier, read and written a
                // chunk at a time.
                let src = sys.open_read(dest.join(target))?;
                sys.write_from(&full, src, Some(attrs))?;
                return Ok(());
            }
            Kind::Dir => {
                sys.mkdir_all(&full)?;
            }
            Kind::Symlink(target) => {
                if let Some(parent) = full.parent() {
                    sys.mkdir_all(parent)?;
                }
                // Made beside `full` under a temporary name and renamed over
                // it, so whatever was at `full` stays there until the new link
                // replaces it: a failed `symlink` leaves it as it was, and a
                // failed `rename` takes the temporary link away again. The
                // name is short and random, not `full`'s with a suffix: a
                // member's name may already be near `NAME_MAX`, and a link
                // left by a killed run never collides with the next.
                let tmp = full.with_file_name(format!(".rustible-{}", random_hex()));
                let link = || -> Result<()> {
                    sys.symlink(target, &tmp)?;
                    if let Err(e) = sys.rename(&tmp, &full) {
                        let _ = sys.remove(&tmp);
                        return Err(e);
                    }
                    Ok(())
                };
                link().with_context(|| {
                    format!(
                        "linking member `{}` at {}",
                        m.path.display(),
                        full.display()
                    )
                })?;
                // `set_mode` and `set_owner` follow links; neither is
                // applied to a symlink member.
                return Ok(());
            }
        }
        // A directory, made with `mkdir_all`: its full mode, its owner, then
        // its full mode again when it carries setuid or setgid, which
        // `chown` clears on a directory on macOS.
        set_mode_and_owner(sys, &full, true, Some(m.mode), self.owner)
    }

    /// The directories between `dest` and `full` that do not exist yet,
    /// deepest first: what `mkdir_all` of `full`'s parent would create.
    fn missing_parents(&self, sys: &System, dest: &Path, full: &Path) -> Result<Vec<PathBuf>> {
        let mut made = vec![];
        let mut p = full.parent();
        while let Some(dir) = p.filter(|d| d.starts_with(dest) && *d != dest) {
            if sys.stat(dir)?.is_some() {
                break;
            }
            made.push(dir.to_path_buf());
            p = dir.parent();
        }
        Ok(made)
    }
}

impl ExtractedBuilder {
    /// The directory to extract into. Must exist. Finishes the builder.
    pub fn to(self, dest: impl Into<PathBuf>) -> Extracted {
        Extracted {
            src: self.src,
            dest: dest.into(),
            creates: None,
            strip: 0,
            owner: None,
        }
    }
}

/// The first tar block of `r`, or all of it when it is shorter: what
/// [`detect_format`] and the tar check after decompression look at.
fn read_head(r: &mut dyn Read) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::with_capacity(BLOCK);
    r.take(BLOCK as u64).read_to_end(&mut head)?;
    Ok(head)
}

/// One tar block, and the most of an archive read to detect its format.
const BLOCK: usize = 512;

/// A reader of the tar stream inside `raw`, decompressing as it is read.
fn decompress<'a>(format: Format, raw: Box<dyn Read + 'a>) -> Box<dyn Read + 'a> {
    match format {
        Format::Tar => raw,
        Format::TarGz => Box::new(flate2::read::MultiGzDecoder::new(raw)),
        Format::TarXz => Box::new(lzma_rust2::XzReader::new(raw, true)),
        Format::TarZst => Box::new(Zstd::new(raw)),
    }
}

/// How much a zstd frame is decoded ahead of what is read from it, beyond
/// the window the frame keeps.
const ZSTD_AHEAD: usize = 1 << 20;

/// Every frame of a zstd stream, decoded as it is read: skippable frames
/// skipped, each frame's content checksum verified once it is read to its
/// end. `ruzstd`'s own `StreamingDecoder` stops at the end of the first
/// frame, and a stream may hold several, as `cat` of several `.zst` files
/// does. Holds the
/// frame's window (what the compressor chose, 8 MiB at `zstd -19`; `ruzstd`
/// refuses a frame asking for more than 100 MiB) plus up to
/// [`ZSTD_AHEAD`] decoded ahead of the reader.
struct Zstd<R: Read> {
    src: std::io::BufReader<R>,
    dec: ruzstd::decoding::FrameDecoder,
    /// A frame was begun and not yet read to its end.
    in_frame: bool,
}

impl<R: Read> Zstd<R> {
    fn new(src: R) -> Self {
        Zstd {
            src: std::io::BufReader::new(src),
            dec: ruzstd::decoding::FrameDecoder::new(),
            in_frame: false,
        }
    }
}

/// A zstd failure as the reader reports it, prefixed so the step's error
/// says which decoder spoke.
fn zstd_error(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::other(format!("zstd: {e}"))
}

impl<R: Read> Read for Zstd<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::BufRead;

        use ruzstd::decoding::BlockDecodingStrategy;
        use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};

        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.in_frame {
                if self.dec.can_collect() > 0 {
                    let n = self.dec.read(buf)?;
                    if n > 0 {
                        return Ok(n);
                    }
                }
                if !self.dec.is_finished() {
                    self.dec
                        .decode_blocks(&mut self.src, BlockDecodingStrategy::UptoBytes(ZSTD_AHEAD))
                        .map_err(zstd_error)?;
                    continue;
                }
                // The frame is decoded and read to its end, so the checksum
                // covers every byte it produced.
                if let (Some(want), Some(got)) = (
                    self.dec.get_checksum_from_data(),
                    self.dec.get_calculated_checksum(),
                ) && want != got
                {
                    return Err(zstd_error(format!(
                        "frame checksum mismatch (got {got:08x}, want {want:08x})"
                    )));
                }
                self.in_frame = false;
            }
            // Between frames: the end of the stream, a skippable frame, or
            // the next frame.
            if self.src.fill_buf()?.is_empty() {
                return Ok(0);
            }
            match self.dec.init(&mut self.src) {
                Ok(()) => self.in_frame = true,
                Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                    length,
                    ..
                })) => {
                    let skipped = std::io::copy(
                        &mut (&mut self.src).take(u64::from(length)),
                        &mut std::io::sink(),
                    )?;
                    if skipped != u64::from(length) {
                        return Err(zstd_error("truncated skippable frame"));
                    }
                }
                Err(e) => return Err(zstd_error(e)),
            }
        }
    }
}

/// A file member's data as `write_from` reads it, which fails rather than
/// end before the length the archive declares for the member: the
/// member's own reader stops where the archive does, without an error, and
/// an error is what keeps the staged file from being renamed into place.
struct Declared<'a> {
    data: &'a mut dyn Read,
    declared: u64,
    /// Bytes read so far.
    read: u64,
    /// The data ended before `declared`.
    short: bool,
    /// Reading the archive failed (a corrupt or cut compressed stream).
    failed: bool,
}

impl Read for Declared<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = match self.data.read(buf) {
            Ok(n) => n,
            Err(e) => {
                self.failed |= e.kind() != std::io::ErrorKind::Interrupted;
                return Err(e);
            }
        };
        if n == 0 && !buf.is_empty() && self.read < self.declared {
            self.short = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "the member ends before its declared length",
            ));
        }
        self.read += n as u64;
        Ok(n)
    }
}

/// Sixteen hex digits nobody can predict, for a temporary name. Each
/// `RandomState` is keyed from the OS's randomness (per thread, then stepped
/// for every new one), so hashing nothing with a fresh one is enough and
/// needs no dependency, as the SDK's backup names do.
fn random_hex() -> String {
    use std::hash::BuildHasher;
    let h = std::collections::hash_map::RandomState::new().hash_one(());
    format!("{h:016x}")
}

/// What a member is, for a collision message.
fn kind_label(kind: &Kind) -> &'static str {
    match kind {
        Kind::Dir => "directory",
        Kind::File => "file",
        Kind::Symlink(_) => "symlink",
        Kind::Hardlink(_) => "hard link",
    }
}

/// What the refusal names when `entry` carries pax sparse records, `None`
/// when it carries none. GNU tar's `--format=posix -S` writes a sparse
/// member in three formats (0.0, 0.1, 1.0), each marked by `GNU.sparse.*`
/// records, and macOS's `tar` (bsdtar) writes 1.0 for any sparse file
/// unless given `--no-read-sparse`. The `tar` crate expands none of them:
/// 0.0 lands at the right path with its holes squeezed out, 0.1 and 1.0
/// under `GNUSparseFile.<pid>/`, and 1.0 with the sparse map as text ahead
/// of the data. 0.1 and 1.0 put a placeholder in the header and the real name in a
/// `GNU.sparse.name` record, so that record is the name when there is one,
/// and `raw`, the member's path, otherwise. A global header (`g`) is no
/// member, and is named as what it is.
///
/// The `tar` crate returns a global header as an entry of its own, so its
/// records are read as bytes and scanned for `GNU.sparse.`, without
/// parsing; an `x` header ahead of it, which the crate attaches to it, is
/// checked parsed, as a member's. A member's own records the crate has already read and offers
/// only parsed, so a record it cannot parse is skipped, as the crate's own
/// xattr extraction skips it, unlike GNU tar, which reads some of those
/// (a sparse record there can go unseen; `[ISSUE-80]` has the shapes).
/// The crate splits records on newlines, so a value holding one (a binary
/// xattr, a multi-line `comment`) reads as malformed, and refusing the
/// archive over it would refuse legal archives.
fn pax_sparse<R: Read>(entry: &mut tar::Entry<'_, R>, raw: &Path) -> Result<Option<String>> {
    if entry.header().entry_type().is_pax_global_extensions() {
        let mut records = vec![];
        entry
            .read_to_end(&mut records)
            .context("reading the archive's pax global header")?;
        if records.windows(11).any(|w| w == b"GNU.sparse.") {
            return Ok(Some(
                "the archive's pax global header declares GNU sparse records".to_string(),
            ));
        }
        // A held `x` header is attached to the next entry the crate
        // returns, and a global header counts; GNU tar applies it to the
        // member after. Read after the global header's own data, which
        // `pax_extensions` would otherwise read in its place.
        let attached = entry
            .pax_extensions()
            .context("reading the pax header ahead of the archive's global header")?
            .is_some_and(|r| {
                r.flatten()
                    .any(|r| r.key_bytes().starts_with(b"GNU.sparse."))
            });
        return Ok(attached.then(|| {
            "a pax header ahead of the archive's global header declares GNU sparse records"
                .to_string()
        }));
    }
    let Some(records) = entry
        .pax_extensions()
        .with_context(|| format!("member `{}`: reading its pax records", raw.display()))?
    else {
        return Ok(None);
    };
    let mut sparse = false;
    let mut name = None;
    for record in records.flatten() {
        let key = record.key_bytes();
        if key.starts_with(b"GNU.sparse.") {
            sparse = true;
            if key == b"GNU.sparse.name" {
                name = Some(String::from_utf8_lossy(record.value_bytes()).into_owned());
            }
        }
    }
    Ok(sparse.then(|| {
        let name = name.unwrap_or_else(|| raw.to_string_lossy().into_owned());
        format!("`{}` is a pax sparse member", quoted(&name))
    }))
}

/// `name`, from the archive, made safe to print: control characters
/// escaped, so it cannot drive the operator's terminal, and cut at 256
/// bytes with `…` after it.
fn quoted(name: &str) -> String {
    const CAP: usize = 256;
    let mut out = String::new();
    for c in name.chars() {
        let piece: String = if c.is_control() {
            c.escape_default().collect()
        } else {
            c.to_string()
        };
        if out.len() + piece.len() > CAP {
            out.push('…');
            break;
        }
        out.push_str(&piece);
    }
    out
}

/// Walk every member in order, validating paths and link targets, without
/// reading any data. Errors name the member.
fn walk<R: Read>(archive: &mut tar::Archive<R>, strip: usize) -> Result<Vec<PlannedEntry>> {
    let mut out = vec![];
    let mut symlinks: Vec<PathBuf> = vec![];
    let mut files: BTreeSet<PathBuf> = BTreeSet::new();
    // Every path the archive has claimed so far, and what it claimed it as.
    // Two members can collide inside one archive without anything being on
    // disk yet, which `check_destination` cannot see.
    let mut claimed: std::collections::BTreeMap<PathBuf, &'static str> =
        std::collections::BTreeMap::new();
    for entry in archive.entries().context("reading tar members")? {
        let mut entry = entry.context("reading tar member")?;
        let raw = entry
            .path()
            .context("a member has a path that is not valid UTF-8")?
            .into_owned();
        if let Some(what) = pax_sparse(&mut entry, &raw)? {
            bail!(
                "{what}, which `archive::Extracted` cannot expand; recreate the archive without \
                 sparse handling (GNU tar: drop `-S`, or use `--format=gnu`; bsdtar: \
                 `--no-read-sparse`)"
            );
        }
        let normalised = validate_entry_path(&raw).map_err(Error::msg)?;
        let Some(path) = strip_components(&normalised, strip) else {
            out.push(PlannedEntry { raw, member: None });
            continue;
        };
        if let Some(link) = symlinks.iter().find(|l| path.starts_with(l)) {
            bail!(
                "member `{}` is under the symlink member `{}`",
                raw.display(),
                link.display()
            );
        }
        let header = entry.header();
        let mode = header.mode().unwrap_or(0o644) & 0o7777;
        // The archive's length for the member: `Header::size()` gives a GNU
        // sparse member's real size but ignores a pax `size` record, which
        // `Entry::size()` honours. `apply` checks what it reads against the
        // same length.
        let size = entry.size();
        let ty = header.entry_type();
        let kind = if ty.is_dir() {
            Kind::Dir
        } else if ty.is_file() || ty.is_contiguous() || ty.is_gnu_sparse() {
            Kind::File
        } else if ty.is_symlink() || ty.is_hard_link() {
            let target = entry
                .link_name()
                .with_context(|| format!("member `{}`: link target", raw.display()))?
                .map(|t| t.into_owned())
                .ok_or_else(|| {
                    Error::msg(format!("member `{}` has no link target", raw.display()))
                })?;
            if ty.is_symlink() {
                validate_link_target(&path, &target).map_err(Error::msg)?;
                Kind::Symlink(target)
            } else {
                let t = validate_entry_path(&target)
                    .map_err(|e| format!("hard link `{}`: target {e}", raw.display()))
                    .map_err(Error::msg)?;
                let t = strip_components(&t, strip).filter(|t| files.contains(t)).ok_or_else(|| {
                    Error::msg(format!(
                        "hard link `{}` -> `{}` points at a file the archive did not extract before it",
                        raw.display(),
                        target.display()
                    ))
                })?;
                Kind::Hardlink(t)
            }
        } else {
            bail!(
                "member `{}` is a special file (type {:?}); only files, directories and links are extracted",
                raw.display(),
                ty
            );
        };
        let label = kind_label(&kind);
        // The same path claimed twice as two different things. `apply`
        // would try to replace one with the other and, for a populated
        // directory, fail partway through the archive.
        if let Some(earlier) = claimed.get(&path)
            && *earlier != label
        {
            bail!(
                "member `{}` is a {label} at a path the archive already used for a {earlier}",
                raw.display()
            );
        }
        // A non-directory member sitting on top of a path earlier members
        // populate, whether or not the directory itself was a member:
        // `apply` creates the directory for those, then cannot remove it.
        if !matches!(kind, Kind::Dir)
            && let Some(under) = claimed
                .range((
                    std::ops::Bound::Excluded(path.clone()),
                    std::ops::Bound::Unbounded,
                ))
                .next()
                .map(|(p, _)| p)
                .filter(|p| p.starts_with(&path))
        {
            bail!(
                "member `{}` is a {label} at a path the archive already populated as a directory (`{}`)",
                raw.display(),
                under.display()
            );
        }
        claimed.insert(path.clone(), label);
        match &kind {
            Kind::File | Kind::Hardlink(_) => {
                files.insert(path.clone());
            }
            Kind::Symlink(_) => symlinks.push(path.clone()),
            Kind::Dir => {}
        }
        let member = Member {
            path,
            kind,
            mode,
            size,
        };
        out.push(PlannedEntry {
            raw,
            member: Some(member),
        });
    }
    Ok(out)
}

/// One entry of the archive as `check` walked it: the path the archive
/// gives it, and the validated member it extracts to (`None` for one
/// `.strip_components` or an empty path drops).
#[derive(Debug, Clone)]
struct PlannedEntry {
    raw: PathBuf,
    member: Option<Member>,
}

/// What [`Extracted`]'s `check` decided: extract these validated members, in
/// archive order. The member plan is the walk `check` already did, so
/// `apply` does not validate again; it reads each member's data from the
/// archive (the bytes stay there, never in the intent) and writes it where
/// the plan says.
#[derive(Debug)]
pub struct Extraction {
    src: PathBuf,
    dest: PathBuf,
    format: Format,
    entries: Vec<PlannedEntry>,
}

impl Extraction {
    /// The counts, from the plan: what `apply` writes is exactly this.
    fn report(&self) -> ExtractReport {
        let mut r = ExtractReport {
            src: self.src.clone(),
            dest: self.dest.clone(),
            format: Some(self.format),
            files: 0,
            dirs: 0,
            symlinks: 0,
            bytes: 0,
            skipped: 0,
            extracted: true,
        };
        for e in &self.entries {
            let Some(m) = &e.member else {
                r.skipped += 1;
                continue;
            };
            match m.kind {
                Kind::File | Kind::Hardlink(_) => {
                    r.files += 1;
                    // The sizes come from the archive's headers and are not
                    // bounded, so a crafted set can overflow the sum, which
                    // panics in debug builds.
                    r.bytes = r.bytes.saturating_add(m.size);
                }
                Kind::Dir => r.dirs += 1,
                Kind::Symlink(_) => r.symlinks += 1,
            }
        }
        r
    }
}

impl Intent for Extraction {
    fn diff(&self) -> Diff {
        let r = self.report();
        Diff::summary(format!(
            "extract {} ({}: {} files, {} dirs, {} symlinks, {} bytes) into {}",
            self.src.display(),
            self.format.name(),
            r.files,
            r.dirs,
            r.symlinks,
            r.bytes,
            self.dest.display()
        ))
    }
}

impl Op for Extracted {
    type Output = ExtractReport;
    type Intent = Extraction;

    fn check(&self, sys: &System) -> Result<Plan<Self>> {
        // Portable. archive::Extracted is a pure-Rust extractor writing through `sys`.
        // The supported set is written out rather than left open, so a new
        // platform is a decision made here and not an accident.
        match sys.facts().os {
            Os::Linux | Os::Macos => {}
            ref other => bail!(
                "archive::Extracted has no implementation for {}",
                other.name()
            ),
        }
        if let Some(marker) = self.marker()
            && sys.exists(&marker)?
        {
            return Ok(Plan::Satisfied(ExtractReport {
                src: self.src.clone(),
                dest: self.dest.clone(),
                format: None,
                files: 0,
                dirs: 0,
                symlinks: 0,
                bytes: 0,
                skipped: 0,
                extracted: false,
            }));
        }
        let (format, entries) = self.plan(sys)?;
        Ok(Plan::Change(Extraction {
            src: self.src.clone(),
            dest: self.dest.clone(),
            format,
            entries,
        }))
    }

    fn apply(&self, sys: &System, intent: Extraction) -> Result<ExtractReport> {
        // One read of the archive, for the data: the members and where they
        // go are the plan's. An archive swapped since `check` is caught at
        // the first entry that is not the one the plan has in that place,
        // so data is never written under another member's name.
        let (format, mut archive) = self.open(sys)?;
        let mut planned = intent.entries.iter();
        let entries = archive
            .entries()
            .with_context(|| format!("extracting {}: reading tar members", intent.src.display()))?;
        for entry in entries {
            let mut entry = entry.with_context(|| {
                format!("extracting {}: reading tar member", intent.src.display())
            })?;
            let raw = entry
                .path()
                .with_context(|| format!("extracting {}", intent.src.display()))?
                .into_owned();
            let Some(slot) = planned.next().filter(|slot| slot.raw == raw) else {
                bail!(
                    "{}: reading member `{}`, which is not the member `check` walked there; \
                     the archive changed between check and apply, and nothing further was \
                     extracted",
                    intent.src.display(),
                    raw.display()
                );
            };
            if let Some(m) = &slot.member {
                let declared = entry.size();
                self.write_member(sys, &intent.dest, m, declared, &mut entry)
                    .with_context(|| format!("extracting {}", intent.src.display()))?;
            }
        }
        ensure!(
            planned.next().is_none(),
            "{}: the archive ended before every member `check` walked was read; it changed \
             between check and apply",
            intent.src.display()
        );
        self.finish(format, archive)?;
        if let Some(marker) = self.marker()
            && !sys.exists(&marker)?
        {
            sys.warn(format!(
                "archive {} extracted but its creates marker {} does not exist; the next run will extract again",
                intent.src.display(),
                marker.display()
            ));
        }
        Ok(intent.report())
    }

    fn always_changes(&self) -> bool {
        self.creates.is_none()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::{
        AttrCall, Backend, CmdSpec, Fake, Output, ReadCall, Stat, WriteAttrs,
    };
    use rustible_sdk::event::Collect;

    use super::*;
    use crate::file::testing::{Set, expect_change, fake_sys, staged};

    /// `archive::Extracted` claims a mac — it is a pure-Rust extractor
    /// writing through `sys`, and the macOS spike unpacked a tarball the
    /// mac's own `tar` made — and refuses a platform nobody claimed.
    #[test]
    fn runs_on_a_mac_and_refuses_an_unclaimed_platform() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/tmp/hello.tar", TAR),
        );
        let base_sys = fake_sys(&fake);

        let mut mac = base_sys.facts().clone();
        mac.os = Os::Macos;
        let sys = base_sys.clone().with_facts(mac);
        let op = Extracted::from_path("/tmp/hello.tar").to("/opt");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(
            fake.file("/opt/hello/README.txt").unwrap().bytes,
            b"hello from rustible\n"
        );

        let mut bsd = base_sys.facts().clone();
        bsd.os = Os::Other("freebsd".into());
        let sys = base_sys.with_facts(bsd);
        let err = Extracted::from_path("/tmp/hello.tar")
            .to("/opt")
            .check(&sys)
            .unwrap_err()
            .chain();
        assert!(
            err.contains("archive::Extracted has no implementation for freebsd"),
            "{err}"
        );
    }

    // `fixtures/archive/hello*.tar*`: one tree, five encodings, made with
    // GNU tar 1.35, gzip -9 -n, xz -9 and zstd -19, the last twice: one
    // frame, and two frames cut inside `README.txt` with a skippable frame
    // between them (the README beside them has the commands):
    //   hello/            0775
    //   hello/README.txt  0644  "hello from rustible\n"
    //   hello/bin/        0775
    //   hello/bin/run     0755  "#!/bin/sh\necho hi\n"
    //   hello/empty/      0775
    //   hello/link -> README.txt
    const TAR: &[u8] = include_bytes!("../fixtures/archive/hello.tar");
    const TGZ: &[u8] = include_bytes!("../fixtures/archive/hello.tar.gz");
    const TXZ: &[u8] = include_bytes!("../fixtures/archive/hello.tar.xz");
    const TZST: &[u8] = include_bytes!("../fixtures/archive/hello.tar.zst");
    const TZST_FRAMES: &[u8] = include_bytes!("../fixtures/archive/hello-frames.tar.zst");

    fn all() -> [(&'static str, Format, &'static [u8]); 5] {
        [
            ("/tmp/hello.tar", Format::Tar, TAR),
            ("/tmp/hello.tar.gz", Format::TarGz, TGZ),
            ("/tmp/hello.tar.xz", Format::TarXz, TXZ),
            ("/tmp/hello.tar.zst", Format::TarZst, TZST),
            ("/tmp/hello-frames.tar.zst", Format::TarZst, TZST_FRAMES),
        ]
    }

    fn hello_report(src: &str, format: Format) -> ExtractReport {
        ExtractReport {
            src: src.into(),
            dest: "/opt".into(),
            format: Some(format),
            files: 2,
            dirs: 3,
            symlinks: 1,
            bytes: 38,
            skipped: 0,
            extracted: true,
        }
    }

    /// A hand-built GNU tar: `(name, typeflag, data, linkname)` per member,
    /// bypassing `tar::Builder`'s own path checks so bad names get through.
    fn raw_tar(members: &[(&str, u8, &[u8], &str)]) -> Vec<u8> {
        let mut out = vec![];
        for (name, ty, data, link) in members {
            let mut h = tar::Header::new_gnu();
            {
                let g = h.as_gnu_mut().unwrap();
                g.name[..name.len()].copy_from_slice(name.as_bytes());
                g.linkname[..link.len()].copy_from_slice(link.as_bytes());
            }
            h.set_entry_type(tar::EntryType::new(*ty));
            h.set_mode(if *ty == b'5' { 0o755 } else { 0o644 });
            h.set_size(data.len() as u64);
            h.set_cksum();
            out.extend_from_slice(h.as_bytes());
            out.extend_from_slice(data);
            let pad = (512 - data.len() % 512) % 512;
            out.extend(std::iter::repeat_n(0u8, pad));
        }
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    fn sys_with(archive: &[u8]) -> (Arc<Fake>, System) {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/tmp/a.tar", archive),
        );
        let sys = fake_sys(&fake);
        (fake, sys)
    }

    fn check_err(archive: &[u8]) -> String {
        let (_, sys) = sys_with(archive);
        Extracted::from_path("/tmp/a.tar")
            .to("/opt")
            .check(&sys)
            .unwrap_err()
            .chain()
    }

    // ---- pure ----

    #[test]
    fn format_detection_by_magic() {
        assert_eq!(detect_format(TAR), Ok(Format::Tar));
        assert_eq!(detect_format(TGZ), Ok(Format::TarGz));
        assert_eq!(detect_format(TXZ), Ok(Format::TarXz));
        assert_eq!(detect_format(TZST), Ok(Format::TarZst));
        assert_eq!(detect_format(TZST_FRAMES), Ok(Format::TarZst));
        assert!(
            detect_format(b"PK\x03\x04rest")
                .unwrap_err()
                .starts_with("zip archives are not supported; supported formats: tar, tar.gz")
        );
        assert!(
            detect_format(b"BZh91AY")
                .unwrap_err()
                .starts_with("bzip2 archives")
        );
        assert!(
            detect_format(b"7z\xbc\xaf\x27\x1c")
                .unwrap_err()
                .starts_with("7z archives")
        );
        assert!(
            detect_format(b"Rar!\x1a\x07")
                .unwrap_err()
                .starts_with("rar archives")
        );
        assert!(
            detect_format(b"hello")
                .unwrap_err()
                .starts_with("not a recognised archive")
        );
        // A tar needs a whole header block; 511 bytes is not one.
        assert!(detect_format(&TAR[..511]).is_err());
    }

    #[test]
    fn entry_path_validation() {
        let ok = |s: &str| validate_entry_path(Path::new(s)).unwrap();
        assert_eq!(ok("a/b"), PathBuf::from("a/b"));
        assert_eq!(ok("./a/./b/"), PathBuf::from("a/b"));
        assert_eq!(ok("./"), PathBuf::new());
        assert_eq!(
            validate_entry_path(Path::new("/etc/passwd")).unwrap_err(),
            "`/etc/passwd` is an absolute path"
        );
        assert_eq!(
            validate_entry_path(Path::new("a/../../x")).unwrap_err(),
            "`a/../../x` contains `..`"
        );
    }

    #[test]
    fn strip_components_drops_leading_parts_and_skips_short_paths() {
        let p = Path::new("hello/bin/run");
        assert_eq!(strip_components(p, 0), Some(p.to_path_buf()));
        assert_eq!(strip_components(p, 1), Some("bin/run".into()));
        assert_eq!(strip_components(p, 2), Some("run".into()));
        assert_eq!(strip_components(p, 3), None);
        assert_eq!(strip_components(Path::new("hello"), 1), None);
        assert_eq!(strip_components(Path::new(""), 0), None);
    }

    #[test]
    fn link_target_validation() {
        let ok = |l: &str, t: &str| validate_link_target(Path::new(l), Path::new(t));
        assert_eq!(ok("hello/link", "README.txt"), Ok(()));
        assert_eq!(ok("hello/bin/x", "../README.txt"), Ok(()));
        assert_eq!(
            ok("hello/bin/x", "../../other/y"),
            Ok(()),
            "stays inside dest"
        );
        assert_eq!(ok("hello/bin/x", "./a/../b"), Ok(()));
        assert!(
            ok("hello/bin/x", "../../../etc")
                .unwrap_err()
                .contains("outside the destination")
        );
        assert!(
            ok("top", "../x")
                .unwrap_err()
                .contains("outside the destination")
        );
        assert!(
            ok("hello/link", "/etc/passwd")
                .unwrap_err()
                .contains("absolute target")
        );
    }

    // ---- fake ----

    #[test]
    fn creates_marker_is_satisfied_without_opening_the_archive() {
        // No archive planted at all: the marker alone decides.
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello/README.txt", "x"),
        );
        let sys = fake_sys(&fake);
        let op = Extracted::from_path("/tmp/missing.tar.gz")
            .to("/opt")
            .creates("hello/README.txt");
        let Plan::Satisfied(r) = op.check(&sys).unwrap() else {
            panic!("expected satisfied")
        };
        assert!(!r.extracted && r.format.is_none());
        assert!(!op.always_changes());
        // An absolute marker works too.
        let op = Extracted::from_path("/tmp/missing.tar.gz")
            .to("/opt")
            .creates("/opt/hello/README.txt");
        assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
    }

    #[test]
    fn every_format_extracts_the_same_tree() {
        for (src, format, bytes) in all() {
            let fake = Arc::new(Fake::new().with_dir("/opt").with_file(src, bytes));
            let sys = fake_sys(&fake);
            let op = Extracted::from_path(src).to("/opt");
            assert!(op.always_changes(), "no creates marker");
            let c = expect_change(&op, &sys);
            assert_eq!(
                c.diff().short(),
                format!(
                    "extract {src} ({}: 2 files, 3 dirs, 1 symlinks, 38 bytes) into /opt",
                    format.name()
                )
            );
            let r = op.apply(&sys, c).unwrap();
            assert_eq!(r, hello_report(src, format));

            let readme = fake.file("/opt/hello/README.txt").unwrap();
            assert_eq!(
                (readme.mode, readme.bytes.as_slice()),
                (0o644, b"hello from rustible\n".as_slice())
            );
            let run = fake.file("/opt/hello/bin/run").unwrap();
            assert_eq!(
                (run.mode, run.bytes.as_slice()),
                (0o755, b"#!/bin/sh\necho hi\n".as_slice())
            );
            assert_eq!(fake.file("/opt/hello/empty").unwrap().kind, FileKind::Dir);
            assert_eq!(fake.file("/opt/hello").unwrap().mode, 0o775);
            assert_eq!(
                sys.read_link("/opt/hello/link").unwrap(),
                PathBuf::from("README.txt")
            );
            assert!(fake.commands().is_empty(), "no shelling out to tar");

            // Without a marker the second check is a change again; with
            // one it is satisfied.
            assert!(op.check(&sys).unwrap().is_change());
            assert!(matches!(
                op.clone().creates("hello/bin/run").check(&sys).unwrap(),
                Plan::Satisfied(_)
            ));
        }
    }

    #[test]
    fn re_extracting_over_an_existing_tree_replaces_files_and_links() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_dir("/opt/hello")
                .with_file_mode("/opt/hello/README.txt", "older", 0o600)
                .with_symlink("/opt/hello/link", "elsewhere")
                .with_file("/opt/hello/keep.me", "untouched")
                .with_file("/tmp/hello.tar", TAR),
        );
        let sys = fake_sys(&fake);
        let op = Extracted::from_path("/tmp/hello.tar").to("/opt");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        let readme = fake.file("/opt/hello/README.txt").unwrap();
        assert_eq!(
            (readme.mode, readme.bytes.as_slice()),
            (0o644, b"hello from rustible\n".as_slice())
        );
        assert_eq!(
            sys.read_link("/opt/hello/link").unwrap(),
            PathBuf::from("README.txt")
        );
        assert_eq!(fake.content("/opt/hello/keep.me").unwrap(), "untouched");
    }

    #[test]
    fn strip_components_and_owner() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/tmp/hello.tar.gz", TGZ),
        );
        let sys = fake_sys(&fake);
        let op = Extracted::from_path("/tmp/hello.tar.gz")
            .to("/opt")
            .strip_components(1)
            .owner(1000, 1000);
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!(
            (r.files, r.dirs, r.symlinks, r.skipped),
            (2, 2, 1, 1),
            "`hello/` itself is skipped"
        );
        assert!(fake.file("/opt/hello").is_none());
        let run = fake.file("/opt/bin/run").unwrap();
        assert_eq!((run.mode, run.uid, run.gid), (0o755, 1000, 1000));
        let bin = fake.file("/opt/bin").unwrap();
        assert_eq!((bin.uid, bin.gid), (1000, 1000));
        let link = fake.file("/opt/link").unwrap();
        assert_eq!(
            (link.kind, link.uid),
            (FileKind::Symlink, 0),
            "symlinks are not chowned"
        );
        // Stripping everything leaves nothing to do, which is still a change.
        let all_stripped = op.clone().strip_components(9);
        let c = expect_change(&all_stripped, &sys);
        let r = all_stripped.apply(&sys, c).unwrap();
        assert_eq!((r.files, r.dirs, r.skipped), (0, 0, 6));
    }

    #[test]
    fn hard_links_are_copies_of_the_earlier_file() {
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_size(5);
        h.set_mode(0o600);
        h.set_cksum();
        b.append_data(&mut h, "d/orig", &b"data\n"[..]).unwrap();
        let mut h = tar::Header::new_gnu();
        h.set_size(0);
        h.set_mode(0o600);
        h.set_entry_type(tar::EntryType::Link);
        h.set_cksum();
        b.append_link(&mut h, "d/copy", "d/orig").unwrap();
        let bytes = b.into_inner().unwrap();

        let (fake, sys) = sys_with(&bytes);
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        assert_eq!((r.files, r.bytes), (2, 5));
        let copy = fake.file("/opt/d/copy").unwrap();
        assert_eq!(
            (copy.mode, copy.bytes.as_slice()),
            (0o600, b"data\n".as_slice())
        );
        assert_eq!(
            fake.file("/opt/d").unwrap().kind,
            FileKind::Dir,
            "parent made on demand"
        );
    }

    #[test]
    fn tar_slip_paths_are_refused_naming_the_member() {
        let err = check_err(&raw_tar(&[("/etc/passwd", b'0', b"root::0", "")]));
        assert!(err.contains("/tmp/a.tar: refusing to extract"), "{err}");
        assert!(err.contains("`/etc/passwd` is an absolute path"), "{err}");

        let err = check_err(&raw_tar(&[
            ("ok.txt", b'0', b"x", ""),
            ("a/../../evil", b'0', b"x", ""),
        ]));
        assert!(err.contains("`a/../../evil` contains `..`"), "{err}");
    }

    #[test]
    fn escaping_symlinks_and_writes_through_symlinks_are_refused() {
        let err = check_err(&raw_tar(&[("out", b'2', b"", "../../etc")]));
        assert!(
            err.contains("symlink `out` -> `../../etc` points outside the destination"),
            "{err}"
        );

        let err = check_err(&raw_tar(&[("abs", b'2', b"", "/etc")]));
        assert!(err.contains("has an absolute target"), "{err}");

        let err = check_err(&raw_tar(&[
            ("d", b'2', b"", "other"),
            ("d/passwd", b'0', b"pwned", ""),
        ]));
        assert!(
            err.contains("member `d/passwd` is under the symlink member `d`"),
            "{err}"
        );

        let err = check_err(&raw_tar(&[("copy", b'1', b"", "nowhere")]));
        assert!(err.contains("hard link `copy` -> `nowhere` points at a file the archive did not extract before it"), "{err}");

        let err = check_err(&raw_tar(&[("dev/null", b'3', b"", "")]));
        assert!(err.contains("member `dev/null` is a special file"), "{err}");
    }

    #[test]
    fn intra_archive_path_collisions_are_refused_at_check() {
        // The reviewer's case: a directory, something inside it, then a
        // symlink claiming the directory's own path. `apply` would create
        // the directory, populate it, then fail to remove it, leaving a
        // half-written tree. It has to be refused at check, where the
        // other refusals are.
        let err = check_err(&raw_tar(&[
            ("d/", b'5', b"", ""),
            ("d/f", b'0', b"data", ""),
            ("d", b'2', b"", "elsewhere"),
        ]));
        assert!(
            err.contains(
                "member `d` is a symlink at a path the archive already used for a directory"
            ),
            "{err}"
        );

        // The same without an explicit directory member: `apply` creates
        // `d` implicitly for `d/f`, so the symlink still lands on a
        // populated directory.
        let err = check_err(&raw_tar(&[
            ("d/f", b'0', b"data", ""),
            ("d", b'2', b"", "elsewhere"),
        ]));
        assert!(
            err.contains("member `d` is a symlink at a path the archive already populated as a directory (`d/f`)"),
            "{err}"
        );

        // A plain file over a populated directory is refused the same way.
        let err = check_err(&raw_tar(&[
            ("d/f", b'0', b"data", ""),
            ("d", b'0', b"data", ""),
        ]));
        assert!(
            err.contains(
                "member `d` is a file at a path the archive already populated as a directory"
            ),
            "{err}"
        );

        // A file and a directory fighting over one path, in either order.
        let err = check_err(&raw_tar(&[("x", b'0', b"data", ""), ("x/", b'5', b"", "")]));
        assert!(
            err.contains(
                "member `x/` is a directory at a path the archive already used for a file"
            ),
            "{err}"
        );

        // Repeating the same path as the same kind is legal tar (the last
        // one wins, as GNU tar does) and must keep working.
        let (_, sys) = sys_with(&raw_tar(&[
            ("f", b'0', b"one", ""),
            ("f", b'0', b"two", ""),
        ]));
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.diff().short(),
            "extract /tmp/a.tar (tar: 2 files, 0 dirs, 0 symlinks, 6 bytes) into /opt"
        );
    }

    /// A tar holding one member whose header claims `size` bytes but
    /// carries only one block, so the stream is short of the claim.
    fn tar_claiming_size(name: &str, size: u64) -> Vec<u8> {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::new(b'0'));
        h.set_mode(0o644);
        h.set_size(size);
        {
            let g = h.as_gnu_mut().unwrap();
            g.name[..name.len()].copy_from_slice(name.as_bytes());
        }
        h.set_cksum();
        let mut out = h.as_bytes().to_vec();
        out.extend_from_slice(&[b'x'; 512]);
        out.extend(std::iter::repeat_n(0u8, 1024));
        out
    }

    #[test]
    fn an_absurd_header_size_is_refused_at_check_without_aborting() {
        // 2^62 asks for a 4.6 EB allocation, and Rust aborts the process on
        // allocation failure. `check` never reaches an allocation because
        // the stream is short of the claim, so it refuses; reaching the
        // assertion at all is what this pins.
        let err = check_err(&tar_claiming_size("big", 1u64 << 62));
        assert!(err.contains("refusing to extract"), "{err}");
        assert!(err.contains("EOF"), "{err}");
    }

    /// A tar of one regular member `name` holding `#!`, at `mode`.
    fn one_member_tar(name: &str, mode: u32) -> Vec<u8> {
        let mut h = tar::Header::new_gnu();
        h.set_path(name).unwrap();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(mode);
        h.set_size(2);
        h.set_cksum();
        let mut archive = h.as_bytes().to_vec();
        archive.extend_from_slice(b"#!");
        archive.resize(1024, 0);
        archive.extend_from_slice(&[0u8; 1024]);
        archive
    }

    /// A setuid member extracted with `.owner(..)` keeps the bit, and never
    /// carries it under the wrong owner: the mode without setuid, the
    /// owner, then the full mode, because `chown` clears setuid (the `Fake`
    /// models it). All three land on the staged file before it is renamed
    /// into place (decision 24 on #85), none on the member's path after.
    #[test]
    fn a_setuid_member_keeps_the_bit_under_owner() {
        let (fake, sys) = sys_with(&one_member_tar("tool", 0o4755));
        let planted = fake.attr_calls().len();
        let op = Extracted::from_path("/tmp/a.tar").to("/opt").owner(5, 6);
        let intent = expect_change(&op, &sys);
        op.apply(&sys, intent).unwrap();
        let f = fake.file("/opt/tool").unwrap();
        assert_eq!((f.mode, f.uid, f.gid), (0o4755, 5, 6));
        assert_eq!(
            staged(&fake, planted, "/opt/tool"),
            [Set::Mode(0o755), Set::Owner(5, 6), Set::Mode(0o4755)]
        );
    }

    /// Every member under `.owner(..)` gets its mode before its owner
    /// (issue #79), directories included, and a mode without setuid or
    /// setgid gets no third call. A file gets them on its staged file
    /// before the rename, a directory on itself once made. Without
    /// `.owner(..)`, one `chmod` each.
    #[test]
    fn members_get_their_mode_before_their_owner() {
        let archive = raw_tar(&[("d", b'5', b"", ""), ("d/f", b'0', b"x", "")]);
        let (fake, sys) = sys_with(&archive);
        let planted = fake.attr_calls().len();
        let op = Extracted::from_path("/tmp/a.tar").to("/opt").owner(5, 6);
        let intent = expect_change(&op, &sys);
        op.apply(&sys, intent).unwrap();
        assert_eq!(
            fake.attr_calls()[planted..planted + 2],
            [
                AttrCall::Chmod {
                    path: "/opt/d".into(),
                    mode: 0o755
                },
                AttrCall::Chown {
                    path: "/opt/d".into(),
                    uid: 5,
                    gid: 6
                },
            ]
        );
        assert_eq!(
            staged(&fake, planted + 2, "/opt/d/f"),
            [Set::Mode(0o644), Set::Owner(5, 6)]
        );

        let (fake, sys) = sys_with(&archive);
        let planted = fake.attr_calls().len();
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let intent = expect_change(&op, &sys);
        op.apply(&sys, intent).unwrap();
        assert_eq!(
            fake.attr_calls()[planted..planted + 1],
            [AttrCall::Chmod {
                path: "/opt/d".into(),
                mode: 0o755
            }]
        );
        assert_eq!(staged(&fake, planted + 1, "/opt/d/f"), [Set::Mode(0o644)]);
    }

    /// A setgid directory member gets its full mode, its owner, then its
    /// full mode again: nothing is cleared first, so a refused `chown`
    /// leaves the bit on, and the last call puts it back where `chown`
    /// clears it on a directory (macOS).
    #[test]
    fn a_setgid_directory_member_gets_its_full_mode_around_its_owner() {
        let mut h = tar::Header::new_gnu();
        h.set_path("shared").unwrap();
        h.set_entry_type(tar::EntryType::Directory);
        h.set_mode(0o2775);
        h.set_size(0);
        h.set_cksum();
        let mut archive = h.as_bytes().to_vec();
        archive.extend_from_slice(&[0u8; 1024]);

        let (fake, sys) = sys_with(&archive);
        let planted = fake.attr_calls().len();
        let op = Extracted::from_path("/tmp/a.tar").to("/opt").owner(5, 6);
        let intent = expect_change(&op, &sys);
        op.apply(&sys, intent).unwrap();
        assert_eq!(
            fake.attr_calls()[planted..],
            [
                AttrCall::Chmod {
                    path: "/opt/shared".into(),
                    mode: 0o2775
                },
                AttrCall::Chown {
                    path: "/opt/shared".into(),
                    uid: 5,
                    gid: 6
                },
                AttrCall::Chmod {
                    path: "/opt/shared".into(),
                    mode: 0o2775
                },
            ]
        );

        let (fake, _) = sys_with(&archive);
        let sys = crate::file::testing::chown_refused_sys(&fake);
        let intent = expect_change(&op, &sys);
        op.apply(&sys, intent).unwrap_err();
        assert_eq!(fake.file("/opt/shared").unwrap().mode, 0o2775);
    }

    /// A member wanted at 0600 whose `chown` is refused, as for an identity
    /// without `CAP_CHOWN`, fails the step with nothing at its path: the
    /// mode and owner are set on the staged file, which is never renamed
    /// into place (decision 24 on #85; before it, issue #79 left the file
    /// written, at 0600). The mode came first, so the staged file was never
    /// wider than the member's mode either.
    #[test]
    fn a_refused_chown_writes_nothing_at_the_members_path() {
        let fake = Arc::new(
            Fake::new()
                .with_chown_refused()
                .with_dir("/opt")
                .with_file("/tmp/a.tar", one_member_tar("key", 0o600)),
        );
        let sys = fake_sys(&fake);
        let planted = fake.attr_calls().len();
        let op = Extracted::from_path("/tmp/a.tar").to("/opt").owner(5, 6);
        let intent = expect_change(&op, &sys);
        let err = op.apply(&sys, intent).unwrap_err().chain();
        assert!(err.contains("Operation not permitted"), "{err}");
        assert!(fake.file("/opt/key").is_none());
        assert_eq!(sys.read_dir("/opt").unwrap(), Vec::<PathBuf>::new());
        // The refused `chown` is recorded too, after the mode.
        assert_eq!(
            staged(&fake, planted, "/opt/key"),
            [Set::Mode(0o600), Set::Owner(5, 6)]
        );
    }

    /// `apply` writes the members `check` walked and reads only their data
    /// from the archive. An archive swapped in between, with a member under
    /// another name, is refused at that member: its data never lands under
    /// a name `check` did not validate.
    #[test]
    fn apply_refuses_an_archive_whose_member_changed_since_check() {
        let (fake, sys) = sys_with(&raw_tar(&[
            ("a", b'0', b"one", ""),
            ("b", b'0', b"two", ""),
        ]));
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let intent = expect_change(&op, &sys);
        sys.write_atomic(
            "/tmp/a.tar",
            &raw_tar(&[("a", b'0', b"one", ""), ("evil", b'0', b"two", "")]),
        )
        .unwrap();
        let err = op.apply(&sys, intent).unwrap_err().chain();
        assert!(err.contains("not the member `check` walked"), "{err}");
        assert!(
            fake.file("/opt/evil").is_none(),
            "nothing under the new name"
        );
        assert!(fake.file("/opt/b").is_none());
    }

    /// The same for an archive that lost members: `apply` refuses rather
    /// than report an extraction it did not finish.
    #[test]
    fn apply_refuses_an_archive_that_ends_early() {
        let (_, sys) = sys_with(&raw_tar(&[
            ("a", b'0', b"one", ""),
            ("b", b'0', b"two", ""),
        ]));
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let intent = expect_change(&op, &sys);
        sys.write_atomic("/tmp/a.tar", &raw_tar(&[("a", b'0', b"one", "")]))
            .unwrap();
        let err = op.apply(&sys, intent).unwrap_err().chain();
        assert!(err.contains("ended before"), "{err}");
    }

    /// The tar from `raw_tar(members)` followed by one member `name` whose
    /// header claims 2^62 bytes and whose stream holds 1536: `members`
    /// complete, then the archive running out inside `name`.
    fn tar_ending_inside(members: &[(&str, u8, &[u8], &str)], name: &str) -> Vec<u8> {
        let mut out = raw_tar(members);
        out.truncate(out.len() - 1024);
        out.extend(tar_claiming_size(name, 1u64 << 62));
        out
    }

    /// What `apply` says about the member `tar_ending_inside` cuts short.
    fn short_member_err(path: &str) -> String {
        format!(
            "member `{path}` ends after 1536 of the 4611686018427387904 bytes the archive \
             declares for it; the archive is truncated, or changed between check and apply, \
             and nothing was written at /opt/{path}; run the step again once the archive is whole"
        )
    }

    #[test]
    fn a_swapped_member_whose_header_lies_fails_the_step() {
        // `apply` reads each member's data from the archive again. Swapped
        // after `check` for one with the same name whose header claims 2^62
        // bytes the stream does not hold, the name check passes; the
        // member's reader hands over the 1536 bytes the stream still holds
        // and stops without an error, so the length is what fails the step.
        // The member `check` planned is 1536 bytes, as many as the cut
        // stream still holds, so only the length the archive declares where
        // `apply` reads it (2^62) tells the read apart from a complete one;
        // the planned size would pass it.
        let (fake, sys) = sys_with(&raw_tar(&[("sub/f", b'0', &[b'p'; 1536], "")]));
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        sys.write_atomic("/tmp/a.tar", &tar_ending_inside(&[], "sub/f"))
            .unwrap();
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains(&short_member_err("sub/f")), "{err}");
        // Nothing under the member's name, not even the parent directory
        // the archive implies for it, and nothing beside it: no partial
        // file, no staging file left behind (issue #52).
        assert!(fake.file("/opt/sub/f").is_none(), "no partial file");
        assert!(fake.file("/opt/sub").is_none(), "no parent created");
        assert_eq!(sys.read_dir("/opt").unwrap(), Vec::<PathBuf>::new());
    }

    /// A file already at the short member's path is left exactly as it was:
    /// content, mode and owner, and no `chmod` or `chown` issued on it.
    #[test]
    fn a_short_member_leaves_the_file_already_there_untouched() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file_mode("/opt/f", "old contents\n", 0o4750)
                .with_file("/tmp/a.tar", raw_tar(&[("f", b'0', b"data", "")])),
        );
        let sys = fake_sys(&fake);
        let before = fake.file("/opt/f").unwrap();
        let op = Extracted::from_path("/tmp/a.tar").to("/opt").owner(5, 6);
        let c = expect_change(&op, &sys);
        sys.write_atomic("/tmp/a.tar", &tar_ending_inside(&[], "f"))
            .unwrap();
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains(&short_member_err("f")), "{err}");
        let after = fake.file("/opt/f").unwrap();
        assert_eq!(
            (after.bytes, after.mode, after.uid, after.gid, after.kind),
            (
                before.bytes,
                before.mode,
                before.uid,
                before.gid,
                before.kind
            )
        );
        assert!(fake.attr_calls().is_empty(), "{:?}", fake.attr_calls());
        assert_eq!(sys.read_dir("/opt").unwrap(), vec![PathBuf::from("/opt/f")]);
    }

    /// The step is not rolled back: members extracted before the short one
    /// stay, as they did before #52; only the short member writes nothing.
    #[test]
    fn members_before_a_short_one_stay_extracted() {
        let (fake, sys) = sys_with(&raw_tar(&[
            ("a", b'0', b"one", ""),
            ("f", b'0', b"data", ""),
        ]));
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        sys.write_atomic(
            "/tmp/a.tar",
            &tar_ending_inside(&[("a", b'0', b"one", "")], "f"),
        )
        .unwrap();
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains(&short_member_err("f")), "{err}");
        assert_eq!(fake.content("/opt/a").unwrap(), "one");
        assert!(fake.file("/opt/f").is_none());
    }

    /// The length a member must come to is the one the archive gives it
    /// where `apply` reads it, which a pax `size` record overrides: here the
    /// header's own field says 0 and the record says 4. Checked against the
    /// header field (the `size` `check` planned), this complete member would
    /// be refused as short.
    #[test]
    fn a_member_sized_by_a_pax_record_is_read_to_that_length() {
        let mut archive = raw_tar(&[("PaxHeaders/f", b'x', b"10 size=4\n", "")]);
        archive.truncate(archive.len() - 1024);
        let mut h = tar::Header::new_gnu();
        h.set_path("f").unwrap();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(0o644);
        h.set_size(0);
        h.set_cksum();
        archive.extend_from_slice(h.as_bytes());
        archive.extend_from_slice(b"data");
        archive.extend(std::iter::repeat_n(0u8, 508 + 1024));
        let (fake, sys) = sys_with(&archive);
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        // The report counts the record's 4 bytes, not the header's 0.
        assert_eq!(
            c.diff().short(),
            "extract /tmp/a.tar (tar: 1 files, 0 dirs, 0 symlinks, 4 bytes) into /opt"
        );
        let report = op.apply(&sys, c).unwrap();
        assert_eq!(report.bytes, 4);
        assert_eq!(fake.content("/opt/f").unwrap(), "data");
    }

    /// A GNU sparse member (type `S`): a hole at the start, 4 bytes of data,
    /// and a hole at the end. The header's `size` is the 4 bytes stored;
    /// the sparse map's real size is 2048, which is what `apply` reads, what
    /// the member must come to, and what the report counts.
    #[test]
    fn a_gnu_sparse_member_extracts_with_its_holes() {
        let mut h = tar::Header::new_gnu();
        h.set_path("sparse").unwrap();
        h.set_entry_type(tar::EntryType::GNUSparse);
        h.set_mode(0o644);
        h.set_size(4);
        {
            let g = h.as_gnu_mut().unwrap();
            g.set_real_size(2048);
            g.sparse[0].set_offset(1024);
            g.sparse[0].set_length(4);
            // The zero-length block at the real size is how GNU tar marks
            // a hole running to the end of the file.
            g.sparse[1].set_offset(2048);
            g.sparse[1].set_length(0);
        }
        h.set_cksum();
        let mut archive = h.as_bytes().to_vec();
        archive.extend_from_slice(b"data");
        archive.extend(std::iter::repeat_n(0u8, 508 + 1024));
        let (fake, sys) = sys_with(&archive);
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.diff().short(),
            "extract /tmp/a.tar (tar: 1 files, 0 dirs, 0 symlinks, 2048 bytes) into /opt"
        );
        let report = op.apply(&sys, c).unwrap();
        assert_eq!(report.bytes, 2048);
        let mut want = vec![0u8; 2048];
        want[1024..1028].copy_from_slice(b"data");
        assert_eq!(fake.file("/opt/sparse").unwrap().bytes, want);
    }

    // `fixtures/archive/pax-sparse-*.tar` and `gnu-sparse.tar`: one sparse
    // file, `sparse`, in GNU tar's three pax sparse formats and as an
    // old-GNU sparse member. 65536 bytes, `hello, sparse\n` at 32768 in a
    // 4096-byte data block, holes around it. The README beside them has
    // the commands.
    const PAX_SPARSE: [(&str, &[u8]); 3] = [
        (
            "0.0",
            include_bytes!("../fixtures/archive/pax-sparse-0.0.tar"),
        ),
        (
            "0.1",
            include_bytes!("../fixtures/archive/pax-sparse-0.1.tar"),
        ),
        (
            "1.0",
            include_bytes!("../fixtures/archive/pax-sparse-1.0.tar"),
        ),
    ];
    const GNU_SPARSE: &[u8] = include_bytes!("../fixtures/archive/gnu-sparse.tar");

    /// `tar` cannot expand a pax sparse member, and what it does instead is
    /// silent: 0.0 lands at the right path with the holes squeezed out,
    /// 0.1 and 1.0 under `GNUSparseFile.<pid>/`. Each is refused by
    /// `check`, under `--check` too, naming the member by its real name and
    /// not the header's `GNUSparseFile.<pid>/sparse` placeholder.
    #[test]
    fn pax_sparse_members_are_refused_at_check_naming_the_member() {
        for (version, archive) in PAX_SPARSE {
            let (_, sys) = sys_with(archive);
            for sys in [sys.clone(), sys.with_check_mode(true)] {
                let err = Extracted::from_path("/tmp/a.tar")
                    .to("/opt")
                    .check(&sys)
                    .unwrap_err()
                    .chain();
                assert!(
                    err.contains(&format!(
                        "/tmp/a.tar: refusing to extract: `sparse` is a pax sparse member{UNSPARSE}"
                    )),
                    "pax sparse {version}: {err}"
                );
            }
        }
    }

    /// The refusal's tail, after what it names.
    const UNSPARSE: &str = ", which `archive::Extracted` cannot expand; recreate the archive \
        without sparse handling (GNU tar: drop `-S`, or use `--format=gnu`; bsdtar: \
        `--no-read-sparse`)";

    /// One pax record, `"<len> <key>=<value>\n"`, its length counting itself.
    fn pax_record(key: &str, value: &[u8]) -> Vec<u8> {
        let body = key.len() + value.len() + 3;
        let len = (1..)
            .map(|digits| digits + body)
            .find(|n| n.to_string().len() + body == *n)
            .unwrap();
        let mut out = format!("{len} {key}=").into_bytes();
        out.extend_from_slice(value);
        out.push(b'\n');
        out
    }

    /// `records` as a pax header (`x`, or `g` for a global one) ahead of a
    /// four-byte file `f`.
    fn tar_with_pax(ty: u8, records: &[Vec<u8>]) -> Vec<u8> {
        raw_tar(&[
            ("PaxHeaders/f", ty, &records.concat(), ""),
            ("f", b'0', b"data", ""),
        ])
    }

    /// A pax value may hold a newline (a binary xattr, a multi-line
    /// `comment`), and `tar`'s record parser splits on newlines, so the
    /// record reads as malformed. That is not a sparse record and does not
    /// refuse the archive; `tar` skips it the same way.
    #[test]
    fn a_pax_record_whose_value_holds_a_newline_does_not_refuse_the_archive() {
        let archive = tar_with_pax(
            b'x',
            &[
                pax_record("SCHILY.xattr.security.capability", b"\x01\0\0\x02\n \0\0"),
                pax_record("comment", b"two\nlines"),
            ],
        );
        let (fake, sys) = sys_with(&archive);
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(fake.content("/opt/f").unwrap(), "data");
    }

    /// A sparse record after one the parser cannot read is still seen.
    #[test]
    fn a_sparse_record_after_a_malformed_one_is_still_refused() {
        let err = check_err(&tar_with_pax(
            b'x',
            &[
                pax_record("comment", b"two\nlines"),
                pax_record("GNU.sparse.major", b"1"),
                pax_record("GNU.sparse.name", b"real/name"),
            ],
        ));
        assert!(
            err.contains(&format!("`real/name` is a pax sparse member{UNSPARSE}")),
            "{err}"
        );
    }

    /// Sparse records in a global header (`g`) belong to no member, so the
    /// refusal says where they are rather than naming the header's own
    /// path as if it were one.
    #[test]
    fn sparse_records_in_a_pax_global_header_are_refused_as_such() {
        let err = check_err(&tar_with_pax(b'g', &[pax_record("GNU.sparse.major", b"1")]));
        assert!(
            err.contains(&format!(
                "the archive's pax global header declares GNU sparse records{UNSPARSE}"
            )),
            "{err}"
        );
        assert!(!err.contains("PaxHeaders"), "{err}");
    }

    /// A global header's records are scanned as bytes, not parsed, so a
    /// sparse record GNU tar reads and the `tar` crate cannot (whitespace
    /// before its length, or after a value ending in a newline, which ends
    /// the crate's iteration) is still seen.
    #[test]
    fn sparse_records_the_tar_crate_cannot_parse_in_a_global_header_are_refused() {
        for records in [
            b"\t23 GNU.sparse.major=1\n".to_vec(),
            b" 23 GNU.sparse.major=1\n".to_vec(),
            [
                pax_record("comment", b"ends\n"),
                pax_record("GNU.sparse.major", b"1"),
            ]
            .concat(),
        ] {
            let err = check_err(&tar_with_pax(b'g', &[records]));
            assert!(
                err.contains(&format!(
                    "the archive's pax global header declares GNU sparse records{UNSPARSE}"
                )),
                "{err}"
            );
        }
    }

    /// The `tar` crate attaches a held `x` header to the next entry it
    /// returns, a global header included, where GNU tar applies it to the
    /// member after. Sparse records arriving that way are refused too,
    /// whether or not `.strip_components` drops the global header itself.
    #[test]
    fn sparse_records_in_a_pax_header_ahead_of_a_global_header_are_refused() {
        let archive = raw_tar(&[
            (
                "PaxHeaders/f",
                b'x',
                &pax_record("GNU.sparse.major", b"1"),
                "",
            ),
            (
                "pax_global_header",
                b'g',
                &pax_record("comment", b"hello"),
                "",
            ),
            ("d/f", b'0', b"data", ""),
        ]);
        let (_, sys) = sys_with(&archive);
        for strip in [0, 1] {
            let err = Extracted::from_path("/tmp/a.tar")
                .to("/opt")
                .strip_components(strip)
                .check(&sys)
                .unwrap_err()
                .chain();
            assert!(
                err.contains(&format!(
                    "a pax header ahead of the archive's global header declares GNU sparse \
                     records{UNSPARSE}"
                )),
                "strip {strip}: {err}"
            );
        }
    }

    /// The name comes from the archive: control characters are escaped, so
    /// it cannot drive a terminal, and it is cut to 256 bytes.
    #[test]
    fn a_sparse_name_from_the_archive_is_escaped_and_capped() {
        let mut name = b"\x1b[31m".to_vec();
        name.extend(std::iter::repeat_n(b'x', 1000));
        let err = check_err(&tar_with_pax(b'x', &[pax_record("GNU.sparse.name", &name)]));
        assert!(!err.contains('\x1b'), "{err:?}");
        let shown = format!("\\u{{1b}}[31m{}…", "x".repeat(256 - 10));
        assert!(
            err.contains(&format!("`{shown}` is a pax sparse member{UNSPARSE}")),
            "{err}"
        );
    }

    /// The same file as GNU tar writes it with `--format=gnu -S`, a type `S`
    /// member, extracts in full: the holes come back as zeros.
    #[test]
    fn a_gnu_tar_old_gnu_sparse_member_extracts() {
        let (fake, sys) = sys_with(GNU_SPARSE);
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        assert_eq!(
            c.diff().short(),
            "extract /tmp/a.tar (tar: 1 files, 0 dirs, 0 symlinks, 65536 bytes) into /opt"
        );
        op.apply(&sys, c).unwrap();
        let mut want = vec![0u8; 65536];
        want[32768..32782].copy_from_slice(b"hello, sparse\n");
        let got = fake.file("/opt/sparse").unwrap();
        assert_eq!((got.mode, got.bytes == want), (0o644, true));
    }

    /// What `/opt` holds, read through the `Fake` itself.
    fn sys_read_dir(fake: &Arc<Fake>) -> Vec<PathBuf> {
        fake_sys(fake).read_dir("/opt").unwrap()
    }

    /// A `Fake` that refuses a path with a component over `NAME_MAX` (255
    /// bytes) as a real filesystem does, and whose `symlink` or `rename`
    /// fails on request, as a full disk or a read-only mount would. The
    /// `Fake` does neither, so this wraps it through [`System::new`];
    /// everything else passes through.
    struct Failing {
        fake: Arc<Fake>,
        symlink: bool,
        rename: bool,
    }

    impl Failing {
        fn sys(fake: &Arc<Fake>, symlink: bool, rename: bool) -> System {
            let facts = fake_sys(fake).facts().clone();
            let backend = Arc::new(Failing {
                fake: fake.clone(),
                symlink,
                rename,
            });
            System::new(backend, facts, false, Arc::new(Collect::default()))
        }

        fn refused(what: &str, p: &Path) -> std::io::Error {
            std::io::Error::other(format!("{what} {}: refused (test)", p.display()))
        }

        fn name_max(p: &Path) -> std::io::Result<()> {
            match p.components().any(|c| c.as_os_str().len() > 255) {
                true => Err(std::io::Error::other("File name too long (os error 36)")),
                false => Ok(()),
            }
        }
    }

    impl Backend for Failing {
        fn read(&self, p: &Path) -> std::io::Result<Vec<u8>> {
            self.fake.read(p)
        }
        fn write(&self, p: &Path, bytes: &[u8]) -> std::io::Result<()> {
            Self::name_max(p)?;
            self.fake.write(p, bytes)
        }
        fn write_from(
            &self,
            p: &Path,
            src: &mut dyn std::io::Read,
            attrs: Option<WriteAttrs>,
        ) -> std::io::Result<u64> {
            Self::name_max(p)?;
            self.fake.write_from(p, src, attrs)
        }
        fn open_read(&self, p: &Path) -> std::io::Result<Box<dyn std::io::Read + Send + '_>> {
            self.fake.open_read(p)
        }
        fn stat(&self, p: &Path) -> std::io::Result<Option<Stat>> {
            self.fake.stat(p)
        }
        fn stat_follow(&self, p: &Path) -> std::io::Result<Option<Stat>> {
            self.fake.stat_follow(p)
        }
        fn mkdir_all(&self, p: &Path) -> std::io::Result<()> {
            Self::name_max(p)?;
            self.fake.mkdir_all(p)
        }
        fn remove(&self, p: &Path) -> std::io::Result<()> {
            self.fake.remove(p)
        }
        fn remove_all(&self, p: &Path) -> std::io::Result<()> {
            self.fake.remove_all(p)
        }
        fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            if self.rename {
                return Err(Self::refused("rename", to));
            }
            Self::name_max(from)?;
            Self::name_max(to)?;
            self.fake.rename(from, to)
        }
        fn set_mode(&self, p: &Path, mode: u32) -> std::io::Result<()> {
            self.fake.set_mode(p, mode)
        }
        fn set_owner(&self, p: &Path, uid: u32, gid: u32) -> std::io::Result<()> {
            self.fake.set_owner(p, uid, gid)
        }
        fn copy(&self, from: &Path, to: &Path) -> std::io::Result<()> {
            self.fake.copy(from, to)
        }
        fn symlink(&self, target: &Path, link: &Path) -> std::io::Result<()> {
            if self.symlink {
                return Err(Self::refused("symlink", link));
            }
            Self::name_max(link)?;
            self.fake.symlink(target, link)
        }
        fn read_link(&self, p: &Path) -> std::io::Result<PathBuf> {
            self.fake.read_link(p)
        }
        fn read_dir(&self, p: &Path) -> std::io::Result<Vec<PathBuf>> {
            self.fake.read_dir(p)
        }
        fn spawn(&self, spec: &CmdSpec) -> std::io::Result<Output> {
            self.fake.spawn(spec)
        }
    }

    /// A symlink member is made under a temporary name and renamed over its
    /// path, so the entry already there, a link or a file, is replaced
    /// whole or not at all: a failed `symlink` leaves it untouched, and a
    /// failed `rename` takes the temporary link away and leaves it too.
    /// Removing the old entry first, as `apply` once did, loses it on
    /// either failure.
    #[test]
    fn a_symlink_member_replaces_the_entry_there_whole_or_not_at_all() {
        let archive = raw_tar(&[("link", b'2', b"", "new")]);
        let planted = |old: &str| {
            let fake = Fake::new()
                .with_dir("/opt")
                .with_file("/tmp/a.tar", &archive);
            Arc::new(match old {
                "symlink" => fake.with_symlink("/opt/link", "old"),
                _ => fake.with_file("/opt/link", "old contents"),
            })
        };
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        for old in ["symlink", "file"] {
            for (symlink, rename, refused) in [(true, false, "symlink"), (false, true, "rename")] {
                let fake = planted(old);
                let before = fake.file("/opt/link").unwrap();
                let sys = Failing::sys(&fake, symlink, rename);
                let c = expect_change(&op, &sys);
                let err = op.apply(&sys, c).unwrap_err().chain();
                assert!(
                    err.contains("linking member `link` at /opt/link: "),
                    "{err}"
                );
                assert!(err.contains(&format!("{refused} ")), "{err}");
                assert!(err.contains("refused (test)"), "{err}");
                let after = fake.file("/opt/link").unwrap();
                assert_eq!(
                    (after.kind, after.bytes),
                    (before.kind, before.bytes),
                    "{old} after a failed {refused}"
                );
                assert_eq!(
                    sys_read_dir(&fake),
                    vec![PathBuf::from("/opt/link")],
                    "no temporary link left behind after a failed {refused}"
                );
            }
            // Nothing failing: the link replaces the entry, and only the
            // link is left.
            let fake = planted(old);
            let sys = fake_sys(&fake);
            let c = expect_change(&op, &sys);
            op.apply(&sys, c).unwrap();
            assert_eq!(sys.read_link("/opt/link").unwrap(), PathBuf::from("new"));
            assert_eq!(sys_read_dir(&fake), vec![PathBuf::from("/opt/link")]);
        }
    }

    /// A symlink member whose name is near `NAME_MAX` still replaces the
    /// link there: the temporary link has a short name of its own, not the
    /// member's with a suffix that would push it over.
    #[test]
    fn a_symlink_member_with_a_long_name_replaces_the_link_there() {
        let name = "l".repeat(240);
        let mut b = tar::Builder::new(Vec::new());
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        h.set_mode(0o777);
        b.append_link(&mut h, &name, "new").unwrap();
        let archive = b.into_inner().unwrap();
        let full = PathBuf::from("/opt").join(&name);
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/tmp/a.tar", &archive)
                .with_symlink(&full, "old"),
        );
        let sys = Failing::sys(&fake, false, false);
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        assert_eq!(sys.read_link(&full).unwrap(), PathBuf::from("new"));
        assert_eq!(sys_read_dir(&fake), vec![full]);
    }

    /// A member's size from `check` is only counted in the report, never
    /// reserved: a claim of 2^62 bytes (through `apply` only a GNU sparse
    /// member can carry one its stream does not back) streams the two
    /// bytes the archive declares where `apply` reads it. Reserving from
    /// the claim, as `apply` once did, aborts the whole test binary (Rust
    /// aborts on allocation failure).
    #[test]
    fn write_member_reserves_nothing_from_the_planned_size() {
        let (fake, sys) = sys_with(&raw_tar(&[]));
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let member = Member {
            path: "big".into(),
            kind: Kind::File,
            mode: 0o644,
            size: 1u64 << 62,
        };
        op.write_member(&sys, Path::new("/opt"), &member, 2, &mut &b"ok"[..])
            .unwrap();
        assert_eq!(fake.content("/opt/big").unwrap(), "ok");
    }

    #[test]
    fn on_disk_conflicts_are_refused_at_check() {
        // An ancestor that is a symlink on disk.
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_dir("/elsewhere")
                .with_symlink("/opt/hello", "/elsewhere")
                .with_file("/tmp/hello.tar", TAR),
        );
        let sys = fake_sys(&fake);
        let err = Extracted::from_path("/tmp/hello.tar")
            .to("/opt")
            .check(&sys)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("/opt/hello is a symlink; refusing to extract through it"),
            "{err}"
        );

        // A directory where the archive has a file, and a file where it has a directory.
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_dir("/opt/hello")
                .with_dir("/opt/hello/README.txt")
                .with_file("/tmp/hello.tar", TAR),
        );
        let err = Extracted::from_path("/tmp/hello.tar")
            .to("/opt")
            .check(&fake_sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("/opt/hello/README.txt is a directory; the archive has a file there"),
            "{err}"
        );
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/opt/hello", "not a dir")
                .with_file("/tmp/hello.tar", TAR),
        );
        let err = Extracted::from_path("/tmp/hello.tar")
            .to("/opt")
            .check(&fake_sys(&fake))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("/opt/hello is not a directory but the archive puts entries under it"),
            "{err}"
        );
    }

    #[test]
    fn destination_and_source_problems() {
        let fake = Arc::new(
            Fake::new()
                .with_file("/tmp/hello.tar", TAR)
                .with_file("/f", "x"),
        );
        let sys = fake_sys(&fake);
        let err = |op: Extracted| op.check(&sys).unwrap_err().chain();
        assert!(
            err(Extracted::from_path("/tmp/hello.tar").to("/opt"))
                .contains("/opt does not exist; create it first with file::Directory")
        );
        // Under --check the missing destination is one an earlier
        // file::Directory may create (vision 12): would change, nothing written.
        let dry = fake_sys(&fake).with_check_mode(true);
        let c = expect_change(&Extracted::from_path("/tmp/hello.tar").to("/opt"), &dry);
        assert!(
            c.diff().short().starts_with("extract /tmp/hello.tar"),
            "{}",
            c.diff().short()
        );
        assert!(fake.file("/opt").is_none());
        assert!(
            err(Extracted::from_path("/tmp/hello.tar").to("/f"))
                .contains("/f is not a directory (File)")
        );
        assert!(
            err(Extracted::from_path("/tmp/nope.tar").to("/"))
                .contains("reading archive /tmp/nope.tar")
        );
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/tmp/x.zip", b"PK\x03\x04junk")
                .with_file("/tmp/x.txt", "just text, long enough? no")
                .with_file("/tmp/x.gz", {
                    use std::io::Write;
                    let mut e =
                        flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                    e.write_all(b"not a tar at all").unwrap();
                    e.finish().unwrap()
                }),
        );
        let sys = fake_sys(&fake);
        let err = |op: Extracted| op.check(&sys).unwrap_err().chain();
        assert!(
            err(Extracted::from_path("/tmp/x.zip").to("/opt"))
                .contains("/tmp/x.zip: zip archives are not supported")
        );
        assert!(
            err(Extracted::from_path("/tmp/x.txt").to("/opt"))
                .contains("/tmp/x.txt: not a recognised archive")
        );
        assert!(
            err(Extracted::from_path("/tmp/x.gz").to("/opt"))
                .contains("/tmp/x.gz: the tar.gz stream does not contain a tar archive")
        );
    }

    #[test]
    fn check_mode_reports_the_counts_in_the_diff_and_writes_nothing() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/tmp/hello.tar.zst", TZST),
        );
        let sys = System::fake(fake.clone(), Arc::new(Collect::default())).with_check_mode(true);
        let mut ctx = Ctx::new(sys, rustible_sdk::HostInfo::local());
        let r = ctx
            .step(
                "extract",
                Extracted::from_path("/tmp/hello.tar.zst").to("/opt"),
            )
            .unwrap();
        // A would-change step has no output (vision 12); the diff says what
        // the extraction would do.
        assert!(r.changed && !r.is_available());
        assert_eq!(
            r.diff.as_ref().unwrap().short(),
            "extract /tmp/hello.tar.zst (tar.zst: 2 files, 3 dirs, 1 symlinks, 38 bytes) into /opt"
        );
        assert!(fake.file("/opt/hello").is_none());
    }

    #[test]
    fn missing_creates_marker_after_apply_warns() {
        let fake = Arc::new(
            Fake::new()
                .with_dir("/opt")
                .with_file("/tmp/hello.tar", TAR),
        );
        let sink = Arc::new(Collect::default());
        let sys = System::fake(fake.clone(), sink.clone());
        let op = Extracted::from_path("/tmp/hello.tar")
            .to("/opt")
            .creates("hello/nope");
        let c = expect_change(&op, &sys);
        op.apply(&sys, c).unwrap();
        let warned = sink.events().iter().any(|e| {
            matches!(e, rustible_sdk::event::Event::Log { level: rustible_sdk::event::Level::Warn, msg }
                if msg.contains("creates marker /opt/hello/nope does not exist"))
        });
        assert!(warned, "{:?}", sink.events());
    }

    // ---- streaming: one path for every format (decision 29 on #88) ----

    use rustible_sdk::protocol::CHUNK_SIZE;

    /// The big member's size: over one helper chunk and over the zstd
    /// decoder's look-ahead, and not a multiple of either.
    const BIG: usize = 3 * CHUNK_SIZE / 2 + 7;

    /// `n` bytes of numbered lines: compressible, so the encoders are quick,
    /// and different at every offset, so a dropped, repeated or shifted
    /// piece shows.
    fn numbered(n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n + 16);
        let mut i = 0u64;
        while out.len() < n {
            out.extend_from_slice(format!("{i:015}\n").as_bytes());
            i += 1;
        }
        out.truncate(n);
        out
    }

    /// One tree as a tar: a directory, a small file, a member of `big`
    /// bytes, a hard link to the small file, a symlink, and a file after
    /// them all.
    fn tree_tar(big: usize) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        let header = |ty: tar::EntryType, mode: u32, size: usize| {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(ty);
            h.set_mode(mode);
            h.set_size(size as u64);
            h.set_mtime(0);
            h
        };
        let file = |b: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32, data: &[u8]| {
            let mut h = header(tar::EntryType::Regular, mode, data.len());
            b.append_data(&mut h, path, data).unwrap();
        };
        let mut h = header(tar::EntryType::Directory, 0o755, 0);
        b.append_data(&mut h, "m/", &[][..]).unwrap();
        file(&mut b, "m/small", 0o644, b"small\n");
        file(&mut b, "m/big", 0o640, &numbered(big));
        let mut h = header(tar::EntryType::Link, 0o644, 0);
        b.append_link(&mut h, "m/hard", "m/small").unwrap();
        let mut h = header(tar::EntryType::Symlink, 0o777, 0);
        b.append_link(&mut h, "m/link", "small").unwrap();
        file(&mut b, "m/after", 0o600, b"after\n");
        b.into_inner().unwrap()
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut w =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(0)).unwrap();
        w.write_all(bytes).unwrap();
        w.finish().unwrap()
    }

    /// One zstd frame, with its content checksum.
    fn zst(bytes: &[u8]) -> Vec<u8> {
        ruzstd::encoding::compress_to_vec(bytes, ruzstd::encoding::CompressionLevel::Fastest)
    }

    /// A zstd skippable frame carrying `payload`.
    fn skippable(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x50, 0x2a, 0x4d, 0x18];
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    /// An encoder: bytes in, one frame of them out.
    type Encode = fn(&[u8]) -> Vec<u8>;

    /// The compressed formats, by the encoder that makes one frame (one
    /// gzip member, one xz stream) of their kind.
    fn encoders() -> [(Format, Encode); 3] {
        [
            (Format::TarGz, gz),
            (Format::TarXz, xz),
            (Format::TarZst, zst),
        ]
    }

    /// Where the multi-frame cases cut the tar: inside the big member, at
    /// no block or chunk boundary.
    const CUT: usize = 3 * 512 + CHUNK_SIZE / 2 + 3;

    /// Everything under `/opt` in `fake`: each path with its kind, its mode
    /// and its content, or a link's target.
    fn tree(fake: &Arc<Fake>) -> Vec<(PathBuf, String, u32, Vec<u8>)> {
        let sys = fake_sys(fake);
        let mut out = vec![];
        let mut todo = vec![PathBuf::from("/opt")];
        while let Some(dir) = todo.pop() {
            for p in sys.read_dir(&dir).unwrap() {
                let f = fake.file(&p).unwrap();
                let body = match f.kind {
                    FileKind::Dir => {
                        todo.push(p.clone());
                        vec![]
                    }
                    FileKind::Symlink => sys
                        .read_link(&p)
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes(),
                    _ => f.bytes,
                };
                out.push((p, format!("{:?}", f.kind), f.mode, body));
            }
        }
        out.sort();
        out
    }

    /// `archive` extracted into an empty `/opt`, check then apply: the
    /// `Fake`, the report, and the reads the extraction made (decision 28).
    fn extract(archive: &[u8]) -> (Arc<Fake>, ExtractReport, Vec<ReadCall>) {
        let (fake, sys) = sys_with(archive);
        let planted = fake.reads().len();
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        let r = op.apply(&sys, c).unwrap();
        let reads = fake.reads()[planted..].to_vec();
        (fake, r, reads)
    }

    /// What extracting the plain tar gives: the tree every encoding of it
    /// must give too, byte for byte.
    fn plain_tree(tar: &[u8]) -> Vec<(PathBuf, String, u32, Vec<u8>)> {
        let (fake, r, _) = extract(tar);
        assert_eq!(
            (r.format, r.files, r.dirs, r.symlinks),
            (Some(Format::Tar), 4, 1, 1)
        );
        let t = tree(&fake);
        assert_eq!(fake.file("/opt/m/big").unwrap().bytes, numbered(BIG));
        assert_eq!(fake.content("/opt/m/hard").unwrap(), "small\n");
        t
    }

    /// Every read an extraction made is a stream (`open_read`), never a
    /// whole-file `read`: the archive twice, once by `check` and once by
    /// `apply`, and the hard link's source once.
    fn assert_streamed(reads: &[ReadCall], archive_len: usize) {
        assert!(reads.iter().all(|r| r.streamed), "{reads:?}");
        let archive: Vec<_> = reads
            .iter()
            .filter(|r| r.path == Path::new("/tmp/a.tar"))
            .collect();
        assert_eq!(archive.len(), 2, "{reads:?}");
        assert!(
            archive.iter().all(|r| r.bytes <= archive_len as u64),
            "{reads:?}"
        );
        assert_eq!(
            reads
                .iter()
                .filter(|r| r.path == Path::new("/opt/m/small"))
                .count(),
            1,
            "{reads:?}"
        );
    }

    /// One frame, or one gzip member or xz stream, of a tar with a member
    /// over one chunk: the same tree as the plain tar, byte for byte, every
    /// read a stream.
    #[test]
    fn every_format_streams_a_member_over_one_chunk_byte_identical() {
        let tar = tree_tar(BIG);
        let want = plain_tree(&tar);
        let (_, _, reads) = extract(&tar);
        assert_streamed(&reads, tar.len());
        for (format, encode) in encoders() {
            let archive = encode(&tar);
            let (fake, r, reads) = extract(&archive);
            assert_eq!(r.format, Some(format));
            // `small` and `after`, 6 bytes each, and `big`; a hard link
            // adds nothing.
            assert_eq!(r.bytes, (BIG + 12) as u64);
            assert_eq!(tree(&fake), want, "{format:?}");
            assert_streamed(&reads, archive.len());
        }
    }

    /// Several frames, the tar cut between them inside the big member:
    /// concatenated gzip members (`pigz`, `cat a.gz b.gz`), xz streams, and
    /// zstd frames. Each is read to its end, not just to the end of its
    /// first frame.
    #[test]
    fn every_format_reads_several_frames_byte_identical() {
        let tar = tree_tar(BIG);
        let want = plain_tree(&tar);
        for (format, encode) in encoders() {
            let archive = [encode(&tar[..CUT]), encode(&tar[CUT..])].concat();
            let (fake, r, _) = extract(&archive);
            assert_eq!(r.format, Some(format));
            assert_eq!(tree(&fake), want, "{format:?}");
            // Three frames, the middle one empty of tar but not of data.
            let archive = [
                encode(&tar[..512]),
                encode(&tar[512..CUT]),
                encode(&tar[CUT..]),
            ]
            .concat();
            let (fake, _, _) = extract(&archive);
            assert_eq!(tree(&fake), want, "{format:?}, three frames");
        }
    }

    /// What lies between frames and is not data: a zstd skippable frame,
    /// and xz's stream padding (zero bytes, a multiple of four). Skipped,
    /// and the tree is the plain tar's.
    #[test]
    fn skippable_frames_and_stream_padding_are_skipped() {
        let tar = tree_tar(BIG);
        let want = plain_tree(&tar);
        for archive in [
            [
                zst(&tar[..CUT]),
                skippable(b"rustible"),
                skippable(b""),
                zst(&tar[CUT..]),
            ]
            .concat(),
            // After the last frame too.
            [zst(&tar), skippable(&[7; 300])].concat(),
            [xz(&tar[..CUT]), vec![0; 8], xz(&tar[CUT..])].concat(),
        ] {
            let (fake, _, _) = extract(&archive);
            assert_eq!(tree(&fake), want);
        }
        // A skippable frame cut short is refused, naming what is wrong.
        let cut = [zst(&tar), skippable(&[7; 300])].concat();
        let err = check_err(&cut[..cut.len() - 10]);
        assert!(err.contains("zstd: truncated skippable frame"), "{err}");
    }

    /// The last frame's checksum is verified though `tar` stops reading at
    /// the archive's end marker, ahead of it, and so is anything after the
    /// last frame: both refused at `check`, as when the stream was decoded
    /// whole.
    #[test]
    fn the_last_zstd_frame_is_read_to_its_checksum() {
        let tar = tree_tar(BIG);
        let mut bad = zst(&tar);
        *bad.last_mut().unwrap() ^= 0xff;
        let err = check_err(&bad);
        assert!(
            err.contains("/tmp/a.tar: decompressing: zstd: frame checksum mismatch"),
            "{err}"
        );
        let trailing = [zst(&tar), b"junk".to_vec()].concat();
        let err = check_err(&trailing);
        assert!(err.contains("/tmp/a.tar: decompressing: zstd: "), "{err}");
    }

    /// An archive with no members, only the end marker, is refused in every
    /// format, the same way it always was: a plain tar's first block is no
    /// `ustar` header, so there is no format to name, and a compressed
    /// one's stream holds no tar.
    #[test]
    fn an_empty_archive_is_refused_alike_in_every_format() {
        let tar = tar::Builder::new(Vec::new()).into_inner().unwrap();
        assert_eq!(tar, vec![0; 1024]);
        let err = check_err(&tar);
        assert!(
            err.contains("/tmp/a.tar: not a recognised archive"),
            "{err}"
        );
        for (format, encode) in encoders() {
            let err = check_err(&encode(&tar));
            assert!(
                err.contains(&format!(
                    "/tmp/a.tar: the {} stream does not contain a tar archive",
                    format.name()
                )),
                "{err}"
            );
        }
    }

    /// A stream cut inside the big member, in every format. `check` refuses
    /// it, writing nothing. Swapped in after `check` instead, `apply`
    /// extracts the members before the cut and fails at the cut one, which
    /// leaves nothing at its path and nothing staged beside it.
    #[test]
    fn a_truncated_stream_writes_nothing_for_the_cut_member() {
        // Big enough that the cut lands after the first of xz's LZMA2
        // chunks (up to 2 MiB of output each, decoded whole), so `small`
        // comes out of the cut stream in every format.
        let tar = tree_tar(5 * CHUNK_SIZE + 7);
        let plain: (Format, Encode) = (Format::Tar, |b| b.to_vec());
        for (format, encode) in [plain].into_iter().chain(encoders()) {
            let whole = encode(&tar);
            let cut = &whole[..whole.len() * 3 / 4];

            let err = check_err(cut);
            assert!(err.contains("/tmp/a.tar"), "{format:?}: {err}");

            let (fake, sys) = sys_with(&whole);
            let op = Extracted::from_path("/tmp/a.tar").to("/opt");
            let c = expect_change(&op, &sys);
            sys.write_atomic("/tmp/a.tar", cut).unwrap();
            let err = op.apply(&sys, c).unwrap_err().chain();
            assert!(err.contains("member `m/big`"), "{format:?}: {err}");
            assert!(
                err.contains("nothing was written at /opt/m/big"),
                "{format:?}: {err}"
            );
            assert_eq!(fake.content("/opt/m/small").unwrap(), "small\n");
            assert_eq!(
                sys.read_dir("/opt/m").unwrap(),
                [PathBuf::from("/opt/m/small")],
                "{format:?}: nothing at the cut member's path, nothing staged, nothing after it"
            );
        }
    }

    // ---- through a helper ----

    /// `f` on a thread of its own, failing the test when it takes longer
    /// than `secs`: a deadlock between the archive's reads and the members'
    /// writes on one helper fails instead of hanging the suite.
    fn bounded<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
            Ok(t) => t,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("not done after {secs} s: reads and writes deadlocked on the helper?")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("the bounded body panicked; its message is above")
            }
        }
    }

    /// A system over a real escalation helper in this process, and a fresh
    /// directory for it, removed when the guard drops.
    fn helper_scratch(name: &str) -> (System, PathBuf, impl Drop) {
        struct Cleanup(System, PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.remove_all(&self.1);
            }
        }
        let sys = System::in_process_helper(Arc::new(Collect::default())).unwrap();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "rustible-archive-{name}-{}-{nanos}",
            std::process::id()
        ));
        sys.mkdir_all(dir.join("dest")).unwrap();
        let guard = Cleanup(sys.clone(), dir.clone());
        (sys, dir, guard)
    }

    /// Through one real escalation helper, as root's or a user's would be:
    /// the archive is read with `open_read` while each member is written
    /// with `write_from`, so the two streams alternate on the helper chunk
    /// by chunk. A plain tar, which is itself several chunks, and a
    /// `.tar.zst`, whose member is; each with a member of four chunks and
    /// more, which comes out byte for byte, with its mode, and with nothing
    /// staged left beside it.
    #[test]
    fn extracting_through_one_helper_interleaves_reads_and_writes() {
        let big = 4 * CHUNK_SIZE + 9;
        let tar = tree_tar(big);
        for (name, archive) in [("plain.tar", tar.clone()), ("z.tar.zst", zst(&tar))] {
            bounded(180, move || {
                let (sys, dir, _guard) = helper_scratch("interleave");
                let (src, dest) = (dir.join(name), dir.join("dest"));
                sys.write_atomic(&src, &archive).unwrap();
                let op = Extracted::from_path(&src).to(&dest).creates("m/after");
                let c = expect_change(&op, &sys);
                let r = op.apply(&sys, c).unwrap();
                assert_eq!((r.files, r.bytes), (4, (big + 12) as u64), "{name}");
                let m = dest.join("m");
                assert_eq!(sys.read(m.join("big")).unwrap(), numbered(big), "{name}");
                assert_eq!(sys.stat(m.join("big")).unwrap().unwrap().mode, 0o640);
                assert_eq!(sys.read(m.join("hard")).unwrap(), b"small\n");
                assert_eq!(sys.stat(m.join("after")).unwrap().unwrap().mode, 0o600);
                assert_eq!(
                    sys.read_link(m.join("link")).unwrap(),
                    PathBuf::from("small")
                );
                let mut left = sys.read_dir(&m).unwrap();
                left.sort();
                assert_eq!(
                    left,
                    ["after", "big", "hard", "link", "small"].map(|n| m.join(n)),
                    "{name}: nothing staged left beside the members"
                );
                assert!(matches!(op.check(&sys).unwrap(), Plan::Satisfied(_)));
            });
        }
    }

    /// A dry run through the helper reads an archive of several chunks
    /// whole, to walk it, and reports `would change` with the counts. The
    /// helper is left serving: a read, a write and a listing after it
    /// still work (before #84 an answer over one frame killed it for the
    /// run, so a dry run of a large archive as root failed every escalated
    /// step after it). Nothing is written.
    #[test]
    fn a_dry_run_through_the_helper_reads_the_archive_in_chunks_and_leaves_it_serving() {
        let big = 5 * CHUNK_SIZE + 3;
        let tar = tree_tar(big);
        assert!(tar.len() > 5 * CHUNK_SIZE);
        bounded(180, move || {
            let (sys, dir, _guard) = helper_scratch("dry");
            let (src, dest) = (dir.join("a.tar"), dir.join("dest"));
            sys.write_atomic(&src, &tar).unwrap();
            let mut ctx = Ctx::new(
                sys.clone().with_check_mode(true),
                rustible_sdk::HostInfo::local(),
            );
            let step = ctx
                .step("extract", Extracted::from_path(&src).to(&dest))
                .unwrap();
            assert!(step.changed && !step.is_available());
            assert_eq!(
                step.diff.as_ref().unwrap().short(),
                format!(
                    "extract {} (tar: 4 files, 1 dirs, 1 symlinks, {} bytes) into {}",
                    src.display(),
                    big + 12,
                    dest.display()
                )
            );
            assert_eq!(sys.read_dir(&dest).unwrap(), Vec::<PathBuf>::new());
            assert_eq!(sys.read(&src).unwrap(), tar);
            sys.write_atomic(dir.join("after"), b"still serving")
                .unwrap();
            assert_eq!(sys.read(dir.join("after")).unwrap(), b"still serving");
        });
    }
}
