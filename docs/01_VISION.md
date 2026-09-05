# Rustible: Vision and Architecture Decisions

**Status:** living document, first written 2026-09-05.
**Purpose:** this is the handoff. If every other context is lost, a reader of this
document should be able to continue designing and building Rustible without
re-deriving or re-arguing anything below. Every decision records the alternatives
that were considered and why they lost. Sections marked **OPEN** are not yet decided.

---

## 1. Problem statement

Rustible is an independent replacement for Ansible. It keeps the parts of Ansible's
architecture that are good and removes two specific pains:

1. **YAML as a programming language.** Ansible playbooks are untyped, uninferred,
   and unlinted. Parameters are strings you copy from documentation. Outputs are
   dicts you index by magic keys. Mistakes surface at runtime, often on the
   sixteenth step of a run, and only after several failed executions. Chaining the
   output of one task into the next is verbose and fragile (`register`, `set_fact`,
   `loop` over `results`).

2. **AnsiballZ.** Ansible zips Python module code plus its helper library into a
   blob, ships it to the target, and executes it with whatever Python interpreter
   the target happens to have. This creates a hard dependency on the target's
   Python version and packages, produces version-mismatch failures, and is
   brittle by construction.

What Ansible gets right and Rustible keeps:

- The **orchestrator/targets topology**: one machine drives the run, connects to
  many hosts (in parallel), pushes code, and renders per-host, per-step progress
  with `changed / ok / skipped / failed` semantics.
- The **desired-state model**: a task declares a state (`present`, `absent`,
  `enabled`), not an action. The framework reconciles the current state against it
  and reports whether anything changed. Check mode (dry run) and diff fall out of
  this model.
- **Agentless targets** that need nothing preinstalled beyond SSH and a shell.

## 2. The vision in one paragraph

A Rustible project is a Cargo package. Playbooks are ordinary Rust files. Operations
are typed structs with builders and typed outputs, so the compiler catches
parameter typos, wrong types, and invalid chaining at build time, and the editor
autocompletes both inputs and outputs. When you run a playbook, Rustible probes the
target hosts, compiles the playbook into a **static binary per target architecture**,
uploads it over SSH, runs it, and streams structured events back to render the
familiar Ansible-style progress view. The binary depends on nothing on the target.
Collections of new operations are plain crates built on a public SDK, added with
`cargo add`.

## 3. The intended user experience

```
$ rustible init
```
Creates a Cargo package in the current directory (which must be empty) or a chosen
folder. Adds `rustible` (runtime) and `rustible-std` (the base operations, mirroring
Ansible's builtin modules: files, users, groups, packages, services, ssh keys, and
so on) as dependencies. Creates an opinionated layout: `.gitignore`, an inventory
file, a `playbooks/` folder (with `.gitkeep`), and any config files that turn out to
be necessary.

```
$ rustible create playbook ./playbooks/cadu/ssh_enable_root_user.rs
```
Scaffolds a playbook file with a `main` function and the metadata attribute.

```
$ rustible run playbook ./playbooks/cadu/ssh_enable_root_user.rs
```
Reads the playbook's metadata (target hosts), probes the hosts, compiles per
architecture, uploads, runs, and renders progress. See section 5.2 for the pipeline.

```
$ cargo add rustible-docker
```
Adds a third-party collection. Its operations are immediately usable in every
playbook of the project, fully typed.

## 4. Prior art

Researched 2026-09-05. Nobody has shipped "typed real language + compiled agent on
the target".

