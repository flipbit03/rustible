# Check mode without predictions — vision amendment and plan

Date: 2026-09-24. Status: **DECIDED by Cadu 2026-09-24**, both open items
(systemd units included; no compatibility shims), and the amendment applied to
`docs/01_VISION.md` the same day. Branch: `feat/remove-plan-mode`, one
squash-merged pull request for the whole change: amendment, code, tests, docs.

## 1. What is being decided

Check mode stays. What goes is the machinery that tried to make a dry run
walk further than the facts allow:

1. **Predictions.** `Change<T>` carries `predicted: Option<T>`, an op's guess
   at its post-apply output, set through `Plan::change_predicting`; `Applied`
   carries a `predicted: bool` flag. Eighteen ops predict; the rule was
   "predict by default" (vision 6.2) with per-op rulings on when a guess is
   honest (`[M6-ug]`, `[M6-so]`, `[M6-sh]`, `[M6-na]` in DECISIONS.md). Four
   ops (`group::Absent`, `http::Download`, `file::Copy`, `sysctl::Present`)
   also use the slot as a private channel from `check` to `apply`.
2. **The planned-resource registry.** `System::note_would_create`,
   `would_create`, `would_create_id`, `would_create_id_by_name` and the
   `Planned` type (`system.rs:63-75, 290-345`), added by `[M6-sh] 2026-09-08`
   so a dry run of `group::Present` then `user::Present.gid("app")` gets past
   the user step. One writer (`group::Present`), two readers (`user::Present`,
   `user::Membership`). Every later gap of the same shape was answered by
   declining to extend it (`[USING_RUSTIBLE] 2026-09-11` twice, `[OPS]
   2026-09-11`, `[ISSUE-40] 2026-09-17`).

What replaces both is Ansible's rule, verified against `ansible/ansible`
`devel` on 2026-09-24: in `user.py`'s `main()`, `state == 'present'` on an
account that does not exist runs `if module.check_mode: module.exit_json(changed=True)`
before it validates the group, the home's parent, or anything else;
`authorized_key.py` returns from `keyfile()` under check mode before it looks
at the directory. A step that would create something reports `changed` and
asks no further questions; prerequisites are verified when the run is about
to act. Rustible keeps the part Ansible lacks: reading a would-change step's
output fails loudly instead of yielding garbage.

## 2. The amendment to `docs/01_VISION.md`

Seven hunks. Each gives the current text and its exact replacement.

### Hunk 1 — §6.2, the `Op`, `Plan`, `Change` and `Applied` sketch

In the `Op` trait sketch:

```
    fn apply(&self, sys: &System, change: Change<Self::Output>) -> Result<Self::Output>;
```
becomes
```
    fn apply(&self, sys: &System, change: Change) -> Result<Self::Output>;
```

In `Plan<T>`:

```
    Change(Change<T>),
```
becomes
```
    Change(Change),
```

Current:

```
/// `diff` is what the report shows; `predicted` (opt-in, section 12) is what
/// the output would be after apply, so chained steps continue in check mode.
pub struct Change<T> { pub diff: Diff, pub predicted: Option<T> }
// (Spike 3 finding: `apply` takes `Change<T>` rather than `Plan<T>`; see docs/02_SPIKE_SDK_CORE.md.)

pub struct Applied<T> {
    value: Option<T>,        // None only in check mode when the op did not predict (section 12)
    pub changed: bool,
    pub predicted: bool,     // value is a prediction
    pub diff: Option<Diff>,
    pub elapsed: Duration,
}
```

Replacement:

```
/// `diff` is what the report shows and what `apply` executes. Nothing else
/// rides along: a would-change step has no output until `apply` has run
/// (section 12, revised 2026-09-24).
pub struct Change { pub diff: Diff }
// (Spike 3 finding: `apply` takes the change rather than `Plan<T>`; see docs/02_SPIKE_SDK_CORE.md.
//  `Change` lost its type parameter together with the prediction slot, 2026-09-24.)

pub struct Applied<T> {
    value: Option<T>,        // None only in check mode, when the step would change (section 12)
    pub changed: bool,
    pub diff: Option<Diff>,
    pub elapsed: Duration,
}
```

### Hunk 2 — §6.2, the step driver

Current:

```
        Plan::Change(c) if sys.check_mode() => {
            emit(StepFinished { status: WouldChange, diff: c.diff });
            Applied { value: c.predicted, changed: true, predicted: c.predicted.is_some(), .. }   // no apply
        }
```

Replacement:

```
        Plan::Change(c) if sys.check_mode() => {
            emit(StepFinished { status: WouldChange, diff: c.diff });
            Applied { value: None, changed: true, .. }   // no apply, so no output (section 12)
        }
```

### Hunk 3 — §6.2, the first policy bullet

Current:

```
- **Predict by default.** Both ops in the spike already computed their
  post-apply output while planning, so `Plan::change_predicting(diff, output)`
  cost nothing. Every stdlib op predicts unless it genuinely cannot (uid
  allocation, versions apt has not resolved yet). `apply` may reuse
  `change.predicted` for what it cannot cheaply recompute.
```

Replacement:

```
- **No predictions** (reversed 2026-09-24; the original rule is kept here for
  the record). Spike 3 found that both of its ops computed their post-apply
  output while planning, so handing it over as a prediction cost nothing, and
  "every stdlib op predicts unless it genuinely cannot" became the rule. Two
  waves of the stdlib showed where the cost lands: not in the code but in the
  judgement. Every op that creates something had to rule on which fields it
  may honestly claim before the tool has run (a uid it has not allocated, the
  shell BusyBox picks from an environment the op cannot see, a version apt
  has not resolved), each ruling needed its own decision-log entry, and the
  report never distinguished a prediction from a fact. A would-change step
  now has no output in check mode; section 12 has the rule. `apply` executes
  the `Diff` that `check` produced and reads for itself whatever the diff
  does not carry (a gid to report, a digest for the output): a read is not a
  decision, and the change carries no private payload from `check` to
  `apply`.
```

### Hunk 4 — §6.7, a paragraph appended after the home-directory exception

Added text:

```
**In a dry run the refusal waits.** A prerequisite that another op in the same
run could create — a group, an account, its home, a parent directory, a unit
file — is verified when the run is about to act, not while it is only
looking: under `--check` the op reports `would change` with the prerequisite
named in its diff, and a real run refuses exactly as this rule says, because
its `check` runs with check mode off and a dry run's plan never reaches
`apply`. Section 12 has the reasoning and the limits.
```

### Hunk 5 — §11.1, the check-mode bullet

Current:

```
- **In check mode `changed` means "would change".** A playbook that logs after
  a changed step should branch on `applied.predicted` to word it honestly
  (spike 2 caught the `mc` playbook logging "installed mc" in a dry run).
```

Replacement:

```
- **In check mode `changed` means "would change".** A playbook that logs after
  a changed step should branch on `ctx.check_mode()` to word it honestly
  (spike 2 caught the `mc` playbook logging "installed mc" in a dry run).
```

### Hunk 6 — §12, the whole section

Current: the section as it stands from `## 12. Check-mode semantics (DECIDED
2026-09-06)` to the line before `## 13. Facts`.

Replacement:

```
## 12. Check-mode semantics (DECIDED 2026-09-06, REVISED 2026-09-24)

Problem: `Plan::Satisfied(T)` carries an output, `Plan::Change { diff }` does
not, so in a dry run a step that *would* change has nothing to return, and a
later step that chains from it has no value.

Options considered:
1. Stop the host at the first would-change step. Honest but shows only the
   first change; useless for "what would this playbook do". Rejected.
2. Continue; the output is unavailable; fail loudly only when a later step
   actually reads it.
3. Let ops predict their output. Most fidelity, more work per op, and a wrong
   prediction is a lie in a dry run.

**Decision (2026-09-06): 2 as the rule, 3 as opt-in. Revised 2026-09-24: 2
alone.** Two waves of the stdlib were built under the first decision, and
what they showed is recorded so the reversal is not relitigated:

- Prediction moved the work from code into judgement. Every op that creates
  something had to rule on which fields it may claim before the tool has run,
  and the rulings piled up: a new account predicts only with an explicit uid,
  gid *and* shell; apt only when `apt-cache policy` names a candidate; a
  download never, except when only its permissions change. Each ruling was a
  decision-log entry, a guide paragraph and a test.
- The predictions rarely fired where a dry run matters most. On a fresh host
  nearly every step creates something, and the honest answer for a created
  thing was usually "cannot predict", so the playbook author was told to add
  `.uid(3000).gid(3000)` for the dry run's sake.
- The report never showed which values were predictions. The distinction
  existed for the playbook and not for the person reading the run.
- To let a dry run get past a step that *needs* what an earlier step would
  create, `System` grew a registry of planned resources
  (`note_would_create`, 2026-09-08). One op ever wrote to it. Every later gap
  of the same shape — the unit a package ships, the directory a step would
  make, the home an account would get — was answered by declining to extend
  it and adding a check-mode branch in the op instead, so three different
  answers to one question were in the tree at once.

Ansible's check mode has neither mechanism and one rule: a step that would
create something reports `changed` and asks no further questions (`user.py`'s
`main()` exits `changed` under check mode before it validates the group;
`authorized_key` returns before it looks at the directory). That rule is
adopted, with the loud failure Ansible lacks.

**The rules:**
- In check mode, a would-change step reports `WouldChange` with its diff and
  the run continues. Nothing is applied and nothing is predicted: `Change`
  carries the diff and no post-apply output.
- A would-change step's output does not exist. `Applied<T>` holds
  `Option<T>`; reading the output of a would-change step (via `Deref`) fails
  with "step `<name>` would have changed; its output is unavailable in check
  mode". `.changed` and `.diff` remain readable; `.is_available()` is the
  guard for a playbook that wants to keep going. Playbooks that do not chain
  get a full dry run; those that chain get as far as the first dependent
  read, with a clear message. Ansible does the same with silent garbage
  instead of a loud error.
- **Prerequisites are verified when the run is about to act.** An op whose
  `check` would refuse for want of a resource another op in the same run
  could create — a group, an account, its home, a parent directory, a unit —
  reports `would change` under check mode instead, naming the prerequisite in
  its diff. The tolerance is gated on check mode, so a real run's `check`
  takes the refusal, and a dry run's plan never reaches `apply` (`Ctx::step`
  returns at its check-mode arm): the refusal is never skipped on a run that
  can act. What a dry run therefore does not catch is a forgotten
  prerequisite step; the real run refuses before touching anything, as 6.7
  requires.
- That deferral covers only what another step could supply. A refusal about
  the machine or the request itself — wrong platform, not root, the tool the
  op drives is absent, a malformed key, a sysctl key this kernel does not
  have — stands in check mode as in a real run, because no earlier step
  changes it.
- `check` still cannot mutate (7.3), and `apt::Latest` with `.update_cache`
  is still the one place a dry run writes (6.8).
```

### Hunk 7 — §15 glossary and the §17 summary line

Glossary, current:

```
- **Predicted**: an op's declared post-apply output, returned in check mode
  instead of applying.
```

Replacement:

```
- **Would change**: a step's status in check mode when `check` found a
  difference. `apply` does not run and the step has no output (section 12).
```

§17 summary, current fragment:

```
`Change<T>` and ops predict by default (6.2);
```

Replacement:

```
`apply` takes the change, not the plan (6.2; the predict-by-default rule that
came with it was reversed 2026-09-24, section 12);
```

The spike-3 record two lines above ("prediction is nearly free") is history
and stays as written.

## 3. Blast radius

SDK (`rustible-sdk`, public surface):
- `op.rs`: `Change<T>` → `Change { diff }`; `Plan::change_predicting` and
  `Change::predicted` removed; `Applied::predicted` removed; `Applied::new`
  loses a parameter.
- `ctx.rs:256-270`: the check-mode branch builds `Applied` with `None`.
- `system.rs`: `Planned`, the `planned` field and the four registry methods
  removed.
- Wire format, events, `Diff`, `Backend`, `Fake`: untouched (verified: no
  event or frame carries `predicted`).

Standard library (`rustible-std`) and `rustible-github`:
- 27 `change_predicting` call sites in 18 ops become `Plan::change`.
- `group::Absent`, `http::Download`, `file::Copy`, `sysctl::Present`: `apply`
  takes its instruction from the `Diff` (attributes-only versus content) and
  reads what it needs for its output.
