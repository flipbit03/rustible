# Progress

State per milestone: `todo` | `in-progress (branch)` | `pr-open (#n)` | `merged` | `blocked (why)` | `deferred`.
Updated by whoever is working; read first on every resume.

| Milestone | State | Notes |
|---|---|---|
| M1 foundation | merged | PR #1, 2026-09-08; ARM VM leg deferred (VM unreachable for an hour), see logs/M1-arm.txt |
| M2 inventory | merged | PR #4, 2026-09-08; review: 10 fixes applied by the lead; --check-vars recommendation recorded for M3 |
| M3 real run | in-progress (m3-run) | subagent, started after M2 merged |
| M4 init | merged | PR #2, 2026-09-08; review: 10 findings, 9 fixed, 1 recorded as proposed amendment |
| M5 elevated/cancel/streaming | in-progress (m5-elevated-streaming) | subagent, unattended run 1; ARM VM may be down |
| M6 stdlib wave one | in-progress | `ssh::authorized_keys` merged (PR #3); `user`+`group` pr-open (#6, review running); `file` family and `systemd` in progress (subagents); others todo |
| M6 rustible-github | todo | |
| M6 docker harness | pr-open (#5) | review queued behind #3 and #4 |
| D dogfood | human-only | never unattended |
| M7 release prep | todo | workflow + dry run only; publishing is Cadu's |

## Log
(append one line per event: date, milestone, what happened)
- 2026-09-08 00:55 UTC  M1  started on branch m1-foundation (unattended run 1, merge=yes)
- 2026-09-08 02:55 UTC  M1  merged as PR #1 after code-review (10 findings, 9 fixed, 1 dismissed with reason); ARM VM leg deferred
- 2026-09-08 03:00 UTC  M2,M4,M5,M6-harness  started in parallel on their own worktrees (four subagents)
- 2026-09-08 03:45 UTC  M4  merged as PR #2 after code-review (10 findings, 9 fixed by the lead, 1 proposed amendment in DECISIONS.md)
- 2026-09-08 02:49 UTC  all   the four subagents (M2, M5, M6-harness, M6-authorized-keys) were killed by an account session rate limit; work preserved in their worktrees (M2: code+log committed; M5: code committed, report drafted; harness: code committed, log written; authorized-keys: just started)
- 2026-09-08 10:20 UTC  all   Cadu re-invoked /goal ("keep going on all fronts"); four resume agents launched against the existing worktrees; ARM VM still unreachable (timeout)
- 2026-09-08 10:32 UTC  M2,M6  PRs #3 (authorized_keys), #4 (inventory), #5 (harness) opened by the resume agents; reviews in progress; user/group and file-ops agents started
- 2026-09-08 10:50 UTC  M6  ssh::authorized_keys merged as PR #3 after review (7 findings, all addressed; ~/.ssh creation reversed per vision 6.7)
- 2026-09-08 11:25 UTC  M2  merged as PR #4 after review (lead consolidated eight finder reports; 10 fixes)
- 2026-09-08 11:30 UTC  M3  started on branch m3-run (subagent); active: M3, M5 (ARM pending), user/group, file ops; PR #5 harness under review
- 2026-09-08 11:45 UTC  M6  user/group PR #6 opened (55 tests + for_user wrapper); systemd agent started; PR #5 harness review in finder phase
