# Rustible: Vision and Architecture Decisions

**Status:** living document, first written 2026-09-05, restructured 2026-09-06
for vetting. **Vetted by Cadu: pending.**
**Purpose:** this is the handoff. If every other context is lost, a reader of this
document should be able to continue designing and building Rustible without
re-deriving or re-arguing anything below. Every decision records the alternatives
that were considered and why they lost. Sections marked **OPEN** are not yet
decided; section 16 lists all of them with the milestone by which each must be
settled.

---

## 0. Where the project is, and how to read this document

**Phase.** Design and spiking are complete as of 2026-09-06. Every architectural
question has a decision below together with the alternatives it beat. Three
spikes validated the design in running code across two machines and two CPU
architectures (section 17). The next phase is **building the real thing**
against a milestone plan, `docs/05_BUILD_PLAN.md`, which is written only after
this document has been vetted.

**The two modes, and their rules.**

| | Spiking (done) | Building (next) |
|---|---|---|
| Goal | Learn, then write it down | Ship code that stays |
| Code quality | Throwaway and stand-ins allowed | Tests and docs in the same commit; no stand-ins |
| Where decisions land | This doc and the spike docs | This doc first (as an amendment), then code |
| Trying things | On `main` | On a branch |
| Scope unit | One question | One milestone with a done condition |

**What is in the tree today** (`crates/`), classified honestly:

- **Keepers**, written to this design and tested: `Op`, `Plan`, `Change`,
  `Applied`, `System` over `Backend` with `Local` and `Fake`, `Facts` with real
  probes, `Diff`, the event model, the protocol codec, and the ops `file::Line`,
  `file::Directory`, `apt::Present`, `shell::Command`. 14 unit tests, clippy
  clean, edition 2024.
- **Stand-ins** the real thing replaces: `rustible_sdk::runtime::run` and its
  flag parsing (the `#[playbook]` macro replaces it), the structured error enum
  in `error.rs` (section 14 replaces it), `Ctx::local_file` (a local-path stub),
  the `Pretty` renderer living in the SDK (moves to the CLI), and the whole
  `crates/rustible` orchestrator (hardcoded package, hosts on the command line,
  `$HOME` resolved through `sh`).
- **Throwaway**: `crates/spike-playbook`. Deleted once a real workspace can run
  playbooks.

**How to read.** Sections 1 to 4 are context. Sections 5 to 14 are the
decisions, each with alternatives and reasons. Section 15 is the glossary,
section 16 the open questions with decide-by milestones, section 17 the spike
history. The spike documents `02` to `04` are frozen history: if one of them
disagrees with this document, this document wins.

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

A Rustible project (or a "Rustible workspace") is a Cargo package. Playbooks are ordinary Rust files. Operations
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
$ rustible playbook create ./playbooks/cadu/ssh_enable_root_user.rs
```
Scaffolds a playbook file with a `main` function and the metadata attribute.

```
$ rustible playbook run ./playbooks/cadu/ssh_enable_root_user.rs [--check] [-v|-vv] [--var key=value]
```
Reads the playbook's metadata (target hosts), validates the inventory vars
against the playbook's typed struct, probes the hosts, compiles per
architecture, uploads, runs, and renders progress. See section 5.2 for the
pipeline. `--check` is dry-run; `-v` shows diffs, `-vv` shows every command.

```
$ rustible inventory show web2      # resolved parameters and vars, with their source
$ rustible inventory check          # validate hosts.kdl, and vars against every playbook
```

**Verb order (decided 2026-09-07): noun first, then verb.** `rustible playbook
run`, `rustible playbook create`, `rustible inventory show`, `rustible
inventory check`. The subject comes first, like `gh pr create`; the earlier
`rustible run playbook` form is not used.

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

### 5.2 Run pipeline (settled by spikes 1 and 2)

`rustible playbook run <file>` does, in order:

1. **Locate the workspace** by walking up from the current directory to the
   nearest `rustible.toml` (section 10.4), load `hosts.kdl`.
2. **Read playbook metadata** by doing a host-native debug build of the playbook
   and running it with `--describe`, which the `#[playbook]` macro generates.
   This yields the target hosts, `escalate`, and a JSON schema of the typed vars
   struct (section 10.3). **Accepted trade-off (final, 2026-09-06):** the
   pre-check costs a cold build the first time (about a minute) and seconds
   afterwards. In exchange the schema comes from the real compiled types, so
   there is no source parser of our own to maintain and no restriction on var
   field types. (Alternative considered twice: parse the source with `syn`. It is
   instant but only sound for a closed set of canonically spelled types, cannot
   see through aliases or imports, and needs a second parser kept in sync with
   the proc macro. Rejected.) Mitigations: cache describe output by hash of the
   playbook source plus `Cargo.lock`; dev profile with a shared target dir; the
   describe build shares dependency compilation with the target build for
   same-arch hosts; `rustible inventory check` runs only this step.
3. **Resolve hosts and validate vars.** For every resolved host, merge its vars
   (section 10.3) and check them against the schema. Any failure aborts the
   whole run before anything is compiled or uploaded, naming each host and each
   missing or mistyped var.
