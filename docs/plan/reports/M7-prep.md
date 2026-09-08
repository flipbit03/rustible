# M7 prep: CI and release workflows

Lead-run, ahead of the milestone proper. M7 items 3 and 4 (the two workflows)
plus the fixes the workflows immediately exposed. Items 1 (README rewrite) and
2 (rustdoc pass) wait for M3, because the tutorial in the README is a sequence
of `rustible playbook run` invocations that only M3 makes real.

## What was built

- **`.github/workflows/ci.yml`**, four jobs on push to main and on every PR:
  - `check`: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
    -- -D warnings`, `cargo test --workspace`, and `cargo doc --workspace
    --no-deps --lib` with `RUSTDOCFLAGS: -D warnings`.
  - `msrv`: `cargo check --workspace --all-targets` on Rust 1.88, the
    `rust-version` in the manifest (let-chains are the floor).
  - `example-workspace`: `cargo build --manifest-path
    examples/workspace/Cargo.toml`. That workspace is excluded from the cargo
    workspace on purpose, so nothing else compiles it.
  - `harness`: `cargo test -p rustible-std --tests` with
    `RUSTIBLE_INTEGRATION=1`, which runs the container tests instead of
    skipping them (vision 8, tier 3).
- **`.github/workflows/release.yml`**, on a published release tagged `vX.Y.Z`:
  - patches the workspace version (both `[workspace.package]` and the six
    inter-crate entries under `[workspace.dependencies]`) and fails loudly if
    any `0.0.1` survives the patch;
  - publishes the seven crates in dependency order (`rustible-sdk`,
    `rustible-macros`, `rustible-build`, `rustible-std`, `rustible-github`,
    `rustible`, `rustible-cli`), retrying each up to eight times with 30 s
    between tries for crates.io index lag, and treating "already at this
    version" as success so a re-run is safe;
  - builds `rustible-cli` with `--profile dist` for
    `x86_64-unknown-linux-musl` (ubuntu-latest), `aarch64-unknown-linux-musl`
    (ubuntu-24.04-arm) and `aarch64-apple-darwin` (macos-latest), with no
    musl-tools and no cross-compiler: `rust-lld` plus rustup's self-contained
    musl crt, as `.cargo/config.toml` sets up;
  - uploads the three binaries to the release as `rustible_linux_x86_64`,
    `rustible_linux_aarch64`, `rustible_macos_aarch64`.

## Two real breakages the workflows exposed

- **`examples/workspace` did not compile on main.** PR #9 changed
  `apt::Present::update_cache` from `bool` to `Duration`, and
  `playbooks/cadu/mc.rs` still passed a bool. Nothing in `cargo test
  --workspace` compiles that directory, so it went unnoticed; the new
  `example-workspace` CI job is exactly the guard for it. The playbook now
  maps its `update_cache` var onto `Duration::ZERO` ("always refresh") and
  documents why.
- **`cargo doc` failed on main** with three errors: an unclosed HTML tag from
  `"installing nginx: <inner>"` in `rustible-sdk`'s error docs, a public doc
  link to the private `TEXT_DIFF_LIMIT` in `file::Copy`, and an ambiguous
  `[`Copy`]` link (struct versus the derive macro) in the `file` module table.
  All three fixed. The doc job also passes `--lib`, because documenting the
  `rustible` facade crate and rustible-cli's `rustible` binary in one run
  collides on `target/doc/rustible/index.html`.

## Verified

Log: `docs/plan/logs/M7-prep-done.txt`.

- All seven publishable crates package cleanly (`cargo package --no-verify
  --allow-dirty`), which is as close to a publish as the hard limits allow;
  `spike-playbook` carries `publish = false`.
- The version-patch sed was run against a copy of the real manifest with
  `VERSION=0.1.0`: all seven `0.0.1` occurrences move, and the guard grep
  finds nothing left.
- Both musl dist builds produce static binaries locally: 1.64 MB for x86_64
  (static-pie), 1.41 MB for aarch64.
- `cargo test --workspace` 366 passed; `RUSTIBLE_INTEGRATION=1 cargo test -p
  rustible-std --tests` 268 unit plus six container tests green.
- `cargo doc --workspace --no-deps --lib` with warnings denied is clean.
- The CI workflow's own first run is this PR, which is the only real proof
  that the job definitions work on GitHub's runners.

## Not done here

- The README rewrite and the full rustdoc pass over every public item (M7
  items 1 and 2): they wait on M3.
- Nothing was published, and nothing in these workflows runs outside a release
  Cadu cuts by hand (protocol hard limit).
- The macOS build is unverified locally (no macOS machine); it is the same
  recipe as flipbit03/terminal-use, which does ship that asset.
