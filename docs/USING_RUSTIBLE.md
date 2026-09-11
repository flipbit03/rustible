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
| `--tags` / `--skip-tags` | separate playbooks, or an `if` on a var |
| custom facts | `ctx.sys()` — see §10 |
| module docs on docs.ansible.com | [docs.rs/rustible-std](https://docs.rs/rustible-std) |

⚠️ **Do not transliterate a YAML playbook.** Ansible's `set_fact`, `include_role`
and `delegate_to` have no equivalent because Rust already has `let`, function
calls, and — for delegation — a separate playbook.

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

A workspace is a Cargo package with a particular shape. Create one:

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
├── src/
│   ├── main.rs        # generated shim. Do not edit.
│   └── lib.rs         # yours: shared helpers, roles
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
- **`src/lib.rs` is yours.** Put shared helpers there — a function taking
  `&mut Ctx` is Rustible's equivalent of an Ansible role. Playbooks reach it
  by the package name, so in a workspace named `infra` that is `use infra::my_helper;`.
- **It is a normal Cargo package.** `cargo add` a dependency, use any crate.

⚠️ `rustible init` refuses only if a file it would write already exists, so
running it inside an existing git clone with a `README.md` and a `LICENSE` is
fine.

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
| `1` | refused before touching anything — bad inventory, missing var, build failure |
| `2` | a host or step failed — **something may have been changed** |
| `3` | bad command line |

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
| `addr` | hostname or address to connect to | none |
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
  `no host or group named 'all'` unless you define one. Ansible's most common
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
time: `ctx.local_secret(path)` uploads a file from the controller and redacts
it from every diff and log, and `--escalate-password-env` reads the sudo
password from the environment (§12).

Validate before running:

```sh
rustible inventory check          # parses, and checks every playbook's vars
rustible inventory show web1      # resolved values, with where each came from
```

## 7. Writing a playbook

The whole skeleton:

```rust
use rustible::prelude::*;
use rustible_std::{apt, file, systemd};

#[rustible::playbook(hosts = "web", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    ctx.step("nginx installed", apt::Present::new(["nginx"]))?;
    Ok(())
}
```

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
generated `src/main.rs` is the real entry point. Each `playbooks/*.rs` file is
a module of that binary, so ordinary Rust rules apply: `use` what you need,
and `pub fn` anything another file should see.

One playbook per file, and the filename is the playbook's name.

## 8. `Ctx`: everything a playbook can do

`ctx` is the single handle. Every method:

```rust
// Run an operation. The only verb. Returns what the op produced (§9).
ctx.step(name: impl Into<String>, op: impl Op) -> Result<Applied<O::Output>>

// Record that a step was deliberately not run. Shows as `skipped`.
ctx.skip(name: impl Into<String>, reason: impl Into<String>)

// Group steps under a heading in the output.
ctx.section(name, |ctx| { ... })

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
ctx.local_secret(path) -> Result<Secret>  // same, but redacted everywhere
ctx.fetch(remote, local_dest) -> Result<()>  // download from the target

// The escape hatch: run something no op covers.
ctx.sys() -> &System
```

⚠️ `ctx.local_file` and `ctx.local_secret` are how a playbook uses a file that
lives on the **controller** — a TLS key, a config template you build locally.
The playbook binary runs on the target and cannot see your disk otherwise.

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
not exist yet predicts only when you supply both `.uid()` and `.gid()` —
otherwise it will not invent them. A playbook that must survive `--check`
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
| `user` | `String` — who the steps run as |
| `is_root` | `bool` |

The enums are all in the prelude. Match on them rather than on
`distro_version` strings:

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

⚠️ `is_root` answers on **identity, not process**: under escalation it is true
because the steps run as root, even though the binary did not start that way.

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
- ⚠️ **Vars are flat.** Scalars, lists and enums. A struct or a map field is a
  compile error — there is no nested `vars` tree.
- **Precedence, nearest wins:** `--var` on the command line, then the host,
  then the closest group, then outer groups, then `defaults`, then the
  workspace-wide `vars { }` block. So a host setting `nginx_workers 8` beats
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
    ctx.step("read something", file::Line::in_path("/tmp/x").set("y"))?;
    ctx.as_root().step("nginx", apt::Present::new(["nginx"]))?;
    Ok(())
}
```

⚠️ A host with `escalate="none"` under a playbook with `escalate = true` runs
**unescalated**, with a note in the output saying so. It does not refuse and
it does not fail — so a host you deliberately exempted stays exempt, and you
are told each time.

`sudo` must work without a password, or pass one:

```sh
RUSTIBLE_SUDO=hunter2 rustible playbook run site --escalate-password-env RUSTIBLE_SUDO
```

The password travels in the start frame, never on a command line, and is
zeroized after use.

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

The modules are `apt`, `archive`, `file`, `group`, `hostname`, `http`, `shell`,
`ssh`, `sysctl`, `systemd` and `user`. What is in each is a `grep` away; what
you cannot get that way — which shape to reach for, and what bites — is the
rest of this section.

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
| everything else | `new(...)` | — nothing, it is already the op |

That list is short and you can regenerate it yourself:

```sh
grep -rn "pub fn to(self\|pub fn set(self\|pub fn pointing_to(self" \
  ~/.cargo/registry/src/*/rustible-std-*/src/
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
http::Download::get(URL).checksum("sha256:...").to("/tmp/x")
file::Line::in_path("/etc/hosts").set("10.0.0.1 db")
```

⚠️ Builder methods **consume `self`**, so chain them. Assigning a half-built
op to a variable and calling a method on it will not compile the way you
expect.

⚠️ **The apt operations take a list; everything else takes one name.**
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
use rustible_std::{apt, file, systemd, user};

const CONF: &str = include_str!("../files/nginx.conf");

#[rustible::playbook(hosts = "web", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    let f = ctx.facts();
    ensure!(f.package_manager == Pm::Apt, "{} is not apt-based", f.hostname);

    ctx.step("nginx installed", apt::Present::new(["nginx"]))?;

    let deploy = ctx.step(
        "deploy user",
        user::Present::new("deploy").shell("/bin/bash").groups(["www-data"]),
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

**`file::Line` without `matching` grows the file.** With only `.set(..)`, the
match is whole-line equality, so once the value differs from the one already
there, nothing matches and a *second* line is appended — one more on every
run. Give it a regex that matches the **old** value too:

```rust
file::Line::in_path("/etc/ssh/sshd_config")
    .matching(r"^#?\s*PasswordAuthentication\b")
    .set("PasswordAuthentication no")