4. **Connect** to every host in parallel. SSH uses a ControlMaster session
   opened here and reused for everything after (spike 1 measured 20 s for a
   cold Tailscale connection versus 0.4 s for a warm upload, so the connection
   is the expensive part, not the bytes). `connection="local"` hosts run the
   binary as a child process instead.
5. **Probe** each host with one shell command, `uname -sm`, mapped to a musl
   triple. The real CLI also resolves `$HOME` here so later paths are absolute.
   This bootstrap probe is the only shell-dependent step; everything after it
   is the static binary.
6. **Compile** once for all needed triples in **one cargo invocation**
   (`RUSTIBLE_PLAYBOOK=<name> cargo build --profile dist --target A --target B`;
   the build script includes only that playbook, section 9).
   Cargo accepts several `--target` flags and locks the target directory, so one
   invocation is both simplest and fastest. Per-triple target directories keep
   the caches independent. Use the `dist` profile (section 5.3).
7. **Upload if missing.** SHA-256 the artifact; the target path is
   `~/.cache/rustible/bin/<playbook>-<sha256>`. If `test -x` finds it, skip the
   upload; otherwise stream the bytes through stdin of
   `sh -c 'cat > tmp; chmod 755; mv'`.
8. **Execute** `[sudo -n] <path> --remote` (the prefix when the playbook says
   `escalate = true`, using the host's escalation method), write the `Start`
   frame (section 5.5) to its stdin, read frames from its stdout until EOF,
   capture stderr separately (panics land there), wait for the exit code.
9. **Render** the per-host, per-step view from the event stream as it arrives.
   Facts gathering is the first thing the binary does and is reported as a
   frame. Sub-events between `StepStarted` and `StepFinished` (`CmdRan`, debug
   logs) belong to that step; the renderer buffers them under it.

Measured in spike 2 (x86 dev box, ARM VM over Tailscale, warm ControlMaster):
exec to `Hello` 17 to 21 ms over SSH, 5 ms locally; a whole no-op run on the
ARM VM 30 ms; two hosts on two architectures, warm, 345 ms total. Orchestrator
overhead is negligible next to the ops themselves (apt installing one package
took 4.5 s).

### 5.3 Cross-compilation constraints (DECIDED for MVP)

- **Linux only, `*-unknown-linux-musl` targets only**, for the MVP. Static musl
  binaries run on any Linux regardless of libc version.
- **Rustible is pure Rust, all the way down the dependency tree (DECIDED
  2026-09-07).** Playbooks, the SDK, the stdlib, and every collection are Rust
  crates whose transitive dependencies contain no C code. Pure-Rust crates
  targeting musl link with the bundled `rust-lld` after `rustup target add`,
  with `linker = "rust-lld"` and `-C link-self-contained=yes` set per target in
  `.cargo/config.toml` (validated in spike 1, `docs/03_SPIKE_CROSS_COMPILE.md`:
  3.5 s link, no zig, no distro toolchain). A crate that bundles C under the
  hood (`openssl-sys`, `libgit2-sys`, `libsqlite3-sys`) is **unsupported**: the
  link fails, and the fix is the pure-Rust alternative (`rustls`, `gix`,
  `rustix`). There is no escape hatch and no C cross-toolchain story, on
  purpose: Ansible never had C modules either, and one rule is simpler than a
  toolchain matrix. (`cargo-zigbuild` was considered as an escape hatch and
  dropped.)
- **Shipped binaries use the `dist` profile** (strip, fat LTO, `opt-level = "z"`):
  1.4 MB for the spike playbook on aarch64 versus 3.1 MB for plain release.
- macOS and Windows targets are deferred. They have their own toolchain and SDK
  requirements.

Environment facts recorded 2026-09-05 on the primary dev box: rustc 1.97.1,
targets installed: `x86_64-unknown-linux-gnu`, `x86_64-unknown-linux-musl`, and
since 2026-09-06 `aarch64-unknown-linux-musl`. No zig, no `cross`, no sccache.
Docker present. The ARM Linux VM (`cadu-cogram-vm-arm`, Ubuntu 24.04 aarch64,
reachable via Tailscale, passwordless SSH as `cadu`) is the aarch64 test target.

### 5.4 Transport (DECIDED for MVP)

Use the system `ssh` binary via the `openssh` crate (ControlMaster multiplexing).
This inherits `~/.ssh/config`, ssh-agent, jump hosts, ProxyCommand, and 2FA for
free, and is what Ansible itself does. `russh` (pure Rust SSH) is the later option
for zero external dependencies. "Local" is the other transport (run the binary on
the orchestrator machine itself).

The binary on the target never opens a socket. Everything rides the SSH session
the orchestrator established, so firewalling and authentication are entirely
SSH's concern.

### 5.5 Protocol (DECIDED; in use since spike 2)

**Framing.** A u32 big-endian length followed by a JSON body, on the binary's
stdin (down) and stdout (up). Stderr stays raw for panics and crash output.
JSON was chosen over postcard or msgpack because the frames are small, it is
debuggable with `--json` locally, and the codec is one function on each side
if that ever changes. Nothing else may write to stdout in `--remote` mode.

**Frames in use:**

```rust
enum Down {
    Start { run_id, host: HostInfo, vars: serde_json::Value, check_mode: bool, verbosity: u8 },
    Cancel,                                    // ctrl-c on the orchestrator (handling: M5)
}

enum Up {
    Hello { protocol: u32, playbook: String },  // first frame; playbook name comes from the macro, not argv[0]
    Event(Event),
}

enum Event {
    Facts(Facts),
    SectionStarted { depth, name },  SectionFinished { depth, name },
    StepStarted  { id, depth, name, identity },
    StepFinished { id, depth, name, identity, status: Status, diff: Option<Diff>, note: Option<String>, elapsed_ms },
    StepSkipped  { id, depth, name, reason },
    Log { level: Debug | Info | Warn, msg },
    CmdRan { identity, argv: Vec<String>, status: i32, elapsed_ms },   // rendered at -vv
    Failed { step: Option<String>, error: String },
    Finished(Summary),   // ok, changed, would_change, skipped, failed, warnings
}

enum Status { Ok, Changed, WouldChange, Skipped, Failed }
```

`identity` is `"self"` or the user a step ran as (`as root`), so the renderer
can mark escalated steps. `note` carries short hints such as `action` for
always-changing ops.

**Frames reserved, not yet implemented** (the signatures exist so adding them
changes no playbook):

```rust
// Down
FileChunk { req: u32, offset: u64, bytes: Vec<u8>, last: bool }   // answers FileRequest
FileDenied { req: u32, reason: String }
BarrierRelease { name }, PeerFacts { host, facts }                // section 11 tier 3
// Up
FileRequest { req: u32, path: String }                             // "send me files/x"
FetchChunk  { req: u32, dest: String, offset: u64, bytes: Vec<u8>, last: bool }
BarrierWait { name }
```

Requests are correlated by id and may overlap: the binary can request a file
while a step runs, and the orchestrator can serve several hosts from one local
read.

**Protocol versioning.** `Hello.protocol` is a constant bumped on incompatible
change. Beyond that, the binary is built from the same dependency graph the
orchestrator knows, so a hash of the SDK and op-crate set can serve as the
protocol identity; a mismatch means "rebuild", never silent breakage.

**The binary's modes.** One playbook binary answers to four flags: none
(local pretty run, for development), `--remote` (driven by an orchestrator),
`--describe` (print metadata and vars schema as JSON, section 5.2), and
`--helper` (serve `Backend` primitives to a sibling process, section 11.3).

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
use rustible_std::ssh::authorized_keys;
use rustible_std::{file, user};

#[rustible::playbook(hosts = "local", escalate = true)]
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
        authorized_keys::Present::for_user(&account).keys(keys).exclusive(true),
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
  or group from the inventory), `escalate`, and later things like `serial`. The
  macro wraps `main` with the runtime that speaks the protocol.
