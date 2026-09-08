# M6: `http::Download` and `archive::Extracted`

**Branch:** `m6-net-archive`. **Governing sections:** vision 5.3 (pure Rust, no
C), 6.2, 6.3, 6.4, 6.7 (one op, one job), 6.9, 7 (System), 8 (testing), 12
(check mode). **Files:** `crates/rustible-std/src/http.rs` (new, 904 lines),
`crates/rustible-std/src/archive.rs` (new, 1221 lines),
`crates/rustible-std/src/lib.rs` (two `pub mod` lines),
`crates/rustible-std/src/file/mod.rs` (`apply_attrs` becomes `pub(crate)`),
`crates/rustible-std/tests/it_http_download.rs` (new),
`crates/rustible-std/tests/it_archive_extracted.rs` (new),
`crates/rustible-std/fixtures/archive/hello.tar{,.gz,.xz,.zst}` (new),
`Cargo.toml`, `crates/rustible-std/Cargo.toml`, `Cargo.lock`,
`docs/plan/DECISIONS.md`, `docs/plan/logs/M6-net-archive-done.txt`.
No SDK changes.

## What was built

### `http::Download` — Ansible `ansible.builtin.get_url`

`Download::get(url).to(dest)` with `.checksum("<alg>:<hex>")`, `.mode`,
`.owner(uid, gid)`, `.force`, `.backup`, `.timeout`, `.max_bytes`,
`.header(name, value)`. Output is `DownloadReport { url, path, downloaded,
bytes, sha256, backup_path }`, where `sha256` is `Option<String>` and is
`None` when no checksum was configured and the file was already in place.

The transport is `ureq` 3.4 over `rustls` 0.23 with the `rustls-rustcrypto`
crypto provider and Mozilla's bundled roots (`webpki-roots`). There is no
`validate_certs: no` escape hatch.

`check` never opens a socket. It decides from the file on disk alone:

| `dest` | `.checksum` | `.force` | result |
|---|---|---|---|
| missing | any | any | download |
| exists, digest matches | given | any | `ok` (a matching file is never re-fetched) |
| exists, digest differs | given | any | download |
| exists | none | `false` | `ok` |
| exists | none | `true` | download |

That is `get_url`'s own behaviour: without a checksum or `force`, an existing
file is left alone. A `.mode`/`.owner` mismatch is an attributes-only change,
fixed without a download and shown as an attribute diff.

Honesty details: the write goes through `sys.write_atomic`, so the old file
survives a failed fetch intact; a post-download checksum mismatch fails the
step and writes nothing, naming both digests; a non-2xx status fails naming
the status and the URL; redirects are followed. A due download is **not**
predicted, because size and digest are unknown before the fetch, so a
check-mode run that reads the step's output stops with the vision's message
(vision 12). An attributes-only change is predicted.

Refusals at `check`, before any network use: a `dest` whose parent directory
does not exist (names `file::Directory`, vision 6.7), a `dest` that exists and
is not a regular file, a `file://` URL (names `file::Copy::from_local_path`),
anything that is not `http://` or `https://`, and `md5`/`sha1` checksum
algorithms by name.

Pure functions with their own tests: `parse_checksum`, `digest`,
`validate_url`, `Algorithm::{hex_len, name}`.

### `archive::Extracted` — Ansible `ansible.builtin.unarchive` (`remote_src: yes`)

`Extracted::from_path(src).to(dest)` with `.creates(path)`,
`.strip_components(n)`, `.owner(uid, gid)`. Output is `ExtractReport { src, dest,
format, files, dirs, symlinks, bytes, skipped, extracted }`, where `format` is
`None` when the `creates` marker made the step `ok` without opening the
archive.

Formats: tar, tar.gz, tar.xz, tar.zst, decoded by `tar`, `flate2`
(miniz_oxide), `lzma-rust2` and `ruzstd` — pure Rust throughout. The format
comes from the file's magic bytes, never its name, so `release.tgz` and
`blob.bin` both work and a mislabelled file is refused instead of misread.
After decompression the first block is checked for the `ustar` magic, so a
gzipped text file is refused as "the tar.gz stream does not contain a tar
archive" rather than surfacing a `tar` crate error.

