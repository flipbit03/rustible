# Spike 3: SDK core, run locally

**Date:** 2026-09-06. **Status:** done, in-tree. **Verdict:** the sketches in
`01_VISION.md` sections 6, 7, 14, 15, 16 hold up in code with one small
deviation. No blockers found.

## What was built

Workspace with three crates:

- `crates/rustible-sdk`: `Op`, `Plan`, `Change`, `Applied`, `Diff`, `System`
  over `Backend` with `Local` and `Fake`, `Facts` with real probes, `Ctx` with
  `step`, `section`, `skip`, `log`/`warn`/`debug`, `as_user`/`as_root`, events
  (`JsonLines`, `Pretty`, `Collect` sinks), and a `runtime::run` that stands in
  for the future `#[rustible::playbook]` macro.
- `crates/rustible-std`: `file::Line` (Ansible `lineinfile`), `file::Directory`
  (minimal), `shell::Command` with `creates`/`removes`.
- `crates/spike-playbook`: a playbook that hardens a scratch copy of an
  `sshd_config` on the local machine.

```
cargo test --workspace            # 10 tests
cargo run -p spike-playbook -- [-v|-vv] [--check] [--json]
RUSTIBLE_SPIKE_DIR=/tmp/x cargo run -p spike-playbook   # fixed scratch dir for idempotency runs
```

## What was verified

- **Idempotency**: first run `changed=6`, second run on the same scratch
  `ok=5 skipped=1 changed=0`.
- **Check mode**: `would_change=6`, the file on disk is byte-identical after,
  and chained reads of predicted outputs work.
- **Mutation guard**: an op that writes inside `check()` fails with
  "op attempted to mutate `/x` during check()" and the fake shows no write
  (test `mutation_during_check_is_refused`).
- **Check-mode output rule** (section 15): an op that predicts yields an
  available output; one that does not yields `OutputUnavailable` on
  `.output()` and a panic (caught by the runtime) on deref
  (test `check_mode_output_unavailable_unless_predicted`).
- **Diff rendering**: unified diff for text, attr list for mode changes,
  `+1 -1 lines` short form in the step list, full diff at `-v`.
- **Facts**: gathered in well under a millisecond from `/etc/os-release`,
  `/proc/*`, and PATH probes. Correctly reported Ubuntu 24.04, x86_64, apt,
  systemd, 16 cpus, on the dev box.
- **Events**: JSON lines carry everything the orchestrator needs to render the
  same view the `Pretty` sink renders locally.
- **Fake backend tests** run in 0.00s and can assert on content written and
  commands run.

## Findings and deviations from the vision doc

1. **`apply` receives `Change<T>`, not `Plan<T>`.** Only the change branch is
   meaningful in `apply`, so passing `Plan` forced a pointless match. The doc's
   signature should be updated to `fn apply(&self, sys, change: Change<Output>)`.
2. **Prediction is nearly free and should be the norm.** Both `Line` and
   `Directory` already compute their post-apply output while planning, so
   `Plan::change_predicting(diff, output)` cost nothing. `apply` then reuses
   `change.predicted` for the parts it cannot cheaply recompute. Recommendation:
   keep it opt-in in the trait, but write every stdlib op to predict unless it
   genuinely cannot (uid allocation, package versions resolved by apt).
3. **Builder that ends in a finishing call reads well.**
   `Line::in_path(p).matching(re).backup(true).set(line)` where `set` returns
   the `Op`. You cannot construct a `Line` without a line. Worth adopting as a
   pattern where an op has one mandatory piece of desired state.
4. **Event ordering inside a step.** `debug: wrote ...` and `CmdRan` events are
   emitted between `StepStarted` and `StepFinished`, so the pretty view prints
   them above the step line. The orchestrator should buffer sub-events under
   their step (it has `StepStarted` to open the scope). Not an SDK problem.
5. **Commands are not guarded during `check`.** `sys.cmd()` must work in
   `check` (`dpkg-query`, `systemctl is-enabled`), so there is no way to stop
   a `check` that runs `apt-get install`. This is a documented limit of the
   guard; file mutations are guarded, processes are the op author's honesty.
6. **`as_user` in this spike only affects commands** (sudo prefix). The
   `Elevated` helper-process backend from section 14.3 is not built; file ops
   under `as_root()` would still run as the process user. Next spike material.
7. **`Pretty` renderer lives in the SDK for now** because the spike has no
   orchestrator. It belongs to the `rustible` CLI crate once that exists; the
   binary should emit only frames.
8. **`Applied<T>` deref panic is acceptable.** The runtime catches the panic and
   reports a failed run with the same message as `.output()`. Playbooks that
   want to branch on availability use `.is_available()`.
9. **`Line` normalizes a missing trailing newline.** Deliberate; noted so it is
   not reported as a bug later.
10. **Edition 2024 with let-chains** compiles clean on rustc 1.97 and reads
    well in op code. Keep.
11. **No macro yet.** `runtime::run(RunOptions, fn)` was enough to learn what
    the macro must generate: option parsing from the `Start` frame, facts
    gathering, `Ctx` construction, panic catching, summary emission, exit code.
    The macro adds the vars struct deserialization and `--describe` on top.

## Not covered by this spike

SSH, cross-compilation, the framed protocol, `local_file` streaming (stubbed
to a local path), `local_secret`, the `Elevated` backend, the `#[playbook]`
macro, and the `rustible` CLI. Spikes 1 (cross-compile, blocked on the ARM VM)
and 2 (protocol over SSH) remain.