- `escalate = true` (Ansible's `become`; see section 16 for the name) means
  the binary is launched under `sudo` on the target. Per-step escalation is
  `ctx.as_root()` (section 11.3).
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
    /// Perform the change described by the plan. Only called when `check`
    /// returned `Change` and we are not in check mode.
    fn apply(&self, sys: &System, change: Change<Self::Output>) -> Result<Self::Output>;
}

pub enum Plan<T> {
    /// Already in desired state. Carries the output so `step` can return it without applying.
    Satisfied(T),
    /// Something must change.
    Change(Change<T>),
}

/// `diff` is what the report shows; `predicted` (opt-in, section 12) is what
/// the output would be after apply, so chained steps continue in check mode.
pub struct Change<T> { pub diff: Diff, pub predicted: Option<T> }
// (Spike 3 finding: `apply` takes `Change<T>` rather than `Plan<T>`; see docs/02_SPIKE_SDK_CORE.md.)

pub struct Applied<T> {
    value: Option<T>,        // None only in check mode when the op did not predict (section 12)
    pub changed: bool,
    pub predicted: bool,     // value is a prediction
    pub diff: Option<Diff>,
    pub elapsed: Duration,
}
// Applied<T> derefs to T, so `account.home` works and `account.changed` is there too.
// `.output() -> Result<&T>` and `.is_available()` exist for the check-mode case.

