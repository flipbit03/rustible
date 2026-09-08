# M7 item 2: rustdoc on every public item

**Branch:** `m7-rustdoc`
**Scope:** documentation only. No behaviour change, no signature change, no
rename, no new or removed public item.

## The gap

`cargo doc --no-deps` was already warning-free before this branch, so the
second half of the M7 item held. The first half did not: nothing enforced that
a public item was documented, so coverage was good where it had been written
carefully and unmeasured everywhere else.

Adding `#![warn(missing_docs)]` to the seven library crates and building
produced **564 undocumented public items**:

| Crate | Undocumented |
|---|---:|
| `rustible-sdk` | 326 |
| `rustible-std` | 145 |
| `rustible-cli` | 78 |
| `rustible-build` | 14 |
| `rustible` | 1 |
| `rustible-github` | 0 |
| `rustible-macros` | 0 |

`rustible-github` was already at zero because it is the only crate that
carried the lint: it has had `#![forbid(unsafe_code)]` and
`#![warn(missing_docs)]` since it was written. That is the whole explanation
for the spread. Where the lint was on, coverage was total; where it was off,
coverage tracked how carefully the file had been written.

By kind, the 564 were:

| Kind | Count |
|---|---:|
| struct field | 260 |
| enum variant | 133 |
| method | 84 |
| associated function | 35 |
| struct | 21 |
| enum | 17 |
| function | 6 |
| module | 3 |
| type alias | 2 |
| associated constant | 2 |
| trait | 1 |

Public struct fields and enum variants are 70% of the gap, which matches how
the codebase is shaped: the ops are builders with public report types, and a
report type is mostly fields.

After the pass the count is **0**, with the lint at `deny`.

## What was documented

The work was split across seven agents by file, all against one worktree,
each given the same written brief: the three files named as the reference
voice (`rustible-std/src/systemd.rs`, `rustible-std/src/tls.rs`,
`rustible-sdk/src/op.rs`), the rule that a comment restating the item's name
is worse than none, and the op-specific requirements (state ensured, Ansible
equivalent, what it refuses and why, whether it predicts in check mode).

**`rustible-sdk` (326).** `event.rs` was the single largest file at 59: each
variant now names the stage that emits it and what a reporter does with it,
written after reading both the producers (`Ctx::step`, `Ctx::skip`,
`System::warn`, `Cmd::run`, `runtime::execute`) and the consumers
(`rustible-cli/src/render.rs`, the `Compact` and `JsonLines` sinks). The
`backend/` tree (86) was the highest-value work: the module header for
`elevated.rs` now states the privilege model as the code actually implements
it, that the escalation password never enters an argv because `/proc` makes
argv world-readable and so goes to `sudo -S` on stdin, that no message in the
module quotes file contents because a write payload may be a secret, that the
check-mode guard holds on both sides but the helper's copy is a second latch
against a buggy op rather than a boundary against a hostile parent, and that
the helper narrows nothing since the far side is a full `Local` as the target
user. `system.rs`, `facts.rs` and `protocol.rs` (97) name, for every fact
field, the file or command it comes from and its value when that source is
missing.

**`rustible-std` (145).** The op crate. Every op type states its Ansible
equivalent, its refusals with the reason attached, and its check-mode
prediction behaviour. The two type docs that were extended beyond filling a
gap are the ones a user gets wrong: `file::Line`, where idempotence rests
entirely on `matching` and a first-match-only rule means a changed value
appends a second line rather than replacing the old one, and `file::Block`,
where the markers are the block's identity and matched by whole-line
equality, so re-indenting or changing `.marker(..)` orphans the old block.

**`rustible-cli` (78).** The inventory is the KDL file a user writes by hand,
so every field now names the KDL syntax that produces it, the built-in
default, and the placement rule. The module header of `resolve.rs` gained the
two facts a user gets wrong: `defaults` is a parameter level only, and
*nothing merges*, `ssh_args` included, with every beaten level kept in
`overridden_params` so `inventory show` can print what was shadowed.

**`rustible-build` (14) and `rustible` (1).** Small; the existing module
header of `rustible-build` was left untouched and the error docs written to
match it.

## Doctests