Idempotence is the `creates` marker, as in Ansible: when `.creates(path)`
exists the step is `ok` and the archive is not opened at all. Without it the op
extracts every run, every run is `changed`, and it flags itself with
`always_changes` so the report says so. A relative `creates` is taken under
`dest`.

`check` decompresses and walks every member without writing, so the report's
counts are exact and every refusal fires before a single byte is written. It
refuses the whole archive (never a partial extract) when any member has an
absolute path or a `..` component, is a symlink whose target is absolute or
climbs above `dest`, sits under an earlier symlink member, is a hard link to
something not extracted before it, or is a device, fifo or other special file.
Each refusal names the offending member. It also refuses a missing or
non-directory `dest`, a member whose path collides with a directory on disk
where a file goes (and the reverse), and any directory on a member's path that
exists as a symlink. The report is fully predicted, so chained steps keep
working in check mode.

`apply` walks the archive a second time and writes each member through `sys`:
files with `write_atomic` and the archive's permission bits, directories with
`mkdir_all`, symlinks replaced if already present, hard links as copies of the
already-extracted file.

Pure functions with their own tests: `detect_format`, `validate_entry_path`,
`strip_components`, `validate_link_target`, `Format::name`.

## Verification

`main` was merged three times while this branch was being finished, and every
gate was re-run after each merge:

1. M7's CI workflows.
2. M3 (the real playbook run) and the README, which also removed
   `crates/spike-playbook`. Conflicts in `Cargo.lock` and `DECISIONS.md`,
   both sides kept.
3. The `rustible-github` collection (PR 14). See "Converging the network
   dependencies" below.
4. PR 16, `shell::Command` extras and container tests for the merged file
   ops. Those tests exercise `file::apply_attrs`, which this branch
   reordered for the setuid fix, so they are a useful independent check;
   they pass. DECISIONS.md conflict, both sides kept.

The table is the last run, at merge commit `3e2278e`. Full output is in
`docs/plan/logs/M6-net-archive-done.txt`, which has six UTC-stamped sections:
the pre-merge run at `01623e5`, one after each of the four merges, and one
after the review fixes.

| command | result | wall |
|---|---|---|
| `cargo fmt --all --check` | pass | 0.18s |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass | 2.66s |
| `cargo test --workspace` | pass | 12.91s |
| `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --lib` | pass | 1.54s |
| `cargo build --manifest-path examples/workspace/Cargo.toml` | pass | 1.95s |
| `cargo +1.88 check --workspace --all-targets` (MSRV) | pass | 2.00s |
| `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --tests` (all 13 harness files, as CI runs them) | pass | 46.16s |

Run after the previous merge and unchanged by this one: `cargo test -p
rustible-github` (1.00s), which shares the merged `ureq` entry, and the
`#[ignore]`d TLS test `https_download_from_github_with_rustcrypto_tls`
(0.36s).

Times are with a warm `target/`, so they measure the gates and not the build;
the first run of the same set on a cold tree took roughly four times as long.
The two new container tests were also run on their own after each merge,
`it_http_download` in about 3s and `it_archive_extracted` in about 1s. All
are in the log.

CI on GitHub was green on all four jobs (format/clippy/test, MSRV 1.88,
example workspace, Docker harness) at `33cf1ff` and again at `f65aa54`.

Counts: `rustible-std` has 318 unit tests passing and one ignored (the
network TLS test, run separately). Of those, 20 are `http::` (19 running plus
the ignored TLS one) and 18 are `archive::`; this branch added seven of them
for the review fixes, and the jump from 305 to 318 is PR 16 arriving in the
last merge. Both new ops have a compiled doctest. The harness now runs 13
container test files and all pass.

### Converging the network dependencies

