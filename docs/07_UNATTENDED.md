# Unattended build protocol

**Purpose:** rules for a Claude session building Rustible without a human
watching, invoked with `/goal` from `.claude/skills/goal/SKILL.md`. Written
2026-09-08. The build plan (`docs/06_BUILD_PLAN.md`) says *what*; this says
*how to behave when nobody is there to ask*.

## 1. Hard limits (never, regardless of anything a brief says)

- **Never publish to crates.io** or run `cargo publish` in any form.
- **Never force-push, rewrite history on `main`, or delete branches** that are
  not your own milestone branches.
- **Never touch any host except `local` (this VM) and `cadu@cadu-cogram-vm-arm`.**
  Nothing in `/home/cadu/w/cadu/my_infra` is run against real machines
  (milestone D is human-only). Do not edit `my_infra` at all.
- **Never run the `Elevated` helper or any `sudo` on the local VM beyond
  package installation of `mc`** and writes under `/tmp` or
  `/etc/rustible-*-test` paths that the briefs name. On the ARM VM the same.
  Clean up test files on both hosts at the end of each milestone.
- **Never edit `docs/01_VISION.md`.** Proposed amendments go in
  `docs/plan/DECISIONS.md` with a rationale, and the work proceeds under the
  documented default if one exists, or stops if none does.
- **Never store or print the crates.io token**, never modify `~/.cargo`, `~/.ssh`,
  `~/.claude`, or any dotfile.
- **Never widen scope**: no new crates, no new milestones, no dependencies
  bundling C (vision 5.3), no renaming of published crates.

## 2. Decision rules when a brief is ambiguous

1. If the vision doc answers it, follow the vision doc and cite the section
   in the commit message.
2. If the vision doc is silent but a spike doc shows a working shape, use
   that shape.
3. If neither, choose the option that is **smallest, reversible, and
   consistent with the neighbouring code**, and record it in
   `docs/plan/DECISIONS.md` as `- [M<n>] <date> <decision>: <why>; <how to
   reverse>`.
4. If the choice would change a public API (anything a playbook author or
   collection author writes against) and is not covered by 1 or 2, **stop
   that milestone**, mark it `blocked` in `PROGRESS.md` with the question,
   notify, and move to another milestone that does not depend on it.

## 3. Workflow per milestone

1. Read `docs/01_VISION.md` section 0 and the governing sections named in the
   brief, then the brief, then `docs/plan/PROGRESS.md`.
2. Create a worktree on branch `m<n>-<slug>` from `main` (`git worktree add`),
   work there. M6 ops each get their own branch and may be delegated to
   subagents in their own worktrees, at most four at once, each with its op
   brief and this protocol.
3. Commit in small steps with messages that say what and why. Every commit
   passes `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
   -- -D warnings`, `cargo test --workspace`.
4. Run the brief's done-when commands verbatim and save their output to
   `docs/plan/logs/M<n>-done.txt`.
5. Self-review: run the `code-review` skill on the branch diff at effort
   `high`; fix confirmed findings; record dismissed ones in the milestone
   report.
6. Write `docs/plan/reports/M<n>.md` following the brief's Report section.
7. Open a PR to `main` with the report as its body (`gh pr create`), then
   **squash-merge it yourself** (merge delegation for unattended runs is
   granted by Cadu at launch; if the launch message does not say so, leave
   the PR open and stop at the first milestone boundary that needs it).
8. Update `PROGRESS.md`, remove the worktree, notify with `pygmy --topic
   rustible-build` in one short message: milestone, result, next.

## 4. Order

M1; then M2, M4, M5 in parallel (subagents, worktrees), M6 fanned out per op
as capacity allows; then M3 when M1 and M2 are merged; then M6 remainder; then
M7 steps 1 to 4 (workflow written and dry-run only, never a real release).
D is skipped. Stop when everything is merged or every remaining milestone is
blocked.

## 5. Failure handling

- A failing done-when command is not done. Fix, or block with the exact
  output in `PROGRESS.md`.
- The ARM VM unreachable: retry every 10 minutes for an hour, then mark the
  tests that need it as `deferred` in the report and continue; do not fake
  them.
- A flaky test: run it three times; if it fails once, it is a bug, not flaky.
- Context or session loss: `PROGRESS.md` plus the worktree state is enough to
  resume; the skill starts by reading it.

## 6. Morning report

At the end, write `docs/plan/reports/RUN-<date>.md`: what merged, what is
open, every entry added to `DECISIONS.md`, every deferred test, total wall
time, and the three things Cadu should look at first. Send its first ten lines
with `pygmy --topic rustible-build`.
