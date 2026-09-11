# workspace

A [Rustible](https://github.com/flipbit03/rustible) workspace. Rustible is a
replacement for Ansible: a playbook is an ordinary Rust file with typed
operations, compiled to a static binary, shipped over SSH and run on the
target. The targets need nothing installed — no Python, no agent, no runtime.

| | |
|---|---|
| `playbooks/` | one file per playbook |
| `hosts.kdl` | the machines this workspace manages |
| `src/lib.rs` | code shared between playbooks |
| `rustible.toml` | which inventory file to use |

```sh
rustible playbook list
rustible playbook run <name> --check   # dry run: diffs, no changes
rustible playbook run <name>
rustible inventory show <host>         # resolved values, and where each came from
```

`build.rs` and `src/main.rs` are generated. Do not edit them; `rustible init
--refresh .` rewrites them if they drift.

## Working on this with an agent

Point it at this one document:

```
https://github.com/flipbit03/rustible/blob/main/docs/USING_RUSTIBLE.md
```

It is the whole of how to write playbooks and operate Rustible — the
operations, the inventory format, check mode, escalation, and the traps. An
agent that reads it can work in this repository without any other context.