- **JetPorch** (Michael DeHaan, Ansible's original author). Rust engine, but kept a
  YAML dialect. Discontinued in 2024 when the author lost personal need for it.
  Lesson: a single-maintainer project survives on the maintainer's own need.
- **glidesh** (2025). Rust, agentless, uses KDL instead of YAML for explicit typing,
  runs shell commands over SSH, check/apply pattern per module. Closest in the
  "escape YAML" direction, but still a data format, not a language.
- **pyinfra**. Python code as playbooks (the "real language" thesis), compiles to
  shell commands executed over SSH. No typed outputs, Python on the controller only.
- **Pulumi, Terraform CDK**. Prove that "real language beats DSL" for infrastructure
  in the cloud-resource domain.
- **Chef, Puppet**. Agent-based with a pull model. Rustible is push-based like Ansible.
- **Mitogen for Ansible**. Tried to give Ansible a persistent typed agent to fix
  per-task latency. Relevant to the execution model discussion in 5.1.

Every survivor in this space got one thing right early: a clean idempotent
operation model with `changed/ok/skipped`, check mode, and diff. That is the center
of the Rustible SDK, not an afterthought.

## 5. Architecture

### 5.1 Execution model: remote-brain (DECIDED)

**Decision:** each playbook is compiled into a self-contained static binary per
target triple. The binary is uploaded to the target and runs there. The playbook's
control flow, loops, and operation logic all execute on the target. A bidirectional
framed protocol between the running binary and the orchestrator carries events up
and data down. This mirrors AnsiballZ's "ship the whole thing and run it there",
with a static Rust binary in place of a Python blob.

**Alternative considered and rejected: local-brain / thin agent.** The playbook
would run on the developer's machine as a host binary, and each operation would be
a typed RPC to a small generic agent binary on the target (SDK runtime plus the
project's op crates, compiled once per triple rather than once per playbook).

Arguments for local-brain that were weighed:
- Compile cost: only the agent needs cross-compiling, and only when op crates change.
- Multi-host orchestration (Ansible's `delegate_to`, `run_once`, `serial`, "gather
  facts on DB nodes then configure the LB") becomes plain Rust loops and joins.
- Secrets and inventory never leave the controller.
- Ops stay typed on both sides because the op structs are shared code.
- Per-op latency is one round trip over a multiplexed SSH channel (~ms on a LAN),
  still far below Ansible's per-task cost.

Arguments against, and why remote-brain won:
- Every op input and output would have to be `serde`-serializable and could not
  hold closures or borrowed data.
- "Heavy logic near the data" (scan ten thousand files and decide) would have to be
  written as an op rather than as playbook code.
- The compile-cost argument is weaker than it first looks: Cargo caches dependencies
  per triple in `target/<triple>/`, so after the first cold build, editing a
  playbook only recompiles that one bin crate and relinks. The dev loop is seconds.
- The project owner explicitly wants the binary that runs on the target to *be the
  playbook*, self-contained, with zero controller dependency during execution
  (e.g. the run survives the controller dying, which local-brain cannot offer).

**Do not re-propose the thin-agent model.** It was considered in full and rejected.

Consequences accepted with remote-brain:
- Cross-host coordination requires explicit protocol support (see 5.5). The
  protocol is bidirectional from day one so that `ctx.barrier(..)`,
  `ctx.run_once(..)`, and "facts of other hosts" can be added later as calls
  over the channel without redesign. None of these are in the MVP.
- Host-specific inputs (vars, secrets) must be sent over the channel after the
  binary starts, never baked into the binary (see 5.4).

### 5.2 Run pipeline

`rustible run playbook <file>` does:

1. **Read metadata** from the playbook's `#[rustible::playbook(...)]` attribute by
   parsing the source with `syn`. No compilation is needed to learn the target
   hosts. (Alternative considered: compile a host build and run it with a
   `--describe` flag. Rejected as slower and unnecessary.)
2. **Resolve hosts** from the inventory and **open SSH** to each, in parallel.
3. **Probe** each host with one tiny shell command (`uname -sm`, `/etc/os-release`)
   to learn its target triple. This bootstrap probe is the only shell-dependent
   step; everything after it is the static binary.
4. **Compile** the playbook once per distinct triple, in parallel, as a static
   binary. Artifacts are cached by (playbook, triple, dependency hash).
5. **Upload** the binary if the target does not already have that content hash in
   its cache directory; **execute** it with stdin/stdout as the protocol channel;
   send the `Start` frame with host vars, check-mode flag, and verbosity.
6. **Stream events** back and **render** the per-host, per-step view. Facts
   gathering is the first thing the binary does and is reported as a frame.

### 5.3 Cross-compilation constraints (DECIDED for MVP)

- **Linux only, `*-unknown-linux-musl` targets only**, for the MVP. Static musl
  binaries run on any Linux regardless of libc version.
- **Op crates must be pure Rust by convention.** Pure-Rust crates targeting musl
  link with the bundled `rust-lld` after `rustup target add`. Any C dependency
  (openssl, libgit2, sqlite) turns cross-compilation into "install a C toolchain per
  triple". Use `rustls`, `rustix`, and pure-Rust alternatives.
- **`cargo-zigbuild`** (zig as a universal C cross-linker with bundled sysroots) is
  the escape hatch when a C dependency is unavoidable.
- macOS and Windows targets are deferred. They have their own toolchain and SDK
  requirements.

Environment facts recorded 2026-09-05 on the primary dev box: rustc 1.97.1,
targets installed: `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`. No zig,
no `cross`, no sccache. Docker present. An ARM Linux VM (`cadu-cogram-vm-arm`,
reachable via Tailscale when online) is the intended aarch64 test target. The
cross-compile spike is blocked until it is online.

### 5.4 Transport (DECIDED for MVP)

Use the system `ssh` binary via the `openssh` crate (ControlMaster multiplexing).
This inherits `~/.ssh/config`, ssh-agent, jump hosts, ProxyCommand, and 2FA for
free, and is what Ansible itself does. `russh` (pure Rust SSH) is the later option
for zero external dependencies. "Local" is the other transport (run the binary on
the orchestrator machine itself).

The binary on the target never opens a socket. Everything rides the SSH session
the orchestrator established, so firewalling and authentication are entirely
SSH's concern.

### 5.5 Protocol (sketched, format OPEN)

Length-prefixed frames, `serde`-serialized (postcard vs msgpack vs JSON to be
decided by a spike; JSON is the debuggable option, postcard the compact one), on
the binary's stdin (down) and stdout (up). Stderr stays raw for panics.

```rust
enum Down {
    Start { run_id, host: HostInfo, vars: Vars, check_mode: bool, verbosity: u8 },
    FileChunk { req: u32, offset: u64, bytes: Vec<u8>, last: bool },  // answers FileRequest
    FileDenied { req: u32, reason: String },
    Cancel,                                                            // ctrl-c on orchestrator
    // later: BarrierRelease, PeerFacts, ...
}

enum Up {
    Hello { protocol: u32, playbook: String, build_hash: String },
    Facts(Facts),
    StepStarted { id: u32, name: String },
    StepFinished { id: u32, status: Status, diff: Option<Diff>, elapsed_ms: u64 },
    Log { level, msg },
    CmdRan { step: u32, argv: Vec<String>, exit: i32, elapsed_ms: u64 },  // at -v
    FileRequest { req: u32, path: String },                               // "send me files/x"
    FetchChunk { req: u32, dest: String, offset: u64, bytes: Vec<u8>, last: bool },
    Finished { ok: u32, changed: u32, skipped: u32, failed: u32 },
    Failed { step: u32, error: String },
}
```

Requests are correlated by id and may overlap: the binary can request a file while
a step runs, and the orchestrator can serve several hosts from one local read.

**Protocol versioning:** the binary is built from the same dependency graph the
orchestrator knows, so the orchestrator can hash the SDK/op-crate set and treat it
as the protocol identity. A mismatch means "rebuild", never silent breakage.

### 5.6 Getting local files to the target (DECIDED: both ways)

Ansible's `copy` and `template` ship controller-side files to the target. Rustible
supports two mechanisms, each for a different need:

1. **Embed at compile time** via `include_bytes!` / `include_str!`, or a
   compile-time template engine (askama-style). Use for small, fixed files and
   templates. The binary stays self-contained. Compile-time templates are checked
   against a typed struct, so `{{ server_nmae }}` is a compile error.

   ```rust
   ctx.step("Install nginx config",
       file::Copy::from_bytes(include_bytes!("../files/nginx.conf"))
           .to("/etc/nginx/nginx.conf").mode(0o644))?;

   #[derive(Template)]
   #[template(path = "nginx.conf.j2")]
   struct NginxConf<'a> { server_name: &'a str, workers: u32 }

   ctx.step("Render nginx config",
       file::Template::render(NginxConf { server_name: &vars.domain, workers: facts.cpus })
           .to("/etc/nginx/nginx.conf"))?;
   ```

2. **Stream over the channel at run time.** `ctx.local_file("files/big.tar.gz")`
   sends a `FileRequest` up, receives `FileChunk`s down, and returns a temp path on
   the target. `ctx.local_secret("vault/db_password")` returns bytes in memory
   only. Use for large files, files generated right before the run, and secrets
   that must not sit inside a binary in a build cache.

Rule of thumb: embed by default, stream when large, dynamic, or secret.

The reverse (Ansible's `fetch`) uses the same channel upward (`FetchChunk`).
Cross-host copy (Ansible's `synchronize` with `delegate_to`) is deferred; it is a
coordination feature, not a backend concern.

## 6. Playbook programming model

### 6.1 Playbook file shape

```rust
//! playbooks/cadu/ensure_rustible_user.rs
use rustible::prelude::*;
use rustible_std::{file, ssh, user};

#[rustible::playbook(hosts = "local", become = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    let keys = [
        "ssh-ed25519 AAAAC3...XYZ cadu@x86",
        "ssh-ed25519 AAAAC3...ABC cadu@arm",
    ];

    // Typed output: `account` is a `user::Account` with uid, gid, home, shell.
    let account = ctx.step(
        "Ensure rustible user exists",
        user::Present::new("rustible").shell("/bin/bash").create_home(true),
    )?;

    // Chaining: the next op consumes the previous op's typed result.
    ctx.step(
        "Ensure ~/.ssh exists",
        file::Directory::at(account.home.join(".ssh")).owner(&account).mode(0o700),
    )?;

    let authorized = ctx.step(
        "Install authorized keys",
        ssh::AuthorizedKeys::for_user(&account).keys(keys).exclusive(true),
    )?;

    // No Ansible "handlers": reacting to change is just an `if`.
    if authorized.changed {
        ctx.log(format!("installed {} key(s)", authorized.added.len()));
    }
    Ok(())
}
```

Rendered by the orchestrator from the event stream:

```
PLAYBOOK ensure_rustible_user   hosts: local   (x86_64-unknown-linux-musl, cached)

[local]  Ensure rustible user exists ........ changed   uid=1002
[local]  Ensure ~/.ssh exists ............... ok
[local]  Install authorized keys ............ changed   +2 keys

local    ok=3  changed=2  skipped=0  failed=0     1.2s
```

- The `#[rustible::playbook(...)]` attribute carries metadata: target hosts (a host
  or group from the inventory), `become`, and later things like `serial`. The
  macro wraps `main` with the runtime that speaks the protocol.
- `become = true` means the binary is launched under `sudo` on the target. Per-op
  escalation is a later addition (see `System::as_user` in section 7).
- `?` on a step means "this host's run fails here". Ansible's `ignore_errors` is
  `.ok()` or a `match`; `failed_when` is an `if` after the step.
- Loops, conditionals, helper functions, and third-party crates are all just Rust.
  The author is responsible for determinism and side effects, as in any program.

### 6.2 `ctx.step` and the `Op` trait (DECIDED)

**One verb.** `ctx.step(name, op)` is the only way to execute anything. A step
reconciles the current system state with the desired state described by `op`.

**Alternative considered and rejected: `ctx.ensure(state)` / `ctx.run(action)`**,
a split API where idempotent desired states and always-changing actions used
different verbs and traits. Rejected because it splits the language; the
distinction lives in the op type instead (see 6.4).

**Naming.** The trait is `Op` (operation). The name `State` was tried and
rejected: "state" is what the system has after an op is applied, not the thing
you hand to `step`.

`Op` values are plain data structs built with builders. Constructing one
(`group::Present::new("docker")`) touches nothing. Only `ctx.step` executes.
This is deliberate: a data struct can be inspected before it runs, which is what
gives dry-run, diff rendering, and a future `rustible plan` mode for free. A
closure is opaque and could only be run. Because ops are values, they can be
built conditionally, stored in a `Vec`, or returned from helper functions in
third-party crates.

```rust
pub trait Op {
    type Output;
    /// Inspect the system. Never mutates. Returns what would need to happen.
    fn check(&self, sys: &System) -> Result<Plan<Self::Output>>;
    /// Perform the change described by the plan. Only called if the plan says so.
    fn apply(&self, sys: &System, plan: Plan<Self::Output>) -> Result<Self::Output>;
}

pub enum Plan<T> {
    /// Already in desired state. Carries the output so `step` can return it without applying.
    Satisfied(T),
    /// Something must change. `diff` is what the report shows.
    Change { diff: Diff },
}

pub struct Applied<T> { pub value: T, pub changed: bool, pub diff: Option<Diff>, /* timing */ }
// Applied<T> derefs to T, so `account.home` works and `account.changed` is there too.
```

The step driver, in outline:

```rust
pub fn step<O: Op>(&mut self, name: impl Into<String>, op: O) -> Result<Applied<O::Output>> {
    emit(StepStarted { name });
    match op.check(&self.sys)? {
        Plan::Satisfied(out) => { emit(StepFinished { status: Ok }); Ok(Applied { value: out, changed: false, .. }) }
        Plan::Change { diff } if self.check_mode => { emit(StepFinished { status: WouldChange, diff }); /* skip apply */ }
        Plan::Change { diff } => {
            let out = op.apply(&self.sys, Plan::Change { diff: diff.clone() })?;
            emit(StepFinished { status: Changed, diff });
            Ok(Applied { value: out, changed: true, diff: Some(diff), .. })
        }
    }
}
```

`check` does all the thinking and produces the `Diff`; `apply` executes that
diff. This makes dry-run trustworthy: the diff shown in check mode is exactly the
change that would be applied.

### 6.3 Ops are named after desired state (DECIDED)

Every Ansible `state:` string that changes the meaning of the other parameters
becomes its own struct with its own output type. This is the single biggest
translation rule for the standard library.

| Ansible                    | Rustible                                   |
|----------------------------|--------------------------------------------|
| `user: state=present`      | `user::Present`                            |
| `user: state=absent`       | `user::Absent`                             |
| `apt: state=present`       | `apt::Present`                             |
| `apt: state=absent`        | `apt::Absent`                              |
| `apt: state=latest`        | `apt::Latest`                              |
| `systemd: state=started`   | `systemd::Running`                         |
| `systemd: state=stopped`   | `systemd::Stopped`                         |
| `systemd: enabled=yes`     | `systemd::Enabled`                         |
| `systemd: state=restarted` | `systemd::Restart` (verb: an action)       |
| `getent`/`register`        | `user::Lookup` (read-only op)              |

**Alternative considered and rejected:** one struct per resource with a state
parameter, e.g. `apt::Package::new(..).state(State::Present)`. Rejected because:
- Valid options differ per state (`purge`, `autoremove` only make sense for
  `Absent`; `update_cache` only for `Present`/`Latest`). A shared struct cannot
  stop `.purge(true).state(Present)` at compile time, which is YAML hell in Rust
  clothing.
- Outputs differ per state (`Present` returns installed versions to chain from;
  `Absent` returns what was removed). A shared struct means one output with a
  pile of `Option`s.
- Reading the playbook, intent is on the left: `apt::Absent::new(["apache2"])`.

Cost accepted: more types in the stdlib. Types are cheap; runtime "invalid
parameter for this state" errors are what we are escaping.

### 6.4 Actions and the "always changes" hint

Some things are actions, not states: `systemd::Restart`, `shell::Command`. They
implement the same `Op` trait; their `check` simply always returns `Plan::Change`,
because "restarted" is not a state you can already be in. In check mode they
report "would change", as Ansible does.

`shell::Command` can be promoted toward state-like behavior with Ansible's
escape hatches as builder methods: `.creates(path)` makes `check` return
`Satisfied` when the path exists; `.removes(path)` likewise. `changed_when` maps
to a closure over the output.

An op whose `check` always returns `Change` can flag itself as `always_changes`,
so the orchestrator can mark those steps in the output. This preserves the
"where does this playbook stop being idempotent" scan without a second verb.

### 6.5 Lookups

Read-only lookups are ops too (`user::Lookup::by_name("rustible")`). They run
through `ctx.step`, appear in the step list, are timed, and can never report
`changed`. They fail the run if the thing is missing. In Ansible this is `getent`
plus `register` plus `set_fact`; here it is one typed call.

### 6.6 No handlers; loops; conditionals

- **Handlers are gone.** "Restart sshd only if the config changed" is
  `if cfg.changed { ctx.step("sshd restarted", systemd::Restart::new("sshd"))?; }`.
  Ansible needs `notify`, a handler section, and flush semantics.
- **Loops are `for`.** Each iteration is its own enumerated step; the name is a
  `format!` string the author produces, which is both the price and the win
  ("Add rustible to docker" beats `item=docker`). Loop bodies chain naturally:
  the group op's output feeds the membership op in the same scope.
- **Conditionals are `if`.** `when:` does not exist.

Second example, showing removal and loops:

```rust
#[rustible::playbook(hosts = "local", become = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    let revoked = ["ssh-ed25519 AAAAC3...OLD1 cadu@laptop-2023", "ssh-ed25519 AAAAC3...OLD2 ci@jenkins"];
    let groups = ["docker", "systemd-journal", "adm"];

    let account = ctx.step("Look up rustible user", user::Lookup::by_name("rustible"))?;

    let keys = ctx.step("Revoke compromised keys",
        ssh::AuthorizedKeys::for_user(&account).remove(revoked))?;
    ctx.log(format!("removed {} key(s)", keys.removed.len()));

    for name in groups {
        let grp = ctx.step(format!("Ensure group {name} exists"), group::Present::new(name))?;
        ctx.step(format!("Add rustible to {name}"), user::Membership::of(&account).in_group(&grp))?;
    }
    Ok(())
}
```

Note `AuthorizedKeys::for_user(x).remove(keys)` versus `.keys(keys).exclusive(true)`:
one op type covers "ensure these", "ensure exactly these", and "ensure not these",
because those are options on one resource rather than different states.

### 6.7 Granularity rule (DECIDED)

**An op changes exactly one kind of resource. If it needs a prerequisite, it fails
with a clear message rather than creating it silently.** `user::Membership` does
not create the group; `group::Present` does. This keeps the report honest about
what changed. The stdlib will make this call hundreds of times; this is the rule.

### 6.8 Translations of real Ansible modules

**`ansible.builtin.apt`**
```rust
let pkgs = ctx.step("Install nginx and curl",
    apt::Present::new(["nginx", "curl"]).update_cache(Duration::from_secs(3600)))?;
// pkgs.installed: Vec<Package{name,version}> (only ones this step installed), pkgs.already_present
ctx.step("Remove apache2", apt::Absent::new(["apache2", "sendmail"]).purge(true).autoremove(true))?;
ctx.step("Keep openssl current", apt::Latest::new(["openssl"]).update_cache(Duration::ZERO))?;
```
`check`: `dpkg-query -W` per name, build the missing set, `Satisfied` if empty.
`apply`: `apt-get update` if the cache is older than the max age, then
`apt-get install -y` the missing set. Refuses early on a non-Debian box using
`facts.package_manager`.

**`ansible.builtin.lineinfile`**
```rust
let sshd = ctx.step("Disable password auth",
    file::Line::in_path("/etc/ssh/sshd_config")
        .matching(r"^#?PasswordAuthentication")
        .set("PasswordAuthentication no")
        .backup(true))?;
// sshd.changed, sshd.backup_path: Option<PathBuf>, sshd.line_no
```
`check` reads the file, finds the line by regex (or exact match), and returns
`Satisfied` if it already equals the target, `Change { diff: line_replace }` if it
differs, or `Change { diff: line_insert }` if absent (insert position from
`.insert(Append | After(re) | Before(re))`). `apply` optionally backs up, applies
the diff to the text, and writes atomically. The pure "given text and regex,
produce new text and diff" logic is a free function so it can be unit-tested with
strings.

**`ansible.builtin.systemd`**
```rust
ctx.step("sshd enabled", systemd::Enabled::new("sshd"))?;
if sshd.changed {
    ctx.step("sshd restarted", systemd::Restart::new("sshd").daemon_reload(true))?;
}
```
`Enabled`/`Running`/`Stopped` are states (`systemctl is-enabled` / `is-active` in
`check`). `Restart` is an action: `check` always returns `Change`; `apply` does
optional `daemon-reload`, `restart`, then verifies `is-active`.

## 7. The `System` handle

`System` is the argument every op's `check` and `apply` receive. It is the op's
only view of the machine.

### 7.1 Options considered

| | Free functions in `rustible_sdk::fs` | Methods on `System`, no trait | `Backend` trait + `Fake` |
|---|---|---|---|
| Know identity, check mode, umask policy | No | Yes | Yes |
| Log every write as an event for `-v` | No | Yes | Yes |
| Guard: error if an op mutates during `check` | No | Yes | Yes |
| Author friction | Lowest | One `sys.` prefix | One `sys.` prefix, reads too |
| Fake filesystem/commands for unit tests | No | No | Yes |
| Extra abstraction to maintain | None | None | A trait plus a fake impl |

**Ansible's reasoning**, researched for this decision: `AnsibleModule` grew
helpers (`run_command`, `atomic_move`, `backup_local`,
`set_fs_attributes_if_different`, common file args, `check_mode`, `exit_json`,
`tmpdir`) for **consistency of behavior across hundreds of modules by hundreds of
authors**, not for testability. `atomic_move` preserves mode, owner, SELinux
context, and handles cross-filesystem renames. `run_command` forces `LANG=C`,
controls umask, handles encoding and exit codes. Nothing is enforced; modules call
`os` and `shutil` directly all the time. Ansible's unit tests mock `run_command`
thinly; real confidence comes from `ansible-test integration --docker <distro>`.

The concern raised against a trait-based `System`: it establishes an ad-hoc
protocol that library authors must know ("go through `sys`, not `std::fs`"), which
is not compiler-enforced.

### 7.2 Decision: `Backend` trait with `Local` and `Fake` (DECIDED)

The third column, chosen with the caveat acknowledged: **all file reads, writes,
and process spawns go through `sys`, reads included**, otherwise the fake is one
nobody hits. A clippy `disallowed-methods` config in op crates nudges away from
`std::fs`. This is a lint, so authors can opt out with a reason.

What it buys beyond the middle column:
- Fast unit tests for op logic with no container: "given this `/etc/passwd` and
  this canned `useradd` output, the op plans this diff and runs this command".
- Testing the distro branch itself: set `facts.distro = Alpine` on the fake and
  assert BusyBox `adduser` flags.
- Testing failure paths (exit code 9, permission denied) that are awkward to
  provoke in a real container.
- A future third backend (`Chroot`, `Container`) for applying ops to a mounted
  image without SSH is one more `impl Backend`.

What it does not replace: Docker integration tests remain the source of truth
that the system agrees with our assumptions. The fake tests that the op does what
we intended; the container tests that what we intended is correct.

### 7.3 Sketch

```rust
pub struct System {
    backend: Arc<dyn Backend>,   // Local, Fake, later Chroot/Container
    facts: Arc<Facts>,
    identity: Identity,          // who commands run as
    check_mode: bool,
    events: EventSink,           // so cmd invocations and warnings reach the orchestrator
}

impl System {
    // context
    pub fn facts(&self) -> &Facts;          // typed: Os, Distro, Arch, Pm::{Apt,Dnf,Apk}, hostname, kernel, cpus
    pub fn check_mode(&self) -> bool;
    pub fn as_user(&self, name: &str) -> System;   // clone; commands run via sudo -u; not a mutation
    pub fn is_root(&self) -> bool;
    pub fn tempdir(&self) -> Result<TempDir>;       // location policy lives here

    // processes: logged as CmdRan events, LANG=C, controlled umask, run as identity
    pub fn cmd(&self, program: &str) -> Cmd;        // .arg() .args() .run() -> Output; .ok() -> Option<Output>

    // files: reads are plain; mutations are logged and refuse to run inside check()
    pub fn exists(&self, p: &Path) -> Result<bool>;
    pub fn stat(&self, p: &Path) -> Result<Option<Stat>>;
    pub fn read(&self, p: &Path) -> Result<Vec<u8>>;
    pub fn read_to_string(&self, p: &Path) -> Result<String>;
    pub fn write_atomic(&self, p: &Path, bytes: &[u8]) -> Result<()>;   // tmp + rename, keeps mode/owner/selinux
    pub fn ensure_attrs(&self, p: &Path, attrs: &FileAttrs) -> Result<bool>; // Ansible's common file args
    pub fn set_mode / set_owner / mkdir_all / remove / backup(&self, ..);

    // reporting without failing
    pub fn warn(&self, msg); pub fn debug(&self, msg);
}

pub trait Backend: Send + Sync {
    fn read(&self, p: &Path) -> io::Result<Vec<u8>>;
    fn write(&self, p: &Path, bytes: &[u8]) -> io::Result<()>;
    fn stat(&self, p: &Path) -> io::Result<Option<Stat>>;
    fn spawn(&self, cmd: &CmdSpec) -> io::Result<Output>;
    // one method per primitive, nothing clever
}
pub struct Local;
pub struct Fake { files: Mutex<BTreeMap<PathBuf, FakeFile>>, cmds: Mutex<Vec<CannedCmd>> }
```

Notes:
- `System` is concrete; ops write `fn check(&self, sys: &System)` and never see a
  generic or `dyn`.
- `write_atomic` is the only write path (temp file in the same directory, then
  rename).
- Ops are synchronous. The event channel is the only concurrent thing in the
  binary and is owned by `Ctx`, not `System`.
- **SSH is not a backend.** The binary runs on the target, so every file is local.
  SSH is the orchestrator's transport only. In production the backend is always
  `Local`. (In a local-brain design SSH would have been a backend; this is one of
  the payoffs of remote-brain.)

### 7.4 What is deliberately not on `System`

**Users and groups are not backend primitives.** `user::Present` reads
`/etc/passwd` through `sys.read_to_string` and calls `sys.cmd("useradd")`. Putting
`create_user` on `System` would force `System` to know that Alpine uses `adduser`
with BusyBox flags, which is distro knowledge that belongs in the op, chosen via
`facts.distro`. Layering:

- `System`: primitives identical on every Unix (read, write, stat, spawn, identity).
- Ops: everything distro-specific.
- `Facts`: the data that lets ops choose.

Networking and anything async are also off `System` for now.

## 8. Testing strategy (DECIDED)

1. **Pure functions** for the interesting logic (line replacement and diff,
   `/etc/passwd` parsing, authorized_keys deltas, version comparison). Tested with
   strings, no fakes.
2. **`Fake` backend unit tests** for op behavior: canned files, canned command
   responses, assert planned diff and exact commands run. Hundreds run in a second.
3. **Docker integration tests** per distro, the source of truth. The SDK ships a
   harness (`#[rustible::integration_test(images = ["debian:12", "alpine:3.20",
   "fedora:41"])]`) that builds the test as a static musl binary and runs it in
   each container. A typical test applies an op twice: first run `changed`,
   second run `ok`, and the system looks right. Static binaries drop into any
   image with no setup.
4. **VMs (Vagrant or similar)** only for what Docker does badly: systemd units,
   kernel modules, reboots. Deferred until such ops exist.

## 9. Project layout and ecosystem

- **A Rustible project is one Cargo package.** `rustible init` creates it.
- **Playbooks are `.rs` files under `playbooks/`**, each mapped to a `[[bin]]`
  target. `rustible` keeps the `[[bin]]` entries in sync (autobins off), so
  `rustible run playbook ./playbooks/x.rs` becomes `cargo build --bin x --target
  <triple>` under the hood. (Alternatives noted: `src/bin/` auto-discovery, a
  `build.rs`, or a generated shadow workspace. Syncing `[[bin]]` is the simplest.)
- **Inventory is data**, in a file (`hosts.yml` or `hosts.toml`, format OPEN).
  Dynamic inventories become a trait later.
- **Crates:**
  - `rustible`: the CLI and orchestrator (init, create, run, SSH, compile, render).
  - `rustible-sdk`: `Op`, `Plan`, `Applied`, `System`, `Backend`, `Local`, `Fake`,
    `Facts`, `Diff`, the `Ctx`, the protocol types, the `#[playbook]` macro, the
    test harness. Everything a collection author needs.
  - `rustible-std`: the base operations mirroring Ansible builtins, itself just a
    consumer of `rustible-sdk`.
  - Third-party collections (`rustible-docker`, ...): plain crates on
    `rustible-sdk`, published to crates.io, added with `cargo add`. Because
    playbooks link them directly, no registration mechanism is needed.
- Op crates: pure Rust, no C dependencies (section 5.3).

## 10. Glossary

- **Orchestrator**: the `rustible` CLI process on the developer's machine that
  drives a run.
- **Target**: a host a playbook is applied to.
- **Playbook**: a Rust source file with a `#[rustible::playbook]` main, compiled
  to a static binary per triple.
- **Op**: a struct implementing `Op`, describing a desired state (or an action),
  with `check`/`apply`.
- **Step**: one `ctx.step(name, op)` call; the unit of reporting.
- **Plan**: the result of `check`: `Satisfied` or `Change { diff }`.
- **Applied**: what `step` returns: the op's typed output plus `changed` and `diff`.
- **System**: the op's handle to the machine, over a `Backend`.
- **Facts**: typed data about the target gathered at startup.
- **Collection**: a crate of ops built on `rustible-sdk`.

## 11. What is not yet defined (OPEN)

In the suggested order of attack:

1. **Inventory**: file format, groups, host vars, group vars, how
   `hosts = "web"` resolves, connection settings per host (user, port, become
   method).
2. **`Ctx` beyond `step`**: `log`, `vars` (typed? how?), `facts`, `local_file`,
   `local_secret`, and the future coordination calls (`barrier`, `run_once`,
   peer facts).
3. **Protocol serialization format** (postcard vs msgpack vs JSON) and framing
   details; check-mode semantics for the step driver when a `Change` is planned
   (skip and continue vs stop).
4. **`rustible init` layout in detail**: exact files, config file (if any),
   `.gitignore`, how the `[[bin]]` sync works, how the CLI finds the project root.
5. **Facts**: the exact `Facts` struct, which probes gather it, and how ops
   extend it.
6. **Diff representation** and rendering.
7. **Privilege escalation details**: `become` method (sudo/doas), password
   handling, per-op `as_user` semantics for file ops (write then chown).
8. **Error model**: error types, what a failed step reports, retries.
9. **Output rendering**: the terminal UI, verbosity levels, machine-readable
   output.
10. **Binary caching on targets**: cache dir location, cleanup.
11. **Spikes** (see section 12).

## 12. Planned spikes

1. **Cross-compile**: pure-Rust hello binary to `aarch64-unknown-linux-musl`
   from the x86 dev box, first with plain rustup targets and `rust-lld`, then with
   `cargo-zigbuild` if that fails; run it on the ARM VM. Validates or kills the
   compile story. **Blocked**: ARM VM offline as of 2026-09-05.
2. **Protocol over SSH**: framed bidirectional exchange over the `openssh` crate,
   spawn an uploaded binary, exchange messages, measure round trip. Also pick the
   serialization format.
3. **SDK core**: the `Op` trait, `System` with `Local` and `Fake`, one real op
   (`file::Line` is a good first), a playbook file with the macro, run locally.
   Turns the sketches above into a compiling crate and surfaces what they hide.
   **Not blocked**; can start any time.
