# M7 doc findings: fixing what the rustdoc pass found

**Branch:** `doc-findings`
**Scope:** the ten behaviour problems recorded under "Found while reading,
left alone" in `docs/plan/reports/M7-rustdoc.md`. Five fixed, five recorded
in `DECISIONS.md` with what closing them would take.

Every fix carries a test that fails against the code as it was. That was
verified by running each test before its fix and watching it fail, not by
assuming; the failure output for each is quoted below.

## 1. The summary under-counted warnings

Three places emit `Log { level: Warn }`, which every reporter prints as
`WARNING:`: `Ctx::warn`, `System::warn`, and the runtime's undeclared-var
notice. Only `Ctx::warn` bumped `Summary::warnings`. Two shipped ops warn
through `System::warn` (`archive.rs`, `apt.rs`), so a run could print
warnings on screen and close with `0` in the summary's warning column.

Counting was moved off the producers and onto the one point every event
passes. `event::WarnCounter` is a sink that wraps another sink, counts
`Log { level: Warn }` as it forwards, and `runtime::execute` wraps the real
sink in one and reads the count into the summary just before emitting
`Finished`. It wraps early enough to cover the runtime's own undeclared-var
warnings, which are emitted before the `Ctx` exists. `Ctx::warn` no longer
touches the counter.

A fourth producer written tomorrow is counted the day it is written, which
was the point of moving it. The cost is one `matches!` per event on a
`dyn` call that was already there.

`runtime::tests::every_warning_producer_reaches_the_summary` runs a playbook
that warns from all three places and asserts the summary agrees with the
number of warning frames. Against the old code:

```
assertion `left == right` failed: the summary counts every warning the operator saw
  left: 1
 right: 3
```

## 2. `Event::Failed.step` was never filled

`runtime::execute` sent `None`, so the renderer recovered the step name by
stripping the `` step `…`: `` prefix off the chain. A step name containing a
backtick followed by a colon splits at the wrong place: `` odd `: name ``
came out as `odd ` with `` name`: deeper `` as the cause.

The name is now attached where it is known. `Ctx::step`'s context layer is a
typed `error::StepFailed` instead of a formatted string, and `execute` reads
it back with `Error::step_failed()`. The rendered chain is unchanged, because
`StepFailed`'s `Display` produces the same text; the type exists only so the
name does not have to be parsed out of it.

`Error::step_failed` uses anyhow's own `downcast_ref`, not the crate's
`Error::downcast_ref`. The crate's walks `source()` chains, which is what
finds a `CmdFailed` nested under an `IoAt`; anyhow's walks context layers,
which is what a context layer needs. Neither finds what the other finds, so
both are kept and the new method is one line naming the one type it looks
for. This was checked rather than assumed: a probe with the crate's
`downcast_ref` returned `None` for a context value.

`split_step` in the renderer keeps the string parsing, now reached only for
an error that never went through `ctx.step` (a panic, a `bail!` in the
playbook body). When the field is filled it drops the layer's own text so the
step name is not printed twice, and it does that without knowing the
wording: it strips `` step `<name>` `` and then a leading `": "`, so a
cancelled step's `` step `x` not applied: cancelled `` keeps the words that
say what happened.

Two tests, both failing before:

```
runtime::tests::failed_frame_carries_the_step_name
  left: None
 right: Some("odd `: name")

render::tests::a_step_name_with_a_backtick_needs_the_frames_own_field
  left: (Some("odd `: name"), "step `odd `: name`: deeper")
 right: (Some("odd `: name"), "deeper")
```

## 3. `Line`'s reported line number: the finding is half right

**The finding as written does not reproduce.** `Line::check`'s satisfied
branch takes the first line *equal* to the desired one while `plan_line`
takes the first line matching `matching`, and those two indices cannot
differ. The branch is only reached when the line `matching` selected already
equals the desired text; a regex that matches that line matches every
identical line before it, so the first match is at or before the first equal
line, and the first equal line is at or before the first match. They are the
same index. The check-mode half of the test below passes against the old
code.

**The neighbouring bug is real.** `Line::apply`'s satisfied branch, reached
when the file changed between `check` and `apply` so that nothing needs
writing, reported `check`'s *predicted* `line_no`, or `0` when there was no
prediction. That prediction describes the file as it was before whatever
changed it, so with duplicates it names a different line, and it is the one
case where the report is a straight guess.

Both branches and the planner now go through one `selected()` helper holding
the predicate once, which is what would have stopped the two readings from
being written differently at all.

`file::line::tests::a_satisfied_step_reports_the_line_matching_selected`,
against the old code:

```
assertion `left == right` failed
  left: 99
 right: 1
