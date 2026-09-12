# Operating Rustible

Everything needed to create a Rustible workspace, write playbooks, describe
machines, and run them. Written for someone — or something — with no prior
exposure to Rustible. If you are an AI agent, read this once, end to end,
before writing a playbook.

Describes current Rustible, and the docs.rs links point at the latest release.
If you are pinned to an older version, read that crate's own docs instead —
`cargo doc -p rustible-std --no-deps --open` renders exactly what you have.

Rustible is new and is not in any model's training data. Nothing here can be
guessed from experience with Ansible, and several things that look like
Ansible behave differently. Those are flagged **⚠️**.

**Contents**

1. [What Rustible is](#1-what-rustible-is)
2. [If you know Ansible](#2-if-you-know-ansible)
3. [Install](#3-install)
4. [The workspace](#4-the-workspace)
5. [The CLI](#5-the-cli)
6. [Describing machines: `hosts.kdl`](#6-describing-machines-hostskdl)
7. [Writing a playbook](#7-writing-a-playbook)
8. [`Ctx`: everything a playbook can do](#8-ctx-everything-a-playbook-can-do)
9. [Reading what a step returns](#9-reading-what-a-step-returns)
10. [Facts](#10-facts)
11. [Variables](#11-variables)
12. [Escalation](#12-escalation)
13. [Operations](#13-operations)
14. [Running, and reading the output](#14-running-and-reading-the-output)
15. [Check mode](#15-check-mode)
16. [When something goes wrong](#16-when-something-goes-wrong)
17. [Writing your own operation](#17-writing-your-own-operation)

---

## 1. What Rustible is

**An Ansible substitute written in Rust.**

A configuration management tool: you describe the state a machine should be
in, and Rustible works out what differs and changes only that. Same job as
Ansible, same ideas — desired state, idempotence, dry runs — with playbooks
that are compiled code rather than YAML.

A playbook is an ordinary Rust file. Running one compiles it to a static musl
binary for the target's architecture, copies it over SSH, and runs it **on the
target**. The binary streams results back. Targets need nothing installed: no
Python, no agent, no runtime.

That last point drives most of the design. There is no module library on the
target to call into, so everything a playbook does is compiled into it.

## 2. If you know Ansible

The concepts carry over. The syntax does not.

| Ansible | Rustible |
|---|---|
| `tasks:` in YAML | statements in a Rust `fn` |
| `when: condition` | `if condition { ... }` |
| `loop:` / `with_items:` | `for x in ... { ... }` |
| `register:` then `{{ result.x }}` | the value the step returns |
| handlers and `notify:` | `if step.changed { ... }` |
| `become: true` | `escalate = true`, or `ctx.as_root()` |
| `hosts: all` | ⚠️ **there is no `all`** — see §6 |
| `ignore_errors` / `failed_when` | match on the `Result` — see §14 |
| a `roles/` entry, reused | a function in `src/lib.rs` — see §7 |
| `include_tasks:`, used once | statements in the playbook — see §7 |
| `--tags` / `--skip-tags` | separate playbooks, or an `if` on a var |
| custom facts | `ctx.sys()` — see §10 |
| module docs on docs.ansible.com | [docs.rs/rustible-std](https://docs.rs/rustible-std) |

⚠️ **Do not transliterate a YAML playbook.** Ansible's `set_fact` and
`delegate_to` have no equivalent because Rust already has `let` and — for
delegation — a separate playbook. `include_role` and `include_tasks` are both
function calls in principle, but they are not the same thing in practice: see
"Where the code goes" in §7 before you decide what becomes a function.

## 3. Install

On the machine you run Rustible *from* (the controller):

```sh
cargo install rustible-cli
```

That needs `rustup` and `clang` present. Managed machines need nothing.

Rust targets are installed automatically: Rustible probes your hosts, works
out which architectures are needed, and runs `rustup target add` itself.

Check a machine before relying on it:

```sh
rustible toolchain check
```

**Controllers:** Linux x86_64, Linux aarch64, macOS on Apple silicon.
**Targets:** Linux x86_64 and aarch64, any libc. Not macOS, Windows or BSD.

## 4. The workspace

A **Rustible workspace** is a Cargo package with a particular shape: your
playbooks, your inventory, and the two generated shims that tie them together.
To create one:

```sh
rustible init infra && cd infra
```

That writes:

```
infra/
├── Cargo.toml         # depends on rustible, rustible-std
├── rustible.toml      # workspace config: which inventory file
├── hosts.kdl          # the machines (§6)
├── build.rs           # generated shim: finds playbooks/. Do not edit.
├── README.md          # yours: says what this is, and links this guide
├── .gitignore         # /target and /.rustible
├── .cargo/
│   └── config.toml    # musl cross-linking via rust-lld. Do not edit.
├── src/
│   ├── main.rs        # generated shim. Do not edit.
│   └── lib.rs         # yours: what more than one playbook needs
└── playbooks/         # one file per playbook
```

Add whatever else you need — it is a normal Cargo package. A `files/`
directory beside `playbooks/` is the usual place for content you
`include_str!` into a playbook; `init` does not create one because it does not
know whether you want it.

Things worth knowing:

- **Adding a playbook is adding a file** under `playbooks/`. `build.rs` scans
  the directory and registers every file carrying `#[rustible::playbook]`.
  Nothing else needs updating.
- **`src/main.rs` and `build.rs` are generated shims.** Do not edit them. If
  they drift after an upgrade, `rustible init --refresh .` rewrites exactly
  those two.
- **`src/lib.rs` is yours**, for what more than one playbook needs. A function
  taking `&mut Ctx` is the unit of reuse below a collection. Playbooks reach it
  by the package name, so in a workspace named `infra` that is
  `use infra::my_helper;`. It is *not* where a playbook's steps live by
  default — see "Where the code goes" in §7.
- **It is a normal Cargo package.** `cargo add` a dependency, use any crate.

⚠️ `rustible init` refuses only if one of the files it *generates* is already
there, so running it inside an existing git clone is fine. `README.md`,
`.gitignore` and `playbooks/.gitkeep` are not in that set: the README is
written only when the directory has none and is never overwritten, and
`.gitignore` is appended to. Your own `README.md` and `LICENSE` are left
exactly as they are.

The generated `README.md` exists to answer "what is this directory?" for
whoever opens the repository next — including an agent that has never seen
Rustible. It names the project and links this guide, which is enough to work
from cold. It is yours once written; edit it freely, and `init --refresh`
does not touch it.

## 5. The CLI

```sh
rustible init [DIR]                     # create a workspace
rustible playbook list                  # what this workspace holds
rustible playbook create playbooks/x.rs # scaffold one
rustible playbook run <PLAYBOOK>        # build, ship, run
rustible inventory show <HOST>          # one host, fully resolved
rustible inventory check                # the inventory, and every playbook's vars
rustible toolchain check                # what this machine can build for
```

`<PLAYBOOK>` is either a path (`playbooks/site.rs`) or the name (`site`).
⚠️ That name is the **file** under `playbooks/`, not a group. Examples here
use a playbook called `site` targeting a group called `web`, so the two are
telling apart; if you name them the same, `rustible playbook run web` is still
the playbook.
⚠️ A path is resolved against your **current directory**, not the workspace,
so the name form is safer from a script.

Useful flags on `playbook run`:

| flag | effect |
|---|---|
| `--check` | change nothing; report what would change (§15) |
| `-v` | show diffs and facts |
| `-vv` | show every command the run executes |
| `--limit <HOSTS>` | comma-separated hosts or groups, narrowing the playbook's own `hosts` |
| `--var k=v` | override an inventory var; JSON-looking values parse as JSON |
| `--json` | raw event stream instead of the rendered run |
| `--escalate-password-env VAR` | name of an env var holding the sudo password |

Global flags, valid on either side of the subcommand:

| flag | effect |
|---|---|
| `--workspace <DIR>` | the workspace root; default is to walk up from the cwd |
| `--inventory <FILE>` | use this inventory instead of the workspace's |

Exit codes, which matter if you script this:

| code | meaning |
|---|---|
| `0` | success |
| `1` | refused before touching anything — a bad inventory, a missing var, a build failure |
| `2` | a host or step failed — **something may have been changed** |
| `3` | the command line, the playbook name, or the hosts it names could not be resolved |

⚠️ `1` and `2` are the important distinction: `1` means the fleet was not
touched, `2` means it is in a state you have to go and look at.

## 6. Describing machines: `hosts.kdl`

Full reference: **[docs/HOSTS_KDL_REFERENCE.md](HOSTS_KDL_REFERENCE.md)**.

The shape:

```kdl
defaults ssh_user="deploy" escalate="sudo"

host "laptop" connection="local"

group "web" {
    vars { nginx_workers 4 }
    host "web1" addr="10.0.1.11"
    host "web2" addr="10.0.1.12"
}

group "db" {
    host "db1" addr="10.0.2.11"
}

group "production" {
    members "web" "db"        // a group of groups
}
```

### Host parameters

Seven, and no others. Set any of them on a `host`, on a `group`, or on
`defaults`; the nearest wins.

| parameter | meaning | default |
|---|---|---|
| `addr` | hostname or address to connect to | none — **required** unless `connection="local"` |
| `connection` | `"ssh"` or `"local"` | `ssh` |
| `ssh_user` | account ssh logs in as | your username |
| `port` | ssh port | `22` |
| `escalate` | `"sudo"`, `"doas"` or `"none"` | `sudo` |
| `escalate_user` | account to escalate to | `root` |
| `ssh_args` | extra arguments for `ssh` | none |

```kdl
host "db1" addr="10.0.2.50" port=2222 ssh_user="pgadmin"
```

A host can also carry its own `vars` block, which is how one machine overrides
its group:

```kdl
group "web" {
    vars { nginx_workers 4 }
    host "web1" addr="10.0.1.11"
    host "web2" addr="10.0.1.12" {
        vars { nginx_workers 8 }      // this host only
    }
}
```

Numbers are bare (`port=2222`), strings are quoted, booleans are `#true` /
`#false`.

### Four things that catch people out

- ⚠️ **There is no implicit `all` group.** `hosts = "all"` fails with
  ``no host or group named `all` `` unless you define one. Ansible's most common
  idiom does not exist here. Either define `group "all" { members ... }` or
  name a real group.
- ⚠️ **`escalate` on a host is a *method*** (`"sudo"`, `"doas"`, `"none"`),
  while `escalate` on a playbook is a **bool**. Same word, two things (§12).
- ⚠️ **`addr` cannot be inherited.** It is host-only; setting it on a group or
  on `defaults` is a load error.
- ⚠️ **`ssh_args` does not merge.** The nearest level that sets it wins whole.

### SSH, and secrets

There is no `ssh_key` parameter. Rustible shells out to your `ssh`, so it uses
your agent, your `~/.ssh/config` and your known-hosts exactly as a manual
`ssh` would. Anything else goes through `ssh_args`:

```kdl
host "jump" addr="10.0.0.2" {
    ssh_args "-i" "/home/me/.ssh/deploy_ed25519" "-o" "IdentitiesOnly=yes"
}
```

If `ssh user@host` works from your shell, Rustible works.

There is no vault. Keep secrets out of `hosts.kdl` and bring them in at run
time with `ctx.local_secret(path)` (§8). ⚠️ The redaction is a property of the
`Secret` type, not of everything you do with it: `as_str()` hands back the
plaintext, and whatever you then log or pass as an argument is on you.

Validate before running:

```sh
rustible inventory check          # parses, and checks every playbook's vars
rustible inventory show web1      # resolved values, with where each came from
```

## 7. Writing a playbook

The whole skeleton:

```rust
use rustible::prelude::*;
use rustible_std::apt;

#[rustible::playbook(hosts = "web", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    ctx.step("nginx installed", apt::Present::new(["nginx"]))?;
    Ok(())
}
```

⚠️ **`authorized_keys` is nested**: it is `rustible_std::ssh::authorized_keys`,
so the import is `use rustible_std::ssh::authorized_keys;`.
`use rustible_std::authorized_keys;` does not compile.

⚠️ **Two import lines, and you need both.** `rustible::prelude::*` gives you
`Ctx`, `Result`, `Facts`, `bail!`, `ensure!` and the `Op` machinery. It does
**not** give you any operations — those come from `rustible_std`, named
module by module. Forgetting the second line is the most common first error.

`#[rustible::playbook]` takes exactly three options:

| option | type | meaning |
|---|---|---|
| `hosts` | string, **required** | a host name or a group name from the inventory |
| `vars` | a type | the `#[rustible::vars]` struct this playbook needs (§11) |
| `escalate` | bool | run every step escalated (§12) |

The function signature is fixed:

```rust
fn main(ctx: &mut Ctx) -> Result<()>                  // without vars
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()>      // with vars = Vars
```

⚠️ It is called `main` but it is not a `main`. The macro rewrites it; the
generated `src/main.rs` is the real entry point.

⚠️ **Playbooks cannot call each other.** The build script mounts each one
under a mangled module name (`__pb_nginx_a1b2c3`), and a run compiles only the
playbook you selected, so `crate::other_playbook::helper()` does not compile.
Shared code goes in one of two places, both ordinary Rust:

```rust
use infra::render_config;   // src/lib.rs, by the package name in Cargo.toml
mod helpers;                // a sibling file, declared inside this playbook
```

One playbook per file, and the path under `playbooks/` is the playbook's
name — `playbooks/web/nginx.rs` is the playbook `web/nginx`, which is what
`rustible playbook list` prints and what `run` takes.

### Where the code goes: the playbook, or `src/lib.rs`

**A playbook is the readable record of what happens to a machine.** Someone
opening it — a colleague at 2am, you in six months, an agent asked to change
one thing — should be able to read down the page and see the run. Keeping that
true is the one rule here. Everything below follows from it.

So **steps go in the playbook by default**, and `src/lib.rs` is earned, not
assumed. A helper there is a good thing when it is pulling its weight; it is a
cost when it is only moving code out of sight.

A helper has earned `lib.rs` if **any one** of these is true:

- **A second playbook needs it.** The plain reuse case, and the strongest one.
- **It is called more than once with different arguments** — `nginx_site(ctx,
  "api")` and `nginx_site(ctx, "www")`. That is reuse inside one playbook.
- **It names a policy a reader already understands** — `harden_ssh`,
  `join_tailnet`. The name raises the level; the reader does not need to open
  it to know what happened.

And the check that catches the common mistake: **called once, takes nothing
but `ctx`, and its name just restates its own body — inline it.**
`setup_nginx()` holding install-then-config-then-enable, inside a playbook
whose whole purpose is nginx, is that. `harden_ssh()` is not.

Both of these read well, and both are fine:

```rust
// Everything inline. The default, and never wrong.
ctx.step("nginx installed", apt::Present::new(["nginx"]))?;
ctx.step("nginx.conf", file::Copy::from_str(CONF).to(NGINX_CONF))?;
ctx.step("nginx enabled", systemd::Enabled::new("nginx").now(true))?;

// Helpers, with the playbook still saying what happens.
ctx.step("base packages", apt::Present::new(["curl", "ufw"]))?;
harden_ssh(ctx)?;                       // used by four playbooks
deploy_app(ctx, "v1.2.3")?;             // parameterised
ctx.step("firewall enabled", systemd::Enabled::new("ufw").now(true))?;
```

This one does not:

```rust
#[rustible::playbook(hosts = "web", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    setup_web_server(ctx)               // ⚠️ what does this do? open a second file
}
```

⚠️ **The failure to avoid is a playbook that no longer tells you anything.**
It happens a step at a time: each extraction looks tidy, and at the end the
playbook is two lines and the machine's actual behaviour lives somewhere else.
If the playbook has become shorter than the list of things it does, extract
less. If a playbook is *long* rather than unreadable, group it with
`ctx.section(..)` (§8), which keeps the steps on the page.

**Converting an Ansible repository?** Ansible already draws this line, and you
can follow it mechanically:

| Ansible | where it goes |
|---|---|
| a `roles/` entry used by several playbooks | a `lib.rs` function — this is the reuse it was for |
| `include_tasks: subtasks/10_foo.yaml`, used once | **inline it into the playbook** |

`include_tasks` is how a YAML file gets split when it grows, not a reuse
mechanism. A Rust file does not have YAML's length problem, so those subtasks
become ordinary statements in the playbook, in the order they ran. Turning
each one into a `lib.rs` function reproduces the file-splitting without the
reason for it, and costs you the readable playbook.

There is a third place, between the two: **a sibling file declared inside the
playbook**, `mod helpers;` (§7 above). That is for bulk that belongs to one
playbook and nothing else — a long config template, a parser. It keeps the
material out of the way without pretending it is shared.

| where | what belongs there |
|---|---|
| the playbook | the steps, in order — the default |
| `ctx.section(..)` | grouping a long playbook, without moving anything out of it |
| `mod helpers;` | bulk private to this one playbook |
| `src/lib.rs` | a second caller, a parameterised repeat, or a named policy |
| a collection crate (§13) | reuse across workspaces or teams |

## 8. `Ctx`: everything a playbook can do

`ctx` is the single handle. Every method:

```rust
// Run an operation. The only verb. Returns what the op produced (§9).
ctx.step(name: impl Into<String>, op: impl Op) -> Result<Applied<O::Output>>

// Record that a step was deliberately not run. Shows as `skipped`.
ctx.skip(name: impl Into<String>, reason: impl Into<String>)

// Group steps under a heading in the output. The closure returns a Result,
// so end it with `Ok(())` and use `?` on the section itself.
ctx.section(name, |ctx| -> Result<T> { ... }) -> Result<T>

// What this machine is (§10).
ctx.facts() -> &Facts
ctx.host()  -> &HostInfo
ctx.check_mode() -> bool

// Output.
ctx.log(msg)     // a line in the run
ctx.warn(msg)    // a warning, counted in the summary
ctx.debug(msg)   // only with -vv

// Escalation, per step (§12).
ctx.as_root()          -> Ctx
ctx.as_user(name: &str) -> Ctx
ctx.as_escalated()     -> Ctx

// Files, between controller and target.
ctx.local_file(path) -> Result<PathBuf>   // upload a controller file, get its remote path
ctx.local_secret(path) -> Result<Secret>  // read into memory, never to disk
ctx.fetch(remote, local_dest) -> Result<()>  // download from the target

// The escape hatch: run something no op covers.
ctx.sys() -> &System
```

⚠️ Both reach a file that lives on the **controller** — the binary runs on the
target and cannot see your disk otherwise — but they differ. `local_file`
uploads it and gives you a path on the target. `local_secret` streams the bytes
into memory and gives you a `Secret`: nothing is written to the target's disk,
it is zeroized on drop, and its `Debug` prints `Secret(<n> bytes)`.

## 9. Reading what a step returns

`ctx.step(...)` returns `Applied<T>`, where `T` is that operation's output
type. It carries:

| field / method | what |
|---|---|
| `.changed` | `bool` — did this step change anything |
| `.diff` | `Option<Diff>` — what changed |
| `.elapsed` | `Duration` |
| `.output() -> Result<&T>` | the output, or an error if unavailable |
| `.into_output() -> Result<T>` | the same, by value |
| `.is_available() -> bool` | whether an output is there to read |

`Applied<T>` also **derefs to `T`**, which is why you can use it directly:

```rust
let account = ctx.step("user", user::Present::new("deploy"))?;
ctx.log(format!("uid {}", account.uid));          // Deref
ctx.step("keys", authorized_keys::Present::for_user(&account).keys([KEY]))?;
```

⚠️ **That `Deref` panics in check mode** when the step would have changed and
the op could not predict its output. `user::Present` on an account that does
not exist yet predicts only when it can know every field: `.uid()`, `.gid()`,
and a shell it can determine — on BusyBox (Alpine) there is no default to read,
so `.shell()` is needed there too. A `.gid("name")` naming a group an earlier
step would create also does not predict, because there is no gid yet. A playbook that must survive `--check`
guards it:

```rust
if account.is_available() {
    ctx.step("keys", authorized_keys::Present::for_user(&account).keys([KEY]))?;
}
```

This is the single most common way a playbook that works fails under
`--check`. See §15.

## 10. Facts

Gathered once per host, before the first step.

```rust
let f = ctx.facts();
if f.package_manager == Pm::Apt { ... }
if f.distro == Distro::Alpine { ... }
ensure!(f.is_root, "this playbook needs root");
```

| field | type |
|---|---|
| `os` | `Os` |
| `distro` | `Distro` |
| `distro_version` | `String` |
| `arch` | `Arch` |
| `kernel` | `String` |
| `hostname` | `String` |
| `package_manager` | `Pm` |
| `init` | `Init` |
| `cpus` | `u32` |
| `memory_mb` | `u64` |
| `user` | `String` — the process's user |
| `is_root` | `bool` |

The enums are `Os`, `Distro`, `Arch`, `Pm`, `Init`, all in the prelude, and
each ends in an `Other(String)` carrying whatever Rustible read and did not
recognise — a tuple variant, so it is `Distro::Other(_)` in a pattern. Match
rather than comparing `distro_version` strings, and get the variant names from
the source (see "Finding an operation" below):

```rust
match &f.distro {
    Distro::Debian | Distro::Ubuntu => { /* apt */ }
    Distro::Alpine => { /* apk */ }
    other => bail!("unsupported distribution: {other:?}"),
}
```

**There are no custom facts.** That list is all of them, and there is no
`setup` module or local-facts directory. To answer anything else, ask the
machine yourself through `ctx.sys()`:

```rust
let out = ctx.sys().cmd("findmnt").args(["-no", "FSTYPE", "/"]).run()?;
let root_fs = String::from_utf8_lossy(&out.stdout).trim().to_string();
if root_fs == "btrfs" { /* ... */ }
```

`ctx.sys()` is the same handle operations use — `read_to_string`, `exists`,
`stat`, `cmd`, `write_atomic`, `mkdir_all`. Reach for it to *decide* something;
prefer an operation to *change* something, because `sys` writes have no diff
and no idempotence.

⚠️ **`facts.is_root` is about the process, not the identity a step will run
as.** It is `getuid() == 0`. Under `escalate = true` the whole binary runs
behind `sudo`, so it is true — but under `ctx.as_root()` in an otherwise
unescalated playbook it stays **false**, even though that step will run as
root. To ask whether the work will have privilege, use `ctx.sys().is_root()`,
which accounts for a handle that switched identity. The `user` field has the
same shape: it is the process's user, not necessarily the step's.

## 11. Variables

Declare what a playbook needs; the inventory fills it in.

```rust
#[rustible::vars]
struct Vars {
    /// This doc comment becomes the description in error messages.
    package: String,
    #[default = 4]
    workers: u32,
    #[default = false]
    update_cache: bool,
    #[default = "nginx"]
    service: String,                   // strings too
    optional_note: Option<String>,     // Option = may be absent
}

#[rustible::playbook(hosts = "web", vars = Vars)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
    ctx.step("pkg", apt::Present::new([vars.package.as_str()]))?;
    Ok(())
}
```

In `hosts.kdl`:

```kdl
group "web" {
    vars { package "nginx"; workers 8 }
}
```

Rules:

- A field with no `#[default]` and no `Option` is **required**. Every targeted
  host is checked **before anything is built or shipped**, so a missing var
  fails in a second.
- ⚠️ **Vars are flat.** Scalars, lists and enums; there is no nested `vars`
  tree. A map field fails to compile; a struct field compiles and is then
  rejected before the run with `var 'x' is an object; vars are flat scalars,
  lists, or enums`.
- **Precedence, nearest wins:** `--var` on the command line, then the host,
  then the closest group, then outer groups, then the workspace-wide
  `vars { }` block. ⚠️ `defaults` is for **parameters only** — a `vars` block
  inside it is a load error, not a silently ignored one. So a host setting `nginx_workers 8` beats
  its group's `4`, which is the ordinary case. `rustible inventory show <host>`
  prints the winner and where it came from.
- `--var package=htop` overrides the inventory for one run. A value that looks
  like JSON is parsed as JSON.
- `rustible inventory check` validates every playbook against every host it
  targets, without running anything.

## 12. Escalation

⚠️ Four things are called "escalate". They are different.

| where | what it is | example |
|---|---|---|
| playbook attribute | **bool** — escalate every step | `#[rustible::playbook(hosts = "web", escalate = true)]` |
| inventory parameter | **method** — how to escalate | `escalate="sudo"` (or `"doas"`, `"none"`) |
| inventory parameter | **who** to become | `escalate_user="deploy"` (default `root`) |
| `Ctx` method | escalate **one step** | `ctx.as_root().step(...)` |

Per-step escalation, which is what you want when only one thing needs root:

```rust
#[rustible::playbook(hosts = "web")]              // not escalated by default
fn main(ctx: &mut Ctx) -> Result<()> {
    ctx.step("my own scratch file", file::Line::in_path("/tmp/notes")
        .create(true)
        .set("hello"))?;
    ctx.as_root().step("nginx", apt::Present::new(["nginx"]))?;
    Ok(())
}
```

⚠️ A host with `escalate="none"` under a playbook with `escalate = true` runs
**unescalated**. It does not refuse and it does not fail, so a host you
deliberately exempted stays exempt — but you are only told if you ask. The
note is an orchestrator note, which means it appears under `-v` and not in a
default run, and **never under `--json`** (§14). If something automated has to
know, read the host's `escalate` out of the inventory rather than watching the
output for it.

**`escalate = true` needs passwordless `sudo`.** The orchestrator launches the
whole binary behind `sudo -n`, which fails outright if a password is wanted,
before the playbook starts.

`--escalate-password-env` does **not** change that. It supplies a password to
the per-step helper — `ctx.as_root()` and `ctx.as_user()` — inside an
otherwise unescalated playbook:

```sh
RUSTIBLE_SUDO=hunter2 rustible playbook run site --escalate-password-env RUSTIBLE_SUDO
```

The password travels in the start frame, never on a command line, and is
zeroized after use. ⚠️ `doas` cannot take a password this way at all and is
refused; configure it for passwordless use.

## 13. Operations

An operation is one desired state. The naming rule:

- **A type per state, named for the state**: `apt::Present`, `apt::Absent`,
  `systemd::Enabled`, `user::Absent`. There is no `state:` parameter.
- **Things that are genuinely actions get verbs** and always report changed:
  `systemd::Restart`, `systemd::Reload`, `shell::Command`.

### Finding an operation

This guide does not list the operations or their signatures. It would be out
of date the day an operation is added, and a stale signature is worse than no
signature. Three ways to get the real thing, in the order to reach for them:

**1. The source, on your own disk.** `rustible-std` is an ordinary dependency
of your workspace, so cargo has already vendored it:

```sh
ls ~/.cargo/registry/src/*/rustible-std-*/src/        # the modules
grep -n "^pub struct" ~/.cargo/registry/src/*/rustible-std-*/src/systemd.rs
grep -n "pub fn" ~/.cargo/registry/src/*/rustible-std-*/src/apt.rs
```

Every public item is documented with a `///` comment above it, including what
it refuses to do and why. This is the fastest and most reliable route, and it
works offline.

**2. Rendered docs, locally.** From your workspace:

```sh
cargo doc -p rustible-std --no-deps --open
```

**3. [docs.rs/rustible-std](https://docs.rs/rustible-std/latest/rustible_std/)**,
the same thing on the web.

The modules carrying operations are `apt`, `archive`, `file`, `group`,
`hostname`, `http`, `shell`, `ssh`, `sysctl`, `systemd` and `user`. What is in each is a `grep` away; what
you cannot get that way — which shape to reach for, and what bites — is the
rest of this section.

### Operations from elsewhere

`rustible-std` is not the only source of operations. A **collection** is an
ordinary crate that depends on `rustible-sdk` and exports operations, so you
add one the way you add any dependency:

```sh
cargo add rustible-github
```

```rust
use rustible_github::github_ssh_keys_to_user;

// fetches flipbit03's public keys from GitHub and puts them in cadu's
// authorized_keys, as two visible steps
let keys = github_ssh_keys_to_user(ctx, "flipbit03", "cadu")?;
ctx.log(format!("{} key(s) added", keys.added.len()));
```

`rustible-github` is the worked example of a collection, and small enough to
read end to end if you are writing your own. It also exports `rustible_github::UserKeys` for the fetch on its own,
`GithubSshKeysToUser` for the configurable form of the helper above, and a
`Fetch` trait so the HTTP call can be faked in tests.

There is no galaxy and no roles path: collections are crates, `cargo add`
finds them, and `cargo` pins the version.

### The two builder shapes

⚠️ Most operations are `::new(...)` and are complete immediately. Operations
that need **two** things — a source and a destination, or a file and its
content — return a builder that finishes by naming the second:

| operation | starts with | finishes with |
|---|---|---|
| `file::Copy` | `from_str` / `from_bytes` / `from_local_path` | `.to(dest)` |
| `http::Download` | `get(url)` | `.to(dest)` |
| `archive::Extracted` | `from_path(src)` | `.to(dest)` |
| `file::Symlink` | `at(link)` | `.pointing_to(target)` |
| `file::Line` | `in_path(file)` | `.set(line)` |
| `file::Block` | `in_path(file)` | `.set(block)` |
| `ssh::authorized_keys::*` | `for_user(&acct)` / `for_user_name(n)` | `.keys([..])` |
| `user::Membership` | `of(&acct)` / `of_name(n)` | `.in_group(&g)` / `.in_group_named(n)` |

⚠️ **The constructor is not always `new`.** `file::Directory`, `file::Absent`
and `file::Attrs` start at `at(path)`; `user::Existing` is `named(..)`. When in
doubt, grep the module rather than guessing:

```sh
grep -n "pub fn" ~/.cargo/registry/src/*/rustible-std-*/src/file/directory.rs
```

⚠️ A finisher turns the builder into the operation, so anything on the
*operation* is chained **after** it — `archive::Extracted::from_path(x).to(d).creates(m)`,
not `.creates(m).to(d)`.

So:

```rust
// complete as written
apt::Present::new(["nginx", "curl"])
systemd::Enabled::new("nginx").now(true)
user::Present::new("deploy").shell("/bin/bash").groups(["docker"])
user::Present::new("deploy").gid("www-data")     // primary group

// needs a finisher
file::Copy::from_str(CONF).to("/etc/nginx/nginx.conf").mode(0o644)
http::Download::get(URL).to("/tmp/x").checksum("sha256:...")
file::Line::in_path("/etc/hosts").set("10.0.0.1 db")
```

⚠️ Builder methods **consume `self`**, so chain them. Assigning a half-built
op to a variable and calling a method on it will not compile the way you
expect.

⚠️ **The apt operations take a list where most others take one name.**
`apt::Present::new(["nginx"])` — with the brackets, even for a single package.
`apt::Present::new("nginx")` does not compile. Every other constructor
(`user::Present::new`, `systemd::Enabled::new`, `group::Present::new`,
`hostname::Is::new`) takes a single name, which is exactly why the apt one
catches people.

⚠️ **`file::Copy::from_local_path` reads a path on the *target*, not on your
machine.** "Local" means local to the running playbook, which is the target.
For a file that lives on the controller, either embed it at compile time or
stream it:

```rust
// embedded in the binary
file::Copy::from_bytes(include_bytes!("../files/nginx.conf")).to("/etc/nginx/nginx.conf")

// or uploaded at run time, returning its path on the target
let staged = ctx.local_file("files/nginx.conf")?;
file::Copy::from_local_path(staged).to("/etc/nginx/nginx.conf")
```

### A worked example

```rust
use rustible::prelude::*;
use rustible_std::{apt, file, group, systemd, user};

const CONF: &str = include_str!("../files/nginx.conf");

#[rustible::playbook(hosts = "web", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ensure!(f.package_manager == Pm::Apt, "{} is not apt-based", f.hostname);

    ctx.step("nginx installed", apt::Present::new(["nginx"]))?;

    // The primary group first: `user::Present` resolves `.gid(3000)` against
    // /etc/group and refuses if nothing carries it. See the traps below.
    ctx.step("deploy group", group::Present::new("deploy").gid(3000))?;

    let deploy = ctx.step(
        "deploy user",
        user::Present::new("deploy")
            .uid(3000)                    // uid and gid so --check can predict
            .gid(3000)
            .shell("/bin/bash")
            .groups(["www-data"]),
    )?;
    ctx.log(format!("deploy is uid {}", deploy.uid));

    let conf = ctx.step(
        "nginx.conf",
        file::Copy::from_str(CONF).to("/etc/nginx/nginx.conf").mode(0o644),
    )?;

    ctx.step("nginx enabled", systemd::Enabled::new("nginx").now(true))?;
    if conf.changed {
        ctx.step("nginx reloaded", systemd::Reload::new("nginx"))?;
    }
    Ok(())
}
```

### Traps worth knowing before you use these

**`file::Line` without `matching` leaves the old line behind.** With only
`.set(..)`, the match is whole-line equality, so it matches the value you are
setting and nothing else. That is idempotent — the second run reports `ok` —
but the line it was supposed to replace is still there, above the new one, and
for a config file read top to bottom the stale one may be the one that wins.
The file gains a line every time the *value* changes, not every time you run.
Give it a regex that matches the **old** value too:

```rust
file::Line::in_path("/etc/ssh/sshd_config")
    .matching(r"^#?\s*PasswordAuthentication\b")
    .set("PasswordAuthentication no")
```

It also refuses a file that is not there — `/etc/x does not exist (use
.create(true) to create it)` — because editing a line in a file you have not
created is more often a typo than an intention. `.create(true)` before `.set`
opts in.

Only the **first** match is rewritten, so a file that already has two such
lines keeps the second. And ⚠️ an invalid regex **panics**; it is not an error
you can catch.

**Prerequisites are refused, never created.** Nothing creates a group, a home
directory's parent, or a destination directory as a side effect. Sequence them:

```rust
ctx.step("docker group", group::Present::new("docker"))?;
let app = ctx.step("app user", user::Present::new("app").groups(["docker"]))?;
// `app` is only readable when the op could predict it, so guard for --check
if app.is_available() {
    ctx.step("ssh dir", file::Directory::at(app.home.join(".ssh"))
        .owner(app.uid, app.gid).mode(0o700))?;
    ctx.step("keys", authorized_keys::Present::for_user(&app).keys([KEY]))?;
}
```

Most of these refuse in `check`, before anything is touched, naming the
operation you wanted — `user`, `group` and `authorized_keys` all do.
⚠️ `file::Copy` is the exception: it looks only at the destination, so a copy
into a directory that does not exist fails at `apply`, after earlier steps have
already changed the machine. Create the directory first.

⚠️ Sequencing them correctly still does not make the pair above pass
`--check` on a machine where `.ssh` is missing — `authorized_keys` stats the
directory rather than consulting what the previous step promised. A real run
converges; see §15.

**`archive::Extracted` re-extracts every run unless you give it `.creates()`.**
Nothing about a directory full of files tells it the archive was already
unpacked, so without a marker it reports `changed` every time — which is
otherwise the signature of a bug. Give it a path that exists only after a
successful extraction, relative to the destination:

```rust
ctx.step("app extracted", archive::Extracted::from_path(TARBALL)
    .to("/opt/app")                // .to() first: it produces the operation
    .creates("bin/app")            // then report ok when /opt/app/bin/app exists
    .owner(svc.uid, svc.gid))?;
```

⚠️ Note the order. `.to()` is the finisher that turns the builder into the
operation, and `.creates()` and `.owner()` are on the operation, so they come
**after** it. The same is true of `http::Download`.

⚠️ **Set ownership on the extraction, not only on the directory.** `.owner()`
on `Extracted` chowns **every extracted file and directory**. Creating
`/opt/app` owned by a service account and then extracting into it as root
leaves a correctly-owned directory full of root-owned files, and the run
reports `ok` for the directory step while it happens.

**A new unit file is invisible until systemd re-reads.** Writing
`/etc/systemd/system/x.service` and then `systemd::Enabled::new("x")` fails
with `not found`. Put a `systemd::DaemonReload::new()` between them.

**`owner` takes numeric ids**, never names: `.owner(uid: u32, gid: u32)`. Read
them off a `user::Account` returned by an earlier step.

**`user::Present` has two different group settings.** `.gid(..)` is the
**primary** group and takes a gid, a group name, or a `group::Group` from an
earlier step. `.groups([..])` is the **supplementary** list, and `.append(bool)`
says whether it adds to the current set or replaces it. Both require the groups
to exist already.

**`apt` is the only package manager with operations.** `Pm::Dnf`, `Pm::Apk`
and the rest exist as *facts* with nothing behind them, so guard:

```rust
ensure!(f.package_manager == Pm::Apt, "{} uses {:?}", f.hostname, f.package_manager);
```

**`ctx.as_root()` returns a `Ctx` by value**, and `step` takes `&mut self`.
Chain on the temporary, or bind it `mut`:

```rust
ctx.as_root().step("x", op)?;           // fine
let mut root = ctx.as_root();           // also fine
let root = ctx.as_root(); root.step(..) // E0596: cannot borrow as mutable
```

### Putting a variable into a config file

There is no template operation and no Jinja. A playbook is Rust, so build the
string in Rust and copy it:

```rust
#[rustible::vars]
struct Vars {
    #[default = 4]
    nginx_workers: u32,
}

const NGINX_CONF: &str = "\
user www-data;
worker_processes {workers};
events { worker_connections 768; }
";

#[rustible::playbook(hosts = "web", vars = Vars, escalate = true)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
    let conf = NGINX_CONF.replace("{workers}", &vars.nginx_workers.to_string());
    ctx.step("nginx.conf", file::Copy::from_str(&conf).to("/etc/nginx/nginx.conf"))?;
    Ok(())
}
```

`format!` works too, and for anything bigger, add a real template crate to
your workspace with `cargo add` — it is an ordinary Cargo package, and the
rendering happens on the target inside your playbook binary.

⚠️ `from_str` takes `&str`, so a `String` you built is passed as `&conf`.

### When no operation fits

`shell::Command` runs something Rustible has no operation for:

```rust
// a program and its arguments -- no shell, no word splitting, no globbing
ctx.step("regenerate", shell::Command::new("update-initramfs").arg("-u"))?;

// when you genuinely need a shell
ctx.step("pipeline", shell::Command::sh("apt-get update && apt-get upgrade -y"))?;
```

⚠️ `new` takes a **program name**, not a command line.
`shell::Command::new("apt-get update")` tries to execute a binary with a space
in its name. `sh(script)` is `new("/bin/sh").args(["-c", script])`.

**Making it idempotent.** On its own a command reports `changed` every run,
because Rustible cannot know what it did. Three builders fix that, and the
first two are Ansible's by the same names:

| builder | behaviour |
|---|---|
| `.creates(path)` | skip, reporting `ok`, if `path` **exists** |
| `.removes(path)` | skip, reporting `ok`, if `path` **does not exist** |
| `.changed_when(\|out\| ...)` | run, then decide from the output whether it counted as a change |

```rust
// runs once; afterwards the marker exists and the step reports ok
ctx.step("bootstrap the database",
    shell::Command::sh("initdb -D /var/lib/pg").creates("/var/lib/pg/PG_VERSION"))?;

// only tears down when there is something to tear down
ctx.step("drop the socket",
    shell::Command::new("rm").arg("/run/app.sock").removes("/run/app.sock"))?;

// runs every time, but only counts as changed when it did something
ctx.step("sync",
    shell::Command::new("rsync").args(["-a", "src/", "dst/"])
        .changed_when(|out| !out.stdout.is_empty()))?;
```

⚠️ `.changed_when` still **runs** the command — there is no other way to know
what it would do — so it makes the report honest, not the command safe. Use
`.creates` / `.removes` when you want the command skipped entirely. And in
check mode a `changed_when` step reports `would change` either way, because
the command does not run there.

Other useful builders: `.cwd(dir)` (Ansible's `chdir`), `.env(k, v)`,
`.stdin(bytes)`.

Prefer a real operation where one exists: `shell::Command` has no diff, and
`.creates` is a marker rather than a description of the state you wanted.

## 14. Running, and reading the output

```sh
rustible playbook run site
```

Each step is a line:

```
[web1]  nginx installed ............................ changed    nginx=1.24.0-2
[web1]  deploy user ................................ ok
[web1]  nginx.conf ................................. changed    +3 -1 lines
```

`-v` adds diffs and facts. `-vv` adds every command executed.

Every run ends with one row per host:

```
host    ok  changed  would change  skipped  failed  warnings
web1     3        2             0        0       0         0
web2     3        2             0        0       0         0
```

A host that never got as far as running gets a `failed: <reason>` row instead
— a connect error, a build failure.

**Read the `changed` column on a second run.** An operation that reports
`changed` every time is not idempotent, and that is a bug worth reporting.

`--json` emits the raw event stream, one JSON object per line, for scripting.

### What a run costs

The first run for a given architecture compiles the playbook into a static
binary, which is the slow part and the only slow part. After that the binary
is cached by source hash: it is rebuilt only when the source changes, and
re-uploaded only when the target does not already have that exact binary, so
a run that changes nothing is dominated by the SSH round trip rather than by
cargo. Adding a second architecture adds one more build, not one more
per-run cost. `-v` prints the run's own timings — trust those over any number
here, since the build is your controller's CPU and nobody else's.

### When a step fails

`ctx.step(...)?` propagates, so by default the first failure stops that host.
To carry on regardless, handle the `Result` like any other:

```rust
if let Err(e) = ctx.step("optional thing", op) {
    ctx.warn(format!("skipping: {e}"));
}
```

⚠️ **That continues the playbook, but the step is still counted as failed**,
the host still reports `failed` in the summary, and the run still exits `2`.
There is no `ignore_errors` that makes a failure invisible — you can decide
what to do next, not whether it happened.

**Hosts run in parallel, and one failing does not stop the others.** Every
host runs to completion; the summary says which failed, and the process exits
`2`. There is no `serial:` or `any_errors_fatal:` — to roll a change out in
batches, use `--limit` and run it more than once.

⚠️ A `--limit` that selects nothing is an error, not an empty success, so a
typo cannot look like a clean no-op deploy.

**To see which machines a playbook would touch** before touching them, read
its `hosts` attribute and resolve that name against the inventory:

```sh
rustible inventory show web1    # one host, resolved, with the source of each value
```

`rustible inventory check` reports counts (`ok (4 hosts, 3 groups)`) rather
than a listing, so it tells you the file is sound, not who is in it.

`--check` also tells you, but it builds and connects first.

`--json` writes one object per line, each wrapping something in a `host` key:
protocol frames arrive as `{"host":..,"frame":{"Event":{"StepStarted":{..}}}}`,
and the orchestrator's own lines use `error`, `stderr`, `exit` or `fetched`
instead. Events are nested under `frame.Event.<Variant>`.

The two views differ in both directions. The stream carries step detail the
renderer summarises — and the renderer carries the orchestrator's **notes**,
which the stream has no member for at all: the binary upload, and the
`escalate="none"` note from §12. A script watching `--json` does not see them.
Read one run with `rustible playbook run x --json | head` before writing
against it.

## 15. Check mode

```sh
rustible playbook run site --check
```

Nothing is modified. Steps report `would change` instead of `changed`, with
the diff they would have applied.

Two things to know:

- ⚠️ **An output that cannot be honestly predicted is unavailable.** An op
  that would create a user does not invent a uid. Reading such an output
  fails — and via `Deref`, panics (§9). Guard with `.is_available()`.
- ⚠️ **`apt::Latest::update_cache(..)` refreshes the package lists even in
  check mode**, because its answer is read from them. That is the one place
  `--check` is not entirely read-only, and the run says so when it happens.
  Without `.update_cache(..)` — the default — a dry run writes nothing.

An op that *can* predict does: `apt::Present` knows the candidate version,
`file::Copy` knows the content it would write.

**A dry run mostly survives a fresh host.** A step does not refuse merely
because an earlier step's work has not happened yet: a `user::Present` whose
primary group an earlier `group::Present` would create is accepted, and a
`file::Copy` into a directory an earlier step would create is accepted. What
you do not get is *output* for steps that could not predict it, which is §9's
`is_available()` guard.

⚠️ **The exception is a step whose check asks a *tool* instead of asking
Rustible.** That cascade is a registry of what earlier steps announced they
would create, and only the ops that write to it can be read out of it. An op
that answers by shelling out sees the machine as it is now, not as the
playbook will leave it — so it refuses, and the dry run reports a failure for
something a real run would have handled.

`systemd::Enabled` is the one you will hit. Its check runs `systemctl
is-enabled`, which knows nothing about a unit that has not arrived yet,
whether it would come from a package:

```rust
apt::Present::new(["nginx"])            // then
systemd::Enabled::new("nginx")          // FAILED: unit `nginx` not found
```

or from the step immediately before it, which is the usual way to ship a
service:

```rust
file::Copy::from_str(UNIT).to("/etc/systemd/system/app.service")
systemd::Enabled::new("app")            // FAILED: unit `app` not found
```

That is a limit of the dry run rather than a fault in the playbook. Apply once
and `--check` is meaningful from then on.

It is the *state* operations that do this. `systemd::Enabled` and
`systemd::Running` have to look the unit up to know whether they are
satisfied, so they refuse when it is absent. The verbs — `systemd::Restart`,
`systemd::Reload` — never inspect anything, because they always report
changed, so they pass a dry run against a unit that does not exist yet. That
is why §13's `if conf.changed { ... Reload ... }` is fine under `--check`.

`ssh::authorized_keys` is the other one you will meet. It stats the `.ssh`
directory itself, so a `file::Directory` one line above that *would* create it
does not count — and the refusal says so rather than telling you to add the
step you already wrote:

```
FAILED at `keys`: /home/app/.ssh does not exist; ssh::authorized_keys does not
create it (vision 6.7). Under --check a directory an earlier step would create
is still reported missing, because this op stats the real filesystem. If a step
in this run creates it, the real run converges and there is nothing to fix; if
not, ensure it with file::Directory::at(..).mode(0o700).owner(..)
```

`user::Membership` naming a group an earlier `group::Present` would create is
*not* affected — it consults the registry and passes.

**`.changed` is `true` in check mode** when the step would have changed
something. So `if conf.changed { ... reload ... }` fires under `--check` too,
and the reload appears as its own `would change` line. The dry run shows you
the whole shape of the real run, conditionals included.

## 16. When something goes wrong

| symptom | cause |
|---|---|
| ``no host or group named `all` `` | there is no implicit `all` group (§6); exits 3 |
| `cannot find module or crate 'apt' in this scope` | missing `use rustible_std::apt;` (§7) |
| a step panics under `--check` only | `Deref` on an unavailable output (§9, §15) |
| `var X is not declared by this playbook` | the inventory sets a var the playbook's `Vars` does not declare; harmless, but usually a typo |
| ``missing required var `x` `` | a `Vars` field with no `#[default]` and no value in the inventory |
| `sudo: a password is required` | `escalate = true` needs passwordless sudo; the flag does not help there (§12) |
| ``no `clang` on PATH, and this playbook has to be built for …`` | install clang on the controller (§3) |
| a host is absent from the run | the playbook's `hosts` does not match it; check `rustible inventory show <host>` |

Useful first moves:

```sh
rustible inventory check                 # inventory + every playbook's vars
rustible inventory show web1             # what web1 resolves to, and from where
rustible playbook run site --check -v    # what would change, with diffs
rustible playbook run site -vv           # every command executed
```

## 17. Writing your own operation

If `rustible-std` has no operation for something, you have three options, in
increasing order of effort:

1. **`shell::Command`** — fine for a one-off, never idempotent.
2. **A helper function** in `src/lib.rs` that composes existing operations —
   once a second playbook needs it, or you are calling it repeatedly with
   different arguments (§7). Note the parameters: a helper worth having is one
   the caller configures.
   ```rust
   pub fn nginx_site(ctx: &mut Ctx, name: &str, conf: &str) -> Result<()> {
       ctx.step(format!("{name} config"), file::Copy::from_str(conf)
           .to(format!("/etc/nginx/sites-enabled/{name}")))?;
       Ok(())
   }
   ```
3. **A real operation**, which is a type implementing `Op`. That is two
   methods: `check` decides what would change and produces the diff, `apply`
   executes that decision. Everything it does to the machine goes through the
   `System` handle it is given, which is what makes it testable.

A collection is an ordinary crate that depends on `rustible-sdk` and exports
operations; `cargo add` it and use it, as in "Operations from elsewhere"
above. `rustible-github` is the worked example to copy the shape from.

To contribute an operation to `rustible-std` itself, read
[`CLAUDE.md`](../CLAUDE.md) — it covers the shape, where the tests go, and the
traps that make a test pass while proving nothing.

---

## Where else to look

| | |
|---|---|
| [docs.rs/rustible-std](https://docs.rs/rustible-std/latest/rustible_std/) | every operation's exact signature |
| [docs.rs/rustible-sdk](https://docs.rs/rustible-sdk/latest/rustible_sdk/) | `Ctx`, `System`, `Op`, facts |
| [docs/HOSTS_KDL_REFERENCE.md](HOSTS_KDL_REFERENCE.md) | the inventory format in full |
| [docs/01_VISION.md](01_VISION.md) | why it is built this way |
| [CLAUDE.md](../CLAUDE.md) | contributing to Rustible itself |
