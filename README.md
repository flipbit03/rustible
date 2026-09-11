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
    let cadu = ctx.step(
        "user cadu exists",
        user::Present::new("cadu")
            .shell("/bin/zsh")
            .groups(["docker"]),
    )?;

    ctx.step(
        "cadu's ssh keys are installed",
        authorized_keys::Present::for_user(&cadu).keys([MY_KEY]),
    )?;

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
those values feed the operations after it. `cadu` here is an `Account`, with
`uid`, `gid`, `home` and `groups` as real fields — which is why the next step
takes `&cadu` rather than repeating the username and guessing the home
directory. `cfg` is a `CopyReport`, and its `changed` is a `bool`, so a
conditional reload is an ordinary `if` instead of a handler wired up by a
`notify` string. All of it is checked at compile time: a misspelled field or
a wrong type fails the build.

## Why

Ansible expresses logic in YAML: conditionals are `when:` strings evaluated as
Python, iteration is a `loop:` key, and values are Jinja templates rendered
into whitespace-sensitive markup. None of it is type-checked, and mistakes
surface at run time, on a host, partway through.

Its execution model ships a Python module to the target for every task
(AnsiballZ), so every machine you manage needs a compatible interpreter and
whatever libraries the modules import.

Rustible keeps the parts that work — desired state, idempotence, readable runs
— and changes those two things.

| Ansible | Rustible |
|---|---|
| YAML tasks, Jinja templates | Rust functions, the compiler |
| `when:` strings | `if` |
| handlers and `notify` | `if step.changed { ... }` |
| loops with `item` | `for` |
| `register` + `set_fact` | the value the step returns |
| Python on every target | one static binary, nothing preinstalled |
| `--check` support per module | `check` is half of every op's definition |

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
A Mac cannot be a target, because the operations speak apt, systemd and
`/etc/passwd`; Rustible refuses it by name rather than failing later.

Targets need nothing installed. The playbook arrives as one static musl
binary.

## Install

```sh
cargo install rustible-cli
```

That gives you the `rustible` binary, and it needs **rustup and clang** on
your machine only.

Clang is there because Rustible's TLS provider compiles a small amount of C.
Most systems already have it:

```sh
sudo apt install clang        # Debian, Ubuntu; dnf, pacman and apk all have it
xcode-select --install        # macOS: the command line tools ship clang
```

You do not add Rust targets by hand. Rustible probes your hosts, works out
which architectures the run needs, and installs any missing ones with rustup
before it builds:

```
  installing rust target aarch64-unknown-linux-musl
```

To see what a machine can do before relying on it:

```sh
rustible toolchain check      # what this machine can build for, and how
```

Rustible carries musl's libc headers itself and sets the compiler flags, so
there is no cross-gcc, no zig, no docker and no sysroot to install. If clang
is missing, `rustible init` says so and `rustible playbook run` refuses with
the package name rather than failing inside a build script. Clang is only
needed to reach an architecture other than your own: an x86_64 Linux box with
`gcc` can build for itself.

## Five minutes

```sh
mkdir infra && cd infra && git init
rustible init                                # Cargo.toml, build.rs, src/, hosts.kdl, rustible.toml
rustible playbook create playbooks/hello.rs  # a scaffolded playbook targeting this machine
```

`rustible init` writes a Cargo package. `src/main.rs` and
`build.rs` are generated shims you rarely open: the build script finds every
file under `playbooks/` carrying the attribute and registers it, so adding a
playbook is adding a file.

Describe your machines in `hosts.kdl` (KDL, not YAML: nesting without
indentation traps). `init` starts you with this machine:

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
validated: **[docs/INVENTORY.md](docs/INVENTORY.md)**.

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
keys, and `keys_to_user`, a helper that runs it and
`ssh::authorized_keys::Present` as two visible steps:

```rust
let r = keys_to_user(ctx, "flipbit03", "cadu")?;
```

Both are built on `rustible-sdk`, and so is yours. An op is two functions:

```rust
impl Op for MyOp {
    type Output = MyReport;

    fn check(&self, sys: &System) -> Result<Plan<MyReport>> { /* decide */ }
    fn apply(&self, sys: &System, change: Change<MyReport>) -> Result<MyReport> { /* execute */ }
}
```

Everything an op does to the machine goes through the `System` handle it is
given. That is what lets the same op be tested against a fake filesystem in
microseconds and against real distributions in containers.

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

## Developing

`CLAUDE.md` is how to work in this repository — the rules, the layout, how to
write an operation, and which of the four testing tiers a given test belongs
in. `docs/DEVELOPING.md` is the per-platform setup for the ones that need a
machine.

```sh
make                # fmt, clippy, unit and fake tests, rustdoc, example workspace
make integration    # operations against real distributions, in Docker
make vm-up          # a Debian guest matching this host's architecture
make vm-test        # a playbook against that guest, twice; the second run must change nothing
```

Tests are in four tiers, each seeing something the one below it cannot: pure
functions, ops against a `Fake` backend, ops against real distributions in
containers, and a playbook against a real virtual machine over SSH. The last
one exists because a container shares the host kernel and has no pid 1, so it
cannot honestly test a `/proc/sys` write, a real init system, a real `sudo`,
or the transport itself. All four run in CI, the machine tier on both
x86_64 and aarch64.

Every change goes through a pull request, and eight CI jobs must be green.

## Design

| document | what it is |
|---|---|
| [`docs/01_VISION.md`](docs/01_VISION.md) | the contract: architecture, the playbook model, check mode, the error model |
| [`docs/INVENTORY.md`](docs/INVENTORY.md) | the `hosts.kdl` reference |
| [`docs/DEVELOPING.md`](docs/DEVELOPING.md) | per-platform setup for the tests that need a machine |
| [`CLAUDE.md`](CLAUDE.md) | how to work in this repository |
| [`docs/plan/DECISIONS.md`](docs/plan/DECISIONS.md) | every decision made while building |
| [`docs/plan/PROGRESS.md`](docs/plan/PROGRESS.md) | what is built |

## License

MIT or Apache-2.0, at your option.