// Optional trait method with a default of `false`; a reporting hint only (section 6.4).
fn always_changes(&self) -> bool { false }
```

The step driver, in outline (this is what `crates/rustible-sdk/src/ctx.rs` does):

```rust
pub fn step<O: Op>(&mut self, name, op: O) -> Result<Applied<O::Output>> {
    emit(StepStarted { .. });
    sys.set_phase(Checking);   let plan = op.check(&sys);   sys.set_phase(Idle);
    match plan? {
        Plan::Satisfied(out) => { emit(StepFinished { status: Ok }); Applied { value: Some(out), changed: false, .. } }
        Plan::Change(c) if sys.check_mode() => {
            emit(StepFinished { status: WouldChange, diff: c.diff });
            Applied { value: c.predicted, changed: true, predicted: c.predicted.is_some(), .. }   // no apply
        }
        Plan::Change(c) => {
            sys.set_phase(Applying);   let out = op.apply(&sys, c)?;   sys.set_phase(Idle);
            emit(StepFinished { status: Changed, diff });
            Applied { value: Some(out), changed: true, .. }
        }
    }
}
```

`check` does all the thinking and produces the `Diff`; `apply` executes that
diff. This makes dry-run trustworthy: the diff shown in check mode is exactly the
change that would be applied. The phase markers are what lets `System` refuse
file mutations during `check` (section 7.3).

**Policies learned in spike 3, now rules for the stdlib:**
- **Predict by default.** Both ops in the spike already computed their
  post-apply output while planning, so `Plan::change_predicting(diff, output)` 
  cost nothing. Every stdlib op predicts unless it genuinely cannot (uid
  allocation, versions apt has not resolved yet). `apply` may reuse
  `change.predicted` for what it cannot cheaply recompute.
- **Builders end in a finishing call for the one mandatory piece of desired
  state.** `Line::in_path(p).matching(re).backup(true).set(line)`: `set` returns
  the `Op`, so a `Line` without a line cannot be constructed.
- **`apply` receives `Change<T>`, not `Plan<T>`.** Only the change branch is
  meaningful there; passing `Plan` forced a pointless match.

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
| `authorized_key: state=present` | `ssh::authorized_keys::Present`        |
| `authorized_key: state=absent`  | `ssh::authorized_keys::Absent`         |
| `getent`/`register`        | `user::Existing` (read-only op, 13.1)      |

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

Read-only lookups are ops too (`user::Existing::named("rustible")`, see 13.1). They run
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
#[rustible::playbook(hosts = "local", escalate = true)]
fn main(ctx: &mut Ctx) -> Result<()> {
    let revoked = ["ssh-ed25519 AAAAC3...OLD1 cadu@laptop-2023", "ssh-ed25519 AAAAC3...OLD2 ci@jenkins"];
    let groups = ["docker", "systemd-journal", "adm"];

    let account = ctx.step("Look up rustible user", user::Existing::named("rustible"))?;

    let keys = ctx.step("Revoke compromised keys",
        authorized_keys::Absent::for_user(&account).keys(revoked))?;
    ctx.log(format!("removed {} key(s)", keys.removed.len()));

    for name in groups {
        let grp = ctx.step(format!("Ensure group {name} exists"), group::Present::new(name))?;
        ctx.step(format!("Add rustible to {name}"), user::Membership::of(&account).in_group(&grp))?;
    }
    Ok(())
}
```

Note the three shapes on one resource, following rule 6.3: `authorized_keys::Present`
("ensure these"), `authorized_keys::Present ... .exclusive(true)` ("ensure exactly
these": still the present state, with the option of removing strangers, and the
output gains a `removed` list), and `authorized_keys::Absent` ("ensure not these").
An earlier draft had `.remove(keys)` as a method on one type, which was the
state-as-parameter shape 6.3 rejects; corrected 2026-09-07 during vetting.

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
  `Local` (or `Elevated`, section 11.3, which proxies to a `Local` in a helper
  process). (In a local-brain design SSH would have been a backend; this is one
  of the payoffs of remote-brain.)
- **The mutation guard covers files, not commands** (spike 3). `sys.cmd()` must
  work inside `check` (`dpkg-query`, `systemctl is-enabled`), so nothing can stop
  a `check` that runs `apt-get install`. File mutations error with
  `MutationDuringCheck`; process honesty is the op author's. Verified by a test
  in which a deliberately bad op writes in `check` and is refused.
- **`cmd().run()` errors on non-zero exit** unless `.allow_failure()`; `.ok()`
  returns `Option<Output>` for "does this succeed" probes. Every spawn is
  reported as a `CmdRan` event with the identity it ran as.
- **Test constructors**: `System::fake(Arc<Fake>, sink)` with plausible Debian
  facts, `.with_facts(..)`, `.with_check_mode(..)`. `Fake::with_file`,
  `with_cmd(program, Some(exact_args) | None, status, stdout)`, and `argvs()` to
  assert what ran.
