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

1. **Read metadata** by doing a host-native debug build of the playbook and
   running it with `--describe`, which the `#[playbook]` macro generates. This
   yields the target hosts, `become`, and a JSON schema of the typed vars struct
   (section 13.3). **Accepted trade-off (final, 2026-09-06):** the pre-check costs
   a cold build the first time (about a minute) and seconds afterwards. In
   exchange the schema comes from the real compiled types, so there is no
   source parser of our own to maintain and no restriction on var field types.
   (Alternative considered twice: parse the source with `syn`. It is instant but
   only sound for a closed set of canonically spelled types, cannot see through
   aliases or imports, and needs a second parser kept in sync with the proc
   macro. Rejected.) Mitigations: cache describe output by hash of the playbook
   source plus `Cargo.lock`; dev profile with a shared target dir; the describe
   build shares dependency compilation with the target build for same-arch
   hosts; `rustible inventory check` runs only this step.
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
- **Inventory is data**, in `hosts.kdl` next to `rustible.toml`. See section 13.
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

1. ~~Inventory~~: fully decided in section 13. Sibling-group var conflicts are
   an error naming both groups and the host (fix: set it on the host or a common
   parent).
2. ~~`Ctx` beyond `step`~~: decided in section 14.
3. **Protocol serialization format** (postcard vs msgpack vs JSON) and framing
   details. Check-mode semantics are decided in section 15.
4. **`rustible init` layout in detail**: exact files, config file (if any),
   `.gitignore`, how the `[[bin]]` sync works, how the CLI finds the project root.
5. **Facts**: the exact `Facts` struct, which probes gather it, and how ops
   extend it.
6. **Diff representation** and rendering.
7. ~~Privilege escalation~~: decided in section 14.3 (helper-process backend).
   Remaining: `doas` specifics.
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

## 13. Inventory, typed vars, and the workspace (DECIDED 2026-09-05, format pending)

### 13.1 Structure

The inventory stores single hosts and host groups. Groups can contain groups
(`members = [..]`); a host's group set is the transitive closure, and vars merge
from outermost group to innermost.

**Connection settings are not vars.** `addr`, `port`, `ssh_user`, `connection`
(`ssh` | `local`), and the become method are orchestrator configuration and live
directly on the host or group. Playbook vars live under a separate `vars` table.
Ansible mixes these (`ansible_host`, `ansible_user`) and that is a source of its
precedence confusion.

### 13.2 Format: KDL for the inventory, TOML for `rustible.toml` (DECIDED 2026-09-06)

Candidates were YAML, TOML, RON, KDL, JSON, and a Rust-file inventory.

| | TOML | RON | KDL |
|---|---|---|---|
| Reads well for nested groups | No (every nested map needs a `[a.b.c]` header; inline tables are single-line) | Okay | Best |
| Machine edit preserving comments | `toml_edit`, excellent | Nothing; rewrite loses comments | `kdl` crate round-trips documents |
| Typed scalars, no coercion traps | Yes | Yes | Yes |
| Familiarity | Everyone | Rust people, loosely | Few, learnable in minutes |

- YAML rejected: silent coercion (`no`, `NO`, `1.10`, `022`) and no
  format-preserving editor in Rust.
- TOML was chosen first, then rejected for the inventory because Cadu dislikes
  its nested-table syntax and an inventory is mostly nesting. It stays for
  `rustible.toml`, which is flat.
- RON considered: "typed" is mostly cosmetic since serde validates any format
  into our `Inventory` struct; its only visible gain is unquoted enum variants,
  and it has no comment-preserving editor.
- Rust-file inventory rejected even though a compile is now paid anyway
  (section 5.2): inventories get edited by scripts, agents, and non-Rust
  colleagues.
- **KDL chosen**: node-based, which is the shape of an inventory; braces nest
  without repeating paths; lists are positional arguments; comments survive
  machine edits; slash-dash (`/-node`) disables a whole node with children,
  which is the "take web3 out of tonight's run" move. Use KDL 2.0 syntax
  (`#true`/`#false`).

