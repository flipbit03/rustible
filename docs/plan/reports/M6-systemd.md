# M6: `systemd` ops

**Branch:** `m6-systemd`. **Governing sections:** vision 6.2, 6.3, 6.4, 6.7,
6.8 (systemd translation), 6.9, 7, 8, 12, 13. **Files:**
`crates/rustible-std/src/systemd.rs` (new), `crates/rustible-std/src/lib.rs`
(one `pub mod` line), `docs/plan/DECISIONS.md`, `docs/plan/logs/M6-systemd-done.txt`, `crates/rustible-std/tests/it_systemd.rs` (new).
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

- **Container test landed after the PR opened.** The harness (PR #5) merged
  while this PR was open; `tests/it_systemd.rs` was added on the merge with
  main and the manual run it replaced is kept in the done log for the record.
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
- **Container test** `crates/rustible-std/tests/it_systemd.rs`
  (`#[rustible::integration_test(systemd_images = ["jrei/systemd-debian:12",
  "jrei/systemd-ubuntu:24.04"])]`), run with `RUSTIBLE_INTEGRATION=1 cargo
  test -p rustible-std --test it_systemd -- --nocapture`. It writes
  `/etc/systemd/system/rustible-test.service` (`sleep infinity`), runs
  `Reload::new("systemd-journald").daemon_reload(true).or_restart(true)` so
  systemd sees the file, then `Enabled.now(true)`, `Stopped`, `Running`,
  `Disabled.now(true)` through `changed_then_ok`, `Restart` in between, and
  asserts the two refusals against real systemctl (`Disabled` on the static
  `systemd-journald`, `Enabled` on a unit that does not exist, which is
  `not-found`/exit 4 on Ubuntu's systemd 255 and empty stdout/exit 1 on
  Debian's 252). Run three times (docs/07 rule 5): 6 of 6 green, no
  containers left behind (`docker ps -aq --filter label=rustible.integration`
  empty).

  | image | in-container time | whole test (both images, warm build) |
  |---|---|---|
  | `jrei/systemd-debian:12` | 0.9 to 1.9 s | 1.9 to 5.3 s |
  | `jrei/systemd-ubuntu:24.04` | 0.9 to 1.0 s | |

  Before the harness merged, the same sequence was run by hand with a scratch
  musl binary; that output is still appended to the done log.
- Hard limits: no sudo on the VM (the read-only `systemctl is-enabled` /
  `is-active` were run once as the user to confirm exit codes), no remote
  hosts, `my_infra` untouched, root only inside throwaway containers.

## Self-review: run by the lead on the PR

### Self-review (lead, PR #8)

Reviewed by the lead against vision 6.3, 6.4, 6.7 and 12. No code changes were
needed; the branch is merged as it stands. What was checked:

- **Actions run nothing in `check`** and predict nothing, which is vision 6.4
  exactly; `Restart` and `Reload` flag `always_changes`. The state ops run only
  `is-enabled` and `is-active` in `check`, both read-only.
- **Predictions are honest.** Each state op predicts the state it will have
  produced, and `apply` re-reads and then asserts it (`ensure!(state.enabled)`,
  `verify_running`, `verify_stopped`), so a prediction that turns out wrong
  fails the step instead of being reported as truth.
- **The refusals match vision 6.7**: masked units are refused rather than
  unmasked, `static`, `generated` and `transient` units are refused by
  `Disabled` with the reason, and a unit `is-enabled` cannot find is refused by
  every state op rather than silently counting as stopped.
- **The parsers cover every documented `systemctl` word** including the
  nonzero-exit cases, with an `Other(String)` escape for future systemd
  versions, and the container test pins the two real refusals against systemd
  252 and 255.
- **`--user` support** carries through the probes, the commands, the journal
  tail and the messages, and is what lifts the root requirement.

One gap noted, not changed: there is no way to run `systemctl daemon-reload`
on its own, because `daemon_reload` is a flag on the two actions (the shape
vision 6.4 and the M6 brief name). The container test has to reload journald
to get systemd to notice a new unit file, which is a workaround, not a use.
Recorded as a proposed amendment for Cadu below.

Verification after merging main: `cargo fmt --all --check`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo test --workspace` (366 tests
across the workspace), and the container test re-run green on
jrei/systemd-debian:12 and jrei/systemd-ubuntu:24.04 in 3.8 s.

