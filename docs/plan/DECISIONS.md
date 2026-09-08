# Decisions made without a human

Entries added by unattended runs under docs/07_UNATTENDED.md rule 2.3.
Format: `- [M<n>] <date> <decision>: <why>; <how to reverse>`.
Cadu reviews these; a decision he rejects is reversed in a follow-up task and
the vision doc is amended if the question deserved a real answer.
- [M1] 2026-09-08 `rustible init` must generate `[profile.dist]` and `.cargo/config.toml` (rust-lld per musl target) in every workspace: a detached workspace inherits neither from the repo; found when the orchestrator's dist build failed against `examples/workspace`. Reverse: none needed, this is a gap in the M4 brief, not a design change.
- [M1] 2026-09-08 `#[rustible::vars]` rejects maps and tuples syntactically; nested structs cannot be told from enums in a proc macro, so the orchestrator's vars validation (M2/M3) must reject schema properties of `type: object`. Reverse: add a `#[vars]`-side check if a reliable syntactic rule appears.
- [M1] 2026-09-08 `--var k=v` parses `v` as JSON when it parses, else as a string (numbers, bools, lists work without quoting). Reverse: make it string-only and require typed syntax.
- [M1] 2026-09-08 Vars deserialization does NOT use `deny_unknown_fields`: the host var bag is shared by all playbooks targeting the host (vision 10.3), so undeclared keys are legitimate; the runtime warns per undeclared key with a did-you-mean instead. Reverse: add `deny_unknown_fields` in the `#[vars]` expansion.
- [M1] 2026-09-08 Alternating `rustible playbook run` between two playbooks re-links the bin each time (one `selected` OUT_DIR, `rerun-if-env-changed`). Observation for M3: weigh a per-playbook artifact cache keyed by (playbook, triple, source hash) against duplicated dependency artifacts. Reverse: n/a, nothing built.
- [M1] 2026-09-08 `rust-version` raised from 1.85 to 1.88 because the code uses let-chains (stable in 1.88, edition 2024). Reverse: nest the `if let`s.
- [M1] 2026-09-08 Vars flatness is enforced on the generated JSON Schema (`vars::flatness_violations`, resolving `$ref`s) at `--describe` and at `Start`; the macro's syntactic map/tuple rejection is only an early hint. Supersedes the earlier M1 note that deferred this to M2/M3.
