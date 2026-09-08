# M6: `systemd` ops

**Branch:** `m6-systemd`. **Governing sections:** vision 6.2, 6.3, 6.4, 6.7,
6.8 (systemd translation), 6.9, 7, 8, 12, 13. **Files:**
`crates/rustible-std/src/systemd.rs` (new), `crates/rustible-std/src/lib.rs`
(one `pub mod` line), `docs/plan/DECISIONS.md`, `docs/plan/logs/M6-systemd-done.txt`.
No new dependencies, no SDK changes.

## What was built, per op

All six ops share one private `Unit { name, user }` with the plumbing:
`systemctl(sys)` (adds `--user`), `guard` (unit-name validation, init check,
root check), `probe` (`is-enabled` then `is-active`, both with
`allow_failure`), `probe_existing` (probe plus not-found refusal),
`journal_tail` (`journalctl [--user] -u <unit> --no-pager -n 20`, never
fails), `verify_running` / `verify_stopped` (re-probe after a command, fail
with the journal tail). Output of every op is `UnitState { unit, enabled,
active }`.

- **`Enabled::new(unit).now(bool).user(bool)`** (`ansible.builtin.systemd`
  `enabled: true`). Satisfied when `is-enabled` is in systemctl's exit-0 set
  (`enabled`, `enabled-runtime`, `static`, `alias`, `indirect`, `generated`,
  `transient`), and with `.now(true)` also `is-active` running. Refuses
  `masked` (names `systemctl unmask`) and unknown units. Diff
  `Attrs { subject: unit, changes: [enabled: <word> -> enabled, active: <word> -> active] }`,
  predicts `UnitState`. Apply: `systemctl enable [--now] <unit>`, then both
  probes; with `--now` fails with the journal tail if not running; fails if
  `is-enabled` still does not say enabled.
- **`Disabled::new(unit).now(bool).user(bool)`** (`enabled: false`). Satisfied
  on `disabled`, `masked`, `linked`. Refuses `static`, `generated`, `transient`
  (no `[Install]` section; `systemctl disable` would exit 0 and change
  nothing). Apply: `systemctl disable [--now] <unit>`, re-probe, fail if still
  enabled or (with `--now`) still running.
- **`Running::new(unit).user(bool)`** (`state: started`). Satisfied on
  `active`, `reloading`, `refreshing`, `activating`. Refuses `masked` and
  unknown units. Diff `active: <word> -> active`, predicts. Apply:
  `systemctl start`, re-probe, fail with the last 20 journal lines if the
  unit is not running.
- **`Stopped::new(unit).user(bool)`** (`state: stopped`). Satisfied on
  `inactive`, `failed`, `deactivating`, `maintenance`. Apply: `systemctl
  stop`, re-probe, fail with the journal tail if still running.
- **`Restart::new(unit).daemon_reload(bool).user(bool)`** (`state:
  restarted`). Action: `check` runs nothing and returns
  `Plan::change(Diff::summary("systemctl [daemon-reload && systemctl ]restart <unit>"))`,
  `always_changes() == true`, no prediction. Apply: optional `daemon-reload`,
  `restart`, then the same verification as `Running`.
- **`Reload::new(unit).daemon_reload(bool).or_restart(bool).user(bool)`**
  (`state: reloaded`). Same shape with `reload`, or `reload-or-restart` when
  `or_restart` is set.

Pure functions: `parse_is_enabled(stdout, exit) -> EnabledState` (every
documented word plus `NotFound` for `not-found` or empty-stdout-nonzero-exit,
`Other(String)` otherwise), `parse_is_active(stdout, exit) -> ActiveState`,
`EnabledState::{is_enabled, cannot_be_disabled, is_masked, as_str}`,
`ActiveState::{is_running, as_str}`, `validate_unit`.

Every op refuses `facts.init != Init::Systemd` naming the init found
(`OpenRC`, or the `/proc/1/comm` word), and refuses `!sys.is_root()` naming
the user and pointing at `.user(true)`; both refusals happen before any
command runs.

## Usage

```rust
use rustible_std::systemd;

let sshd = ctx.step("sshd enabled", systemd::Enabled::new("ssh").now(true))?;
if cfg.changed {
    ctx.step("sshd restarted", systemd::Restart::new("ssh").daemon_reload(true))?;
}
ctx.step("nginx reloaded", systemd::Reload::new("nginx").or_restart(true))?;
ctx.step("apache gone", systemd::Disabled::new("apache2").now(true))?;
// The caller's own manager, no root needed:
ctx.step("syncthing up", systemd::Enabled::new("syncthing").now(true).user(true))?;
```

