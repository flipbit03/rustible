# M6 report: `file` ops (Copy, Symlink, Absent, Attrs, Block; Directory extended)

**Branch:** `m6-file-ops`. **Run:** unattended run 1, 2026-09-08. **Brief:** the
`file` row of `docs/plan/M6.md` plus the lead's op brief.
**Done-when log:** `docs/plan/logs/M6-file-ops-done.txt`.

## What was built, per op

All in `crates/rustible-std/src/file/`. The former `file.rs` (463 lines) was
split into a module directory; every public path (`file::Line`,
`file::Insert`, `file::plan_line`, `file::Directory`, `file::DirReport`,
`file::path_str`, ...) is unchanged through re-exports in `mod.rs`.

- **`mod.rs`**: module docs with the Ansible-to-Rustible table, `Insert`
  (now shared by `Line` and `Block`, with `Insert::position` holding the
  after/before/append/prepend rule once), `Owner { uid, gid }`, the pure
  attribute planner `plan_attrs(current: Option<&Stat>, mode, owner) ->
  Vec<AttrChange>` (mode compared `& 0o7777`, missing path rendered as
  `-`), its executor `apply_attrs`, and a `testing` helper module
  (`fake_sys`, `expect_change`) for the op tests.
- **`copy.rs`, `file::Copy`** (`ansible.builtin.copy` with `src`/`content`,
  `mode`, `owner`/`group`, `backup`). `Copy::from_bytes(..)`,
  `from_str(..)`, `from_local_path(..)` return a `CopyBuilder`; `.to(dest)`
  finishes it; `.mode(u32)`, `.owner(uid, gid)`, `.backup(bool)` are on the
  finished op, as in the vision 5.6 example. `from_local_path` reads through
  `sys.read` at check time, on the target (vision 5.1), and the rustdoc says
  so. `check` compares bytes and attributes; a content change is
  `Diff::text` when old and new are both UTF-8 and at most 64 KiB
  (`TEXT_DIFF_LIMIT`), else `Diff::summary("<path>: <n> bytes -> <m>
  bytes")`; an attributes-only change is `Diff::Attrs` and `apply` then
  only chmods/chowns without rewriting or backing up. Fails if the
  destination exists and is not a regular file (directory, symlink). Output
  `CopyReport { path, backup_path, bytes }`, predicted with
  `backup_path: None` (the name has a timestamp only `apply` knows).
- **`symlink.rs`, `file::Symlink`** (`file` with `state=link`).
  `Symlink::at(link).pointing_to(target)` finishes; `.force(bool)` on the
  op. `check` uses `sys.read_link`; a link with another target is replaced
  (diff `target: old -> new`); a regular file is refused unless `.force(true)`
  (diff adds `kind: file -> symlink`); a directory is always refused. The
  target need not exist. Output `SymlinkReport { link, target }`.
- **`absent.rs`, `file::Absent`** (`file` with `state=absent`).
  `Absent::at(path)`, `.recursive(bool)`. Removes a file, a symlink (not
  its target) or a directory; a directory with entries is refused without
  `.recursive(true)`, with the entry count in the message. Diff `exists:
  yes (<kind>) -> no` plus `entries: n -> 0` for a tree. Output
  `AbsentReport { path, removed }`, satisfied with `removed: false` when
  nothing is there.
- **`attrs.rs`, `file::Attrs`** (`file` with `state=file`). `Attrs::at(path)`,
  `.mode(u32)`, `.owner(uid, gid)`. Existing paths only: a missing path is
  an error pointing at `Copy`/`Directory` (vision 6.7); with nothing asked it
  is an existence assertion. Refuses symlinks (see Decisions). Diff is the
  `plan_attrs` list. Output `AttrsReport { path }`.
- **`directory.rs`, `file::Directory`** now takes `.owner(uid, gid)` and
  plans its attributes through `plan_attrs`; the `exists: no -> yes` change
  and the "exists and is not a directory" refusal are unchanged.
- **`block.rs`, `file::Block`** (`ansible.builtin.blockinfile`).
  `Block::in_path(path)`, `.marker("# {mark} MANAGED BY RUSTIBLE")` (the
  default; `{mark}` becomes `BEGIN`/`END`, a marker without `{mark}` is
  an error at check), `.insert(Insert)`, `.backup(bool)`, `.create(bool)`,
  then `.set(block)` finishes. An empty block removes the managed block.
  Pure `plan_block(text, begin, end, block, insert) -> Option<(String,
  usize)>` mirrors `plan_line`: first BEGIN, first END after it; replace in
  place, insert per `Insert`, or drain; output always newline-terminated
  unless empty. Output `BlockReport { path, line_no, backup_path }` with
  `line_no` the 1-based BEGIN line, 0 when there is no block.
- **`line.rs`, `file::Line`**: moved unchanged apart from using
  `Insert::position`.

Every op predicts (`Plan::change_predicting`) and `apply` reuses
`change.predicted`. Each rustdoc names the Ansible module and state.

## SDK change: symlink primitives (outside the op brief, requested by the lead)

