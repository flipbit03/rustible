# Spike 2: framed protocol over SSH, multi-arch, with a real apt op

**Date:** 2026-09-06. **Status:** done, in-tree. **Verdict:** the
orchestrator/binary split works as designed. Two hosts, two architectures, one
command, 345 ms warm.

## What was built

- `rustible-sdk::protocol`: `Down` (`Start`, `Cancel`) and `Up` (`Hello`,
  `Event`) frames, u32 big-endian length prefix plus JSON body, sync codec, and
  a `FrameSink` that writes `Up::Event` frames.
- `runtime::run` gained `--remote`: block on a `Start` frame from stdin, take
  host name, check mode, and verbosity from it, write `Hello` then frames to
  stdout. Nothing else may touch stdout in this mode.
- `rustible-std::apt::Present`: `dpkg-query -W -f='${Status}\t${Version}'` in
  `check`, `apt-get install -y --no-install-recommends` with
  `DEBIAN_FRONTEND=noninteractive` in `apply`, refuses on non-apt hosts via
  `facts.package_manager`. Predicts its report so check mode chains. Four
  fake-backend tests, including "deinstall ok config-files counts as missing",
  which the ARM VM then exercised for real.
- `crates/spike-playbook/src/bin/mc.rs`: refuses without apt or root, one
  step, logs the version.
- `crates/rustible`: the orchestrator. `--host local` or `--host user@addr`,
  repeatable; `--become`, `--check`, `-v`. Transports: local process, or SSH
  through the `openssh` crate (system `ssh`, ControlMaster).

```
cargo run -p rustible -- --bin mc --host local --host cadu@cadu-cogram-vm-arm --become [--check] [-v]
```

## The pipeline as run

1. Connect to all hosts in parallel; probe `uname -sm`; map to a musl triple.
2. One `cargo build --profile dist -p spike-playbook --bin mc --target A --target B`.
   Cargo accepts several `--target` flags in one invocation, which matters
   because it locks the target dir and parallel invocations would serialize.
3. SHA-256 each artifact. Remote path is `~/.cache/rustible/bin/<bin>-<hash>`.
4. Per host, in parallel: `test -x` the cached path; if missing, stream the
   bytes through `sh -c 'cat > tmp; chmod; mv'` stdin. Then exec
   `[sudo -n] <path> --remote`, write `Start`, read frames until EOF, render
   with the `Pretty` sink keyed by host, collect stderr, wait for exit.

## Measurements (x86 dev box to ARM VM over Tailscale, warm ControlMaster)

| Phase | local | ARM VM over SSH |
|---|---|---|
| connect | 0 | 150 ms |
| probe `uname -sm` | 1.3 ms | 90 ms |
| upload 570 KB (first run) | 8 ms | 174 ms |
| cache check (later runs) | 1 ms | 16 ms |
| exec to `Hello` | 5 ms | 17 to 21 ms |
| whole binary run, one no-op apt step | 15 ms | 30 ms |
| whole binary run, apt actually installing mc | | 4.55 s (apt itself) |
| **orchestrator total, two hosts, warm** | | **345 ms** |
| orchestrator total with a cold `dist` build | | 4.85 s (4.4 s is cargo) |

The `mc` playbook binary is 570 KB on aarch64 and 638 KB on x86 in the `dist`
profile: it has no regex or similar, so smaller than the sshd spike.

## What was verified

- Check mode against a host lacking `mc`: `would_change=1`, and dpkg still
  reports it absent afterwards.
- Real run: `changed=1` on the ARM VM with the attr diff rendered at `-v`,
  `ok=1` locally, exit 0.
- Second run: `ok=1` on both, binaries reported "already cached", no upload.
- `sudo -n` launch works without a tty on both hosts; facts inside the binary
  report `user=root`.
- `apt::Present` correctly treated a package in `deinstall ok config-files`
  state (what `apt-get remove` leaves behind) as missing.
- Stderr of the remote process is captured separately and shown only if
  non-empty. Panics would land there.

## Findings

1. **`become` is a reserved keyword in Rust** (reserved for guaranteed tail
   calls, `#![feature(explicit_tail_calls)]`). A struct field or variable
   cannot be named `become`. For the playbook attribute,
   `#[rustible::playbook(become = true)]` is still parseable by a proc macro
   (attribute args are tokens; `syn` can accept keywords with
   `Ident::parse_any`), but internal code must use another name. Options for
   the attribute: keep `become` for Ansible familiarity and handle it in the
   macro, or rename to `sudo`/`as_root`. **OPEN.**
2. **Cargo builds all triples in one invocation.** Use it; do not spawn one
   cargo per triple.
3. **The binary's own name is the hash-suffixed file name**, so `Hello`
   reports `mc-a55b6d...`. The playbook name should come from the macro (the
   source file) or the `Start` frame, not `argv[0]`.
4. **Check-mode logging in playbooks needs `predicted`.** The first version of
   the `mc` playbook logged "installed mc" with an empty version in check
   mode. Fixed by branching on `mc.predicted`. Worth a note in the SDK docs:
   in check mode, `changed` means "would change".
5. **SSH exec latency is ~20 ms and connect is ~150 ms** on this link. Per-host
   work is dominated by the actual ops (apt took 4.5 s). The design goal of
   "orchestrator overhead is negligible next to the work" holds.
6. **`$HOME` expansion is done by the remote shell** because the orchestrator
   does not know the remote home. The upload and exec both go through
   `sh -c`. Fine for a spike; the real CLI should resolve the home once during
   the probe and use absolute paths.
7. **JSON framing was never a problem to debug** and the frames are small. No
   reason to move to a binary codec yet; the codec is one function on each
   side if that changes.
8. **`Pretty` rendering interleaves lines across hosts** since both sinks
   write to the same stdout. Acceptable here; the real CLI wants a per-host
   buffer or a live multi-host view.
9. **The remote binary cache directory** `~/.cache/rustible/bin/` was left in
   place on both hosts (that is the design). `mc` was reinstalled on the ARM
   VM by the playbook itself.

## Not covered

`Cancel` handling (no stdin reader thread in the binary yet), file streaming
frames, secrets, vars deserialization (the `Start` frame carries `vars` as
JSON `null`), inventory parsing, the `Elevated` helper backend, and the
`#[playbook]` macro. All three planned spikes are now done; what remains is
building the real thing.