The workspace had thirteen examples fenced ```ignore. An ignored example is
never compiled, so it rots silently, and one already had: the
`rustible-github` module header showed
`file::Directory::at(..).owner(&account)`, a method that does not exist. The
real signature is `owner(uid: u32, gid: u32)`, and the fence was the only
reason nobody had noticed.

All thirteen were converted to ```no_run with a hidden `#` preamble, the
pattern already used at the top of `systemd.rs`. The visible body of each was
kept; only unresolvable references were substituted (`include_str!("schema.sql")`
became a literal SQL string). One new example was added, on `file::Line`,
showing the `^#?\s*PasswordAuthentication\b` regex shape that avoids the
duplicate-line trap.

The two in `rustible-macros` were expected to be the genuine exception, on
the theory that a proc-macro crate cannot depend on the facade whose macros
the examples invoke. That turned out to be false: the crate already carries
`rustible` as a path dev-dependency for its `trybuild` tests, rustdoc links
dev-dependencies into doctests, and cargo permits the dev-dependency cycle.
Both now compile. They each needed a trailing hidden `# fn main() {}`,
because the `playbook` and `integration_test` macros rename the user's
function and rustdoc's "does this snippet declare `fn main`?" check runs
against the pre-expansion source.

Workspace doctests went from 16 passing with 13 ignored to **29 passing with
none ignored**. Total workspace tests went from 540 to 554: thirteen
conversions plus one new example, and no test was changed.

## What is enforced, and how

`#![deny(missing_docs)]` in each of the seven library crates. Not a flag in
`.github/workflows/ci.yml`, for three reasons.

It fires at the contributor's own `cargo check`, so an undocumented item
never reaches a pull request rather than failing ten minutes later in CI. It
sits beside the `#![forbid(unsafe_code)]` that `rustible-github` already
carries, so each crate states its own contract instead of depending on a
workflow file staying correct through a future reorganization. And `deny` in
a published crate is normally a hazard, because a lint that widens in a
future rustc breaks downstream builds, but cargo passes `--cap-lints allow`
when building a registry dependency, so it can only bite someone building
from this checkout, which is exactly who it should bite.

Nothing under `.github/` changed. The CI doc job already runs
`cargo doc --workspace --no-deps --lib` with `RUSTDOCFLAGS: -D warnings`, and
rustdoc runs `missing_docs` too, so that job is a second and independent path
to the same failure. Two of the other three jobs (`clippy --all-targets -D
warnings`, and the MSRV `cargo check --workspace --all-targets` on 1.89) also
catch it now that the lint is `deny`.

`rustible-cli/src/main.rs` is a binary target and is not covered. That is
correct rather than an omission: nothing in a binary is public API, and
`missing_docs` does not meaningfully apply to it.

## Public items deliberately not documented individually

Three enums carry a targeted `#[allow(missing_docs)]` with a comment naming
the reason, rather than a doc line per variant:

- **`systemd::EnabledState`** and **`systemd::ActiveState`**. The variants
  transcribe the words `systemctl is-enabled` and `systemctl is-active`
  print. The meaning is in the type's own doc and in `as_str`, `is_enabled`,
  `is_running` and `cannot_be_disabled`, so a per-variant line could only
  restate the name.
- **`http::Algorithm`**. Same shape: the variants transcribe the words
  accepted before the colon in a `"<algorithm>:<hex>"` checksum, and the only
  per-variant facts, the word and the digest length, are already returned by
  `name` and `hex_len`.

In all three, the variants that carry a real fact keep their own doc:
`EnabledState::NotFound` on exit-code behaviour across systemd versions,
`ActiveState::Refreshing` on systemd 257+. Nothing was silenced wholesale,
and no crate-level or module-level `allow` was added anywhere.

Several enums were considered for this treatment and rejected, because their
variants encode a rule or behaviour of Rustible's own rather than an external
vocabulary: `facts::Os`, `facts::Distro`, `facts::Arch`, `facts::Pm`,
`facts::Init`, `archive::Format`, `archive::Kind`, `file::Insert`,
`backend::FileKind`, `event::Status`, `event::Level`,
`elevated::HelperOp` and `elevated::HelperResponse`. Those are documented per
variant, and the docs name the concrete rule: `Pm::Apt` is
`/usr/bin/apt-get` existing, `Init::OpenRc` is `/proc/1/comm` reading `init`
*and* `/sbin/openrc` existing.

## Links fixed