`crates/rustible-sdk`: `Backend` gains `symlink(target, link)`,
`read_link(p)` and `read_dir(p) -> Vec<PathBuf>` (the third is a deviation,
see below). `Local` implements them with `std::os::unix::fs::symlink`,
`std::fs::read_link` and a sorted `std::fs::read_dir`. `Fake` implements
them (a symlink is a `FakeFile` of kind `Symlink` whose bytes are the
target; `symlink` over an existing path fails with `AlreadyExists` like
`symlink(2)`; `read_dir` lists direct children of a `Dir` entry) and gains
`Fake::with_symlink(p, target)`. `System` exposes `read_link` and
`read_dir` as plain reads and `symlink` as a guarded, logged mutation
(`MutationDuringCheck` inside `check`, tested in `system.rs`). `Local` has a
tempdir test for the three. **Any other `Backend` impl (the `Elevated`
backend on `m5-elevated-streaming`) must add these three methods when it
merges**; main's authorized_keys merge added `stat_follow` the same way and
merged cleanly into this branch.

## Deviations from the brief

- `read_dir` added as a third backend primitive (`Absent` needs it).
- No container tests: the Docker harness (PR #5) was not on main when this
  finished. `file/mod.rs` carries a `TODO(m6-harness)` naming the two tests
  to add (`it_file_copy.rs`, `it_file_block.rs`, `debian:12` and
  `ubuntu:24.04`, changed-then-ok) in the shape of the harness branch's
  `it_file_line.rs`. Nothing fake was written.
- `Copy` builder order: setters after `.to(dest)`, following the vision
  example rather than the brief's wording (see Decisions).
- `Copy::from_str` carries `#[allow(clippy::should_implement_trait)]` with a
  reason: the brief names the method and it is not a parser.

## Decisions

Recorded in `docs/plan/DECISIONS.md` under `[M6] 2026-09-08`:

1. `read_dir` as a backend primitive rather than `ls` through `cmd`.
2. `Copy` setters on the finished op (vision 5.6 example).
3. A `Copy` changing content and attributes shows the content diff only;
   `apply` still sets the attributes. Attributes-only changes do not rewrite.
4. `Attrs` refuses symlinks (lstat vs chmod-follows would never converge);
   `Symlink` never replaces a directory, `.force` covers a file only.
5. Empty `Block` removes the managed block instead of a `BlockAbsent` type;
   first BEGIN, first END after it; orphan BEGIN counts as absent.
6. Observation, not changed: `Local::write` creates new files at 0600
   (tempfile's default), so `Copy`/`Line::create` without `.mode()` differ
   from Ansible's umask behaviour on a real box. Worth a two-line SDK fix.

## How it was verified

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`: clean, output in the done log.
- `cargo test -p rustible-std file`: 63 tests. New in this branch: 45 in
  `rustible-std` (5 pure `plan_attrs`/`Insert` tests, 7 pure `plan_block`
  tests, and per op the Fake cases: satisfied, change with the predicted
  report, apply result, failure path, check-mode step that predicts and
  mutates nothing; `Copy` also binary summary diff, large-text fallback,
  mode-preserving rewrite, attrs-only change, backup, `from_local_path`
  through `sys`, directory/symlink in the way) plus 4 in `rustible-sdk`
  (Fake symlink/read_link/read_dir, `System::symlink` guard, Local on a
  tempdir). The 14 pre-existing tests are unchanged and green.
- Merged `origin/main` (authorized_keys, `stat_follow`) into the branch and
  re-ran everything.
- Not verified: container runs (harness not merged), real `chown` (needs
  root; the Local backend's `set_owner` is untouched).

## Usage

```rust
use rustible_std::file::{self, Insert};

// copy (embedded), with attributes and a backup of the old version
let cfg = ctx.step("nginx.conf",
    file::Copy::from_bytes(include_bytes!("../files/nginx.conf"))
        .to("/etc/nginx/nginx.conf").mode(0o644).owner(0, 0).backup(true))?;
if cfg.changed { /* restart nginx */ }

// file state=link
ctx.step("enable site",
    file::Symlink::at("/etc/nginx/sites-enabled/app")
        .pointing_to("/etc/nginx/sites-available/app"))?;

// file state=absent
ctx.step("drop default site", file::Absent::at("/etc/nginx/sites-enabled/default"))?;
ctx.step("clear cache", file::Absent::at("/var/cache/app").recursive(true))?;

// file state=file / state=directory with owner
ctx.step("lock sshd_config", file::Attrs::at("/etc/ssh/sshd_config").mode(0o600))?;
ctx.step("~/.ssh", file::Directory::at(home.join(".ssh")).mode(0o700).owner(uid, gid))?;

// blockinfile
let hosts = ctx.step("lab hosts",
    file::Block::in_path("/etc/hosts")
        .marker("# {mark} rustible: lab")
        .insert(Insert::After(regex::Regex::new("^127\\.0\\.0\\.1").unwrap()))
        .set("10.0.0.1 lab1\n10.0.0.2 lab2\n"))?;
ctx.log(format!("block at line {}", hosts.line_no));
```

Self-review: run by the lead on the PR.