### 13.2.1 Parameters versus vars

Two kinds of data with two syntactic homes so they cannot be confused:

- **Parameters** are the fixed, typed, closed set `rustible` itself understands
  (how to connect). They are **properties on the node** (`key=value` after the
  name). Misspelling one is a load-time error with a suggestion.
- **Vars** are the open bag the playbook consumes. They live **only inside a
  `vars` child block**. `rustible` never interprets them; it merges them and
  hands them to the playbook's typed struct. A var named `port` is unrelated to
  the `port` parameter: the block boundary is the namespace. (Ansible needs the
  reserved `ansible_*` prefix for the same separation.)

| Parameter | Type | Required | Default | Settable on |
|---|---|---|---|---|
| `addr` | string (IP or hostname) | yes unless `connection="local"` | none | host only |
| `connection` | `"ssh"` \| `"local"` | no | `"ssh"` | host, group, defaults |
| `ssh_user` | string | no | local username | host, group, defaults |
| `port` | u16 | no | 22 | host, group, defaults |
| `become` | `"sudo"` \| `"doas"` \| `"none"` | no | `"sudo"` | host, group, defaults |
| `become_user` | string | no | `"root"` | host, group, defaults |
| `ssh_args` | list of strings | no | empty | host, group, defaults |

Parameter resolution: host, then nearest group outward, then `defaults`, then
the built-in default. Parameters never come from `vars` and vars never from
properties. On the orchestrator side parameters deserialize into a `HostParams`
struct via serde.

### 13.2.2 Full example

```kdl
// hosts.kdl  (KDL 2.0)
// Nodes: vars, defaults, host, group.

vars {                                  // workspace-wide vars: the "all" level
    fruit "banana"
    timezone "America/Sao_Paulo"
}

defaults ssh_user="cadu" port=22 become="sudo"   // workspace-wide parameters

host "laptop" connection="local"

group "web" ssh_user="deploy" {         // group-level parameter
    vars {
        nginx_workers 4
        allowed_ports 22 80 443         // list: positional arguments -> Vec<u16>
        tls #true
    }
    host "web1" addr="10.0.1.11"
    host "web2" addr="10.0.1.12" {
        vars { nginx_workers 8 }        // host-level override
    }
}

group "db" {
    vars { pg_version 16 }
    host "db1" addr="10.0.2.11" ssh_user="pgadmin" port=2222 {
        vars { role "primary" }
    }
    host "db2" addr="10.0.2.12" {
        vars {
            role "replica"
            replica_of "db1"
        }
    }
}

group "production" {                    // group of groups
    members "web" "db"
    vars { env "production" }
}

group "monitored" {
    members "web1" "db1"                // cherry-pick hosts by name
    vars { alerts #true }
}

/-host "web3" addr="10.0.1.13" {        // slash-dash: disabled, children included
    vars { nginx_workers 2 }
}
```

`rustible inventory show web2` prints the resolved parameters and vars with the
source of each (all / group X / host / defaults), including what was overridden.

### 13.2.3 Structural rules

- Names are unique across hosts and groups, so `members` can reference either.
- A host is defined exactly once (inside at most one group by nesting) and
  referenced from other groups by name. Defining it twice is an error.
- Group membership is the transitive closure through `members`.
- `vars { a 1 }` (children form) and `vars a=1` (property form) are equivalent;
  use children form for lists and many vars.
- Var names use underscores and match the Rust field names one to one. No case
  mapping.
- Load-time errors name the file and line, e.g. missing `addr` on an ssh host,
  unknown parameter with a did-you-mean, `addr` on a group.

### 13.3 Typed vars: bridging the untyped bag and the typed playbook