No pre-existing intra-doc link was broken. Four `redundant explicit link
target` warnings appeared from links written during this pass and were
removed. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib`
passes.

One doc-comment misplacement was fixed in `archive.rs`: the comment for `fn
walk` had drifted onto `fn kind_label`, which then carried two unrelated
paragraphs. Both are private, so no lint caught it.

## One dependency added

`rustible-github` gained `rustible = { path = "../rustible" }` under
`[dev-dependencies]`, with no version, so that its module-header example can
show the `#[rustible::playbook]` attribute the example is about. Path-only is
load-bearing: cargo strips a versionless dev-dependency when packaging, so
the release workflow's publish order is unaffected even though
`rustible-github` publishes before `rustible`. Verified rather than assumed,
by reading the manifest inside the `.crate` archive that
`cargo package -p rustible-github` produces: its `[dev-dependencies]` section
is empty. `rustible-macros` and `rustible-std` already carry the dependency
the same way.

## Found while reading, left alone

Nothing below was changed. Each is a behaviour question, not a documentation
one, and the docs describe what the code does today rather than what it looks
like it meant to do.

1. **`Summary::warnings` under-counts.** Only `Ctx::warn`
   (`crates/rustible-sdk/src/ctx.rs:284`) bumps the counter. `System::warn`
   (`crates/rustible-sdk/src/system.rs:453`) and the runtime's undeclared-var
   notice (`crates/rustible-sdk/src/runtime.rs:436`) emit
   `Log { level: Warn }` frames that print as `WARNING:` and never reach it.
   Two real callers are affected, `rustible-std/src/archive.rs:741` and
   `rustible-std/src/apt.rs:531`, so the summary's warning column can read 0
   with warnings visibly on screen. Found independently by two agents. This
   is the one I would fix first.

2. **`Down::Start.verbosity` is dead on the receiving end.**
   `runtime::remote` destructures `Start` with `..` and never binds it, and
   `execute` is never passed it, so the binary emits every event regardless
   of the operator's `-v` count and all filtering happens in the
   orchestrator's renderer. The field is carried and versioned but unused.

3. **`Event::Failed.step` is always `None`.** `runtime::execute` never fills
   it, so `rustible` recovers the step name by string-parsing the
   `` step `…`: `` prefix off the error chain
   (`rustible-cli/src/render.rs::split_step`). A step name containing
   `` `:  `` splits at the wrong place.

4. **`Status::Skipped` is unreachable.** Nothing emits a `StepFinished`
   carrying it, because `Ctx::skip` emits `Event::StepSkipped` instead. Both
   `render.rs:319` and the `Compact` sink keep a `Status::Skipped` arm that
   cannot fire.

5. **`Line::check` reports the wrong `line_no` on a satisfied step with
   duplicates.** The satisfied branch uses
   `text.lines().position(|l| l == self.line)`, the first line *equal* to the
   desired one rather than the line `matching` actually selected. Only the
   report is affected; the file content is correct.

6. **Four public items in `rustible-std::file` are unreachable.**
   `CopyBuilder`, `copy::TEXT_DIFF_LIMIT`, `copy::content_diff` and
   `block::DEFAULT_MARKER` are `pub` but omitted from the `pub use` in
   `file/mod.rs:40-41`. `Copy::from_bytes`, `from_str` and `from_local_path`
   all *return* `CopyBuilder`, so a caller cannot name the type they are
   handed. Fixing it is a public-API change, out of scope here, and it is why
   two references in those docs are plain backticks rather than links.

7. **`Elevated::call` detects an oversized response by
   `e.to_string().contains("exceeds limit")`**, string-matching an error
   message produced in `protocol.rs`. Reword that message and the friendly
   refusal silently degrades to the raw framing error. No test pins the
   coupling.

8. **`Fake::file` returns `Option<FakeFile>` where `FakeFile` is unnameable
   downstream**, the same shape as item 6.

9. **`resolve_checked` runs its sibling-conflict loop over all of
   `HostParams::NAMES` including `addr`**, but `addr` is rejected on a group
   at parse time, so an `addr` conflict can never be produced. Harmless dead
   check.

10. **`CmdFailed::status` is `-1` when a signal killed the process**, since
    `ExitStatus::code()` returns `None`. Indistinguishable from a command
    that genuinely exits `-1`. Plausibly intentional; documented as it
    behaves.

## Verification

Full output in `docs/plan/logs/M7-rustdoc-done.txt`. All commands exit 0.

| Command | Result |
|---|---|
| undocumented public items, before | 564 |
| undocumented public items, after | 0 |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | 554 passed, 0 failed |
| `cargo test --workspace --doc` | 29 passed, 0 ignored |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib` | clean |
| `cargo build --manifest-path examples/workspace/Cargo.toml` | clean |