The `rustible-github` collection landed on `main` first and had already added
`ureq`, `rustls` and `rustls-rustcrypto` to `[workspace.dependencies]`, with
the same reasoning about `ring` being C. The merge left two identical blocks.
They were converged on one, keeping the github branch's entries, because both
sides agreed byte for byte: `ureq` with `default-features = false` plus
`rustls-no-provider` and `rustls-webpki-roots`, `rustls` with
`default-features = false` plus `std` and `tls12`, and
`rustls-rustcrypto = "0.0.2-alpha"`. The comment above them now names both
consumers. The archive-only crates (`sha2`, `tar`, `flate2`, `lzma-rust2`,
`ruzstd`) stay in their own block. `Cargo.lock` was taken from `main` and
re-resolved by cargo; the only packages it gains over `main` are the archive
decoders and their transitive dependencies, and `cargo metadata --locked`
accepts the result. `cargo test -p rustible-github` passes on the merged
tree, so the shared entry works for both crates.

Every job in `.github/workflows/ci.yml` was reproduced locally: the
fmt/clippy/test/rustdoc gate, the 1.88 MSRV check (the toolchain was installed
for this), the `examples/workspace` build, and the Docker harness job in its
CI form (`--tests`, not just the two new files).

### What the container tests showed

Both run on `debian:12` and `ubuntu:24.04` and assert changed-then-ok.

`it_http_download` serves the file from a `TcpListener` on loopback **inside**
the container, so it needs no outbound network. It covers: first download
`changed`; the same step again `ok`; a download without a checksum `ok`; a
`.force(true)` download `changed`; a 302 redirect followed, `changed`; a 404
failing with `returned 404 Not Found`; a wrong checksum failing with both
digests and "nothing written to …"; and a missing parent directory failing
with the `file::Directory` pointer. Roughly 1.4s on debian, 0.4s on ubuntu.

`it_archive_extracted` writes each of the four fixtures into the container and
extracts it: tar, tar.gz, tar.xz and tar.zst each `changed` then `ok` with a
`creates` marker, each reporting `2 files, 3 dirs, 1 symlinks, 38 bytes`. After
each one the test executes the extracted `hello/bin/run` in the container and
checks it exits 0, which proves the permission bits from the archive survived
the write. It also covers `.strip_components(1)` (`2 files, 2 dirs, 1
symlinks`), the no-`creates` case extracting twice and reporting `changed`
both times with the `action` marker, and a missing destination failing. About
0.4s per image.

### What could not be verified

**HTTPS inside the container.** The harness has no way to hand a CA
certificate to `Download`, and the test must not depend on the container
having outbound network, so the container leg of `http::Download` is plain
HTTP end to end. TLS through `rustls-rustcrypto` is covered instead by the
`#[ignore]`d unit test `https_download_from_github_with_rustcrypto_tls`, which
does a real TLS 1.3 fetch from github.com. It was run by hand on this VM and
passed, both before and after the merge, and both runs are in the log. It
stays `#[ignore]`d so CI never depends on the network. Closing this properly
needs a `.ca_cert` option on `Download` plus a self-signed certificate in the
harness, which is a public API change and out of scope here.

**aarch64.** The cross-link check that motivated the TLS provider choice
(`cargo build --target aarch64-unknown-linux-musl`) was run on this x86 VM
with no C toolchain present, which is exactly the condition vision 5.3 cares
about. Neither op has been exercised on the ARM VM at runtime.

**No archive over ~a few hundred MB** was tested; both ops hold their payload
in memory by design (see Decisions).

## Review fixes

The code review of PR 15 raised seven findings. All seven were accepted and
fixed; none was disputed. Each is listed with the test that would fail if the
fix were reverted.

**1. Untrusted allocation from a tar header.** `write_member` reserved
`Vec::with_capacity` from the header's `size`, which the archive controls and
nothing bounds. A base-256 `size` of 2^62 asks for a 4.6 EB allocation, and
Rust aborts the process on allocation failure, so a crafted tarball killed the
run instead of drawing the op's refusal. The capacity is now clamped to
`READ_CAPACITY_CEILING` (1 MiB) and `read_to_end` grows it against the real
stream, and `report()` sums the same untrusted sizes with `saturating_add`,
which would otherwise panic in a debug build. Worth noting for the reviewer:
`check` alone never reaches the allocation, because its walk fails on the
short stream first. `apply` does, because it re-reads the file from disk and
`write_member` runs before the walk hits the truncation, so an archive swapped
between check and apply gets there. Pinned by
`archive::tests::an_absurd_header_size_does_not_abort_apply_either`, which
does exactly that swap, and by
`an_absurd_header_size_is_refused_at_check_without_aborting`. Both tests abort
the whole test binary rather than failing if the clamp is removed.