- The inventory holds a **bag of scalars** per host/group: `HashMap<String,
  Scalar>` where `Scalar` is string, int, float, bool, or a list of one of those.
  (Not `HashMap<String, String>`: the file format already has typed scalars;
  flattening to strings and re-parsing is lossy.)
- A playbook declares a **flat (single-level) typed struct** of the vars it
  needs, in the same `.rs` file, with `Option<T>` for optional fields and
  defaults via attribute or `impl Default`. The `#[rustible::vars]` macro
  enforces flatness and derives `Deserialize` plus a JSON schema.
- Coercion from the bag into the struct goes through `serde`, giving `Option`,
  defaults, and precise error messages for free. No hand-written coercion.
- **Validation happens on the orchestrator, before any cross-compile or upload,
  for every resolved host.** If any host fails, nothing runs and the error names
  each host and each missing or mistyped var.
- **The schema comes from `--describe` on a host-native build** (section 5.2),
  generated from the compiled types. Consequence: **any field type that
  implements `Deserialize` and the schema derive is allowed**, including
  user-defined enums, newtypes, and types from other crates. The only rule is
  that the struct is flat (one level), which is an inventory design choice, not
  a parser limitation.
- A field is *required* iff it is not `Option<T>` and has no default.
- Last line of defense: the binary deserializes its vars again at `Start` on the
  target, so nothing runs with bad vars even if the pre-check were bypassed.
- Options rejected: `syn` on the source (instant, but only sound for a closed
  canonically spelled type set, and needs a second parser kept in sync with the
  proc macro); proc macro writing the schema to disk during compilation (still
  needs a compile, and is hacky); schema in a separate `x.vars.toml` with the
  struct generated from it (splits the playbook across two files); rustdoc JSON
  / rust-analyzer (heavyweight).

```rust
#[rustible::vars]
struct Vars {
    user: String,          // required
    fruit: String,         // required
    port: Option<u16>,     // optional
    #[default = 3]
    retries: u32,          // defaulted
}

#[rustible::playbook(hosts = "myservers", vars = Vars, become = true)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> { .. }
```

```
error: 2 of 3 hosts in group `myservers` do not satisfy the vars of playbooks/thing.rs

  host `a`   ok
  host `b`   missing required var `user`
  host `c`   missing required var `user`

  Vars are resolved from hosts.kdl as: vars -> group vars -> host vars.
  Add `user` to hosts b and c, or to group "myservers" vars if it is shared.
```

