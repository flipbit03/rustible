# Progress

State per milestone: `todo` | `in-progress (branch)` | `pr-open (#n)` | `merged` | `blocked (why)` | `deferred`.
Updated by whoever is working; read first on every resume.

| Milestone | State | Notes |
|---|---|---|
| M1 foundation | merged | PR #1, 2026-09-08; ARM VM leg deferred (VM unreachable for an hour), see logs/M1-arm.txt |
| M2 inventory | todo | |
| M3 real run | todo | needs M1, M2 |
| M4 init | todo | needs M1 |
| M5 elevated/cancel/streaming | todo | needs M1 |
| M6 stdlib wave one | todo | per-op branches; needs M1 |
| M6 rustible-github | todo | |
| M6 docker harness | todo | |
| D dogfood | human-only | never unattended |
| M7 release prep | todo | workflow + dry run only; publishing is Cadu's |

## Log
(append one line per event: date, milestone, what happened)
- 2026-09-08 00:55 UTC  M1  started on branch m1-foundation (unattended run 1, merge=yes)
- 2026-09-08 02:55 UTC  M1  merged as PR #1 after code-review (10 findings, 9 fixed, 1 dismissed with reason); ARM VM leg deferred