```

`99` is the stale prediction the test plants; `1` is where the line actually
is. Only the report was ever affected. File content is correct either way.

## 4. `Elevated::call` no longer string-matches an error message

It detected an oversized helper response with
`e.to_string().contains("exceeds limit")`, matching text written in
`protocol.rs`. Nothing pinned the coupling, so rewording that message would
have silently degraded the friendly refusal, which names the file, both
limits and the identity, back into the raw framing error.

`protocol::FrameTooLarge` is now a typed signal carried inside the
`io::Error` that `read_frame` returns, in the shape the SDK already uses for
`CmdFailed` and friends, and `Elevated::call` recognises it by downcast. The
message text did not change: it is `FrameTooLarge`'s `Display` now, so
nothing an operator reads moved.

This one needs its evidence stated carefully, because a behavioural test of
the refusal passes both before and after: the string match works, right up
until the message changes. What fails against the old code is the coupling
itself. Rewording `protocol.rs`'s message to "frame of {len} bytes is over
the ceiling" and running
`backend::elevated::tests::an_oversized_response_gets_the_friendly_refusal`:

```
thread ... panicked at crates/rustible-sdk/src/backend/elevated.rs:1056:
frame of 67108865 bytes is over the ceiling
```

With the typed detection in place the same reworded message passes. The
reword was then reverted; the shipped text is the original.

The test drives a fake helper that answers with a length prefix one byte over
the ceiling, so it runs in microseconds instead of pushing 48 MB through a
pipe.

## 5. `Status::Skipped` removed

Nothing emitted a `StepFinished` carrying it, and two renderers kept live
arms for it. It was removed rather than made reachable.

A skip never reaches an op. `Ctx::skip` neither checks nor applies, so there
is no verdict to report; it emits `Event::StepSkipped`, which carries the
playbook's reason in the place a status would sit. Making the variant
reachable would mean emitting a second, statusless `StepFinished` after every
`StepSkipped` that every reporter would then have to suppress, to give a
skip a word it already has.

No frame in existence carries it, since nothing has ever emitted it, so
removing it cannot break an older peer's stream in the direction that
matters. `PROTOCOL_VERSION` is unchanged for that reason.

`event::tests::every_status_has_an_emitter` matches exhaustively on `Status`
and names what emits each variant, so a future variant with no producer stops
compiling. Against the old code, with the variant present, that is exactly
what happens:

```
error[E0004]: non-exhaustive patterns: `event::Status::Skipped` not covered
```

## Recorded, not fixed

Each has a `[M7-doc]` entry in `docs/plan/DECISIONS.md` giving the reason and
the work closing it would take. In short:

| Finding | Why it stays | What closing it takes |
|---|---|---|
| 2, `Down::Start.verbosity` dead on the receiving end | Filtering in the orchestrator is defensible: a run can be re-rendered at a verbosity chosen after the fact | Either delete the field (incompatible `Start` change, `PROTOCOL_VERSION` bump) or filter at the source and lose frames the renderer may want |
| 6 and 8, public items that cannot be named | Adding names to the public API, which this branch is scoped out of | Two `pub use` lines, plus deciding whether `TEXT_DIFF_LIMIT` and `content_diff` are API at all (they read as `pub(crate)`) |
| 9, dead `addr` arm in `resolve_checked` | The loop is correct for every other name; excluding `addr` couples it to a parse rule in another file and would stop checking `addr` if that rule were relaxed | Exclude the name, or assert the invariant here |
| 10, `CmdFailed::status` of `-1` for a signalled process | `CmdFailed` is serialized in the `Failed` frame | Widen the field to `Option<i32>` plus a signal, which is an incompatible protocol change and a `PROTOCOL_VERSION` bump |

## What turned out to be more than it looked

**Finding 1 could not be fixed at the producer.** The obvious patch is to
bump the counter in `System::warn` too, and it would have been wrong: the
counter lives in `Ctx`'s shared state and `System` is built before any `Ctx`
exists, which is also why the runtime's own undeclared-var warnings, emitted
before the `Ctx`, could never have reached it that way. Moving the count to
the sink is what made all three reachable at once.

**Finding 3 was one bug, not the one reported.** See section 3.

**Finding 4 needed a demonstration, not a test.** A test of the refusal
passes against the old code, so on its own it is not evidence. The evidence
is the reword experiment.

## Verification

Full output in `docs/plan/logs/M7-doc-findings-done.txt`. All commands exit 0.

| Command | Result |
|---|---|
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | 561 passed, 0 failed |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib` | clean |
| `cargo build --manifest-path examples/workspace/Cargo.toml` | clean |
| `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --tests` | 15 test binaries, all passed |

554 workspace tests before this branch, 561 after: seven added, none changed.
`deny(missing_docs)` is on in every library crate, so the new public items
(`error::StepFailed`, `Error::step_failed`, `protocol::FrameTooLarge`) carry
docs, and `WarnCounter` is `pub(crate)`.
