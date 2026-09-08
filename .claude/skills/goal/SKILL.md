---
name: goal
description: Build Rustible unattended, milestone by milestone, following docs/06_BUILD_PLAN.md and the hard limits in docs/07_UNATTENDED.md. Use when Cadu says "/goal", "build the stack", "run the build plan", or "continue the build".
---

# /goal: build Rustible unattended

You are building Rustible according to a vetted design. Nobody is watching.
Your job is to get as many milestones **merged and verified** as possible
without violating a single hard limit.

## Before anything else, read in this order
1. `docs/07_UNATTENDED.md` (hard limits, decision rules, workflow, order).
2. `docs/01_VISION.md` section 0, then the sections your current milestone names.
3. `docs/06_BUILD_PLAN.md`.
4. `docs/plan/PROGRESS.md` and `docs/plan/DECISIONS.md`.
5. The brief for the milestone you are about to start, `docs/plan/M<n>.md`.

## Launch arguments
`$ARGUMENTS` may contain: `merge=yes` (you may squash-merge your own PRs; without
it, leave PRs open and stop at the first boundary that needs a merge),
`only=M<n>[,M<n>]` (restrict to these milestones), `stop-after=M<n>`, and
`topic=<pygmy topic>` (default `rustible-build`). Anything else is a note from
Cadu; obey it if it does not conflict with the hard limits.

## Loop
1. Pick the next milestone per the order in `docs/07_UNATTENDED.md` section 4
   whose dependencies are `merged`. Set it to `in-progress` in `PROGRESS.md`
   with the branch name, commit that on `main`.
2. Follow section 3 of the protocol step by step: worktree, build, verify with
   the brief's done-when commands verbatim, self-review with the `code-review`
   skill, report, PR, merge if allowed, update `PROGRESS.md`, notify.
3. For M6 ops and for M2/M4/M5 after M1 is merged: delegate to subagents in
   their own worktrees (at most four concurrently), each given: the protocol
   file, the vision doc sections, its brief, and the instruction to write its
   report and open its PR but not merge. You review and merge.
4. On any hard-limit conflict: do not do the thing, record it in
   `PROGRESS.md`, continue with something else.
5. When nothing runnable remains, write the morning report (protocol section 6),
   notify, and stop.

## Non-negotiables
- Done-when commands are run verbatim and their output saved; a milestone with
  a failing done-when is not done.
- Never edit `docs/01_VISION.md`. Never publish. Never touch hosts other than
  `local` and `cadu@cadu-cogram-vm-arm`. Never touch `my_infra`.
- Commit messages carry no tool attribution.
- Keep `PROGRESS.md` truthful at every step so a fresh session can resume.
