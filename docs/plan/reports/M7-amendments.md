# M7 amendments: three decisions applied

Branch `amendments`, worktree `/home/cadu/w/cadu/rustible-amend`.

Three of the four amendments proposed by earlier milestone agents in
`docs/plan/DECISIONS.md` were decided by Cadu on 2026-09-08 and are
implemented here. The fourth (the `rustls-rustcrypto` alpha TLS provider,
`[M6-gh]`) is under separate research and is untouched.

All three contradict `docs/01_VISION.md`. That file is not edited: the exact
edits are handed over as `docs/plan/VISION-AMENDMENTS-2026-09-08.patch`.

## 1. `rustible init` refuses on conflict, not on non-emptiness

`crates/rustible-cli/src/init.rs`, `crates/rustible-cli/tests/init.rs`.

`init` refused any directory holding anything but `.git`, so a freshly cloned
repository with a `README.md`, a `LICENSE` and a `.gitignore` needed `--force`.
It now uses cargo's rule: refuse only when a file `init` would itself write is
already there, and name the ones that clash.

- The conflict set is `GENERATED_PATHS`, the seven paths `generate()` writes:
  `Cargo.toml`, `build.rs`, `src/main.rs`, `src/lib.rs`, `.cargo/config.toml`,
  `hosts.kdl`, `rustible.toml`. A test asserts it equals `generate()`'s own
  paths, so the two cannot drift.
- `.gitignore` is not a conflict: it is appended to, never overwritten. Nor is
  `playbooks/.gitkeep`, which is created only when missing.
- A dangling symlink counts as a conflict (`symlink_metadata`, not `exists`).
- Files outside that set are left alone and never read.
- `--force` is unchanged: add the missing files, keep the user-owned ones,
  rewrite only the two generated shims.
- `is_empty_dir` and its `.git` exception are gone. `.git` was never a file
  `init` writes, so the exception has nothing left to except.

The error names the clashing files in write order:

```
/tmp/x already has Cargo.toml, build.rs, src/main.rs; `rustible init` will not
overwrite them. Use --force to add only the missing files (existing files are
kept, only the two generated shims are rewritten)
```

Tests: three new unit tests (`generated_paths_match_what_generate_writes`,
`only_generated_files_conflict`, `a_dangling_symlink_still_conflicts`) and two
rewritten binary tests. `refuses_a_non_empty_directory_without_force` became
`refuses_only_on_conflicting_files_and_names_them`, and
`a_git_directory_counts_as_empty_and_gitignore_is_appended` became
`a_cloned_repository_is_not_a_conflict`, which is the case that motivated the
amendment. `regenerates_the_example_workspace` was not affected: it generates
into a directory that does not exist yet.

## 2. `apt::Latest` refreshes the cache in `check` too

`crates/rustible-std/src/apt.rs`, `crates/rustible-std/tests/it_apt_latest.rs`.

`Latest` compares installed versions against the candidate versions in
`/var/lib/apt/lists`, but ran `apt-get update` only in `apply`. A dry run
against stale lists therefore reported every package current, which is a wrong
answer rather than a stale one. The refresh moves into `check`, before
`apt-cache policy` is read, which is what Ansible's `apt: state=latest` does.

`apply` no longer refreshes at all. `check` always runs first (the `Change`
`apply` receives can only have come from it), so the candidates in the plan are
already the fresh ones. That also removes a real bug: under `Duration::ZERO`
the old code updated twice per run, and the plan `check` produced had been
computed against the lists as they were *before* `apply` refreshed them, which
is why `apply` carried a comment about the candidate having moved underneath
it. `apt::Present` is unchanged, because dpkg answers its question without the
lists.

### The dry-run tension, and how it is handled

`check` is not supposed to change the target, and `apt-get update` rewrites
`/var/lib/apt/lists`. The check-mode guard does not catch it, because that
guard covers file mutations through `sys` and this is a command. So the guard
is not what makes this acceptable; three other things are:

1. **It only happens when asked.** No `.update_cache(...)`, no refresh, and the
   plan is then computed against whatever the lists already say.
2. **It is documented where it will be read.** The module docs and the rustdoc
   on both `Latest` and `Latest::update_cache` say outright that a dry run with
   `.update_cache(...)` writes the package lists.
3. **It is visible in the run output.** `Latest::check` emits a `Warn` event
   naming `/var/lib/apt/lists` when it refreshes in check mode, and a `Debug`
   event otherwise, where a refresh during a real run is unremarkable.

The alternative, keeping the refresh in `apply`, is worse: it makes `--check`
quietly answer a question it has no data for. A wrong answer that looks
confident beats no answer only for the machine.

Tests: `latest_update_cache_runs_in_check_before_reading_candidates` (pins the
order and that `apply` does not repeat it), `latest_update_cache_skips_a_fresh_cache`,
`latest_check_mode_refreshes_the_cache_and_warns`,
`latest_cache_refresh_outside_check_mode_does_not_warn`, and the renamed
`latest_check_mode_runs_only_read_only_commands_without_update_cache`, which
keeps the old guarantee for the no-`update_cache` case.

New container test `tests/it_apt_latest.rs` on `debian:12` and `ubuntu:24.04`.
It first asserts the stock image names no candidate for `sl`, then runs
`Latest::check` through a check-mode `System` and asserts both that the plan
carries a real candidate version and that the lists are refreshed afterwards.
On the old code that `check` would have failed with "no candidate version in
the apt cache", so the test is a genuine regression guard. It then installs for
real and asserts the second run reports `ok`.

## 3. `systemd::DaemonReload`

`crates/rustible-std/src/systemd.rs`, `crates/rustible-std/tests/it_systemd.rs`.

`systemctl daemon-reload` was reachable only as `.daemon_reload(true)` on
`Restart` or `Reload`, so a playbook that writes a unit file and wants systemd
to notice it had to bounce some unit as a pretext. Ansible allows
`daemon_reload` on its own; so does this now.

An action in the sense of vision 6.4: `check` always returns `Change`, runs
nothing, predicts nothing; `always_changes()` is true; it names no unit.
`apply` runs `systemctl daemon-reload`, or `systemctl --user daemon-reload`
with `.user(true)`. The guards are the family's: a non-systemd init is refused,
and root is required unless `.user(true)` selects the caller's own manager.

To share those guards with an op that has no unit name, `Unit::guard` is split
into the unit-name half and a `guard_manager` half; `Unit::manager()` builds
the no-unit `Unit` that carries only the `--user` choice. No other op's
behaviour changes, and the shared-guard test sweep now covers seven ops.

### Why the output type is `()`

The other six systemd ops return `UnitState`, which is `{ unit, enabled,
active }`. All three fields need a unit, and this op has none, so `UnitState`
is not available honestly. The alternatives considered:

- **A `ManagerReload { user: bool }` struct.** Rejected: it only echoes the
  op's own input back. An input dressed as an output is worse than no output,
  because a chained step could read it and believe it learned something.
- **The manager's state, read back after the reload.** There is nothing worth
  reading. `systemctl daemon-reload` either succeeds or fails, and failure is
  already an `Err`.
- **`()`.** Says exactly what is true: the step either reloaded the manager or
  failed, and there is nothing to carry forward. Chosen.

`Default` is implemented as `new()` only because clippy asks for it on a
`new()` with no arguments; a test pins the two equal so it cannot drift.

Tests: five unit tests covering the always-changes contract, the single command
`apply` runs, user mode without root, `Default`, and the op through `Ctx` in
check mode. `every_op_refuses_a_non_systemd_init_without_running_anything` and
`every_op_refuses_without_root_and_names_the_user` now sweep it too.
`every_op_rejects_a_bad_unit_name` does not, on purpose, with a comment saying
why: it takes no unit name.

