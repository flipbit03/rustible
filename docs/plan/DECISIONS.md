# Decisions made without a human

Entries added by unattended runs under docs/07_UNATTENDED.md rule 2.3.
Format: `- [M<n>] <date> <decision>: <why>; <how to reverse>`.
Cadu reviews these; a decision he rejects is reversed in a follow-up task and
the vision doc is amended if the question deserved a real answer.
- [M1] 2026-09-08 `rustible init` must generate `[profile.dist]` and `.cargo/config.toml` (rust-lld per musl target) in every workspace: a detached workspace inherits neither from the repo; found when the orchestrator's dist build failed against `examples/workspace`. Reverse: none needed, this is a gap in the M4 brief, not a design change.
- [M1] 2026-09-08 `#[rustible::vars]` rejects maps and tuples syntactically; nested structs cannot be told from enums in a proc macro, so the orchestrator's vars validation (M2/M3) must reject schema properties of `type: object`. Reverse: add a `#[vars]`-side check if a reliable syntactic rule appears.
- [M1] 2026-09-08 `--var k=v` parses `v` as JSON when it parses, else as a string (numbers, bools, lists work without quoting). Reverse: make it string-only and require typed syntax.