- Not yet built from the sketch: `tempdir`, `ensure_attrs`, the `Elevated`
  backend (M5).

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
- **A playbook is any `.rs` file under `playbooks/` that carries
  `#[rustible::playbook]`. A build script finds them. Nothing is ever indexed
  by hand (DECIDED 2026-09-07).** The requirement, from vetting: the Ansible
  experience, where a playbook file simply exists and gets used, with no
  manifest entry to add on create or remove on delete, while keeping full
  rust-analyzer, clippy, types, and completion in every playbook file.

  **Mechanism.** The workspace package has one bin target, `src/main.rs`,
  written once by `rustible init` and never edited, and a `build.rs`, also
  generated once. The build script walks `playbooks/**/*.rs`, parses each
  file with `syn` (milliseconds per file; immune to the attribute appearing in
  a comment or string), and for every file containing a function marked
  `#[rustible::playbook(..)]` writes into `OUT_DIR` a
  `#[path = "<abs path>"] mod <name>;` line plus a registry entry
  `("cadu/x", entry)`. `src/main.rs` is essentially
  `include!(concat!(env!("OUT_DIR"), "/playbooks.rs"))` plus the SDK's
  dispatcher, which picks the entry by name and speaks the protocol. Files
  without the marker are not playbooks: they are ignored unless a playbook
  pulls them in with `mod helpers;` or `#[path]`, so helper code may live
  next to playbooks. Two marked functions in one file is a build error naming
  the file. The same scan backs `rustible playbook list`.

  Two Cargo behaviours make this work, both verified in a scratch project on
  2026-09-07: `cargo:rerun-if-changed=playbooks` on a *directory* makes Cargo
  rescan the whole tree, so new and deleted files are picked up on the next
  build with no edits anywhere; and rust-analyzer runs build scripts and
  resolves `OUT_DIR` includes, so `#[path]` modules get full IDE support.

  **Isolation.** The CLI sets `RUSTIBLE_PLAYBOOK=cadu/x` when building for a
  run, and the build script (with `rerun-if-env-changed`) includes only that
  playbook. Verified: a playbook with a type error elsewhere in the tree does
  not affect `rustible playbook run` of another one. The shipped binary
  contains exactly one playbook, stays small, and is hashed per playbook for
  the target-side cache. With the variable unset, as in the IDE, `cargo
  check`, and CI, every playbook is included, so every broken playbook is
  visible while editing and fails CI, which is the desired behaviour, not a
  wart. Switching playbooks between runs re-runs the build script and
  recompiles the bin crate, the same cost as editing a playbook.

  **Consequences for playbook files.** A playbook file is a module, not a
  crate root: `use rustible::prelude::*;` works, `#[rustible::vars] struct
  Vars` is module-local so every playbook may have its own, and the
  `#[rustible::playbook]` attribute on `fn main` registers an entry rather than
  defining the process entry point. `mod helpers;` inside `playbooks/cadu/x.rs`
  resolves to `playbooks/cadu/x/helpers.rs`. Code shared across playbooks
  lives in the package's `src/lib.rs`. A playbook's name is its path under
  `playbooks/` without the extension (`cadu/x`); generated module identifiers
  carry `#[allow(non_snake_case)]`. An unmarked file nobody references is
  silently ignored (rust-analyzer greys it out); `rustible playbook list` may
  warn about such orphans.

  **Alternatives considered and rejected:**
  - Syncing `[[bin]]` entries (the previous plan): a maintenance chore on every
    create and delete, the opposite of "files simply get used".
  - `src/bin/` auto-discovery: no manifest edits and full IDE, but the folder
    must be `src/bin/` and discovery is one level deep, so no `cadu/x.rs`.
  - Workspace member glob with a folder and a five-line `Cargo.toml` per
    playbook: full isolation, but the manifest-per-playbook chore returns and
    adding a collection means editing every playbook's manifest.
  - Group packages (`playbooks/cadu/` as a package with `src/bin/` inside):
    isolation even for a bare `cargo build`, but `src/bin/` in the middle of
    every path and one manifest per group.
  - A shadow package generated per run: same run isolation as the build
    script, but the IDE still needs the build-script modules, so two
    mechanisms and a double compile.
  - Persistent shadow packages as workspace members: total isolation, but a
    missing regeneration (fresh clone, manual delete) breaks the whole
    workspace until `rustible playbook sync` runs.
  - `cargo script` single-file packages: closest to "just a file", but still
    nightly-only (`requires -Zscript` on cargo 1.97.1), one dependency cache
    per playbook, partial rust-analyzer support.
- **Inventory is data**, in `hosts.kdl` next to `rustible.toml`. See section 10.
  Dynamic inventories become a trait later.
- **Crates (DECIDED 2026-09-07):**
  - `rustible`: a **thin facade library** that playbook workspaces depend on.
    Re-exports the SDK, the macros, and the std prelude, so a playbook is
    `use rustible::prelude::*;` and `#[rustible::playbook(..)]`. No logic of
    its own. `rustible init` adds this one dependency plus `rustible-std`.
  - `rustible-cli`: the CLI and orchestrator (init, playbook run/create,
    inventory show/check, SSH, compile, render), installed with
    `cargo install rustible-cli`, binary named `rustible`. Never a dependency of
    a workspace: it would drag tokio and openssh into every cross-compiled
    playbook. The `Pretty` renderer belongs here, not in the SDK.
  - `rustible-sdk`: `Op`, `Plan`, `Change`, `Applied`, `System`, `Backend`,
    `Local`, `Fake`, `Facts`, `Diff`, `Ctx`, the event and protocol types, the
    runtime that the macro expands into, the test harness. Everything a
    collection author needs. Collections depend on this directly.
  - `rustible-macros`: the `#[playbook]` and `#[vars]` proc macros. Proc macros
    must live in their own crate; the facade re-exports them so users never
    name it.
  - `rustible-std`: the base operations mirroring Ansible builtins, itself just a
    consumer of `rustible-sdk`.
  - Third-party collections (`rustible-docker`, ...): plain crates on
    `rustible-sdk`, published to crates.io, added with `cargo add`. Because
    playbooks link them directly, no registration mechanism is needed.
  - The spike orchestrator currently at `crates/rustible` is renamed to
    `crates/rustible-cli` at M1, freeing the name for the facade.
- **A playbook binary has four modes** (section 5.5): plain local run,
  `--remote`, `--describe`, `--helper`. The macro generates all of them.
- Every crate in the tree is pure Rust, transitively (section 5.3).

## 10. Inventory, typed vars, and the workspace (DECIDED 2026-09-05/06)

### 10.1 Structure

The inventory stores single hosts and host groups. Groups can contain groups
(`members = [..]`); a host's group set is the transitive closure, and vars merge
from outermost group to innermost.

