# Rustible

**Configuration management as real code. No YAML.**

Rustible is an independent replacement for Ansible. A playbook is an ordinary
Rust file: typed operations, typed outputs, checked by the compiler and
completed by your editor. Running one compiles it into a static binary for
each target's architecture, ships it over SSH, runs it *there*, and streams
`changed / ok / failed` back, with dry run and diffs built in. The target
needs nothing installed: no Python, no agent, no runtime.

```rust
#[rustible::playbook(hosts = "web", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    ctx.step("nginx installed", apt::Present::new(["nginx"]))?;

    let cfg = ctx.step(
        "nginx.conf is current",
        file::Copy::from_str(NGINX_CONF)
            .to("/etc/nginx/nginx.conf")
            .mode(0o644),
    )?;

    ctx.step(
        "nginx is enabled and running",
        systemd::Enabled::new("nginx").now(true),
    )?;

    if cfg.changed {
        ctx.step("nginx reloaded", systemd::Reload::new("nginx"))?;
    }
    Ok(())
}
```

Every operation returns a typed struct describing what it found or made, and
those values feed the operations after it. `cfg` here is a `CopyReport`, and
its `changed` is a `bool`, so a conditional reload is an ordinary `if` instead
of a handler wired up by a `notify` string. `user::Present` returns an
`Account` with `uid`, `gid` and `home` as real fields, so the step that wants
a home directory is handed one rather than guessing it. All of it is checked
at compile time: a misspelled field or a wrong type fails the build.

## Why

Ansible expresses logic in YAML: conditionals are `when:` strings evaluated as
Python, iteration is a `loop:` key, and values are Jinja templates rendered
into whitespace-sensitive markup. None of it is type-checked, and mistakes
surface at run time, on a host, partway through.

Its execution model ships a Python module to the target for every task
(AnsiballZ), so every machine you manage needs a compatible interpreter and
whatever libraries the modules import.

Rustible keeps the parts that work — desired state, idempotence, readable runs
— and changes what does not.

| Ansible | Rustible |
|---|---|
| YAML tasks, Jinja templates | Rust functions, the compiler |
| `when:` strings | `if` |
| handlers and `notify` | `if step.changed { ... }` |
| loops with `item` | just use `for` |
| `register` + `set_fact` | the value the step returns |
| Python on every target | one static binary, nothing preinstalled |

## Supported platforms

Rustible runs **from** a controller and manages **targets**.

| | x86_64 | aarch64 |
|---|---|---|
| **Controller** — Linux | yes | yes |
| **Controller** — macOS (Apple silicon) | — | yes |
| **Target** — Linux, any libc | yes | yes |
| **Target** — macOS, Windows, BSD | no | no |

A Mac is a first-class controller: it cross-builds playbook binaries for both
Linux targets with the clang that Xcode's command line tools already provide.

Targets need nothing installed. The playbook arrives as one static musl
binary.

## Install

```sh
cargo install rustible-cli
```

That gives you the `rustible` binary. It needs **rustup and clang** on your
machine, and nothing on the machines you manage.

To see what a machine can do before relying on it:

```sh
rustible toolchain check      # what this machine can build for, and how
```

## Point your agent at this

Rustible is new, so an AI agent has no prior knowledge of it. Give it this and
it can create a workspace, write playbooks, manage an inventory and run them:

```
Rustible is a Rust-based replacement for Ansible. Read
https://github.com/flipbit03/rustible/blob/main/docs/USING_RUSTIBLE.md
to understand how to write playbooks and operate it, then help me with my
infrastructure.
```

[`docs/USING_RUSTIBLE.md`](docs/USING_RUSTIBLE.md) is written for a reader
with no exposure to Rustible: the workspace layout, the CLI, the inventory,
the playbook API, every operation, and the traps that catch people who expect
Ansible.

## Five minutes

```sh
mkdir infra && cd infra && git init
rustible init                                # Cargo.toml, build.rs, src/, hosts.kdl, rustible.toml, README.md
rustible playbook create playbooks/hello.rs  # a scaffolded playbook targeting this machine
```

`rustible init` writes a Cargo package. `src/main.rs` and
`build.rs` are generated shims you rarely open: the build script finds every
file under `playbooks/` carrying the attribute and registers it, so adding a
playbook is adding a file.

Describe your machines in `hosts.kdl` ([KDL format](docs/HOSTS_KDL_REFERENCE.md)).
`init` starts you with this machine:

```kdl
host "local" connection="local"
```

and a real fleet looks like:

```kdl
defaults ssh_user="cadu" escalate="sudo"

group "web" {
    vars { nginx_workers 4 }
    host "web1" addr="10.0.1.11"
    host "web2" addr="10.0.1.12" {
        // host beats group
        vars { nginx_workers 8 }
    }
}
```

Seven parameters, variables at three levels, groups of groups, and how it is
validated: **[docs/HOSTS_KDL_REFERENCE.md](docs/HOSTS_KDL_REFERENCE.md)**.

Fill in the playbook (`playbooks/hello.rs`):

```rust
use rustible::prelude::*;

#[rustible::playbook(hosts = "local")]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ctx.log(format!("{} is {:?} {} on {:?}", f.hostname, f.distro, f.distro_version, f.arch));
    Ok(())
}
```

Then:

```sh
rustible playbook list                            # what this workspace holds
rustible inventory check                          # inventory and every playbook's vars
rustible playbook run playbooks/hello.rs          # build, ship, run, render
rustible playbook run playbooks/hello.rs --check  # change nothing, show what would change
rustible playbook run playbooks/hello.rs -vv      # every command the run executed
```

A run ends with a table, one row per host:

```
host    ok  changed  would change  skipped  failed  warnings
local    1        1             0        0       0         0
web1     2        0             0        0       0         0
```

Vars come from the inventory, typed per playbook:

```rust
#[rustible::vars]
struct Vars {
    package: String,
    #[default = false]
    update_cache: bool,
}

#[rustible::playbook(hosts = "lab", vars = Vars, escalate = true)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> { /* ... */ }
```

Every host's vars are validated against that struct **before** anything is
built or shipped, so a missing var fails in a second. `--var package=htop`
overrides the inventory.

## The model

- **One verb.** `ctx.step(name, op)` runs everything. There is no separate
  "task" and "command" API.
- **Ops are desired state, named for it.** `apt::Present`, `apt::Absent`,
  `systemd::Enabled`, `user::Present`. Things that are genuinely actions get
  verbs and always report changed: `systemd::Restart`, `shell::Command`.
- **Every op defines both halves.** `check` decides what would change and
  produces the diff; `apply` executes that decision. Dry run and diff are not
  a per-module afterthought.
- **Outputs chain.** A step returns a typed value describing what it found or
  made, and later steps use it. In check mode an op that cannot predict a
  field leaves it unavailable, and reading it returns an error saying so
  rather than a guess.
- **Prerequisites are refused.** An op that manages a user does
  not create the group it references; it fails and names the op you wanted.

## Operations and collections

An operation is one desired state: `apt::Present`, `systemd::Enabled`,
`user::Absent`. A **collection** is a library of them — an ordinary Rust crate
that depends on `rustible-sdk` and implements its `Op` trait. You add one with
`cargo add`. There is no galaxy, no roles directory, no path search order.

Two collections ship from this repository.

**`rustible-std`** — the operations a fleet needs, always available:

| module | ops |
|---|---|
| `apt` | `Present`, `Absent`, `Latest` |
| `file` | `Copy`, `Directory`, `Symlink`, `Absent`, `Attrs`, `Line`, `Block` |
| `user`, `group` | `Present`, `Absent`, `Membership` |
| `ssh::authorized_keys` | `Present` (with `exclusive`), `Absent` |
| `systemd` | `Enabled`, `Disabled`, `Running`, `Stopped`, `Restart`, `Reload`, `DaemonReload` |
| `hostname`, `sysctl` | `Is`, `Present` |
| `http`, `archive` | `Download`, `Extracted` |
| `shell` | `Command` |

**`rustible-github`** — a small collection showing what a third-party one
looks like. It adds `github::UserKeys`, which fetches a GitHub user's public
keys, and `github_ssh_keys_to_user`, a helper that runs it and
`ssh::authorized_keys::Present` as two visible steps:

```rust
let r = github_ssh_keys_to_user(ctx, "flipbit03", "cadu")?;
```

Both are built on `rustible-sdk`, which is what you use to write your own
operations and collections.

## How a run works

1. Find the workspace (`rustible.toml`), read the inventory.
2. Ask the playbook about itself with a host-native build (`--describe`):
   which hosts, which vars, whether it escalates. Cached by source hash.
3. Validate every target host's vars. Abort before touching anything if one
   fails.
4. Connect to all hosts in parallel, learn each one's architecture.
5. One cargo build for every architecture in play, `--profile dist`
   (stripped, LTO, ~1.4 MB static musl binaries).
6. Upload to `~/.cache/rustible/bin/<name>-<sha256>` unless it is already
   there.
7. Run it, escalating if the playbook says so, and stream framed events back
   over the SSH channel: steps, diffs, commands, logs, the summary.

The playbook binary never opens a socket. SSH is the orchestrator's business.

## Crates

| crate | what |
|---|---|
| `rustible-cli` | the `rustible` command |
| `rustible` | the facade a playbook imports |
| `rustible-sdk` | everything you need to write your own collection: `Op`, `Ctx`, `System`, facts, protocol, testing |
| `rustible-std` | the standard operations |
| `rustible-macros` | `#[playbook]`, `#[vars]`, `#[integration_test]` |
| `rustible-build` | playbook discovery for the generated `build.rs` |
| `rustible-github` | a collection, and the worked example of one |

## Documentation

| document | what it is |
|---|---|
| [`CLAUDE.md`](CLAUDE.md) | how to work in this repository: the rules, writing an operation, the testing tiers and the `make` targets |
| [`docs/01_VISION.md`](docs/01_VISION.md) | the contract: architecture, the playbook model, check mode, the error model |
| [`docs/USING_RUSTIBLE.md`](docs/USING_RUSTIBLE.md) | operating Rustible: workspace, playbooks, operations, the CLI |
| [`docs/HOSTS_KDL_REFERENCE.md`](docs/HOSTS_KDL_REFERENCE.md) | describing your machines: parameters, variables, groups |
| [`docs/DEVELOPING.md`](docs/DEVELOPING.md) | per-platform setup for the tests that need a machine |
| [`docs/plan/DECISIONS.md`](docs/plan/DECISIONS.md) | every decision made while building |
| [`docs/plan/PROGRESS.md`](docs/plan/PROGRESS.md) | what is built |

## License

MIT or Apache-2.0, at your option.
