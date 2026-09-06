# Spike 1: cross-compile to aarch64 musl and run on the ARM VM

**Date:** 2026-09-06. **Status:** done. **Verdict:** the compile story holds
with the stock rustup toolchain. No zig, no distro cross-compiler, no Docker.

## Setup

- Host: x86_64 Ubuntu 24.04, rustc 1.97.1.
- Target: `cadu-cogram-vm-arm` (Ubuntu 24.04, aarch64, glibc 2.39), reached
  over Tailscale with passwordless SSH as `cadu`.
- The spike playbook from spike 3 (`crates/spike-playbook`), unchanged.

## What worked, in order

1. `rustup target add aarch64-unknown-linux-musl` (downloads `rust-std` only).
2. `cargo build --target aarch64-unknown-linux-musl` with the default linker
   **fails at the link step**: rustc compiles every object for aarch64, then
   invokes the host `cc`, which calls the x86 `ld`, which rejects the aarch64
   crt objects ("Relocations in generic ELF (EM: 183)").
3. Setting the linker to the bundled `rust-lld` with self-contained linking
   **links in 3.5 s** and produces a static aarch64 ELF. This is now persisted
   in `.cargo/config.toml` for both musl targets:

   ```toml
   [target.aarch64-unknown-linux-musl]
   linker = "rust-lld"
   rustflags = ["-C", "link-self-contained=yes"]
   ```

   Scoped per target, so host builds and build scripts are unaffected.
   `cargo-zigbuild` was never needed and remains the escape hatch for C deps.
4. `scp` + run on the ARM VM: facts report `Aarch64`, Ubuntu 24.04, 12 cpus.
   First run `changed=6`, second run `ok=5 skipped=1`, check mode
   `would_change=6` with the file untouched. Identical behaviour to x86.

## Binary size and the `dist` profile

A `[profile.dist]` was added to the workspace `Cargo.toml` (inherits release,
`strip`, fat LTO, one codegen unit, `opt-level = "z"`). Sizes for the spike
playbook, which links regex, serde_json, similar, tempfile, rustix:

| Target | Profile | Size | Build time (warm deps) |
|---|---|---|---|
| aarch64 musl | release | 3.1 MB | 3.5 s |
| aarch64 musl | dist | 1.4 MB | 9.5 s |
| x86_64 musl | dist | 1.7 MB | 8.8 s |

Both dist binaries ran correctly on their architecture. `rustible run` should
use `dist` for shipped binaries; the extra seconds of LTO are paid once per
playbook change, and per-triple dependency caches make rebuilds incremental.

## Upload cost

| Transfer | Time |
|---|---|
| First `scp` of the session (cold Tailscale path) | 19.9 s |
| `scp` 1.4 MB dist, warm | 0.4 s |
| `scp` 3.1 MB release, warm | 0.45 s |
| `ssh host true` | 0.24 s |

The cold first connection dominates, not bytes. Two consequences for the
orchestrator: open the SSH ControlMaster early (during the probe) so the
upload reuses it, and cache binaries on the target by content hash
(section 5.2) so repeat runs skip the upload entirely. SHA-256 verified equal
on both ends.

## Findings

1. **Pure-Rust policy is sufficient and cheap.** Every dependency in the spike
   is pure Rust; `rustix` and `tempfile` cross-link without a C toolchain. The
   "no C deps in op crates" rule from section 5.3 is what made this a
   three-second link instead of a toolchain hunt.
2. **`std::env::consts::ARCH` is the right arch fact.** It is baked in at
   compile time, and since the binary is compiled for the target, it is the
   target's arch by construction. No probe needed.
3. **`rust-lld` is shipped by rustup on x86_64 Linux hosts** and needs no
   install. Worth verifying on macOS hosts later (it is shipped there too, but
   untested here).
4. **Per-triple target directories** (`target/<triple>/`) mean the aarch64 and
   x86 builds do not invalidate each other. The `rustible` CLI can build all
   triples in parallel from one workspace.
5. Nothing in the spike playbook is arch-specific, and it was not touched
   between architectures. That is the whole point of the design.

## Not covered

The framed protocol over SSH (spike 2), stdin-driven `Start` frames, and the
binary cache on the target. The ARM VM was left clean (all uploaded binaries
and scratch dirs removed).