- `user::Present` (`resolve_primary`, `same_named_group`, the `.groups()`
  check) and `user::Membership`: the registry lookups become "in check mode,
  name the group in the diff and go on; otherwise refuse as today".
- `ssh::authorized_keys`: its two check-mode branches stay, now as instances
  of the §12 rule rather than exceptions to it.
- `systemd`: a unit `systemctl` cannot find becomes `would change` under
  check mode and the same refusal as today otherwise. This closes the
  `[USING_RUSTIBLE] 2026-09-11` KNOWN GAP and is the one item here that can
  be struck for a smaller change.
- Tests asserting `.predicted`, `change.predicted` or the registry are
  rewritten to assert the new rule.

Docs:
- `docs/01_VISION.md`: the seven hunks above.
- `docs/USING_RUSTIBLE.md` §9 and §15: the prediction paragraphs go, the
  `.uid(3000) // so --check can predict` comments go, §15 states the rule in
  a few lines and the KNOWN GAP warning narrows or goes.
- `CLAUDE.md`: the "predict an output only when every field is honestly
  knowable" bullet and the `Applied.predicted` trap are replaced.
- `docs/06_BUILD_PLAN.md` §4 checklist, if it names prediction.
- `docs/plan/DECISIONS.md`: one entry per decision with its `Reverse:`;
  `docs/plan/PROGRESS.md`: the resume line.

Compatibility: none owed. Rustible is alpha software at 0.x; `Change<T>`,
`change_predicting` and `Applied::predicted` go without a shim, and a playbook
that names them stops compiling with a rustc error naming the item, which is
the fix telling the author where to look (Cadu, 2026-09-24).

## 4. Acceptance criteria

1. **Nothing left.** `grep -rn "predicted\|change_predicting\|would_create\|note_would_create\|Planned\b" crates examples docs/USING_RUSTIBLE.md CLAUDE.md docs/06_BUILD_PLAN.md` is empty. The words survive only in DECISIONS.md, PROGRESS.md, the spike documents and the vision's own dated reversal.
2. **The shape.** `Change` has one field. `Applied<T>` has no `predicted`. `System` has no planned list. `Plan::change(diff)` is the only way to build a change.
3. **`make` is green**: fmt, clippy with `-D warnings`, the tier 1 and 2 suite, rustdoc with `-D warnings`, `examples/workspace`.
4. **`make integration` is green** (docker): every container test that asserted a prediction or the registry now asserts the rule.
5. **The rule is pinned at tier 2**, one test each:
   - a would-change step's output is unavailable under `--check` and `is_available()` is false (`file::Line`, and `user::Present` on an existing account changing its shell — the case prediction used to cover);
   - `user::Present::new("app").gid("app")` with no such group: check mode → `would change` with `app` in the diff; real mode → today's refusal, verbatim;
   - the same pair for `.groups([..])` and `user::Membership`;
   - `systemd::Enabled` on a unit `systemctl` cannot find: check mode → `would change`; real mode → today's refusal;
   - the four former mailbox ops apply correctly from the diff alone (existing apply tests, adapted).
6. **A fresh-host dry run walks to the end**, at tier 3: one new integration test in `crates/rustible-std/tests/` that builds a dry `Ctx` over a stock `debian:12` container (the pattern `it_user_busybox.rs` uses) and runs `group::Present` → `user::Present.gid(..)` → `ssh::authorized_keys::Present` → `user::Membership`, asserting every step reports `would change` and none fails; then the same steps through a real `Ctx`, asserting `changed` then `ok`. That is the scenario the registry was built for, proven without it.
7. **Docs say the rule once each**: USING_RUSTIBLE §15 and CLAUDE.md each state "a would-change step has no output in check mode; a prerequisite another step could create is verified when the run acts" in their own words, with no paragraph on how to make an op predict.
8. **DECISIONS.md and PROGRESS.md** carry the entries; every `Reverse:` names the commit-level undo.

## 5. Out of scope

- Any change to what `--check` reports (`would change`, diffs, the summary
  line) or to the mutation guard in `check`.
- `apt::Latest`'s cache refresh under `--check`.
- Making `Applied<T>` return the *current* state of an existing resource
  under `--check` (Ansible registers `uid`/`home` for an existing user even
  in check mode). Rejected: it is the *pre*-change value presented where the
  playbook expects the post-change one, a different lie from a prediction and
  a third notion of output to explain.

