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
//! An archive that is not a plain tarball (a URL, a zip) is somebody
//! else's job: fetch with [`crate::http::Download`] first (vision 6.7).

use std::collections::BTreeSet;
use std::io::{Cursor, Read};
use std::path::{Component, Path, PathBuf};

use rustible_sdk::backend::FileKind;
use rustible_sdk::prelude::*;

use crate::file::Owner;

/// Ceiling on the buffer capacity reserved from a tar header's `size`
/// field before a member is read. The field is attacker-controlled and
/// unbounded, so it is a hint, not an allocation: `read_to_end` grows the
/// buffer against the real stream from here.
const READ_CAPACITY_CEILING: u64 = 1 << 20;

/// A supported archive format, as detected from the first bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Tar,
    TarGz,
    TarXz,
    TarZst,
}

impl Format {
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
    File,
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
    pub kind: Kind,
    /// Permission bits (`& 0o7777`).
    pub mode: u32,
    pub size: u64,
}

/// Output of [`Extracted`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
    pub src: PathBuf,
    pub dest: PathBuf,
    /// `None` when the `creates` marker made the step `ok` without opening
    /// the archive.
    pub format: Option<Format>,
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    /// Bytes of regular-file content, as the archive's headers declare it.
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
/// - is a device, fifo or other special file.
///
/// It also refuses when `dest` is missing (vision 6.7: create it with
/// [`crate::file::Directory`]) or not a directory, when a member's path is
/// already a directory where a file goes (or a file or symlink where a
/// directory goes), and when any directory on a member's path exists as a
/// symlink on disk. The report is fully predicted (counts are known from
/// the walk), so chained steps keep working in check mode (vision 12).
///
/// **What `apply` does.** Walks the archive again and writes each member
/// through `sys`: files with `write_atomic` and the archive's permission
/// bits, directories with `mkdir_all`, symlinks replaced if present, hard
/// links as copies of the already-extracted file. Ownership from the
/// archive is ignored; `.owner(uid, gid)` sets one owner on every file and
/// directory (not on symlinks). Modification times are not restored.
///
/// **Limits.** The compressed archive is held in memory (zstd: the
/// decompressed stream too); this is for release tarballs, not backups.
/// Files already in `dest` that the archive does not mention are left
/// alone.
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

    fn open(&self, sys: &System) -> Result<(Format, tar::Archive<Box<dyn Read>>)> {
        let bytes = sys
            .read(&self.src)
            .with_context(|| format!("reading archive {}", self.src.display()))?;
        let format = detect_format(&bytes)
            .map_err(|e| Error::msg(format!("{}: {e}", self.src.display())))?;
        let mut reader = decompress(format, bytes)
            .with_context(|| format!("{}: decompressing", self.src.display()))?;
        if format != Format::Tar {
            // Look at the first block before handing over to `tar`, so a
            // gzipped text file gets a plain answer.
            let mut head = vec![0u8; 512];
            let mut n = 0;
            while n < 512 {
                match reader.read(&mut head[n..]) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) => bail!("{}: decompressing: {e}", self.src.display()),
                }
            }
            head.truncate(n);
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

    /// Everything the archive would create, in order, or why it is refused.
    fn plan(&self, sys: &System) -> Result<(Format, Vec<Member>, usize)> {
        let (format, mut archive) = self.open(sys)?;
        let mut members = vec![];
        let mut skipped = 0;
        walk(&mut archive, self.strip, &mut |m, _| {
            match m {
                Some(m) => members.push(m.clone()),
                None => skipped += 1,
            }
            Ok(())
        })
        .with_context(|| format!("{}: refusing to extract", self.src.display()))?;
        self.check_destination(sys, &members)?;
        Ok((format, members, skipped))
    }

    /// The on-disk side of the guard: `dest` is a directory, nothing on a
    /// member's path is a symlink, and no member lands on the wrong kind.
    fn check_destination(&self, sys: &System, members: &[Member]) -> Result<()> {
        match sys.stat_follow(&self.dest)? {
            Some(s) if s.kind == FileKind::Dir => {}
            Some(s) => bail!("{} is not a directory ({:?})", self.dest.display(), s.kind),
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

    fn report(&self, format: Format, members: &[Member], skipped: usize) -> ExtractReport {
        let mut r = ExtractReport {
            src: self.src.clone(),
            dest: self.dest.clone(),
            format: Some(format),
            files: 0,
            dirs: 0,
            symlinks: 0,
            bytes: 0,
            skipped,
            extracted: true,
        };
        for m in members {
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

    fn write_member(&self, sys: &System, m: &Member, data: &mut dyn Read) -> Result<()> {
        let full = self.dest.join(&m.path);
        // `chown(2)` clears setuid/setgid on non-directories, so every arm
        // below defers `set_mode` to the tail and the owner is applied
        // first. Writing the mode inline would drop the setuid bit of a
        // member extracted with `.owner(..)`.
        let mut mode = Some(m.mode);
        let mut chown = true;
        match &m.kind {
            Kind::Dir => {
                sys.mkdir_all(&full)?;
            }
            Kind::File => {
                if let Some(parent) = full.parent() {
                    sys.mkdir_all(parent)?;
                }
                // The capacity is a hint from the archive's own header and
                // nothing bounds it: a base-256 `size` of 2^62 would ask
                // for an allocation the process cannot survive (Rust
                // aborts on allocation failure), turning a malicious
                // tarball into a killed run instead of a refusal. Reserve
                // a small floor and let `read_to_end` grow it against the
                // real stream.
                let hint = m.size.min(READ_CAPACITY_CEILING) as usize;
                let mut bytes = Vec::with_capacity(hint);
                data.read_to_end(&mut bytes)
                    .with_context(|| format!("reading member {}", m.path.display()))?;
                sys.write_atomic(&full, &bytes)?;
            }
            Kind::Hardlink(target) => {
                if let Some(parent) = full.parent() {
                    sys.mkdir_all(parent)?;
                }
                let bytes = sys.read(self.dest.join(target))?;
                sys.write_atomic(&full, &bytes)?;
            }
            Kind::Symlink(target) => {
                if let Some(parent) = full.parent() {
                    sys.mkdir_all(parent)?;
                }
                if sys.stat(&full)?.is_some() {
                    sys.remove(&full)?;
                }
                sys.symlink(target, &full)?;
                // `set_mode` and `set_owner` follow links; neither is
                // applied to a symlink member.
                chown = false;
                mode = None;
            }
        }
        if chown && let Some(o) = self.owner {
            sys.set_owner(&full, o.uid, o.gid)?;
        }
        if let Some(mode) = mode {
            sys.set_mode(&full, mode)?;
        }
        Ok(())
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

/// A reader of the tar stream inside `bytes`.
fn decompress(format: Format, bytes: Vec<u8>) -> Result<Box<dyn Read>> {
    Ok(match format {
        Format::Tar => Box::new(Cursor::new(bytes)),
        Format::TarGz => Box::new(flate2::read::MultiGzDecoder::new(Cursor::new(bytes))),
        Format::TarXz => Box::new(lzma_rust2::XzReader::new(Cursor::new(bytes), true)),
        Format::TarZst => Box::new(Cursor::new(zstd_decompress_all(&bytes)?)),
    })
}

/// Every frame of a zstd file, skippable frames skipped, content checksums
/// verified. `ruzstd`'s streaming reader stops at the first frame, and
/// `pzstd` and `zstd --rsyncable` write several.
fn zstd_decompress_all(mut input: &[u8]) -> Result<Vec<u8>> {
    use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
    use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

    let mut out = Vec::new();
    let mut dec = FrameDecoder::new();
    while !input.is_empty() {
        match dec.init(&mut input) {
            Ok(()) => {}
            Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame {
                length,
                ..
            })) => {
                input = input
                    .get(length as usize..)
                    .ok_or_else(|| Error::msg("zstd: truncated skippable frame"))?;
                continue;
            }
            Err(e) => bail!("zstd: {e}"),
        }
        while !dec.is_finished() {
            dec.decode_blocks(&mut input, BlockDecodingStrategy::UptoBytes(1 << 20))
                .map_err(|e| Error::msg(format!("zstd: {e}")))?;
            if let Some(chunk) = dec.collect() {
                out.extend_from_slice(&chunk);
            }
        }
        if let Some(chunk) = dec.collect() {
            out.extend_from_slice(&chunk);
        }
        if let (Some(want), Some(got)) =
            (dec.get_checksum_from_data(), dec.get_calculated_checksum())
            && want != got
        {
            bail!("zstd: frame checksum mismatch (got {got:08x}, want {want:08x})");
        }
    }
    Ok(out)
}

/// What [`walk`] calls per member: the member (`None` when skipped by
/// `strip_components` or for an empty path) and a reader of its data.
type Visit<'a> = dyn FnMut(Option<&Member>, &mut dyn Read) -> Result<()> + 'a;

/// Walk every member in order, validating paths and link targets, and hand
/// each to `visit` with its data. Errors name the member.
/// What a member is, for a collision message.
fn kind_label(kind: &Kind) -> &'static str {
    match kind {
        Kind::Dir => "directory",
        Kind::File => "file",
        Kind::Symlink(_) => "symlink",
        Kind::Hardlink(_) => "hard link",
    }
}

fn walk<R: Read>(archive: &mut tar::Archive<R>, strip: usize, visit: &mut Visit<'_>) -> Result<()> {
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
        let normalised = validate_entry_path(&raw).map_err(Error::msg)?;
        let Some(path) = strip_components(&normalised, strip) else {
            visit(None, &mut std::io::empty())?;
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
        let size = header.size().unwrap_or(0);
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
        visit(Some(&member), &mut entry)?;
    }
    Ok(())
}

impl Op for Extracted {
    type Output = ExtractReport;

    fn check(&self, sys: &System) -> Result<Plan<ExtractReport>> {
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
        let (format, members, skipped) = self.plan(sys)?;
        let report = self.report(format, &members, skipped);
        let diff = Diff::summary(format!(
            "extract {} ({}: {} files, {} dirs, {} symlinks, {} bytes) into {}",
            self.src.display(),
            format.name(),
            report.files,
            report.dirs,
            report.symlinks,
            report.bytes,
            self.dest.display()
        ));
        Ok(Plan::change_predicting(diff, report))
    }

    fn apply(&self, sys: &System, change: Change<ExtractReport>) -> Result<ExtractReport> {
        let Some(report) = change.predicted else {
            bail!("archive::Extracted::apply received a change without its prediction");
        };
        let (_, mut archive) = self.open(sys)?;
        walk(&mut archive, self.strip, &mut |m, data| match m {
            Some(m) => self.write_member(sys, m, data),
            None => Ok(()),
        })
        .with_context(|| format!("extracting {}", self.src.display()))?;
        if let Some(marker) = self.marker()
            && !sys.exists(&marker)?
        {
            sys.warn(format!(
                "archive {} extracted but its creates marker {} does not exist; the next run will extract again",
                self.src.display(),
                marker.display()
            ));
        }
        Ok(report)
    }

    fn always_changes(&self) -> bool {
        self.creates.is_none()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rustible_sdk::backend::Fake;
    use rustible_sdk::event::Collect;

    use super::*;
    use crate::file::testing::{expect_change, fake_sys};

    // `fixtures/archive/hello.tar*`: one tree, four encodings, made with
    // GNU tar 1.35, gzip -9 -n, xz -9 and zstd -19:
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

    fn all() -> [(&'static str, Format, &'static [u8]); 4] {
        [
            ("/tmp/hello.tar", Format::Tar, TAR),
            ("/tmp/hello.tar.gz", Format::TarGz, TGZ),
            ("/tmp/hello.tar.xz", Format::TarXz, TXZ),
            ("/tmp/hello.tar.zst", Format::TarZst, TZST),
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
                c.diff.short(),
                format!(
                    "extract {src} ({}: 2 files, 3 dirs, 1 symlinks, 38 bytes) into /opt",
                    format.name()
                )
            );
            assert_eq!(c.predicted, Some(hello_report(src, format)));
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
        let p = c.predicted.as_ref().unwrap();
        assert_eq!(
            (p.files, p.dirs, p.symlinks, p.skipped),
            (2, 2, 1, 1),
            "`hello/` itself is skipped"
        );
        op.apply(&sys, c).unwrap();
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
        let c = expect_change(&op.clone().strip_components(9), &sys);
        let p = c.predicted.as_ref().unwrap();
        assert_eq!((p.files, p.dirs, p.skipped), (0, 0, 6));
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
        assert_eq!(
            (
                c.predicted.as_ref().unwrap().files,
                c.predicted.as_ref().unwrap().bytes
            ),
            (2, 5)
        );
        op.apply(&sys, c).unwrap();
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
        assert_eq!(c.predicted.as_ref().unwrap().files, 2);
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

    #[test]
    fn an_absurd_header_size_does_not_abort_apply_either() {
        // `apply` walks the archive again, and `write_member` reserves from
        // the header before reading. An archive swapped between check and
        // apply reaches that reservation with an unvalidated size, so the
        // capacity is a clamped hint, not the claim. Without the clamp
        // this test does not fail, it aborts the whole test binary.
        let (_, sys) = sys_with(&raw_tar(&[("f", b'0', b"data", "")]));
        let op = Extracted::from_path("/tmp/a.tar").to("/opt");
        let c = expect_change(&op, &sys);
        sys.write_atomic("/tmp/a.tar", &tar_claiming_size("big", 1u64 << 62))
            .unwrap();
        let err = op.apply(&sys, c).unwrap_err().chain();
        assert!(err.contains("EOF") || err.contains("reading"), "{err}");
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
    fn check_mode_predicts_and_writes_nothing() {
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
        assert!(r.changed && r.predicted);
        assert_eq!((r.files, r.dirs, r.symlinks, r.bytes), (2, 3, 1, 38));
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
}