**2. chmod before chown drops setuid and setgid.** Linux's `chown(2)` clears
`S_ISUID` and `S_ISGID` on anything that is not a directory, so
`Download::mode(0o4755).owner(..)` and a setuid member of an archive both came
out as plain 0755 while the step reported success. `file::apply_attrs` (shared
with `file::Copy`, so the merged file ops were affected too) and
`archive::Extracted::write_member` now chown first and chmod after; in the
archive path every arm defers its `set_mode` to the tail rather than writing
the mode inline. Pinned by two container cases, "download a setuid helper" and
"extract a setuid member with an owner", which assert mode 0o4755 survives on
both images. The `Fake` cannot catch this at all, since it does not model
chown's bit-clearing, so the container was the only place to prove it. Both
were checked by swapping the order back: each fails with mode 0o755, and each
passes with the fix.

**3. No ceiling on a download body.** ureq's 10 MB cap had been lifted with
`.limit(u64::MAX)` and nothing put in its place, while the body is buffered in
memory before the atomic write, so an endless or hostile response was an
out-of-memory kill on the target. New `Download::max_bytes(u64)` builder,
default `DEFAULT_MAX_BYTES` of 1 GiB, chosen because the op already buffers in
memory and is documented for release tarballs. A `Content-Length` over the
limit fails before the body is read; without one, ureq's `limit` is set one
byte past the ceiling so the read itself stops. Both failures name the limit
and the builder. Pinned by three unit tests
(`a_body_over_max_bytes_is_refused_by_content_length`,
`..._without_a_content_length`, `a_body_exactly_at_max_bytes_is_accepted`) and
one container case. The unit test server grew an `X-Omit-Length` route so the
no-`Content-Length` path is genuinely exercised.

**4. `validate_url` rejected a valid short URL.** The length floor was
`"https://".len()` for both schemes, so `http://x`, exactly eight characters,
was refused. Single-label hosts are real: an `/etc/hosts` name, a container
alias, a service on a LAN. It now measures against the scheme that matched.
Pinned by additions to `http::tests::url_validation`.

**5. Intra-archive collision between a directory and a later symlink.** For
`d/`, then `d/f`, then `d` as a symlink, `apply` created and populated the
directory and then could not remove it, aborting partway and leaving a
half-written tree. `check_destination` cannot see it because nothing is on
disk yet, and the existing guard only rejected members *under* an earlier
symlink. `walk` now carries what each path has been claimed as and refuses two
members colliding on one path with different kinds, plus a non-directory
member at a path earlier members populated, which covers directories the
archive never listed explicitly. Repeating a path as the same kind stays legal
and last-one-wins, as GNU tar does. Pinned by
`archive::tests::intra_archive_path_collisions_are_refused_at_check`, five
cases including the legal one.

**6. `ExtractReport::bytes` doc contradicted the code.** The doc claimed hard
links were counted again; their headers carry `size == 0`, so they raise
`files` and add nothing. I fixed the doc rather than the code. Resolving each
hard link's target size would mean carrying the sizes of earlier members
through the walk to report a number nothing depends on, and the field is more
useful as what the archive declares than as what lands on disk. The doc now
says so explicitly, and the existing test at `hard_links_are_copies_of_the_earlier_file`
keeps locking the behaviour in.

**7. Hashing on every check with no checksum asked for.** `content_state` read
and SHA-256'd the whole destination on every check, check mode included, only
to fill `DownloadReport::sha256`. That is a full read plus digest of a 500 MB
tarball per run for a value that decided nothing. The read and the digest now
happen only when a `.checksum` makes them decide something.
`DownloadReport::sha256` became `Option<String>`, `None` in exactly that case
and always `Some` after a download. This is a public type change, taken
deliberately at 0.0.1 and recorded in DECISIONS with its reverse. Pinned by
`http::tests::check_hashes_the_destination_only_when_a_checksum_asks_for_it`.

