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

Every operation returns a typed struct, and those values feed the steps after
it: `cfg.changed` is a `bool`, so the reload is an `if`, not a handler. A
misspelled field or a wrong type fails the build.

## Why

Rustible keeps the parts of Ansible that work — desired state, idempotence,
readable runs — and changes what does not.

| Ansible | Rustible |
|---|---|
| YAML tasks, Jinja templates | Rust functions, the compiler |
| `when:` strings | `if` |
| handlers and `notify` | `if step.changed { ... }` |
| loops with `item` | just use `for` :-) |
| `register` + `set_fact` | just use the value the step returns |
| Python on every target | one static binary, nothing preinstalled |

## Supported platforms

Rustible runs **from** a controller and manages **targets**.

| | x86_64 | aarch64 |
|---|---|---|
| **Controller** — Linux | yes | yes |
| **Controller** — macOS | yes (Intel) | yes (Apple silicon) |
| **Target** — Linux, any libc | yes | yes |
| **Target** — macOS | yes (basic support) | yes (basic support) |
| **Target** — BSD | not yet (planned) | not yet (planned) |
| **Target** — Windows | nope | no way |

Targets need nothing installed. A playbook arrives on the target machine as one static binary, that's it.

## Install

```sh
cargo install rustible-cli
```

On the machine you run `rustible` from (the controller) you need:

- rustup (https://rustup.rs/)
- a C compiler (`cc`, `gcc` or `clang`)
- `curl`

Rustible also uses zig (for playbook cross-compilation), but it is installed automatically if not already present.


## Point your agent at this

Rustible is new, so LLMs have no prior knowledge of it. Paste this into your coding agent's session for a quick bootstrap:

```
Rustible is a Rust-based replacement for Ansible. Read
https://github.com/flipbit03/rustible/blob/main/docs/USING_RUSTIBLE.md
to understand how to write playbooks and operate it, then help me with my
infrastructure.
```

## Five minutes

```sh
mkdir infra && cd infra && git init
rustible init                                # Cargo.toml, build.rs, src/, hosts.kdl, rustible.toml, README.md
rustible playbook create playbooks/hello.rs  # a scaffolded playbook targeting this machine
```

Describe your machines in `hosts.kdl` ([KDL format](docs/HOSTS_KDL_REFERENCE.md)).
`rustible init` starts you with your local machine only:

```kdl
host "local" connection="local"
```

and here's a more fleshed out example of a `hosts.kdl` file:

```kdl
defaults ssh_user="cadu" escalate="sudo"

group "web" {
    vars { nginx_workers 4 }
    host "web1" addr="10.0.1.11"
    host "web2" addr="10.0.1.12" {
        // host variable override
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

#[rustible::playbook(hosts = "vagrant", vars = Vars, escalate = true)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> { /* ... */ }
```

Every host's vars are validated against that struct **before** anything is
built or shipped, so a missing var fails in a second. `--var package=htop`
overrides the inventory.

## The model

- **One verb.** `ctx.step(name, op)` to run operations on a target machine.
- **Ops are desired state, named for it.** `apt::Present`, `apt::Absent`,
  `systemd::Enabled`, `user::Present`. Things that are genuinely actions get
  verbs and always report changed: `systemd::Restart`, `shell::Command`.
- **Every op defines both halves.** `check` decides what would change and
  produces the diff; `apply` executes that decision. Dry run and diff are not
  a per-module afterthought.
- **Outputs chain.** A step returns a typed value describing what it found or
  made, and later steps use it. In check mode a step that would change has
  no output yet, and reading it returns an error saying so rather than a
  guess.
- **Prerequisites are refused.** An op that manages a user does not create
  the group it references; it fails and names the op you wanted. For an
  account the run is creating, a dry run defers that refusal to the real run,
  since an earlier step may create the group — the same line Ansible draws.

## Operations and collections

A Rustible Operation (Op) is one desired state: `apt::Present`, `systemd::Enabled`,
`user::Absent`. A **Rustible collection** is a library of them — an ordinary Rust crate that depends on `rustible-sdk`. You add one with
`cargo add`. Contrasting with Ansible, there is no "galaxy" - it's just crates.

Two collections ship from this repository.

**`rustible-std`** — the operations a fleet needs, always available:

| module | ops |
|---|---|
| `apt` | `Present`, `Absent`, `Latest` — Debian and Ubuntu |
| `brew` | `Present`, `Absent` — Homebrew, on a mac or Linuxbrew, as the login user |
| `file` | `Copy`, `Directory`, `Symlink`, `Absent`, `Attrs`, `Line`, `Block` |
| `user`, `group` | `Present`, `Absent`, `Membership` |
| `ssh::authorized_keys` | `Present` (with `exclusive`), `Absent` |
| `systemd` | `Enabled`, `Disabled`, `Running`, `Stopped`, `Restart`, `Reload`, `DaemonReload` |
| `hostname`, `sysctl` | `Is`, `Present` |
| `http`, `archive` | `Download`, `Extracted` |
| `shell` | `Command` |

Every operation declares where it runs and refuses other platforms by name.
On macOS `file`, `shell`, `http`, `archive` and `brew` run, and
`ssh::authorized_keys` runs when given the account rather than a name to look
up; the rest refuse.

**`rustible-github`** — a small collection showing what a third-party one
looks like. It adds `rustible_github::UserKeys`, which fetches a GitHub user's public
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
