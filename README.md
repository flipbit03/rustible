# Rustible

**Configuration management as real code. No more YAML hell.**

Rustible is an independent replacement for Ansible. A playbook is an ordinary
Rust file: typed operations, typed outputs, checked by the compiler and
completed by your editor. When you run one, Rustible compiles it into a static
binary for each target's architecture, ships it over SSH, runs it *there*, and
streams `changed / ok / failed` back with dry-run and diffs built in. The
target needs nothing installed: no Python, no agent, no runtime.

```rust
#[rustible::playbook(hosts = "web", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    let cadu = ctx.step("cadu exists", user::Present::new("cadu").shell("/bin/zsh").groups(["docker"]))?;
    ctx.step("cadu's keys", authorized_keys::Present::for_user(&cadu).keys([MY_KEY]))?;

    let cfg = ctx.step("nginx.conf", file::Copy::from_str(NGINX_CONF).to("/etc/nginx/nginx.conf").mode(0o644))?;
    ctx.step("nginx enabled", systemd::Enabled::new("nginx").now(true))?;
    if cfg.changed {
        ctx.step("nginx reloaded", systemd::Reload::new("nginx"))?;
    }
    Ok(())
}
```

`cadu` is a value. Its `uid`, `home` and `groups` are fields you can read in
the next step. `cfg.changed` is a bool, not a handler with a `notify` string.
A typo in a field name is a compile error, not a 40-minute run that fails on
the last host.

## Why

Ansible's YAML is a programming language that refuses to admit it: no types, no
functions, `when:` strings evaluated as Python, and Jinja templating over
whitespace-sensitive markup. Its execution model ships a Python module to the
target for every task (AnsiballZ) and needs a compatible interpreter there.
Rustible keeps what Ansible got right, desired state and idempotence and
readable runs, and throws out both of those problems.

| Ansible | Rustible |
|---|---|
| YAML tasks, Jinja templates | Rust functions, the compiler |
| `when:` strings | `if` |
| handlers and `notify` | `if step.changed { ... }` |
| loops with `item` | `for` |
| `register` + `set_fact` | the value the step returns |
| Python on every target | one static binary, nothing preinstalled |
| `--check` support per module | `check` is half of every op's definition |

## Install

**Two things: rustup and clang.** Nothing else, and nothing at all on the
machines you manage.

```sh
sudo apt install clang          # Debian/Ubuntu; dnf, pacman and apk all have it too
```

macOS already has it: the command line tools ship clang, so `xcode-select
--install` is the whole story there.

Clang is there because Rustible's TLS provider (`ring`) compiles a small amount
of C, and one clang cross-compiles for every architecture you might target.
Rustible carries musl's own libc headers and sets the compiler flags itself, so
there is no cross-gcc, no zig, no docker and no sysroot to install. If clang is
missing, `rustible init` says so and `rustible playbook run` refuses with the
package name rather than failing somewhere inside a build script. A build only
needs clang to reach an architecture other than your own: an ordinary x86_64
Linux box with `gcc` can build for itself.

Targets are added with rustup as you need them:

```sh
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
```

## Status

Under active construction and **not released yet**. The crates on crates.io
are `0.0.1` placeholders holding the names, so `cargo install rustible-cli`
does not give you a working tool today. Build from this repository instead:

```sh
git clone https://github.com/flipbit03/rustible && cd rustible
cargo install --path crates/rustible-cli    # the `rustible` command
```

A workspace built from a clone points at it:

```sh
rustible init --path-deps /path/to/rustible
```

When 0.1.0 ships, `cargo install rustible-cli` and a plain `rustible init` are
the whole install. `docs/plan/PROGRESS.md` tracks what is built; the design is
complete and vetted in `docs/01_VISION.md`.

## Five minutes

```sh
mkdir infra && cd infra && git init
rustible init                                # Cargo.toml, build.rs, src/, hosts.kdl, rustible.toml
rustible playbook create playbooks/hello.rs  # a scaffolded playbook targeting this machine
```

`rustible init` writes a Cargo package, not a config tree. `src/main.rs` and
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
        vars { nginx_workers 8 }     // host beats group
    }
}
```

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
built or shipped, so a missing var fails in a second, not halfway through a
run. `--var package=htop` overrides the inventory.

## The model

- **One verb.** `ctx.step(name, op)` runs everything. There is no separate
  "task" and "command" API.
- **Ops are desired state, named for it.** `apt::Present`, `apt::Absent`,
  `systemd::Enabled`, `user::Present`. Things that are genuinely actions get
  verbs and always report changed: `systemd::Restart`, `shell::Command`.
- **Every op defines both halves.** `check` decides what would change and
  produces the diff; `apply` executes that decision. Dry run and diff are not
  a per-module afterthought.
- **Outputs chain.** A step returns what it found or made. In check mode, an
  output an op cannot honestly predict is unavailable, and reading it says so
  loudly instead of handing you a plausible lie.
- **Prerequisites are refused, not invented.** An op that manages a user does
  not create the group it references; it fails and names the op you wanted.

## Operations

`rustible-std` ships the ops a real fleet needs:

| module | ops |
|---|---|
| `apt` | `Present`, `Absent`, `Latest` |
| `file` | `Copy`, `Directory`, `Symlink`, `Absent`, `Attrs`, `Line`, `Block` |
| `user`, `group` | `Present`, `Absent`, `Membership` |
| `ssh::authorized_keys` | `Present` (with `exclusive`), `Absent` |
| `systemd` | `Enabled`, `Disabled`, `Running`, `Stopped`, `Restart`, `Reload`, `DaemonReload` |
| `hostname`, `sysctl` | `Is`, `Present` |
| `shell` | `Command` |

## Collections are just crates

A collection is a normal crate that depends on `rustible-sdk` and implements
the `Op` trait. `rustible-github` is the worked example: it adds
`github::UserKeys::of("flipbit03")` and composes it with
`ssh::authorized_keys::Present`. To use someone's collection, `cargo add` it.
No galaxy, no roles directory, no path search order.

```rust
impl Op for MyOp {
    type Output = MyReport;

    fn check(&self, sys: &System) -> Result<Plan<MyReport>> { /* decide */ }
    fn apply(&self, sys: &System, change: Change<MyReport>) -> Result<MyReport> { /* execute */ }
}
```

Everything an op does to the machine goes through the `System` handle it is
given, which is what lets ops be tested against a fake filesystem in
microseconds and against real distros in containers.

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
| `rustible-sdk` | `Op`, `Ctx`, `System`, facts, protocol, testing |
| `rustible-std` | the standard operations |
| `rustible-macros` | `#[playbook]`, `#[vars]`, `#[integration_test]` |
| `rustible-build` | playbook discovery for the generated `build.rs` |
| `rustible-github` | the example collection |

## Design

`docs/01_VISION.md` is the contract: architecture, the playbook model, the
`System` handle, testing tiers, inventory and vars, check mode, error model.
`docs/plan/DECISIONS.md` records every decision made while building, each with
how to reverse it.

## License

MIT or Apache-2.0, at your option.