**Precedence** (four levels, versus Ansible's twenty-two): top-level `vars` (all), then
group vars outermost to innermost, then host vars, then `--var key=value` on the
command line. **OPEN:** when a host belongs to two sibling groups that define the
same var, error (leaning) or last-declared wins.

### 13.4 Workspace

- A **rustible workspace** is a Cargo package whose root also contains
  `rustible.toml`. (`rustible.toml`, not `rustible.cfg`: same format as
  everything else, sits next to `Cargo.toml`.)
- `rustible.toml` (TOML; flat config) points at the inventory file (default
  `hosts.kdl`) and holds workspace-level settings defined later.
- The CLI finds the workspace root by **walking up from the current directory**
  looking for `rustible.toml`, as Cargo does with `Cargo.toml`. `--workspace
  <dir>` overrides this, for monorepos where the workspace lives in a subfolder.
- Everything is resolved relative to the workspace root: playbook paths,
  inventory, files to embed or stream.
- `rustible init` ensures a `.gitignore` exists (creating or appending if inside
  a git repository) containing `target/` and `.rustible/` (the local cache for
  per-triple artifacts and describe output), so playbook runs never produce git
  noise.

## 14. `Ctx` (DECIDED 2026-09-06)

`Ctx` is what `main` receives. Three tiers: MVP, cheap extras, and reserved
shapes whose signatures and protocol frames exist now so adding them later
changes no playbook.

```rust
pub struct Ctx {
    sys: System,            // Local backend, facts, identity, check_mode
    host: HostInfo,         // from the Start frame
    channel: Channel,       // framed up/down link to the orchestrator
    step_counter: u32,
    section_depth: u8,
}

impl Ctx {
    // ---- tier 1: MVP ----
    pub fn step<O: Op>(&mut self, name: impl Into<String>, op: O) -> Result<Applied<O::Output>>;
    pub fn host(&self) -> &HostInfo;          // name, groups, target-relevant params
    pub fn facts(&self) -> &Facts;
    pub fn check_mode(&self) -> bool;
    pub fn log(&self, msg);   pub fn warn(&self, msg);   pub fn debug(&self, msg);  // Log frames
    pub fn local_file(&mut self, path) -> Result<PathBuf>;   // streamed; temp path; deleted at exit
    pub fn local_secret(&mut self, name) -> Result<Secret>;  // bytes in memory; zeroized on drop
    pub fn sys(&self) -> &System;             // reads fine; mutations should be steps

    // ---- tier 2 ----
    pub fn skip(&mut self, name, reason);     // record a deliberately-not-run step
    pub fn section<T>(&mut self, name, f: impl FnOnce(&mut Ctx) -> Result<T>) -> Result<T>; // output grouping
    pub fn as_user(&self, name: &str) -> Ctx; // same channel/host, different identity
    pub fn as_root(&self) -> Ctx;             // sugar for as_user("root")
    pub fn fetch(&mut self, remote, local_dest) -> Result<()>;  // reverse transfer

    // ---- tier 3: reserved, not MVP ----
    pub fn barrier(&mut self, name: &str) -> Result<()>;                       // blocks until all hosts arrive
    pub fn run_once<T>(&mut self, name: &str, f: impl FnOnce(&mut Ctx) -> Result<T>) -> Result<Option<T>>;
    pub fn peer_facts(&mut self, host: &str) -> Result<Facts>;
}
```

### 14.1 Decisions embedded

- **`vars` is a parameter of `main`** (`fn main(ctx: &mut Ctx, vars: Vars)`), not
  a method on `Ctx`. The macro deserializes it from the `Start` frame before
  `main` runs; failure is a clean per-host message. `Ctx` is untyped w.r.t. vars.
- **`sys()` is exposed.** Playbooks are programs; authors are responsible. Reads
  are unreported. Mutations outside a step still go through the backend (logged
  at `-v`) but do not appear as steps; if it should be in the report, make it a
  step (`shell::Command` exists for that).
- **`skip` is explicit and optional.** An `if` that does not run a step makes it
  vanish from the output; `skip` records it with a reason and the summary counts
  it, for the "twelve steps, three did not apply, here is why" report.
- **`section` is output-only** grouping (indented block under a heading), so a
  collection helper emitting several steps reads as one unit. Sections nest.
- **`bail!`** (re-exported) is Ansible's `fail` module.
- **Tier 3 calls are blocking calls over the channel** (remote-brain): `barrier`
  sends a frame up and waits for `BarrierRelease`; `run_once` is a barrier plus
  an election by the orchestrator. Reserved, not implemented in the MVP.

### 14.2 Example

```rust
#[rustible::vars]
struct Vars { domain: String, #[default = 4] workers: u32 }

#[rustible::playbook(hosts = "web", vars = Vars, become = true)]
fn main(ctx: &mut Ctx, vars: Vars) -> Result<()> {
    if ctx.facts().package_manager != Pm::Apt {
        bail!("this playbook only knows Debian-likes, got {:?}", ctx.facts().distro);
    }
    ctx.step("nginx present", apt::Present::new(["nginx"]))?;

    let cfg = ctx.section("Configure nginx", |ctx| {
        let conf = ctx.step("Render site config",
            file::Template::render(Site { domain: &vars.domain, workers: vars.workers })
                .to("/etc/nginx/sites-available/app"))?;
        ctx.step("Enable site",
            file::Symlink::at("/etc/nginx/sites-enabled/app").pointing_to("/etc/nginx/sites-available/app"))?;
        Ok(conf)
    })?;

    if cfg.changed { ctx.step("nginx restarted", systemd::Restart::new("nginx"))?; }
    else { ctx.skip("nginx restarted", "config unchanged"); }

    if ctx.facts().cpus < 2 { ctx.warn("single-CPU host, workers setting will be ignored"); }

    let cert = ctx.local_secret("tls/app.pem")?;
    ctx.step("Install TLS cert", file::Copy::from_bytes(cert.as_bytes()).to("/etc/ssl/app.pem").mode(0o600))?;
    Ok(())
}
```

### 14.3 Per-step privilege escalation (DECIDED 2026-09-06)

Escalation is a property of how a step runs, not of the op, so it lives on
`Ctx`: `ctx.as_root().step(..)`, or bind `let root = ctx.as_root();` for several
steps, or `ctx.as_user("postgres").step(..)` to step down. Playbook-level
`become = true` remains for the common case and means the binary is launched
under sudo (default identity root). Output marks steps whose identity differs
from the binary's own (`as root`, `as postgres`).

**Mechanism.** A running process cannot change identity per call, and
`sys.write_atomic("/etc/...")` from an unprivileged process gets EACCES.
Ansible's answer is shell tricks (`sudo tee`, chmod dances). Ours: `as_user`
creates a `System` whose backend is `Elevated { user }`. On first use it spawns
**the same binary** under sudo in helper mode:

```
sudo -n -u <user> /tmp/.rustible/<hash> --helper
```

and speaks the `Backend` primitives (`read`, `write`, `stat`, `spawn`) to it over
its stdin/stdout, framed like the main channel. The helper is `Local` wrapped in
a request loop. One helper per identity, spawned lazily, kept alive for the run,
killed at exit. Properties:

- No extra upload: the helper is the binary already on the target.
- Ops know nothing: this is the payoff of routing all I/O through `sys`.
- Stepping down (`as_user("postgres")`) is the same mechanism.
- The check-mode mutation guard holds in the helper (same code).
- Helper commands are reported back and forwarded up as `CmdRan` tagged with
  the identity.
- Cost: one spawn per identity, then a pipe round trip per file primitive.

Sudo passwords: `-n` fails rather than prompts. If the inventory's `become`
needs a password, the orchestrator sends it in the `Start` frame as a secret
and the helper spawn uses `sudo -S`. In memory only, zeroized after use.

## 15. Check-mode semantics (DECIDED 2026-09-06)

Problem: `Plan::Satisfied(T)` carries an output, `Plan::Change { diff }` does
not, so in a dry run a step that *would* change has nothing to return, and a
later step that chains from it has no value.

Options considered:
1. Stop the host at the first would-change step. Honest but shows only the
   first change; useless for "what would this playbook do". Rejected.
2. Continue; the output is unavailable; fail loudly only when a later step
   actually reads it.
3. Let ops predict their output (`Plan::Change { diff, predicted: Option<T> }`).
   Most fidelity, more work per op, and a wrong prediction is a lie in a dry run.

**Decision: 2 as the rule, 3 as opt-in.**
- In check mode, a would-change step reports `WouldChange` with its diff and
  the run continues.
- `Applied<T>` holds `Option<T>` internally in check mode. Reading the output
  of a would-change step (via `Deref`) fails with: "step `<name>` would have
  changed; its output is unavailable in check mode". `.changed` and `.diff`
  remain readable. Playbooks that do not chain get a full dry run; those that
  chain get as far as the first dependent read, with a clear message.
- Ops that can predict cheaply may set `predicted` in `Plan::Change`
  (`user::Present` can predict name, home, shell; not uid). Chained steps then
  continue, and the report marks the step's output as predicted.
- Ansible effectively does option 2 with silent garbage instead of a loud error.