```

Only the **first** match is rewritten, so a file that already has two such
lines keeps the second. And ⚠️ an invalid regex **panics**; it is not an error
you can catch.

**Prerequisites are refused, never created.** Nothing creates a group, a home
directory's parent, or a destination directory as a side effect. Sequence them:

```rust
ctx.step("docker group", group::Present::new("docker"))?;
let app = ctx.step("app user", user::Present::new("app").groups(["docker"]))?;
ctx.step("ssh dir", file::Directory::at(app.home.join(".ssh"))
    .owner(app.uid, app.gid).mode(0o700))?;
ctx.step("keys", authorized_keys::Present::for_user(&app).keys([KEY]))?;
```

The refusal happens in `check`, before anything is touched, and names the
operation you wanted.

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

`shell::Command` exists, and always reports changed because Rustible cannot
know what it did:

```rust
// a program and its arguments -- no shell, no word splitting, no globbing
ctx.step("regenerate", shell::Command::new("update-initramfs").arg("-u"))?;

// when you genuinely need a shell
ctx.step("pipeline", shell::Command::sh("apt-get update && apt-get upgrade -y"))?;
```

⚠️ `new` takes a **program name**, not a command line.
`shell::Command::new("apt-get update")` tries to execute a binary with a space
in its name. `sh(script)` is `new("/bin/sh").args(["-c", script])`.

Prefer a real operation where one exists: a `shell::Command` is never
idempotent and never has a diff.

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
binary. On a small workspace that is about **30 seconds**; a second run that
changes nothing is about **2 seconds**, because the binary is cached by source
hash and is only rebuilt when the source changes, and only re-uploaded when
the target does not already have that exact binary. Adding a second
architecture adds one more build.

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
rustible inventory check        # lists every group and host it loaded
rustible inventory show web1    # confirms one host resolves as you expect
```

`--check` also tells you, but it builds and connects first.

The `--json` stream is the same event sequence the renderer consumes — step
start and finish, diffs, logs, and a final summary object. Read one run with
`rustible playbook run x --json | head` before writing against it; the field
names are the ones in `rustible_sdk::event`.

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
- ⚠️ **`apt::Latest` refreshes the package lists even in check mode**, because
  its answer is read from them. It is the one place `--check` is not entirely
  read-only, and the run warns when it happens. This matches Ansible.

An op that *can* predict does: `apt::Present` knows the candidate version,
`file::Copy` knows the content it would write.

**A dry run of a playbook that builds things from scratch works.** A step does
not refuse because an earlier step's work has not happened yet: `--check` on a
fresh host runs the whole playbook and reports every step as `would change`.
A `user::Present` whose primary group an earlier `group::Present` would create
is accepted, and a `file::Copy` into a directory an earlier step would create
is accepted. What you do not get is *output* for steps that could not predict
it, which is §9's `is_available()` guard.

**`.changed` is `true` in check mode** when the step would have changed
something. So `if conf.changed { ... reload ... }` fires under `--check` too,
and the reload appears as its own `would change` line. The dry run shows you
the whole shape of the real run, conditionals included.

## 16. When something goes wrong

| symptom | cause |
|---|---|
| `no host or group named 'all'` | there is no implicit `all` group (§6) |
| `cannot find type Present in this scope` | missing `use rustible_std::apt;` (§7) |
| a step panics under `--check` only | `Deref` on an unavailable output (§9, §15) |
| `var X is not declared by this playbook` | the inventory sets a var the playbook's `Vars` does not declare; harmless, but usually a typo |
| `X is required but not set` | a `Vars` field with no `#[default]` and no value in the inventory |
| `sudo: a password is required` | passwordless sudo is not set up; use `--escalate-password-env` (§12) |
| `cargo build failed` mentioning `cc-rs` | no clang on the controller (§3) |
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
2. **A helper function** in `src/lib.rs` that composes existing operations.
   This is Rustible's equivalent of a role:
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
operations; `cargo add` it and use it. `rustible-github` in this repository is
a small worked example.

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
