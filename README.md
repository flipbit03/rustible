# Rustible

**Configuration management as real code. No more YAML hell.**

Rustible is an independent replacement for Ansible. Playbooks are ordinary Rust
files with typed operations and typed outputs, checked by the compiler and
completed by your editor. When you run one, Rustible compiles it into a static
binary for each target architecture, ships it over SSH, runs it there, and
streams `changed / ok / skipped / failed` progress back, with dry-run and diff
built in. Nothing needs to be installed on the target: no Python, no agent.

## Status

Design complete and validated by spikes; the real implementation is starting.
The crates on crates.io are placeholders reserving the names. Read the design:
[`docs/01_VISION.md`](docs/01_VISION.md).

## Crates

| crate | what |
|---|---|
| `rustible` | facade a playbook workspace depends on (`use rustible::prelude::*`) |
| `rustible-cli` | the `rustible` command (`cargo install rustible-cli`) |
| `rustible-sdk` | `Op`, `System`, `Ctx`, facts, protocol: what collections build on |
| `rustible-std` | the standard operations (files, users, packages, services, ...) |
| `rustible-macros` | `#[rustible::playbook]`, `#[rustible::vars]` |
| `rustible-build` | build-script helper that discovers playbook files |
| `rustible-github` | first collection: GitHub-backed operations |

## License

MIT or Apache-2.0, at your option.