## Decisions

Recorded in `docs/plan/DECISIONS.md` under `[M6-sd]`:

1. State ops run both probes in `check` and refuse a unit `is-enabled` does
   not know: `is-active` says `inactive` for a nonexistent unit, so `Stopped`
   on a typo would otherwise be silently satisfied.
2. `Disabled` refuses `generated` and `transient` next to the brief's
   `static`, same reason (no `[Install]` section, `disable` is a no-op that
   would report `changed` forever).
3. `Enabled` and `Running` refuse `masked` with a message naming `systemctl
   unmask` instead of unmasking (vision 6.7).
4. `is_enabled` follows systemctl's own exit-0 table; `linked` counts as not
   enabled.
5. After `enable` / `disable` the op re-reads `is-enabled` and fails if the
   word did not flip, so no step reports `changed` for a no-op.
6. `Fake` gained no sequenced responses; apply tests use two fakes (one for
   the world before, one for after) because the post-apply probes share their
   argv with the check probes. Restart / Reload, whose `check` runs nothing,
   go through `Ctx` end to end with a single fake.

## Deviations from the brief

- **Container test not added.** The harness (`m6-harness`, PR #5) is still
  open, so `#[rustible::integration_test(systemd_images = [..])]` is not on
  `main`. A `TODO(M6 harness)` comment at the bottom of `systemd.rs` spells
  out the test to write (`/etc/systemd/system/rustible-test.service` with
  `sleep infinity`, `Enabled.now` changed-then-ok, `Restart`, `Stopped`,
  `Disabled`). In its place the ops were run by hand inside both systemd
  images, see Verification.
- `Disabled` refuses more than `static` (decision 2).
- The action summary includes `daemon-reload` when set
  (`systemctl daemon-reload && systemctl restart <unit>`) and `--user` when
  set, so the check-mode line says exactly what apply would run.

## Verification

- `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace`: clean (one doctest fixed on the way:
  a doctest fn cannot be called `main`).
- `cargo test -p rustible-std systemd`: 40 tests, saved in
  `docs/plan/logs/M6-systemd-done.txt`. Seven pure-parsing tests (every
  documented `is-enabled` / `is-active` word, `disabled` with exit 1,
  `inactive`/`failed` with exit 3, `inactive` with exit 4, empty stdout with
  and without an exit code, unknown words), the rest `Fake`-backend: per op
  satisfied / change with exact argv via `fake.argvs()` / apply with the
  follow-up probes / failure with journal lines in the message; refusal on
  masked, static, missing units; `CmdFailed` surfacing; non-systemd init and
  not-root refusals for all six ops with no command run; `--user` on every
  argv including `journalctl`; check mode through `Ctx` running only the
  probes with state ops predicted and actions unavailable (vision 12);
  `Restart` and `Running` through `Ctx` under the mutation guard.
- **Manual container run** (appended to the done log): a scratch musl binary
  in the session scratchpad, depending on the worktree's `rustible-sdk` and
  `rustible-std` by path, was copied into `jrei/systemd-debian:12` and
  `jrei/systemd-ubuntu:24.04` booted the way the harness boots them
  (`--privileged --cgroupns=host`, cgroup mount, wait for
  `is-system-running`). It wrote `rustible-test.service` (`sleep infinity`)
  and drove every op through `Ctx`: `Enabled.now(true)`, `Stopped`,
  `Running`, `Disabled.now(true)` each changed then ok; `Restart` with
  `daemon_reload` and `Reload.or_restart` changed; plain `Reload` on a
  stopped unit surfaces `systemctl reload ... exited 3`; `Disabled` on
  `systemd-journald` refuses with the static message; `Enabled` on
  `nope-xyz` refuses as not found on both systemd 252 (empty stdout, exit 1)
  and 255 (`not-found`, exit 4); `Running` on a unit whose `ExecStart` is
  `/bin/false` fails with the three journal lines showing the exit. Identical
  results on both images; no containers left behind.
- Hard limits: no sudo on the VM (the read-only `systemctl is-enabled` /
  `is-active` were run once as the user to confirm exit codes), no remote
  hosts, `my_infra` untouched, root only inside throwaway containers.

## Self-review: run by the lead on the PR