**Connection settings are not vars.** `addr`, `port`, `ssh_user`, `connection`
(`ssh` | `local`), and the escalation method are orchestrator configuration and
live directly on the host or group. Playbook vars live under a separate `vars` table.
Ansible mixes these (`ansible_host`, `ansible_user`) and that is a source of its
precedence confusion.

### 10.2 Format: KDL for the inventory, TOML for `rustible.toml` (DECIDED 2026-09-06)

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

### 10.2.1 Parameters versus vars

Two kinds of data with two syntactic homes so they cannot be confused:

- **Parameters** are the fixed, typed, closed set `rustible` itself understands
  (how to connect, how to escalate). They are **properties on the node**
  (`key=value` after the name). Misspelling one is a load-time error with a
  suggestion.
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
| `escalate` | `"sudo"` \| `"doas"` \| `"none"` | no | `"sudo"` | host, group, defaults |
| `escalate_user` | string | no | `"root"` | host, group, defaults |
| `ssh_args` | list of strings | no | empty | host, group, defaults |

Parameter resolution: host, then nearest group outward, then `defaults`, then
the built-in default. Parameters never come from `vars` and vars never from
properties. On the orchestrator side parameters deserialize into a `HostParams`
struct via serde.

### 10.2.2 Full example

```kdl
// hosts.kdl  (KDL 2.0)
// Nodes: vars, defaults, host, group.

vars {                                  // workspace-wide vars: the "all" level
    fruit "banana"
    timezone "America/Sao_Paulo"
}

defaults ssh_user="cadu" port=22 escalate="sudo"   // workspace-wide parameters

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

### 10.2.3 Structural rules

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

### 10.3 Typed vars: bridging the untyped bag and the typed playbook

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

#[rustible::playbook(hosts = "myservers", vars = Vars, escalate = true)]
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
command line. **Sibling-group conflicts are an error** (decided 2026-09-06): when
a host belongs to two groups at the same distance that both define a var and
the host does not override it, loading fails naming both groups and the host,
with the fix being "set it on the host or on a common parent". Consistent with
everything else here failing loudly before a run.

### 10.4 Workspace

- A **rustible workspace** is a Cargo package whose root also contains
  `rustible.toml`, with the generated `src/main.rs` and `build.rs` from
  section 9 and a `src/lib.rs` for code shared across playbooks. (`rustible.toml`, not `rustible.cfg`: same format as
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

## 11. `Ctx` (DECIDED 2026-09-06)

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
    pub fn as_root(&self) -> Ctx;             // literally as_user("root"), never follows the inventory
    pub fn as_escalated(&self) -> Ctx;        // as_user(host.escalate_user): the inventory's privileged account
    pub fn fetch(&mut self, remote, local_dest) -> Result<()>;  // reverse transfer

    // ---- tier 3: reserved, not MVP ----
    pub fn barrier(&mut self, name: &str) -> Result<()>;                       // blocks until all hosts arrive
    pub fn run_once<T>(&mut self, name: &str, f: impl FnOnce(&mut Ctx) -> Result<T>) -> Result<Option<T>>;
    pub fn peer_facts(&mut self, host: &str) -> Result<Facts>;
}
```

### 11.1 Decisions embedded

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
- **`HostInfo`** (from the `Start` frame) carries `name`, `groups`, and the
  parameters that matter on the target: `escalate_user` (for `as_escalated`)
  and `connection`. Never `addr` or `port`; those are the orchestrator's.
- **`step` numbering and the summary are shared** across `section` and
  `as_user` contexts (they clone a shared counter), so a run has one step
  sequence regardless of how many `Ctx` values exist.
- **In check mode `changed` means "would change".** A playbook that logs after
  a changed step should branch on `applied.predicted` to word it honestly
  (spike 2 caught the `mc` playbook logging "installed mc" in a dry run).

### 11.2 Example

