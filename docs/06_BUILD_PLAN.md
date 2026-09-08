# Rustible build plan

**Written:** 2026-09-08, after `docs/01_VISION.md` was vetted. **Governs:** all
implementation work from here on. The vision doc is the contract; this document
is the schedule and the working rules. If they disagree, the vision doc wins and
this document gets fixed.

## 1. Rules of build mode

These come from section 0 of the vision doc and apply to every task, whether a
person or an agent does it.

1. **A task has a brief.** Scope, the vision-doc sections that govern it, a done
   condition that is a *command*, and a do-not-touch list. Briefs live in
   `docs/plan/<milestone>.md`. An agent's context is: read the vision doc, read
   the brief, go.
2. **The vision doc is amended before code, never by code.** A task that finds
   the design wrong stops, writes a proposed amendment in its report, and waits.
   Agents do not edit `docs/01_VISION.md`.
3. **Tests and docs land in the same commit as the code.** Every op ships with
   pure-function tests and `Fake`-backend tests; every crate has rustdoc on its
   public items; `cargo test --workspace` and
   `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check`
   pass on every commit.
4. **No stand-ins.** Placeholders are for name reservation only. If a piece is
   out of scope for the task, it is left absent with a `todo!()`-free boundary,
   not faked.
5. **One branch per task, PR to `main`, squash merge.** Review before merge is a
   code-review pass against the brief. Merging is Cadu's call at milestone
   boundaries and can be delegated inside a milestone.
6. **Trying things happens on a branch or in the scratchpad**, never on `main`.
7. **Pure Rust, transitively** (vision 5.3). A PR that adds a crate bundling C
   is rejected.

## 2. Milestones and their dependency graph

```
M1 foundation ──┬── M2 inventory ────┐
                ├── M4 init          ├── M3 real `playbook run` ── D dogfood ── M7 release
                ├── M5 elevated, cancel, streaming
                └── M6 stdlib wave one, rustible-github, docker harness
```

Three waves: **M1 alone** (single-threaded, it fixes the shapes everything else
builds on); then **M2, M4, M5, M6 fanned out** (separate crates or files, low
conflict); then **M3** joining M1 and M2, followed by dogfooding and release.

| # | Milestone | Depends on | Governing sections | Brief |
|---|---|---|---|---|
| M1 | Foundation: macros, build scanner, runtime modes, error model, facade | nothing | 5.5, 6.2, 9, 10.3, 11, 12, 14 | `docs/plan/M1.md` |
| M2 | Inventory: KDL parser, parameters and vars, precedence, `inventory show`/`check` | M1 (schema JSON shape only) | 10 | `docs/plan/M2.md` |
| M3 | Real CLI: `playbook run`, `playbook list`, workspace discovery, pipeline, renderer; delete spikes | M1, M2 | 3, 5.2, 5.4, 5.5, 9, 10.4 | `docs/plan/M3.md` |
| M4 | `rustible init`, `playbook create` | M1 | 3, 9, 10.4 | `docs/plan/M4.md` |
| M5 | `Elevated` helper backend, `Cancel`, `local_file`, `local_secret`, `fetch` | M1 | 5.5, 5.6, 11, 11.3 | `docs/plan/M5.md` |
| M6 | `rustible-std` wave one, `rustible-github`, Docker integration harness | M1 | 6, 7, 8, 6.9 | `docs/plan/M6.md` plus one brief per op from the template |
| D | Dogfood: workspace inside `my_infra`, port `ourserver/00_basic`, then the GitHub-keys role, then a host | M3, M4, parts of M6 | 6.9 | `docs/plan/D.md` |
| M7 | Release: README, rustdoc, release workflow, `0.1.0` on crates.io | everything | 9 | `docs/plan/M7.md` |

### Done conditions, in one line each

- **M1**: in `examples/workspace/`, `cargo run -- --describe` prints JSON with
  hosts, escalate, and the vars schema for every playbook; `cargo run -- hello`
  runs a playbook locally; `RUSTIBLE_PLAYBOOK=cadu/mc cargo build --features
  selected` produces a binary whose `--describe` lists exactly one playbook.
- **M2**: `cargo test -p rustible-cli inventory` covers the full example from
  vision 10.2.2 plus every load-time error in 10.2.3, and a `resolve(host)`
  function returns parameters and merged vars with their sources.
- **M3**: from `examples/workspace/`, `rustible playbook run playbooks/cadu/mc.rs`
  against `local` and the ARM VM does what the spike-2 run did, driven by
  `hosts.kdl` and the playbook attribute; `crates/spike-playbook` and the spike
  code paths are deleted.
- **M4**: `rustible init /tmp/x && cd /tmp/x && rustible playbook create
  playbooks/hello.rs && rustible playbook run playbooks/hello.rs` works on a
  fresh directory with only the toolchain installed.
- **M5**: a playbook without `escalate` runs `ctx.as_root().step(..)` that
  writes under `/etc` on the ARM VM; ctrl-c on the orchestrator stops the
  remote binary between steps; `ctx.local_file` streams a 50 MB file and
  `ctx.local_secret` never touches the target's disk.
- **M6**: every wave-one op from vision 6.9 exists with pure and fake tests, and
  the Docker harness runs each op twice on `debian:12` and `ubuntu:24.04`
  (changed then ok) in CI.
- **D**: `ourserver/00_basic` and the GitHub-keys role run from Rustible against
  the real host with the same end state Ansible produces.
- **M7**: `cargo install rustible-cli` from crates.io gives a working `rustible`
  0.1.0; the release workflow publishes all seven crates in order and uploads
  musl binaries.

## 3. Brief template

```markdown
# <Milestone or task name>

**Governing sections of docs/01_VISION.md:** <list>
**Depends on:** <milestones or PRs>
**Branch:** <name>

## Scope
<what to build, as a list of deliverables with file paths>

## Out of scope / do not touch
<crates, files, decisions this task must not change>

## Done when
<commands, with expected output>

## Notes for the implementer
<known traps, spike findings to reuse, exact shapes to match>

## Report
<what the final report must contain: what was built, what deviated, proposed
amendments if any, how it was verified>
```

## 4. Wave one op brief template (M6)

Each op is one task. The brief is the template above with these fixed parts:

- Governing sections: 6.2 (predict by default, finishing builders), 6.3 (one
  type per desired state), 6.4 (actions), 6.7 (one resource per op), 7.3 (all
  I/O through `sys`), 8 (test tiers), 12 (check mode).
- Scope: the op struct(s) and builder, the `Op` impl with `check` producing a
  `Diff` and a prediction, `apply` reusing the prediction, an `Output` struct,
  rustdoc with the Ansible equivalent named, pure-function tests for the
  planning logic, `Fake`-backend tests for satisfied / change / apply /
  failure / wrong-distro, and a Docker harness test doing changed-then-ok.
- Do not touch: the SDK, other ops, the CLI.
- Done when: `cargo test -p rustible-std <module>` passes and the harness test
  passes on both images.