On the TLS provider, the reviewer's note stands and is worth repeating here:
`rustls-rustcrypto` is an alpha crate sitting on the security-critical path,
which is a real cost of the pure-Rust constraint rather than an oversight. The
question is recorded once for Cadu as the `[M6-gh]` proposed amendment in
`DECISIONS.md`, raised by the `rustible-github` collection and covering
`http::Download` too.

## Deviations from the brief

- The brief's `archive` row says "tar (+gz/xz/zst)". That is what shipped.
  **bzip2 and zip are refused** with a message naming the supported set.
  bzip2's only pure-Rust decoder path (`bzip2` with the `libbz2-rs-sys`
  backend) was not evaluated for cross-linking, and zip has different
  semantics from tar — many members carry no mode, symlinks are an extension —
  so it needs a design rather than a decoder swap. Both are additive later.
- `file::apply_attrs` went from private to `pub(crate)` so `http::Download`
  reuses it instead of duplicating `file::Copy`'s attribute logic. That is the
  only edit to a module outside the two new ones.
- No `rustible-github` work is in this branch; that is a separate op branch.

## Decisions

Twenty-six `[M6-na]` entries are in `docs/plan/DECISIONS.md`, each with a
"Reverse:" clause. The ones worth a look:

- **TLS provider.** `rustls-rustcrypto` 0.0.2-alpha, via `ureq`'s
  `rustls-no-provider` feature. rustls's default providers (`ring`,
  `aws-lc-rs`) bundle C: an `aarch64-unknown-linux-musl` build of a `ureq` +
  `ring` probe fails here with "failed to find tool aarch64-linux-musl-gcc",
  precisely the failure vision 5.3 forbids. The rustcrypto build links with
  `rust-lld` and completes a real TLS 1.3 fetch. The alternative,
  `rustls-graviola`, also links but requires AVX2/BMI2/ADX on x86_64 and
  AES/PMULL on aarch64 **at runtime**, which excludes pre-2014 x86 CPUs and
  the Raspberry Pi 4 — not an assumption a fleet tool can make. The `alpha`
  label is on the rustls glue; the primitives underneath are RustCrypto's
  `aes-gcm`, `chacha20poly1305`, `p256`, `x25519-dalek`, `rsa` and `sha2`.
  Reverse is a one-line change in `http::tls_provider()`. The same question is
  recorded once for Cadu as the `[M6-gh]` proposed amendment in
  `DECISIONS.md`, raised by the `rustible-github` collection; it covers
  `http::Download` too, and this branch does not restate it.
- **Both ops hold their payload in memory.** `Backend::write` takes `&[u8]`
  and there is no streaming write primitive, so `Download` buffers the body
  before `write_atomic` and `Extracted` buffers the archive (and, for zstd,
  the decompressed stream). Documented as "release tarballs, not disk images",
  and since the review a download is bounded by `.max_bytes` (1 GiB by
  default) rather than trusted to be reasonable. Reverse: add
  `Backend::write_atomic_from(&mut dyn Read)`, an SDK change the brief ruled
  out.
- **`check` walks the whole archive**, paying two decompressions on a changed
  run, so the counts are exact and the tar-slip refusals all fire before
  anything is written.
- **No transparent gzip.** `ureq` is pulled with `default-features = false`,
  so no `Accept-Encoding` is sent and a `.tar.gz` arrives as stored rather
  than being inflated into `dest`.
- **Checksums** are `sha224`/`sha256`/`sha384`/`sha512`; `md5` and `sha1` are
  refused by name, and Ansible's `checksum: sha256:<url>` form is not
  supported. The report always carries the file's SHA-256.
- **Archive fixtures are checked in** (four files, under 11 KB together)
  rather than generated at test time, because generating xz and zstd needs
  encoders and because what the op must read is what real `tar`/`xz`/`zstd`
  wrote. The generating commands are recorded in the archive tests.
- **Hard links are written as copies** and modification times are not
  restored; `Backend` has no `hard_link` or `set_mtime` primitive.

## Self-review

Not run on this branch. The `code-review` skill pass is left to the lead on
the PR, as with the other M6 op branches.