`tests/it_systemd.rs` used to reload `systemd-journald` purely to carry a
`.daemon_reload(true)` flag past a freshly written unit file. It now uses
`DaemonReload::new()` in one step, which is the container proof the op works on
both `jrei/systemd-*` images.

## The vision patch

`docs/plan/VISION-AMENDMENTS-2026-09-08.patch`. Six hunks against
`docs/01_VISION.md` as it stands at `1eaa988`, each with one or two rationale
lines above it:

| hunk | section | change |
|---|---|---|
| 1 | 3 | `init` no longer requires an empty directory; conflicts and `--force` described |
| 2 | 6.3 op table | a `systemd: daemon_reload=yes` row |
| 3 | 6.4 | `DaemonReload` joins the actions; an action may name no subject and return `()` |
| 4 | 6.8 apt | `Latest` refreshes in `check`, with the check-mode cost stated |
| 5 | 6.8 systemd | `DaemonReload` alongside `Restart` |
| 6 | 6.9 | `DaemonReload` in the wave-one op list |

Rationale lines start with `#` and hunks are separated by an empty line;
neither is part of the diff, so it applies with:

```
grep -vE '^#|^$' docs/plan/VISION-AMENDMENTS-2026-09-08.patch | git apply -
```

That was run against a scratch copy and produces exactly the intended file;
the branch itself leaves `docs/01_VISION.md` untouched.

## Decisions recorded

Three `[M7-amend]` entries appended to `docs/plan/DECISIONS.md`, each with a
`Reverse:` clause. The three `PROPOSED AMENDMENT` lines (`[M4]`, `[M6-so]`,
`[M6-sd]`) are marked decided by Cadu on 2026-09-08 in place, each pointing at
its new entry. The fourth proposed amendment (`[M6-gh]`, the TLS provider) is
untouched.

## Verification

`docs/plan/logs/M7-amendments-done.txt`. Every command exited 0.

| command | result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | all green |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib` | clean |
| `cargo +1.88 check --workspace --all-targets` | clean (CI's MSRV job) |
| `cargo build --manifest-path examples/workspace/Cargo.toml` | builds |
| container: `it_apt_latest`, `it_apt_present`, `it_apt_absent`, `it_systemd`, `it_systemd_image` | all green on both images each |

## What surprised me

**Systemd already sees a new unit file without a reload.** The first draft of
the container test asserted that `systemd::Enabled` on a freshly written
`rustible-test.service` fails with `not found` before any daemon-reload. It
does not: on both `jrei/systemd-debian:12` and `jrei/systemd-ubuntu:24.04`,
`systemctl is-enabled` answered `disabled` and the enable succeeded, with no
reload anywhere before it. Modern systemd rescans on demand. The assertion was
dropped. This does not weaken the amendment, since `DaemonReload` is still the
honest way to say "make systemd re-read its units" and is still needed in the
cases where the rescan does not happen, but it does mean the old journald
workaround in `it_systemd.rs` was probably never load-bearing.

**Moving the apt refresh into `check` removed a bug rather than adding a
trade-off.** The tension the brief flagged is real for `--check` runs, and it
is handled by documenting and warning. But for a normal run the old placement
was simply wrong: `check` planned "upgrade openssl 3.0.15 to 3.0.16" from stale
lists, `apply` then refreshed and installed whatever the *new* lists said, and
the report claimed the version the old lists had named. The code carried a
comment acknowledging exactly this. Refreshing first makes the plan and the
report agree.

**The `--force` semantics survive the change untouched, but the reason
changed.** Before, `--force` meant "I know this directory is not empty".
Now non-emptiness is not a thing `init` objects to, so `--force` means only "I
know some of these files are mine and I want them kept". The flag's help text
and behaviour are the same; only the situation that produces it is rarer.

## Not done

- `docs/01_VISION.md` is not edited, per `docs/07_UNATTENDED.md`. The patch
  file is the deliverable.
- The PR is not merged.
- The worktree is kept.