## 6. Verification (2026-09-24, on the branch before commit)

Against the acceptance criteria in section 4:

1. **Nothing left.** The grep is empty for `predicted`, `change_predicting`,
   `would_create` and `note_would_create` in `crates`, `examples`,
   `docs/USING_RUSTIBLE.md`, `CLAUDE.md`, `docs/06_BUILD_PLAN.md` and
   `README.md`. `\bPlanned\b` matches only `ssh::authorized_keys::Planned`,
   the pure result of `plan_present`/`plan_absent`, which predates the
   registry and is unrelated to it; left as is. Three comments still contain
   the word "predict" as history ("back when there were predictions").
2. **The shape.** `Change { diff }`; `Applied<T>` has no `predicted`;
   `System` has no planned list; `Plan::change(diff)` is the only
   constructor.
3. **`make` green**: `cargo fmt --all --check`, `cargo clippy --workspace
   --all-targets -- -D warnings`, `cargo test --workspace` (392 tests in
   `rustible-std`, 78 in `rustible-sdk`, 33 in `rustible-github`, plus the
   CLI's), `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
   --lib`, and `cargo build --manifest-path examples/workspace/Cargo.toml`.
4. **`make integration` green** (docker, `--no-fail-fast`): every `it_*`
   binary passed. One container test had to change: `it_authorized_keys`'s
   `a_dry_run_of_a_first_provision_does_not_fail` read `created_dir` off a
   dry step, which is exactly the read the rule forbids; it now asserts
   `!is_available()` and the diff.
5. **The rule at tier 2**, one test each, all present: the would-change
   output pin (`file/line.rs`
   `a_would_change_step_has_no_output_in_check_mode`, and `user.rs`'s
   existing-account shell change through a dry `Ctx`); `user::Present`
   `.gid("app")`, `.gid(3000)`, `.groups([..])` and `user::Membership`
   (missing group, missing user) under both modes, refusals verbatim;
   `systemd` `Enabled`/`Disabled`/`Running`/`Stopped` on `not-found` under
   both modes; the former mailbox ops apply from the diff alone (`hostname`
   `apply_reads_the_previous_kernel_name_itself`, `sysctl`
   `apply_runs_sysctl_w_only_when_the_diff_says_so`, `http`
   `attrs_only_change_applies_without_downloading`, `copy`, `directory`,
   `group::Absent`, `apt::Absent`/`Latest`, `brew::Absent`, `archive`).
6. **A fresh-host dry run walks to the end**, at tier 3, in
   `crates/rustible-std/tests/it_user_group.rs` rather than a new file (one
   file per musl build, so cases go into an existing one): a dry `Ctx` over
   `debian:12`/`ubuntu:24.04` runs `group::Present` → `user::Present` with
   `.groups(..)` → `authorized_keys::Present::for_user_name` → `Membership`
   → `group::Present.gid(4343)` → `user::Present.gid("rustible-dry2")`, every
   step `would change`, none with an output, the last diff naming
   `group=rustible-dry2`; then the same steps through the real `Ctx` refuse
   (`does not exist`, `does not exist in /etc/passwd`) and nothing was
   created. The real half — group, user, keys for that user, membership —
   is the rest of the same test, `changed` then `ok`.
7. **Docs say the rule once each**: `docs/USING_RUSTIBLE.md` §9 and §15,
   `CLAUDE.md` (the op-writing bullets and the testing trap), `README.md`,
   `docs/06_BUILD_PLAN.md` §4; no "so `--check` can predict" remains.
8. **DECISIONS.md** carries six `[CHECK-MODE]` entries with `Reverse:`
   clauses; **PROGRESS.md** has the resume line.

One decision taken during implementation and recorded in DECISIONS.md:
`systemd::Disabled` and `systemd::Stopped` on a `not-found` unit report
`would change` under `--check` rather than `Satisfied`, for the same reason
`Enabled` and `Running` do.

One correction folded in before it shipped: the first draft of the vision
prose said a real run "calls `check` again before `apply`", the claim
`[ISSUE-40] 2026-09-18` had already corrected. The prose now says what is
true — a real run's `check` runs with check mode off and takes the refusal;
a dry run's plan never reaches `apply`.