```rust
#[rustible::vars]
struct Vars { domain: String, #[default = 4] workers: u32 }

#[rustible::playbook(hosts = "web", vars = Vars, escalate = true)]
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

### 11.3 Per-step privilege escalation (DECIDED 2026-09-06)

Escalation is a property of how a step runs, not of the op, so it lives on
`Ctx`: `ctx.as_root().step(..)`, or bind `let root = ctx.as_root();` for several
steps, or `ctx.as_user("postgres").step(..)` to step down. Playbook-level
`escalate = true` remains for the common case and means the binary is launched
via the inventory's escalation method as `escalate_user` (default root).

**Three identity methods (DECIDED 2026-09-06):**
- `as_user(name)`: explicit user.
- `as_root()`: literally `as_user("root")`. It never follows the inventory; a
  method named `as_root` that might run as `admin` would be hidden indirection.
- `as_escalated()`: `as_user(host.escalate_user)`, i.e. the privileged account
  the inventory chose for this host (root by default, or a shared admin
  account where direct root is not allowed). This is what `escalate = true`
  uses at launch, exposed per step. Output marks steps whose identity differs
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

Sudo passwords: `-n` fails rather than prompts. If the inventory's `escalate`
needs a password, the orchestrator sends it in the `Start` frame as a secret
and the helper spawn uses `sudo -S`. In memory only, zeroized after use.

## 12. Check-mode semantics (DECIDED 2026-09-06)

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

## 13. Facts (DECIDED 2026-09-06)

**How Ansible does it.** The `setup` module eagerly returns a dict of well over
a hundred keys, taking one to five seconds per host (hence `gather_facts: no`
and `gather_subset`). Extension: "local facts" JSON files in
`/etc/ansible/facts.d/` on the target, and "fact modules" (`package_facts`,
`service_facts`, `docker_host_info`) whose `ansible_facts` result is merged into
the global untyped namespace.

**Insight.** A fact module is a task that reads the system and returns data. In
Rustible that is just an op with typed output run through `ctx.step`. No
registry, no merge, no extension mechanism. `rustible-docker` needs no facts:
`docker::Container::Running` fails with "docker not found" at the step where it
is used.

**Decision: `Facts` is a fixed core struct.** Rule for membership: data that
many *ops* need for their internal decisions, cheap to gather, stable for the
run. Gathered eagerly at startup (about ten reads: `/etc/os-release`, `uname`,
`/proc/cpuinfo`, `/proc/meminfo`, `/proc/1/comm`, PATH checks), milliseconds,
sent up once in the `Facts` frame. No lazy facts, no dynamic facts.

```rust
pub struct Facts {
    pub os: Os,                  // Linux for now
    pub distro: Distro,          // Debian, Ubuntu, Alpine, Fedora, Rhel, Arch, Other(String)
    pub distro_version: String,  // "12", "24.04", "3.20"
    pub arch: Arch,              // X86_64, Aarch64, Other(String)
    pub kernel: String,
    pub hostname: String,
    pub package_manager: Pm,     // Apt, Dnf, Apk, Pacman, Zypper, Other(String)
    pub init: Init,              // Systemd, OpenRc, Other(String)
    pub cpus: u32,
    pub memory_mb: u64,
    pub user: String,            // who the binary runs as
    pub is_root: bool,
}
```

Enums carry an `Other(String)` variant so unknown values degrade to strings.
Deliberately excluded from the core: mounts, network interfaces, users and
groups, installed packages, environment. Each is an op when a playbook needs
it. No versioning needed: binary and orchestrator share one dependency set.

### 13.1 Desired-state ops are the lookups

There is no generic `::lookup()`. The desired-state op's typed output already
says what was found: `pkg::Present::new(["python3"])` returns
`already_present` and `installed`, and `.changed` is false when nothing was
done (reported as `ok`, never `skipped`; `skipped` means deliberately not run).

A small minority of **read-only ops** exists for *observe without changing*:
"if docker is installed, configure it" (where `pkg::Present` would install it)
and "this user must already exist, fail otherwise" (`user::Existing`, where
`user::Present` would create it). They appear as named steps with typed
output, never report `changed`, and most resources do not need one.

## 14. Error model (DECIDED 2026-09-06)

**Semantics** are Ansible's: a failed step fails that host and the run
continues on the other hosts. In code that is `?` on `ctx.step`. Ignoring is
`let _ = ctx.step(..)` or `.ok()`; rescue is `if let Err(e) = ctx.step(..)`;
retry is a loop. None of these need to know the error's kind, and no playbook
or op is expected to match on errors.

**Type.** An opaque, `anyhow`-style error with a context chain, wrapping the
`anyhow` crate. `rustible_sdk::Result<T>` is `Result<T, rustible_sdk::Error>`
where `Error` converts from any `std::error::Error` via `?`.

Why not the structured enum from spike 3:
- The drawback is on the *producer* side, not the consumer side. `?` on a
  foreign error (`serde_yaml::Error`, `regex::Error`, anything from a crate
  we do not own) does not compile against an enum unless we wrote a `From`
  for it, so op and playbook authors end up writing `.map_err(..)` on every
  line. A catch-all variant plus a blanket `From<E: std::error::Error>` is
  rejected by coherence when the enum itself implements `std::error::Error`;
  `anyhow`'s design (its `Error` deliberately does not implement that trait)
  is the one shape that makes the blanket conversion legal.
- No context chain: a deep failure renders flat, like Ansible's `msg`.
- The enum would need `#[non_exhaustive]`, which removes exhaustive matching,
  its only advantage, and nobody was going to match anyway.

**Context is optional.** Bare `?` is the norm. The SDK adds the two most
useful layers automatically: `ctx.step` wraps any failure with the step name,
and primitives carry their own detail (`sys.cmd().run()` fails with argv, exit
code, and stderr; file primitives with the path). `.context("installing
{pkg}")` is for ops or playbooks that do several similar things where the raw
error would not say which.

**Typed values inside the chain.** The SDK's own signals remain concrete
types that the orchestrator can `downcast_ref` for rendering:
`MutationDuringCheck { path }`, `OutputUnavailable { step }`,
`CmdFailed { argv, status, stderr }`. Playbooks never need them.

**On the wire.** `Failed { step, error }` carries the rendered chain as text,
plus the structured fields of a `CmdFailed` when present, so `-v` can show
the command and its stderr separately.

Rendered example:

```
[web1]  FAILED at `nginx present`: installing nginx: `apt-get install -y nginx` exited 100
        E: Unable to locate package nginx
```
## 15. Glossary

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
- **Escalate**: Ansible's `become`. Running the binary or a step as another
  user, root by default. Named `escalate` because `become` is a reserved Rust
  keyword (section 16).
