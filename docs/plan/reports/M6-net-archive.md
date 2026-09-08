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
`.owner(uid, gid)`, `.force`, `.backup`, `.timeout`, `.header(name, value)`.
Output is `DownloadReport { url, path, downloaded, bytes, sha256, backup_path }`.

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

`main` was merged twice while this branch was being finished: first for M7's
CI workflows, then again for M3 (the real playbook run) and the README, which
also removed `crates/spike-playbook`. The Cargo.lock and DECISIONS.md
conflicts of the second merge were resolved keeping both sides; `cargo
metadata --locked` then accepted the lock unchanged.

The table below is the run on the final tree at merge commit `c37d128`. Full
output is in `docs/plan/logs/M6-net-archive-done.txt`, which has three
UTC-stamped sections: the pre-merge run at `01623e5`, the first re-run at
`85d6a62`, and this one.

| command | result | wall |
|---|---|---|
| `cargo fmt --all --check` | pass | 0.18s |
| `cargo clippy --workspace --all-targets -- -D warnings` | pass | 3.06s |
| `cargo test --workspace` | pass | 13.94s |
| `RUSTDOCFLAGS=-D warnings cargo doc --workspace --no-deps --lib` | pass | 1.79s |
| `cargo build --manifest-path examples/workspace/Cargo.toml` | pass | 2.79s |
| `cargo +1.88 check --workspace --all-targets` (MSRV) | pass | 2.17s |
| `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --tests` (all harness tests, as CI runs them) | pass | 41.77s |

Times are with a warm `target/`. At `85d6a62`, before the second merge, the
two new container tests were also run on their own: `it_http_download` in
5.84s and `it_archive_extracted` in 2.97s, and the `#[ignore]`d TLS test
`https_download_from_github_with_rustcrypto_tls` passed in 0.46s. All are in
the log.

Counts: `rustible-std` has 298 unit tests passing, one
ignored (the network TLS test, run separately above); 16 of them are
`http::`, 15 are `archive::`. Both new ops have a compiled doctest.

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

Eighteen `[M6-na]` entries are in `docs/plan/DECISIONS.md`, each with a
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
  Reverse is a one-line change in `http::tls_provider()`.
  **This is the decision I would most like reviewed.**
- **Both ops hold their payload in memory.** `Backend::write` takes `&[u8]`
  and there is no streaming write primitive, so `Download` buffers the body
  before `write_atomic` and `Extracted` buffers the archive (and, for zstd,
  the decompressed stream). Documented as "release tarballs, not disk images".
  Reverse: add `Backend::write_atomic_from(&mut dyn Read)`, an SDK change the
  brief ruled out.
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