- **Section**: `ctx.section(name, |ctx| ..)`, output-only grouping of steps.
- **Skip**: `ctx.skip(name, reason)`, a step deliberately not run, counted in
  the summary.
- **Parameter** (inventory): a connection or escalation setting `rustible`
  itself understands, written as a property on a host or group node.
- **Var** (inventory): a value for the playbook, written only inside a `vars`
  block, delivered to the playbook's typed struct.
- **Workspace**: a Cargo package whose root also holds `rustible.toml` and
  `hosts.kdl`.
- **Transport**: how the orchestrator reaches a host: `local` (child process)
  or `ssh` (system `ssh` via the `openssh` crate).
- **Frame**: one length-prefixed JSON message on the channel.
- **Remote mode / describe mode / helper mode**: the playbook binary's
  `--remote`, `--describe`, `--helper` flags (section 5.5).
- **Predicted**: an op's declared post-apply output, returned in check mode
  instead of applying.

## 16. Open questions, with the milestone by which each is decided

Everything architectural is decided. What remains is local to one crate each.
The verdict column says whether deciding late has a cost.

| # | Question | Decide by | Why it can wait (or cannot) |
|---|---|---|---|
| 1 | ~~Crate naming~~ | decided 2026-09-07 | `rustible` is the facade lib, `rustible-cli` the CLI with binary `rustible` (section 9). |
| 2 | ~~CLI verb order~~ | decided 2026-09-07 | `rustible playbook run`, noun then verb (section 3). |
| 3 | ~~Playbook-to-bin mapping~~ | decided 2026-09-07 | Build-script discovery of files marked `#[rustible::playbook]`, one playbook per shipped binary via `RUSTIBLE_PLAYBOOK` (section 9). |
| 4 | **`rustible init` file layout**: exact files, `rustible.toml` contents, `.gitignore` handling | M4 | It is a generator; nothing depends on it. |
| 5 | **Diff representation**: today `Text`, `Attrs`, `Summary`; more variants for package sets, permissions, services | M6 | Additive; ops construct variants, nobody matches exhaustively. |
| 6 | **Output rendering**: per-host buffering vs live interleaving, verbosity levels, machine-readable mode | M3, then iterate | Orchestrator UX, not API. |
| 7 | **Target-side cache cleanup** for `~/.cache/rustible/bin/` | whenever | Trivial. |
| 8 | **`doas` specifics** for `escalate="doas"` | M5 | Same shape as sudo. |
| 9 | **Docker integration-test harness** shape (section 8) | M6 | Needed once the stdlib grows. |
| 10 | **`Cancel` handling** in the binary (a stdin reader that interrupts between steps) | M5 | Reserved frame exists. |

Decided items that used to live here have moved into their sections: inventory
and vars (10), `Ctx` (11), check mode (12), facts (13), error model (14),
privilege escalation (11.3), protocol format (5.5), the `escalate` name (below).

**The `escalate` name (decided 2026-09-06, revised the same day).** The word
`become` is not used anywhere in Rustible; it is `escalate` everywhere: the
playbook attribute (`escalate = true`), the CLI flag (`--escalate`), the
inventory parameters (`escalate="sudo"`, `escalate_user`), and all code.
Reason: `become` is a reserved Rust keyword (for guaranteed tail calls).
Options weighed: `r#become` internally (ugly all over the code), a
`rustible_become` prefix in the attribute (redundant inside
`rustible::playbook(...)` and the `ansible_*` smell), a different internal name
mapped from `become` in the attribute (two names for one thing). One word
everywhere won. Where `escalate` is defined, a comment says it is Ansible's
`become`.

## 17. Spikes (all done)

1. ~~**Cross-compile**~~: **done 2026-09-06**, see `docs/03_SPIKE_CROSS_COMPILE.md`.
   The spike-3 playbook cross-linked to aarch64 musl with stock rustup plus
   `rust-lld`, ran on the ARM VM with identical behaviour. Cold SSH connection
   (20 s) dominates upload cost, not binary size (0.4 s warm).
2. ~~**Protocol over SSH**~~: **done 2026-09-06**, see `docs/04_SPIKE_PROTOCOL_SSH.md`.
   Orchestrator crate with local and SSH transports, hash-cached upload,
   `Start`/`Hello`/event frames as length-prefixed JSON, `apt::Present` op,
   two hosts on two architectures in one command, 345 ms warm. JSON framing
   is kept for now (finding 7).
3. ~~**SDK core**~~: **done 2026-09-06**, see `docs/02_SPIKE_SDK_CORE.md`. The
   sketches hold; `apply` takes `Change<T>`; prediction is nearly free.

**Spike learnings promoted to policy in this document:** `apply` takes
`Change<T>` and ops predict by default (6.2); builders end in a finishing call
(6.2); the mutation guard covers files not commands (7.3); one cargo
invocation builds all triples, `dist` profile, `rust-lld` per-target linker
config (5.2, 5.3); content-hash cache path and early ControlMaster (5.2); JSON
framing and the four binary modes (5.5); the playbook name comes from the
macro not `argv[0]` (5.5); `changed` means "would change" in check mode (11);
`become` is a reserved keyword (16). The spike docs remain as the record of
how each was found.

