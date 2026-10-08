# Rustible: Vision and Architecture Decisions

**Status:** living document, first written 2026-09-05, restructured 2026-09-06
for vetting. **Vetted by Cadu: pending.**
**Purpose:** this is the handoff. If every other context is lost, a reader of this
document should be able to continue designing and building Rustible without
re-deriving or re-arguing anything below. Every decision records the alternatives
that were considered and why they lost. Section 16 lists what is still open.

---

## 0. Where the project is, and how to read this document

**Where the project is.** `docs/plan/PROGRESS.md` is the current state; this
document is the design. Every architectural question has a decision below
together with the alternatives it beat, three spikes validated the design in
running code before building began (section 17), and `docs/06_BUILD_PLAN.md`
is the milestone plan it was built against.

**The two modes, and their rules.** `docs/06_BUILD_PLAN.md` section 1 spells
out the rules of build mode.

| | Spiking (done) | Building |
|---|---|---|
| Goal | Learn, then write it down | Ship code that stays |
| Code quality | Throwaway and stand-ins allowed | Tests and docs in the same commit; no stand-ins |
| Where decisions land | This doc and the spike docs | This doc first for design (as an amendment), then code |
| Trying things | On `main` | On a branch |
| Scope unit | One question | One milestone with a done condition |

**What this document carries, and what it does not.** Intent, guarantees,
decisions with their reasons, the alternatives they beat and the measurements
that settled them; a tool, a library or a mechanism appears where choosing it
is the decision. It contains no code: no type, trait or signature, no example
playbook or inventory, no rendered output. It says in prose what a thing is
and what must hold, at the level of the idea rather than field by field or
step by step, and points at where the real one lives: the source for a
definition (the `Op` trait in `crates/rustible-sdk/src/op.rs`, say), and for
what a playbook, an inventory or a run looks like, the workspace in
`examples/workspace`, which CI builds and so cannot drift, and
`docs/USING_RUSTIBLE.md`. Types, operations, flags and paths are named inline,
as pointers. A goal the code does not meet yet is marked as such in one
sentence, so that a promise is never read as a fact.

**How to read.** Sections 1 to 4 are context. Sections 5 to 14 are the
decisions, each with alternatives and reasons. Section 15 is the glossary,
section 16 the questions still open, section 17 where the spike reports
live. The spike documents `02` to `04` are frozen history: if one of them
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

The operator meets Rustible through a handful of commands;
`docs/USING_RUSTIBLE.md` (section 5, "The CLI") is how to use them.

**`rustible init`** creates a Cargo package in the current directory or a
chosen folder, depending on `rustible` (the runtime) and `rustible-std` (the
base operations, mirroring Ansible's builtin modules), with an opinionated
layout: an inventory, a `playbooks/` folder and a `.gitignore`. Like `cargo
init`, it refuses only when a file it would write is already there, naming the
clash, and otherwise writes into the directory as it is; an existing
`.gitignore` gains the lines it lacks, because a workspace that does not
ignore `target/` commits build output.

**`rustible playbook create`**, given a path under `playbooks/` such as
`playbooks/ops/ssh_enable_root_user.rs`, scaffolds a playbook file with a
`main` function and the metadata attribute.

**`rustible playbook run`** reads the playbook's metadata, validates the
inventory's vars against it, probes the hosts, compiles per architecture,
uploads, runs, and renders progress (section 5.2). `--check` is a dry run,
`--var` sets a var for the run, and `-v` and `-vv` add detail (section 5.5).

**`rustible inventory show`**, given a host, prints its resolved parameters
and vars with the source of each; **`rustible inventory check`** validates
`hosts.kdl`, and the inventory's vars against every playbook.

**Verb order (decided 2026-09-07): noun first, then verb.** `rustible playbook
run`, `rustible playbook create`, `rustible inventory show`, `rustible
inventory check`. The subject comes first, like `gh pr create`; the earlier
`rustible run playbook` form is not used.

**`cargo add`** of a third-party collection, `rustible-docker` say, is all it
takes to use one: its operations are immediately usable in every playbook of
the project, fully typed.

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
  binary starts, never baked into the binary (see 5.5).
- **Lookups run on the target, not on the controller.** Ansible's
  `lookup('url', ..)`, `lookup('file', ..)`, and friends execute on the machine
  running `ansible-playbook`. In Rustible any op that fetches something (an
  HTTP GET for a GitHub user's SSH keys, for instance) runs inside the binary
  on the target, so the *target* needs the network access, not the operator's
  laptop. This is a known semantic difference, first noticed while surveying
  the dogfooding repository (section 6.9), where eleven `set_fact` tasks fetch
  `https://github.com/<user>.keys`. Controller-side data reaches the target
  only through the two file mechanisms in section 5.6.

### 5.2 Run pipeline (settled by spikes 1 and 2)

`rustible playbook run <file>` does, in order:

1. **Locate the workspace**: the nearest `rustible.toml` above the current
   directory (section 10.4), and its inventory.
2. **Read playbook metadata** from the playbook itself: a host-native build
   run with `--describe` reports its hosts, its escalation and login, and
   a schema of its vars (section 10.3). **Accepted trade-off (final,
   2026-09-06):** this costs a cold build the first time (about a minute)
   and seconds afterwards, and in exchange the schema comes from the real
   compiled types, so there is no parser of our own to keep in sync and no
   restriction on var types. (Parsing the source with `syn` was considered
   twice: instant, but sound only for a closed set of canonically spelled
   types, blind to aliases and imports, and a second parser to maintain.
   Rejected.) The output is cached, keyed on the playbook source and
   `Cargo.lock`, and `rustible inventory check` runs only this step.
3. **Resolve hosts and validate vars** against that schema. Any failure
   aborts the whole run before anything is compiled or uploaded, naming
   each host and each missing or mistyped var.
4. **Connect** to every host in parallel, over one SSH session per host,
   opened here and reused for everything after: spike 1 measured 20 s for a
   cold connection against 0.4 s for a warm upload, so the connection is the
   expensive part, not the bytes. A `connection="local"` host runs the
   binary as a child process instead.
5. **Probe** each host with one shell command, which yields its target
   triple (section 5.3) and who the login is and where its home is, so
   later paths are absolute. This bootstrap probe is the only
   shell-dependent step; everything after it is the static binary.
6. **Compile** once for all needed triples in **one cargo invocation**,
   building only that playbook without disturbing the editor's build
   (section 9), with the `dist` profile (section 5.3). Cargo accepts several
   targets and locks the target directory, so one invocation is both
   simplest and fastest.
7. **Upload if missing.** The binary is cached on the target under its
   hash, in `~/.cache/rustible/bin/`, so an unchanged playbook is not sent
   again, and an upload lands whole or not at all.
8. **Execute** the binary in `--remote` mode, escalated when the playbook
   says `escalate = true`, and speak the protocol (5.5) over its stdin and
   stdout; stderr is kept apart for panics. An `escalate_user` that cannot
   run the login user's copy gets one streamed where it can (11.3).
9. **Render** the per-host, per-step view from the event stream as it
   arrives, with what happens during a step shown under that step.

Measured in spike 2 (x86 dev box, ARM VM over Tailscale, warm ControlMaster):
exec to the binary's first frame 17 to 21 ms over SSH, 5 ms locally; a whole
no-op run on the ARM VM 30 ms; two hosts on two architectures, warm, 345 ms
total. Orchestrator overhead is negligible next to the ops themselves (apt
installing one package took 4.5 s).

### 5.3 Cross-compilation constraints (DECIDED for MVP)

- **Linux targets are `*-unknown-linux-musl` only.** Static musl binaries run
  on any Linux regardless of libc version.
- **The controller needs rustup, a C compiler, and `curl`; zig is fetched by
  `rustible` itself.** The C compiler is cargo's, for a workspace under
  `cargo check`, `cargo test` or rust-analyzer, not `rustible`'s.
  `rustup target add <triple>` is done for the operator, and so is zig: the
  CLI fetches the pinned release into `~/.cache/rustible/zig/<version>/` on
  first use, verified against a checksum in its own source, with the `curl`
  the machine already has; a zig already present (`RUSTIBLE_ZIG`, or on
  `PATH`) wins and nothing is fetched. zig compiles `ring`'s C and links every
  target Rustible ships to, carrying its own libc for each — musl, Darwin,
  FreeBSD — so there is no compiler to choose per target, no header set to
  vendor, and no SDK to obtain from another machine. `cargo-zigbuild` is a
  library dependency of `rustible-cli`, not a program the operator installs.
  Playbook binaries for Linux are fully static musl; for macOS they are
  Mach-O linked against `libSystem` alone, which every mac has by definition.
  **Target hosts still need nothing**, as before. A crate that bundles a C
  *library* (`openssl-sys`, `libgit2-sys`, `libsqlite3-sys`) remains
  **unsupported**: the fix is the pure-Rust alternative (`rustls`, `gix`,
  `rustix`).

  **What this replaced, and why — twice.** The original rule forbade C
  outright and named `cargo-zigbuild` as a rejected escape hatch. It was
  narrowed on 2026-09-08 when the only non-alpha pure-Rust TLS provider
  (`rustls-graviola`) aborted below Intel Broadwell on a live host and `ring`
  measured better (`docs/plan/reports/C-TOOLCHAIN-SPIKE.md`); the lesson was
  that the rule protected the toolchain, not the language, and the toolkit
  became rustup plus clang. It was rewritten again on 2026-09-13 for the same
  lesson a second time. Targeting macOS from Linux needed an Apple SDK copied
  off a mac, which was unacceptable; measuring the alternative
  (`docs/plan/reports/MACOS-TARGET-SPIKE.md`, `docs/plan/M8.md`) showed that
  the SDK was a symptom and the condition was the compiler-selection *matrix*
  clang required — different answers per target, host OS and compiler vendor,
  a thousand lines of it plus vendored musl headers and a workaround for
  Apple's patched `stddef.h`. zig deletes the matrix, and the rule that had
  forbidden it was found to be forbidding the thing that reduces sprawl.
  Ansible's equivalent is the system OpenSSL on every target; ours is one
  toolchain fetched once onto one machine.
- **Shipped binaries use the `dist` profile** (strip, fat LTO, `opt-level = "z"`):
  1.4 MB for the spike playbook on aarch64 versus 3.1 MB for plain release.
- **macOS is a target** (Apple silicon and Intel) from any controller, for the
  operations that make sense there, and each operation declares the platforms
  it runs on and refuses the rest by name
  (`docs/plan/reports/MACOS-TARGET-SPIKE.md` is the measurement). Windows
  targets are deferred. FreeBSD and NetBSD binaries build (zig carries their
  libc; measured 2026-09-13) and wait for operations.

### 5.4 Transport (DECIDED for MVP)

Use the system `ssh` binary, with ControlMaster multiplexing.
This inherits `~/.ssh/config`, ssh-agent, jump hosts, ProxyCommand, and 2FA for
free, and is what Ansible itself does. `russh` (pure Rust SSH) is the later option
for zero external dependencies. "Local" is the other transport (run the binary on
the orchestrator machine itself).

The binary on the target never opens a socket. Everything rides the SSH session
the orchestrator established, so firewalling and authentication are entirely
SSH's concern.

### 5.5 Protocol (DECIDED)

**Framing.** The orchestrator and the binary exchange typed frames, each a
length prefix followed by a JSON body, on the binary's stdin (down) and stdout
(up). Stderr stays raw for panics and crash output. JSON was chosen over
postcard or msgpack because the frames are small, it is debuggable with
`--json` locally, and the codec is one function on each side if that ever
changes. Nothing else may write to stdout in `--remote` mode.

**What crosses the channel.** Down: what the binary needs to start that must
not be baked into it (vars, whether this is a dry run, secrets), then file
contents on request and a cancel. Up: a hello naming the protocol version and
the playbook (from the playbook's own registration, never `argv[0]`, since the
cached binary is named by its hash), then the events the orchestrator renders:
facts, steps and their outcomes, logs, commands run, failures and a summary.
Files move both ways in chunks (5.6), alongside a running step. A cancel stops
the run between steps, never half way through an `apply`; a binary that does
not stop within a grace period is killed.
`crates/rustible-sdk/src/protocol.rs` and `event.rs` define the frames.

**Reserved, not built.** Frames for multi-host coordination (section 11's
tier 3). The channel is bidirectional from day one so that adding them
changes no playbook.

**What the operator sees.** One line per step, with its status and a
one-line summary of its change. `-v` adds detail: facts, full diffs, debug
logs, and a failed command with its stderr whether or not the playbook
caught it. `-vv` adds every command run. `--json` prints the frames instead,
for a program to read. `crates/rustible-cli/src/render.rs` is the renderer.

**Protocol versioning.** The hello carries a protocol version, which the
orchestrator checks before the first step, refusing a mismatch and saying
how to bring the workspace to the CLI's release; an incompatible change
bumps it. The escalation helper's wire (11.3) is outside it, because a
helper is always the same build as its parent.

The goal is that any skew between the CLI and a workspace's crates means
"rebuild", never silent breakage. **That is not yet true:** only the
protocol version is compared, so two releases that speak the same version
are not told apart (`docs/plan/DECISIONS.md` records the gap).

**The binary's modes.** One playbook binary serves every role: a local run
for development, a run under the orchestrator, self-description and the
vars pre-check of 5.2, and the escalation helper of 11.3.
`crates/rustible-sdk/src/runtime.rs` dispatches them.

### 5.6 Getting local files to the target (DECIDED: both ways)

Ansible's `copy` and `template` ship controller-side files to the target. Rustible
supports two mechanisms, each for a different need:

1. **Embed at compile time**, with `include_bytes!` / `include_str!` or a
   compile-time template engine (askama-style), for small, fixed files and
   templates. The binary stays self-contained, and a misspelt field in a
   template is a compile error. `file::Template` is wave two (6.9) and not
   built; today a playbook builds the text in Rust
   (`docs/USING_RUSTIBLE.md`, "Putting a variable into a config file").

2. **Stream over the channel at run time.** `ctx.local_file` brings a
   workspace file to a temp path on the target, removed when the run ends,
   and refuses anything outside the workspace; `ctx.local_secret` holds a
   secret in memory only. Use for large files, files generated right before
   the run, and secrets that must not sit inside a binary in a build cache.
   `examples/workspace/playbooks/demo/streaming.rs` uses both.

Rule of thumb: embed by default, stream when large, dynamic, or secret.

**Large stays large end to end.** Nothing on the way holds a file whole: the
channel, the escalation helper and the operations that move file contents all
carry it in chunks, so a file's size is bounded by the target's disk, not by
memory, and no operation imposes a default size limit. A streamed write lands
whole or not at all, with its mode and owner already set.

The reverse (Ansible's `fetch`) uses the same channel upward, and lands the
same way. Cross-host copy (Ansible's `synchronize` with `delegate_to`) is
deferred; it is a coordination feature, not a backend concern.

## 6. Playbook programming model

### 6.1 Playbook file shape

A playbook is a Rust file under `playbooks/` with one function marked
`#[rustible::playbook(...)]`. Its body is ordinary Rust, and everything it
does to the machine is a `ctx.step`. Two things it shows that YAML could
not:

- **Typed outputs flow into the next step.** A step returns its op's typed
  output, and the next op is built from it: the account `user::Present`
  returns is what `authorized_keys::Present` manages, with no name to look
  up again. A misused output is a compile error.
- **Reacting to change is an `if`** on whether a step changed (6.6).

`examples/workspace/playbooks/hello.rs` is the smallest playbook, and
`examples/workspace/playbooks/vagrant.rs` chains steps this way; CI compiles
both. The orchestrator renders each step as one line with its status, and
ends with a recap per host (5.5).

- The `#[rustible::playbook(...)]` attribute carries metadata: target hosts (a host
  or group from the inventory), `escalate`, `ssh_user`, and later things like
  `serial`. The macro wraps `main` with the runtime that speaks the protocol.
- `escalate = true` (Ansible's `become`; see section 16 for the name) means
  the binary is launched under `sudo` on the target. Per-step escalation is
  `ctx.as_root()` (section 11.3).
- `ssh_user`, naming an account, makes this playbook log in as it instead
  of the inventory's `ssh_user`, overriding every inventory level (host,
  group, `defaults`): an explicit per-file choice wins, unlike Ansible, where
  an inventory `ansible_user` beats a play's `remote_user`. Escalation
  (`escalate = true`, `as_root`, `as_user`) runs from that account. A
  `connection="local"` host has no login to change, so a run that targets one
  with this attribute is refused.
  `examples/workspace/playbooks/vagrant_login.rs` sets it.
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

`Op` values are plain data structs built with builders. Constructing one (a
`group::Present` for `docker`, say) touches nothing. Only `ctx.step` executes.
This is deliberate: a data struct can be inspected before it runs, which is what
gives dry-run, diff rendering, and a future `rustible plan` mode for free. A
closure is opaque and could only be run. Because ops are values, they can be
built conditionally, stored in a `Vec`, or returned from helper functions in
third-party crates.

**The contract.** An op decides in `check` and acts in `apply`, which
executes exactly what `check` decided, so what is reported is what runs.
The definition is the `Op` trait in `crates/rustible-sdk/src/op.rs`.

- **`check` observes and decides, and does not mutate** (7.3 has the one
  deliberate exception). It answers that the system is already in the
  desired state, with the op's output, or that something must change, with
  the op's **intent**: what `check` observed and decided, in the op's own
  types. An intent never holds what `apply` will produce (section 12).
- **`apply` executes that intent.** It does not inspect the system again or
  plan again; it may read what its output needs, because a read is not a
  decision.
- **The report is rendered from the intent**, so the diff shown in check
  mode is exactly the change that would be applied. That is what makes a
  dry run trustworthy.
- **`Diff` is opaque**: it can be built and rendered, never read back, and
  an intent never contains one. An `apply` that read its instruction out of
  the report would let rewording a report change what runs.

`ctx.step` drives the two halves: a satisfied step is `ok`, a change under
`--check` is `would change` and never applied (12), and otherwise the
intent is applied and the step is `changed`. It returns the op's typed
output together with whether the step changed.
`crates/rustible-sdk/src/ctx.rs` is the authority on the driver.

**Policies learned in spike 3, now rules for the stdlib:**
- **No predictions.** Spike 3's ops could predict their post-apply output
  for free, so predicting became the rule. Two waves of the stdlib showed
  that the cost landed in judgement rather than code, and that the report
  never told a prediction from a fact, so the rule was reversed (section
  12).
- **Builders end in a finishing call for the one mandatory piece of desired
  state**, so an op without it cannot be constructed: a `file::Line` without
  its line does not exist.
- **`apply` receives the intent, not the plan.** Only the change branch is
  meaningful there; passing the whole plan forced a pointless match. The
  change was first carried as a diff, and the typed intent replaced it when
  string-typed diffs had become the instruction channel.

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
| `systemd: daemon_reload=yes` | `systemd::DaemonReload` (an action, no unit) |
| `authorized_key: state=present` | `ssh::authorized_keys::Present`        |
| `authorized_key: state=absent`  | `ssh::authorized_keys::Absent`         |
| `getent`/`register`        | `user::Existing` (read-only op, 13.1)      |

**Alternative considered and rejected:** one struct per resource with a state
parameter, e.g. an `apt::Package` with a `state` setting. Rejected because:
- Valid options differ per state (`purge`, `autoremove` only make sense for
  `Absent`; `update_cache` only for `Present`/`Latest`). A shared struct cannot
  stop, at compile time, a purge being asked of a package meant to be
  present, which is YAML hell in Rust clothing.
- Outputs differ per state (`Present` returns installed versions to chain from;
  `Absent` returns what was removed). A shared struct means one output with a
  pile of `Option`s.
- Reading the playbook, intent is on the left: the type is `apt::Absent`
  before any package is named.

Cost accepted: more types in the stdlib. Types are cheap; runtime "invalid
parameter for this state" errors are what we are escaping.

### 6.4 Actions and the "always changes" hint

Some things are actions, not states: `systemd::Restart`, `systemd::DaemonReload`,
`shell::Command`. They implement the same `Op` trait; their `check` simply always
returns `Plan::Change`, because "restarted" is not a state you can already be in.
In check mode they report "would change", as Ansible does.

An action need not name a subject, and its output is only what it can
honestly report: `systemd::DaemonReload` names no unit, so it has no unit
state to return, and returns nothing.
An output that only echoes the op's own inputs is an input, not an output.

`shell::Command` takes Ansible's escape hatches (`creates`, `removes`,
`changed_when`) to behave more like a state.

An op whose `check` always asks for a change can mark itself as always
changing, so the output can mark those steps. This preserves the "where
does this playbook stop being idempotent" scan without a second verb.

An action that can only tell after running whether anything happened
reports `ok` when nothing did, rather than `changed`: a command with
`changed_when`, a read-only HTTP request, a lookup of remote state (6.5).
Under `--check` it has not run, so it reports `would change`.

### 6.5 Lookups

Read-only lookups are ops too (`user::Existing`, see 13.1). They run
through `ctx.step`, appear in the step list, are timed, and never report
`changed` in a real run. They fail the run if the thing is missing. In Ansible
this is `getent` plus `register` plus `set_fact`; here it is one typed call.

A lookup whose answer is remote state (`rustible_github::UserKeys`, say)
cannot be read under `--check`, because a dry run contacts nothing beyond the
target (12): it reports `would change` with no output there, and `ok` in a
real run, where it makes the request (6.4).

### 6.6 No handlers; loops; conditionals

- **Handlers are gone.** "Restart sshd only if the config changed" is an `if`
  on whether the config step changed, with the `systemd::Restart` step inside
  it. Ansible needs `notify`, a handler section, and flush semantics.
- **Loops are `for`.** Each iteration is its own enumerated step; the name is a
  `format!` string the author produces, which is both the price and the win
  ("Add rustible to docker" beats `item=docker`). Loop bodies chain naturally:
  the group op's output feeds the membership op in the same scope.
- **Conditionals are `if`.** `when:` does not exist.

Removal and loops read the same way: revoking keys is an
`authorized_keys::Absent` step for an account a lookup returned, and a
`for` over group names gives each group and the account's membership in it
a step of its own. `examples/workspace/playbooks/vagrant.rs` loops with
`for`; no compiled example yet revokes keys or adds memberships.

Note the three shapes on one resource, following rule 6.3:
`authorized_keys::Present` ("ensure these"), `authorized_keys::Present` with
its `exclusive` option ("ensure exactly these": still the present state, with
the option of removing strangers, and the output says which it removed), and
`authorized_keys::Absent` ("ensure not these"). An earlier draft had a
`remove` method on one type, which was the state-as-parameter shape 6.3
rejects; corrected 2026-09-07 during vetting.

### 6.7 Granularity rule (DECIDED)

**An op changes exactly one kind of resource. If it needs a prerequisite, it fails
with a clear message rather than creating it silently.** `user::Membership` does
not create the group; `group::Present` does. This keeps the report honest about
what changed. The stdlib will make this call hundreds of times; this is the rule.

**One exception, and it is about home directories only.** An op that manages a
file belonging to a single account may create the account's own directory that
holds it — `~/.ssh` for `ssh::authorized_keys`, and nothing outside a home
directory. That directory is not a shared resource: it belongs to that account
alone, and an op that was given the account already has its uid, gid and home
in hand, so there is nothing to guess about who should own it or what mode it
takes. Two limits keep this from becoming the general case. The op still
reports the creation in its own diff, so the report stays honest about what
changed; and it still refuses to create the **home directory** itself, which
belongs to `user::Present::create_home`. Outside a home directory nothing
changes: a shared directory such as `/etc/sysctl.d`, or a download
destination, is a prerequisite and is refused.

**In a dry run the refusal waits.** A prerequisite that another op in the same
run could create — a group, an account, its home, a parent directory, a unit
file — is verified when the run is about to act, not while it is only
looking: under `--check` the op reports `would change`, its diff showing the
state it would set or saying what it waits for, and a real run refuses
exactly as this rule says, because its `check` runs with check mode off and
a dry run's plan never reaches `apply`. Section 12 has the reasoning and the
limits.

### 6.8 Translations of real Ansible modules

Each translation is which op replaces the module, and where the meaning
differs. How the op does it is in its source.

**`ansible.builtin.apt`** becomes three ops, one per `state` (6.3):
`apt::Present`, `apt::Absent` and `apt::Latest`. They refuse a host without
apt, or a run without root. Two defaults differ from Ansible's: recommended
packages are installed only when the playbook asks, where Ansible follows
the system's apt configuration, and `Present` refreshes
the package lists only when it is about to install, so it is not a way to
refresh them for later steps.

`Latest` decides from the candidate versions in the package lists, so a
real run may refresh stale lists from `check`, before it decides: the one
op that changes the machine from `check` (7.3). A dry run never refreshes
(12), so with stale lists `Latest` says it cannot know, reporting `would
change` with no output; with fresh ones the dry run plans exactly as the
real run will. Ansible also skips the refresh under check mode (`apt.py`
guards its cache update with `check_mode`) but then plans against the stale
lists, so its dry run can call a package current that the real run
upgrades.

**`ansible.builtin.lineinfile`** becomes `file::Line`, which replaces the
line a pattern matches, or adds it when nothing matches, as Ansible does.
`check` plans the rewritten text and returns
`Satisfied` when it is already there; `apply` writes exactly that text,
atomically (7.3).

**`ansible.builtin.systemd`** becomes one op per state and one per action.
`Enabled`, `Running` and `Stopped` are states. `Restart` is an action (6.4),
and its step fails unless the unit is up afterwards. `DaemonReload` reloads
the manager alone, for a playbook that writes a unit file without bouncing
anything (6.4). A restart after a config change is a `Restart` step inside
an `if` (6.6).

### 6.9 Initial standard library scope, and the dogfooding repository (DECIDED)

The first ops are chosen from real usage, not from Ansible's module index.
The standard library is shaped by dogfooding against a real infrastructure
repository: Ansible playbooks and roles for a small fleet, surveyed on
2026-09-07, 39 playbook and role files with disabled ones excluded. Module
usage:

| Ansible module | Uses | Rustible op |
|---|---|---|
| `copy` (8 with `src`, 8 with inline `content`) | 16 | `file::Copy` |
| `user` (shell, groups with append, create_home) | 16 | `user::Present` |
| `service` + `systemd` (16 restarted, 5 started, 4 reloaded, 4 enabled) | 28 | `systemd::{Enabled, Running, Restart, Reload}` |
| `authorized_key` | 14 | `ssh::authorized_keys::Present` |
| `file` | 13 | `file::{Directory, Symlink, Absent, Attrs}` |
| `apt` | 12 | `apt::{Present, Absent, Latest}` |
| `set_fact` | 12 | not an op: `let`. Eleven of these fetch GitHub SSH keys, see below |
| `hostname` | 9 | `hostname::Is` |
| `include_role` | 7 | not an op: a role is a function in `src/lib.rs` taking `&mut Ctx` |
| `lineinfile`, `blockinfile` | 4 + 4 | `file::Line`, `file::Block` |
| `get_url`, `unarchive` | 3 + 2 | `http::Download`, `archive::Extracted` |
| `sysctl` | 3 | `sysctl::Present` |
| `group` | 2 | `group::Present` |
| `stat`, `acl`, `mount`, `iptables`, `replace`, `template`, `command`, `shell` | 1 or 2 each | tail |

**Wave one of `rustible-std`**, covering about 95 percent of that repository
with roughly twenty ops: `user::{Present, Absent}`, `group::Present`,
`file::{Copy, Directory, Symlink, Absent, Attrs, Line, Block}`,
`apt::{Present, Absent, Latest}`, `systemd::{Enabled, Disabled, Running,
Stopped, Restart, Reload, DaemonReload}`, `ssh::authorized_keys::{Present, Absent}`,
`hostname::Is`, `sysctl::Present`, `http::Download`, `archive::Extracted`,
`shell::Command`. **Wave two**, the tail: `file::Template` (used once, but
generally important), `file::Replace`, `acl`, `mount`, `iptables`.

**`rustible-github`, the first collection.** The GitHub-keys pattern (a
`url` lookup of `https://github.com/<user>.keys`, combined into a list and
looped into `authorized_key`) is not a `set_fact` problem and not a
`rustible-std` problem. It is a small separate collection, `rustible-github`,
with a read-only op, `rustible_github::UserKeys`, that takes a GitHub user
and returns the parsed keys over `rustible-std`'s HTTP client: synchronous,
with `rustls` over `ring` and no OpenSSL; `ring`'s small C part is compiled
by zig (section 5.3). An async client was rejected because it brings a
runtime and, without careful feature selection, native TLS and
therefore OpenSSL.
Published from this repository alongside the core crates, it is also the
first collection written from the outside of the SDK, which tests the SDK
itself. The `ssh_keys_from_github` role (two task files, a defaults file, and
a `set_fact`/`combine`/`product` dance) becomes a fifteen-line function with
typed arguments.

**Dogfooding.** That repository gains a Rustible workspace beside its
playbooks, and its hosts are ported one at a time. The inventory maps
directly: `ansible_host` becomes `addr`, `ansible_user` becomes `ssh_user`,
and a per-host hostname variable becomes a var. Port order: a host's basic
setup first (hostname, apt, sysctl; three ops, no roles), then the
`ssh_keys_from_github` role as a lib function, then the user setup, then a
whole host. Ansible and Rustible run side by side until parity, and every
port is a test of an op against a real machine. Ansible's `notify: Restart
avahi-daemon` becomes an `if` on whether the hostname step changed.

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
- Testing the distro branch itself: give the fake Alpine's facts and assert
  BusyBox `adduser` flags.
- Testing failure paths (exit code 9, permission denied) that are awkward to
  provoke in a real container.
- A future third backend (`Chroot`, `Container`) for applying ops to a mounted
  image without SSH is one more `impl Backend`.

What it does not replace: Docker integration tests remain the source of truth
that the system agrees with our assumptions. The fake tests that the op does what
we intended; the container tests that what we intended is correct.

### 7.3 Shape and guarantees

`System` is concrete, so ops never see a generic or `dyn`, and behind it
is a swappable backend: `Local` on a real machine, `Fake` in unit tests,
`Elevated` for another identity (11.3). What must hold:

- **Every effect on the machine goes through `sys`, reads included**, or the
  `Fake` is one nobody hits (7.2).
- **Commands run with `LANG=C` and `LC_ALL=C`** and are reported, and a
  failed one is an error that carries its stderr (14).
- **Writes are atomic and keep what they do not set.** A file is never seen
  half written or with the wrong mode or owner, and a rewrite keeps the
  existing mode and owner unless asked otherwise, as Ansible's `atomic_move`
  does (7.1).
- **SSH is not a backend.** The binary runs on the target, so every file is
  local; SSH is only the orchestrator's transport, one of remote-brain's
  payoffs.
- **`check` cannot change a file.** A file mutation through `sys` during
  `check` is refused, in the main process and in an escalation helper.
  Commands cannot be policed, because `check` reads state by asking tools,
  so their honesty is the op author's. The one deliberate exception is
  `apt::Latest`, which in a real run refreshes the package lists from
  `check` because its answer is read from them, and does nothing of the
  kind under `--check` (6.8, 12).
- **Ops are synchronous, and `System` holds primitives only.** A
  temp-directory or common-attributes helper was weighed and left unbuilt
  (decided 2026-09-14, `docs/plan/M9.md` §4.3); an op builds what it needs
  from the primitives.

`crates/rustible-sdk/src/system.rs` and `crates/rustible-sdk/src/backend/`
are the authority.

### 7.4 What is deliberately not on `System`

**Users and groups are not backend primitives.** `user::Present` reads the
account database and runs the distribution's account tool, both through
`sys`. Putting
`create_user` on `System` would force `System` to know that Alpine uses `adduser`
with BusyBox flags, which is distro knowledge that belongs in the op, chosen via
`facts.distro`. Layering:

- `System`: primitives identical on every Unix.
- Ops: everything distro-specific.
- `Facts`: the data that lets ops choose.

Networking and anything async are also off `System` for now.

## 8. Testing strategy (DECIDED)

Three tiers, numbered by what a test needs in order to run: the test process,
a container, a machine. Each sees something the tier below it cannot and
costs more to run (free, seconds, minutes), so a test goes in the lowest tier
that can fail for the right reason. CI job names carry the tier, beside the
mechanism and what it ran against (`Test (T2): Docker (Debian/Ubuntu/Alpine)`).

1. **T1, in-process**: everything a plain `cargo test` runs with nothing
   installed. The T2 tests in `crates/rustible-std/tests/` skip themselves
   there, without `RUSTIBLE_INTEGRATION=1`. T1 includes the CLI's tests of
   its built binary and the macros' trybuild UI tests, but it is chiefly two
   kinds of test, and choosing between them is an authoring choice, not a
   tier:
   - **Pure functions** for the interesting logic (line replacement and diff,
     `/etc/passwd` parsing, authorized_keys deltas, version comparison).
     Tested with strings, no fakes.
   - **`Fake` backend unit tests** for op behavior: canned files, canned
     command responses, assert planned diff and exact commands run. Hundreds
     run in a second.

   The split shapes the op: `check` does all the thinking and `apply`
   executes its intent without inspecting again (section 6.2), so the logic
   is pure and the `Fake` can plant a tool's effect between the two.

   In-process means in the test run, on the developer's machine, with no
   environment, not one OS process: nothing beyond the Rust toolchain, no
   docker, VM, root, or network beyond loopback. Its limit is that nothing in
   it is real: the `Fake` models what we believe a tool does, not what it
   does, and T1 has no real permissions, ownership, processes, users or
   distributions, and no kernel, init, `sudo` or SSH.
2. **T2, Docker integration tests** per distro, the source of truth for how a
   real tool behaves, which the `Fake` only models. The SDK ships a harness,
   the `#[rustible::integration_test]` attribute given a list of images
   (`debian:12`, `alpine:3.20`, `ubuntu:24.04`), that builds the test as a
   static musl binary and runs it in each container. A typical test applies
   an op twice: first run `changed`, second run `ok`, and the system looks
   right. Static binaries drop into any image with no setup.
3. **T3, a real machine**: its own kernel, init and `sudo`, all real, with
   no harness. It sees what the container harness cannot: its own kernel
   and `/proc/sys` and a real boot; a non-root login escalating through the
   host's sudoers and the SSH transport, which the harness, by its own
   choice, does not exercise; and macOS, which no container can be, with
   its launchd and Homebrew. Each playbook that converges a machine is run
   twice, and the second run is the test, because a first run reporting
   `changed` proves only that the op did something. `CLAUDE.md` ("The
   machine tier") and `docs/DEVELOPING.md` name the machines, the playbooks
   and the jobs.

## 9. Project layout and ecosystem

- **A Rustible project is one Cargo package.** `rustible init` creates it.
- **A playbook is any `.rs` file under `playbooks/` that carries
  `#[rustible::playbook]`. A build script finds them. Nothing is ever indexed
  by hand (DECIDED 2026-09-07).** The requirement, from vetting: the Ansible
  experience, where a playbook file simply exists and gets used, with no
  manifest entry to add on create or remove on delete, while keeping full
  rust-analyzer, clippy, types, and completion in every playbook file.

  **How it works.** The workspace has one bin target, `src/main.rs`, and a
  `build.rs`, both written once by `rustible init` and both **shims that must
  stay shims**: each is a doc comment plus a call into a crate (the `rustible`
  runtime, and the `rustible-build` scanner), so all logic lives in crates and
  a fix ships as a version bump, never as "edit your main.rs". Each opens with
  a header saying it is generated, not to be edited, and regenerated by
  `rustible init --refresh`. The CLI does **not** hash-check or police these
  files: a power user may edit them, at their own risk, and the header comment
  is the whole safeguard. (A hash-and-warn scheme was proposed during vetting
  and rejected as unnecessary nannying.) The build script parses every file
  under `playbooks/` (a real parse, so the attribute in a comment or a string
  does not count) and compiles each marked one into the bin crate as a module,
  registered under its name. Files without the marker are not playbooks: they
  are ignored unless a playbook pulls them in with a `mod` declaration or
  `#[path]`, so helper code may live next to playbooks. Two marked functions
  in one file is a build error naming the file. The same scan backs `rustible
  playbook list`.

  Two Cargo behaviours make this work, both verified in scratch projects on
  2026-09-07 (the second time as a 20-hypothesis spike with positive and
  negative cases, `docs/05_SPIKE_PLAYBOOK_DISCOVERY.md`): `cargo:rerun-if-changed=playbooks`
  on a *directory* makes Cargo rescan the whole tree, so new, renamed, and
  deleted files, including new subdirectories, are picked up on the next build
  with no edits anywhere; and rust-analyzer runs build scripts and proc macros
  by default and resolves `OUT_DIR` includes, so `#[path]` modules get full
  IDE support: they are linked as members of the crate, errors are reported at
  the playbook file's own path and line, clippy lints and `#[cfg(test)]` tests
  inside playbook files work, and the prelude resolves. **One dependency to
  know:** rust-analyzer re-runs the build script only through its check-on-save
  (`cargo check`), which is on by default. With it on, a newly created playbook
  is linked within seconds of opening it; with it off, the file shows as
  unlinked until "Rebuild proc macros and build scripts" or any terminal cargo
  invocation. `rustible playbook create` prints this hint. Scan cost is about
  10 ms for 50 playbooks; a no-op build does not re-run the script.

  **Isolation.** The CLI names the playbook in `RUSTIBLE_PLAYBOOK` when
  building for a run, and the build script (with `rerun-if-env-changed`)
  includes only that playbook. Verified: a playbook with a type error
  elsewhere in the tree does not affect `rustible playbook run` of another
  one. The shipped binary contains exactly one playbook, stays small, and is
  hashed per playbook for the target-side cache. With the variable unset, as
  in the IDE, `cargo check`, and CI, every playbook is included, so every
  broken playbook is visible while editing and fails CI, which is the desired
  behaviour, not a wart.

  **The `selected` feature, and why it must exist (DECIDED 2026-09-07).** The
  selected build and the editor's own `cargo check` are the same package with
  the same feature set and profile, so Cargo gives them the **same build-script
  output directory**. The discovery spike showed what that does in practice:
  with `playbooks/top.rs` open and healthy in the editor, a terminal build
  that selected `ops/a` rewrote the registry file rust-analyzer was reading
  down to that one playbook, and `top.rs` immediately
  showed as `unlinked-file` in the editor, with every other playbook likewise
  gone, until the next plain check-on-save wrote the full registry back. Every
  switch of the selected playbook also marked the whole package dirty on both
  sides, so the IDE and the CLI kept re-running the build script and
  recompiling each other's view. Running a playbook from a terminal must not
  make the editor forget the rest of the tree.

  The fix is a Cargo feature that carries no code: `rustible init` declares
  an empty `selected` feature in the workspace manifest, and every CLI build
  that sets `RUSTIBLE_PLAYBOOK` also passes `--features selected`. A different
  feature set changes the package's metadata hash, so the build script run,
  its `OUT_DIR`, and the bin artifact for the selected build live in their own
  directories, while dependencies (the SDK, the stdlib, collections) keep the
  same hash and stay shared and Fresh. Verified: after a selected build, the
  IDE's `cargo check` reports Fresh with nothing to do; switching the selection
  re-runs only the selected side; the editor's registry is never touched. The
  cost is one extra bin compile the first time, and none after. Alternatives
  that also isolate but cost more: a separate `--target-dir` for CLI builds
  (duplicates every dependency's artifacts) or a dedicated Cargo profile (a
  second artifact tree).

  Two rules follow. The build script **refuses `RUSTIBLE_PLAYBOOK` unless
  `CARGO_FEATURE_SELECTED` is set**, with a message saying so, so a variable
  left exported in a shell can never silently narrow an IDE or CI build to one
  playbook. And nothing in a playbook may depend on the `selected` feature; it
  is a cache-key, not a configuration knob, and the SDK does not expose it.

  **Consequences for playbook files.** A playbook file is a module, not a
  crate root: importing `rustible::prelude` works, a `#[rustible::vars]`
  struct is module-local so every playbook may have its own, and the
  `#[rustible::playbook]` attribute on `main` registers an entry rather than
  defining the process entry point. **Helper modules are siblings:** because
  `#[path]`-loaded files get `mod.rs` semantics, a `mod helpers` declaration
  inside `playbooks/ops/x.rs` resolves to `playbooks/ops/helpers.rs`
  (verified; an earlier draft of this paragraph said `ops/x/helpers.rs`, which
  is wrong), as `examples/workspace/playbooks/demo/mc.rs` and its `helpers.rs`
  show. A playbook that wants a subfolder layout puts a `#[path]` attribute
  naming `x/helpers.rs` on the declaration. Two playbooks in one directory
  that both declare `mod helpers` each compile the same file as a private
  module, which works. Code shared across playbooks lives in the package's
  `src/lib.rs` and is reached by the **package name**, not `crate`, because
  playbooks are modules of the bin crate (verified both ways);
  `examples/workspace/playbooks/hello.rs` calls its workspace's `src/lib.rs`
  this way. A playbook's name is its path under `playbooks/` without the
  extension (`ops/x`); generated module identifiers carry the needed
  `#[allow]`s and leak only into test names and backtraces. The scanner
  recognises the marker by name, which is enough for a marker. An unmarked
  file nobody references is silently ignored (rust-analyzer greys it out);
  `rustible playbook list` may warn about such orphans.

  **Alternatives considered and rejected:**
  - Syncing `[[bin]]` entries (the previous plan): a maintenance chore on every
    create and delete, the opposite of "files simply get used".
  - `src/bin/` auto-discovery: no manifest edits and full IDE, but the folder
    must be `src/bin/` and discovery is one level deep, so no `ops/x.rs`.
  - Workspace member glob with a folder and a five-line `Cargo.toml` per
    playbook: full isolation, but the manifest-per-playbook chore returns and
    adding a collection means editing every playbook's manifest.
  - Group packages (`playbooks/ops/` as a package with `src/bin/` inside):
    isolation even for a bare `cargo build`, but `src/bin/` in the middle of
    every path and one manifest per group.
  - A shadow package generated per run: same run isolation as the build
    script, but the IDE still needs the build-script modules, so two
    mechanisms and a double compile.
  - Persistent shadow packages as workspace members: total isolation, but a
    missing regeneration (fresh clone, manual delete) breaks the whole
    workspace until `rustible playbook sync` runs.
  - `cargo script` single-file packages: closest to "just a file", but still
    nightly-only (`-Zscript`), one dependency cache per playbook, partial
    rust-analyzer support.
- **Inventory is data**, in `hosts.kdl` next to `rustible.toml`. See section 10.
  Dynamic inventories become a trait later.
- **Crates (DECIDED 2026-09-07):**
  - `rustible`: a **thin facade library** that playbook workspaces depend on.
    Re-exports the SDK and the macros (and `rustible-std` as a module), so
    a playbook imports `rustible::prelude` and is marked
    `#[rustible::playbook(..)]`.
    No logic of its own. `rustible init` adds this one dependency plus
    `rustible-std`.
  - `rustible-cli`: the CLI and orchestrator (init, playbook run/create,
    inventory show/check, SSH, compile, render), installed with
    `cargo install rustible-cli`, binary named `rustible`. Never a dependency
    of a workspace: it would drag the orchestrator's async runtime and SSH
    client into every cross-compiled playbook. The renderer belongs here,
    not in the SDK.
  - `rustible-sdk`: the `Op` contract, `System` and its backends, `Facts`,
    `Diff`, `Ctx`, the event and protocol types, the runtime the macro
    expands into, and the test harness. Everything a collection author
    needs. Collections depend on this directly.
  - `rustible-macros`: the `#[playbook]`, `#[vars]` and `#[integration_test]`
    proc macros. Proc macros must live in their own crate; the facade
    re-exports them so users never name it.
  - `rustible-build`: the playbook scanner called from the generated
    `build.rs` (a `[build-dependencies]` entry written by `rustible init`), so
    the build script itself stays a one-line shim.
  - `rustible-std`: the base operations mirroring Ansible builtins, itself just a
    consumer of `rustible-sdk`.
  - `rustible-github`: the first collection (section 6.9), published from
    this repository but structured exactly like a third-party one.
  - Third-party collections (`rustible-docker`, ...): plain crates on
    `rustible-sdk`, published to crates.io, added with `cargo add`. Because
    playbooks link them directly, no registration mechanism is needed.
- **Publishing.** The crates ship from this repository: `rustible`,
  `rustible-cli`, `rustible-sdk`, `rustible-macros`, `rustible-build`,
  `rustible-std`, `rustible-github`. They are versioned in lockstep through
  `workspace.package.version`, so one tag releases all of them.
- Every crate in the tree is pure Rust except `ring`, the TLS crypto provider,
  which compiles a little C on the operator's machine and nothing on the target
  (section 5.3).

## 10. Inventory, typed vars, and the workspace (DECIDED)

### 10.1 Structure

The inventory stores single hosts and host groups. Groups can contain groups
(through `members`); a host's group set is the transitive closure, and vars
merge from outermost group to innermost.

**Connection settings are not vars.** `addr`, `port`, `ssh_user`, `connection`
(`ssh` | `local`), and the escalation method are orchestrator configuration and
live directly on the host or group. Playbook vars live under a separate `vars` table.
Ansible mixes these (`ansible_host`, `ansible_user`) and that is a source of its
precedence confusion.

### 10.2 Format: KDL for the inventory, TOML for `rustible.toml` (DECIDED)

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

Parameters say how to reach a host and how to escalate on it (`sudo`,
`doas` or none, and to which account), and each has a built-in default: the
login, for instance, defaults to the local username.
`docs/HOSTS_KDL_REFERENCE.md` lists them with their meanings and defaults.

Parameter resolution: host, then nearest group outward, then `defaults`, then
the built-in default. For `ssh_user` only, a playbook's `ssh_user` attribute
outranks all four (section 6.1); `inventory show` describes the inventory and
does not apply it. Parameters never come from `vars` and vars never from
properties.

### 10.2.2 Full example

`examples/workspace/hosts.kdl` is a complete inventory using every
construct above. The CLI's tests load it (`example_workspace_inventory_loads`)
and resolve the same inventory, kept as
`crates/rustible-cli/testdata/vision/hosts.kdl`, end to end, so neither can
drift from the parser. `docs/HOSTS_KDL_REFERENCE.md` is the reference for
the format.

`rustible inventory show`, given `web2` from that file, prints the resolved
parameters and vars with the source of each (all / group X / host /
defaults), including what was overridden.

### 10.2.3 Structural rules

- Names are unique across hosts and groups, so `members` can reference either.
- A host is defined exactly once (inside at most one group by nesting) and
  referenced from other groups by name. Defining it twice is an error.
- Group membership is the transitive closure through `members`.
- A `vars` node may hold its vars as children or as properties, and the two
  forms are equivalent; use children for lists and many vars.
- Var names use underscores and match the Rust field names one to one. No case
  mapping.
- Load-time errors name the file and line, e.g. missing `addr` on an ssh host,
  unknown parameter with a did-you-mean, `addr` on a group.

### 10.3 Typed vars: bridging the untyped bag and the typed playbook

- The inventory holds a **bag of typed scalars** per host/group: string, int,
  float, bool, or a list of one of those. (Not strings alone: the file format
  already has typed scalars; flattening to strings and re-parsing is lossy.)
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
- Last line of defense: the binary deserializes its vars again from the
  run's start (5.5) on the target, so nothing runs with bad vars even if the
  pre-check were bypassed.
- Options rejected: `syn` on the source (instant, but only sound for a closed
  canonically spelled type set, and needs a second parser kept in sync with the
  proc macro); proc macro writing the schema to disk during compilation (still
  needs a compile, and is hacky); schema in a separate `x.vars.toml` with the
  struct generated from it (splits the playbook across two files); rustdoc JSON
  / rust-analyzer (heavyweight).

`examples/workspace/playbooks/vagrant.rs` and
`examples/workspace/playbooks/demo/mc.rs` declare vars with defaults. A
failed validation names each host and what it lacks or mistypes, and says
where to add it; `crates/rustible-cli/src/inventory/validate.rs` writes it.

**Precedence** (four levels, versus Ansible's twenty-two): top-level `vars` (all), then
group vars outermost to innermost, then host vars, then `--var key=value` on the
command line. **Sibling-group conflicts are an error** (decided 2026-09-06): when
a host belongs to two groups at the same distance that both define a var and
the host does not override it, loading fails naming both groups and the host,
with the fix being "set it on the host or on a common parent". Consistent with
everything else here failing loudly before a run.

### 10.4 Workspace

- A **rustible workspace** is a Cargo package whose root also contains
  `rustible.toml`, with the generated shims `src/main.rs` and `build.rs` from
  section 9 and a `src/lib.rs` for code shared across
  playbooks. (`rustible.toml`, not `rustible.cfg`: same format as
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

## 11. `Ctx` (DECIDED)

`Ctx` is what `main` receives: the playbook's handle on the run, through
which it runs steps, learns about its host and reports to the orchestrator.
Every `Ctx` of a run shares one step sequence and one stack of open blocks.
It offers three tiers:

- **Tier 1, the core**: `ctx.step` (6.2), the host and its facts (13),
  lines in the report, files and secrets streamed at run time (5.6), and
  the machine directly through `ctx.sys()` (11.1).
- **Tier 2**: `ctx.skip`, `ctx.block`, acting as another identity (11.3),
  and `ctx.fetch` to bring a file back to the operator (5.6).
- **Tier 3, reserved, not built**: multi-host coordination, a barrier, a
  run-once and another host's facts, as blocking calls over the channel
  (5.5), which is bidirectional so that adding them changes no playbook.

`crates/rustible-sdk/src/ctx.rs` is the authority on what each offers.

### 11.1 Decisions embedded

- **`vars` is a parameter of `main`**, not a method on `Ctx`, so `Ctx` is
  untyped with respect to vars. The macro deserializes them before `main`
  runs; a failure is a clean per-host message.
- **`sys()` is exposed.** Playbooks are programs; authors are responsible. Reads
  are unreported. Mutations outside a step still go through the backend (logged
  at `-v`) but do not appear as steps; if it should be in the report, make it a
  step (`shell::Command` exists for that).
- **`skip` is explicit and optional.** An `if` that does not run a step makes it
  vanish from the output; `skip` records it with a reason and the summary counts
  it, for the "twelve steps, three did not apply, here is why" report.
- **`block` is a grouping, not an operation.** It has no step and no
  result of its own: steps inside it carry its path as a prefix, so a line
  stands on its own however hosts interleave, and it returns what its
  closure returns. Under `--check` it is where a read of a missing output
  ends (section 12). Blocks nest.
- **`bail!`** (re-exported) is Ansible's `fail` module.
- **The target learns only what a playbook or an escalation needs about
  its host**, never its address or port, which are the orchestrator's.
  `HostInfo` in `crates/rustible-sdk/src/ctx.rs` is the authority.
- **`step` numbering and the summary are shared** across `as_user`
  contexts and blocks, so a run has one step sequence however many `Ctx`
  values exist.
- **In check mode `changed` means "would change".** A playbook that logs after
  a changed step should branch on `ctx.check_mode()` to word it honestly
  (spike 2 caught the `mc` playbook logging "installed mc" in a dry run).

### 11.2 Example

A playbook mixes these freely: it refuses an unsupported host with
`bail!` after reading its facts, groups related steps in a `ctx.block` that
returns whether anything changed, restarts a service only when it did and
records the skip with a reason otherwise, and installs a certificate it
took from the workspace as a secret. No compiled example shows all of it;
`examples/workspace/playbooks/vagrant.rs` gates on facts, and
`examples/workspace/playbooks/demo/streaming.rs` loads a secret. Each piece
is set out in its own section: facts and `bail!` (11.1, 13), blocks and
skips (11.1, 12), secrets (5.6).

### 11.3 Per-step privilege escalation (DECIDED)

Escalation is a property of how a step runs, not of the op, so it lives on
`Ctx`: a step runs as root through `ctx.as_root()`, or steps down through
`ctx.as_user`. `examples/workspace/playbooks/demo/escalation.rs` escalates
one step, and `examples/workspace/playbooks/vagrant.rs` steps into two
unprivileged accounts. Playbook-level `escalate = true` remains for the
common case: the binary is launched as `escalate_user` (default root) via
the inventory's escalation method, from whichever account logged in.

**Three identity methods (DECIDED):**
- `as_user(name)`: explicit user.
- `as_root()`: literally `as_user("root")`. It never follows the inventory; a
  method named `as_root` that might run as `admin` would be hidden indirection.
- `as_escalated()`: `as_user` with the host's `escalate_user`, i.e. the
  privileged account the inventory chose for this host (root by default, or
  a shared admin account where direct root is not allowed). This is what
  `escalate = true` uses at launch, exposed per step. Output marks steps
  whose identity differs from the binary's own (`as root`, `as postgres`).

**How it works.** A process cannot change identity per call, and
Ansible's answer is shell tricks (`sudo tee`, chmod dances). Ours: a step
under another identity does its I/O through a helper, which is **the same
binary** started as that identity through the host's escalation method,
once per identity and kept for the run. Because it is the same build as its
parent, its wire needs no version (5.5); because all I/O goes through
`sys`, ops know nothing of it, the check-mode guard holds inside it, and
stepping down is the same mechanism. It streams large content like the main
channel (5.6), and its commands are reported with the identity. An account
that cannot reach the login user's copy of the binary runs one placed where
it can, in its own cache or in a private directory removed by the end of the
run, because homes are commonly closed to other accounts; a step is refused
only when neither place is usable, naming both causes. Nothing extra is
uploaded from the controller for it. The cost
is one spawn per identity and a pipe round trip per primitive.
`crates/rustible-sdk/src/launch.rs` and `backend/elevated.rs` are the
authority on how.

**Passwords.** Escalation never prompts. A password the inventory's method
needs travels with the run's start as a secret, in memory only, and a wrong
one fails fast instead of hanging the run.

## 12. Check-mode semantics (DECIDED)

Problem: in a dry run a step that *would* change has not run, so it has no
output, and a later step that chains from it has no value.

Options considered:
1. Stop the host at the first would-change step. Honest but shows only the
   first change; useless for "what would this playbook do". Rejected.
2. Continue; the output is unavailable; when playbook code later reads it,
   end the enclosing block there with a warning.
3. Let ops predict their output. Most fidelity, more work per op, and a wrong
   prediction is a lie in a dry run.

**Decision (2026-09-06): 2 as the rule, 3 as opt-in. Revised 2026-09-24: 2
alone.** Two waves of the stdlib were built under the first decision, and
what they showed is recorded so the reversal is not relitigated:

- Prediction moved the work from code into judgement. Every op that creates
  something had to rule on which fields it may claim before the tool has
  run, and each ruling was a decision-log entry, a guide paragraph and a
  test.
- The predictions rarely fired where a dry run matters most. On a fresh host
  nearly every step creates something, and the honest answer for a created
  thing was usually "cannot predict", so the playbook author was told to add
  an explicit uid and gid for the dry run's sake.
- The report never showed which values were predictions. The distinction
  existed for the playbook and not for the person reading the run.
- To let a dry run get past a step that *needs* what an earlier step would
  create, `System` grew a registry of planned resources (2026-09-08). One op
  ever wrote to it; every later gap of the same shape was answered with a
  check-mode branch in the op instead, so three different answers to one
  question were in the tree at once.

Ansible's check mode has neither mechanism and one rule for a step that
would create something: report `changed` and ask no further questions
(`user.py`'s `main()` exits `changed` under check mode before it validates
the group; `ansible.posix.authorized_key` does not look at the directory
under check mode). That rule is adopted, and Ansible's behaviour is
authoritative where the rule could have been read more broadly (decided
2026-09-24): an *existing* account's missing group is refused under
`--check` as in a real run (`user.py` validates it before anything that
respects check mode), and keys for an account that does not exist yet are
refused too (`authorized_key`: "Either user must exist or you must provide
full path to key file in check mode"). Added on top is what Ansible lacks
when a later step reads what a dry run could not produce: a warning saying
where the dry run stopped seeing.

**The rules:**
- In check mode, a would-change step reports `would change` with its diff and
  the run continues. Nothing is applied and nothing is predicted: the intent
  carries what `check` observed and decided, and no post-apply output.
- A would-change step's output does not exist; whether it changed, and its
  diff, remain readable. Under `--check`, reading the output in playbook
  code ends the innermost enclosing `ctx.block` (outside any block, that
  host's playbook body) with a warning naming the block and the step, and
  the run continues after it. That is not a failure, and is counted neither
  `failed` nor `recovered`: nothing failed, the dry run could not see
  further. Read inside an op's own `check`, the missing output is that
  step's failure instead (14). Playbooks are written as if every output
  exists, without guards; one that wants to branch inside a block instead
  can ask whether the output is available (`is_available`). In a real run
  the read cannot fail. Ansible
  carries on with silent garbage; Rustible says where the dry run stopped
  seeing.
- **Prerequisites are verified when the run is about to act.** An op that
  would refuse for want of something another step in the run could create
  (a group, an account, a directory, a unit), within the two limits Ansible
  draws above, reports `would change` under `--check`, its diff saying what
  it would set or what it waits for, naming the prerequisite when the op
  knows it. A real run's `check` still refuses, so the refusal is never
  skipped on a run that can act. The trade accepted: a dry run does not
  catch a forgotten prerequisite step, and the real run refuses at that
  step, before it touches anything, with the steps before it applied. 6.7
  still holds at the step.
- That deferral covers only what another step could supply. A refusal about
  the machine or the request itself (wrong platform, not root, a missing
  tool, a malformed input) stands in check mode as in a real run, because
  no earlier step changes it.
- **A dry run touches nothing outside the target.** Under `--check` no
  operation contacts anything beyond the machine it runs on, however
  read-only the request: a request can be logged, counted against a quota,
  billed, or have effects its method does not advertise, and a dry run is
  only worth running if nobody has to wonder what it did. An operation whose
  answer depends on remote state reports `would change`, saying the remote
  state was not read. The cost, a dry run that cannot call such a step
  `ok`, is accepted.
- **`check` still cannot mutate a file through `sys`** (7.3), in a dry run
  or a real one; `apt::Latest`'s refresh, the one deliberate change from
  `check`, does not run under `--check` (6.8).

## 13. Facts (DECIDED)

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

**Decision: `Facts` is a fixed, typed core.** Rule for membership: data that
many *ops* need for their internal decisions, cheap to gather, stable for the
run. Gathered once, eagerly, at startup, from a handful of cheap reads taking
milliseconds, and sent up once as an event. No lazy facts, no dynamic facts.

The core describes the platform ops choose by (operating system,
distribution, architecture, init system, package managers present), the
machine's size, and the account the binary runs as. A value Rustible does
not recognise degrades instead of failing. Deliberately excluded: mounts,
network interfaces, users and groups, installed packages, environment; each
is an op when a playbook needs it. Facts cross the wire, so changing their
shape is a protocol change (5.5). `crates/rustible-sdk/src/facts.rs` is the
authority.

### 13.1 Desired-state ops are the lookups

There is no generic `::lookup()`. The desired-state op's typed output already
says what was found: `apt::Present` for `python3` says which packages were
already present and which it installed, and `.changed` is false when nothing
was done (reported as `ok`, never `skipped`; `skipped` means deliberately not
run).

A small minority of **read-only ops** exists for *observe without changing*:
"if docker is installed, configure it" (where `apt::Present` would install it)
and "this user must already exist, fail otherwise" (`user::Existing`, where
`user::Present` would create it). They appear as named steps with typed
output, never report `changed` in a real run (a lookup of remote state is
`would change` under `--check`, 6.5), and most resources do not need one.

## 14. Error model (DECIDED)

**Semantics** are Ansible's: a failed step whose error leaves the playbook
fails that host, and the run continues on the other hosts. In code that is
`?` on `ctx.step`. A failure the playbook catches is still reported as a
failed step but does not fail the host: the summary counts it as
`recovered`, Ansible's `ignored` and `rescued` in one column. A cancelled run
and a panic always fail the host. Ignoring is discarding the step's result,
or `.ok()` on it; rescue is a `match` or an `if let` on its `Err`; retry is
a loop. None of these need to know the error's kind, and no playbook or op
is expected to match on errors.
`examples/workspace/playbooks/vagrant_login.rs` catches a step that must
fail and checks its message.

**Type.** An opaque, `anyhow`-style error with a context chain, wrapping the
`anyhow` crate. `rustible_sdk::Result` carries `rustible_sdk::Error`, which
converts from any `std::error::Error` through `?`;
`crates/rustible-sdk/src/error.rs` is the definition.

Why not the structured enum from spike 3:
- The drawback is on the *producer* side, not the consumer side. `?` on a
  foreign error (`serde_yaml::Error`, `regex::Error`, anything from a crate
  we do not own) does not compile against an enum unless we wrote a `From`
  for it, so op and playbook authors end up writing `.map_err(..)` on every
  line. A catch-all variant plus a blanket `From` for every
  `std::error::Error` is rejected by coherence when the enum itself
  implements `std::error::Error`; `anyhow`'s design (its `Error`
  deliberately does not implement that trait) is the one shape that makes
  the blanket conversion legal.
- No context chain: a deep failure renders flat, like Ansible's `msg`.
- The enum would need `#[non_exhaustive]`, which removes exhaustive matching,
  its only advantage, and nobody was going to match anyway.

**Context is optional.** Bare `?` is the norm. The SDK adds the two most
useful layers automatically: `ctx.step` wraps any failure with the step name,
and primitives carry their own detail (a command run through `sys` fails
with its argv, exit code, and stderr; file primitives with the path).
`.context(..)` is for ops or playbooks that do several similar things where
the raw error would not say which.

**Typed values inside the chain.** The SDK's own signals (a mutation during
`check`, a read of a missing output, a failed command) remain concrete types
the orchestrator downcasts for rendering; `crates/rustible-sdk/src/error.rs`
has them. Playbooks never need them.

**On the wire.** A failure crosses as rendered text, tied to the step line it
closes by the step's id, since names repeat, and with the failed command
kept apart so `-v` can show it once, caught or not (5.5).
`crates/rustible-sdk/src/event.rs` defines it.

**What the operator sees.** A failure ends the host with a line naming
the step, its block path, and the error's context chain; `-v` adds the
failed command and its stderr (5.5). `crates/rustible-cli/src/render.rs`
renders it.

## 15. Glossary

- **Orchestrator**: the `rustible` CLI process on the developer's machine that
  drives a run.
- **Target**: a host a playbook is applied to.
- **Playbook**: a Rust source file with a `#[rustible::playbook]` main, compiled
  to a static binary per triple.
- **Op**: a struct implementing `Op`, describing a desired state (or an action),
  with `check`/`apply`.
- **Step**: one `ctx.step(name, op)` call; the unit of reporting.
- **Plan**: the result of `check`: `Satisfied(output)` or `Change(intent)`.
- **Intent**: what `check` decided, specific to the op and never sent over
  the wire; `apply` executes it and the step's `Diff` is rendered from it.
- **Applied**: what `step` returns: the op's typed output plus `changed` and `diff`.
- **System**: the op's handle to the machine, over a `Backend`.
- **Facts**: typed data about the target gathered at startup.
- **Collection**: a crate of ops built on `rustible-sdk`.
- **Escalate**: Ansible's `become`. Running the binary or a step as another
  user, root by default. Named `escalate` because `become` is a reserved Rust
  keyword (section 16).
- **Block**: `ctx.block`, a named grouping of steps, not an
  operation; it returns what its closure returns.
- **Skip**: `ctx.skip(name, reason)`, a step deliberately not run, counted in
  the summary.
- **Recovered**: a step that failed and whose error the playbook caught and
  carried on from (a retry, a fallback, an optional step); shown `FAILED`,
  counted in the summary, and never fails the host (14).
- **Parameter** (inventory): a connection or escalation setting `rustible`
  itself understands, written as a property on a host or group node.
- **Var** (inventory): a value for the playbook, written only inside a `vars`
  block, delivered to the playbook's typed struct.
- **Workspace**: a Cargo package whose root also holds `rustible.toml` and
  `hosts.kdl`.
- **Transport**: how the orchestrator reaches a host: `local` (child process)
  or `ssh` (the system `ssh`).
- **Frame**: one length-prefixed JSON message on the channel.
- **Binary modes**: the roles one playbook binary plays (section 5.5).
- **Would change**: a step's status in check mode when `check` found a
  difference. `apply` does not run and the step has no output (section 12).
- **`selected` feature**: an empty Cargo feature in every workspace manifest,
  enabled only by CLI builds that set `RUSTIBLE_PLAYBOOK`, so the selected
  build gets its own build-script output directory and never clobbers the
  editor's registry (section 9).

## 16. Open questions, with the milestone by which each is decided

Everything architectural is decided. One question is still open, and it is
local to one crate:

| # | Question | Decide by | Why it can wait (or cannot) |
|---|---|---|---|
| 7 | **Target-side cache cleanup** for `~/.cache/rustible/bin/` | whenever | Trivial. |

Rows keep the numbers they were opened under, because code and documents cite
them. The others are answered in the sections they concern: 1, crate naming
(9); 2, verb order (3); 3, playbook-to-binary mapping (9); 4, the `rustible
init` layout (3, 10.4); 5, the diff representation (6.2); 6, output rendering
(5.5); 8, `doas` (10.2.1, 11.3); 9, the container-tier harness (8); 10,
`Cancel`, which stops the run between steps (5.5).

**The `escalate` name (decided 2026-09-06, revised the same day).** The word
`become` is not used anywhere in Rustible; it is `escalate` everywhere: the
playbook attribute (`escalate = true`), the inventory parameters
(`escalate="sudo"`, `escalate_user`), the CLI's `--escalate-password-env`,
and all code.
Reason: `become` is a reserved Rust keyword (for guaranteed tail calls).
Options weighed: `r#become` internally (ugly all over the code), a
`rustible_become` prefix in the attribute (redundant inside
`rustible::playbook(...)` and the `ansible_*` smell), a different internal name
mapped from `become` in the attribute (two names for one thing). One word
everywhere won. Where `escalate` is defined, a comment says it is Ansible's
`become`.

## 17. Spikes (all done)

Three spikes validated the design in running code before building began:
cross-compilation, the protocol over SSH, and the SDK core. Their reports are
`docs/03_SPIKE_CROSS_COMPILE.md`, `docs/04_SPIKE_PROTOCOL_SSH.md` and
`docs/02_SPIKE_SDK_CORE.md`, with the playbook-discovery spike in
`docs/05_SPIKE_PLAYBOOK_DISCOVERY.md`; later measurements are under
`docs/plan/reports/`. What they found that became policy is stated in the
section it governs, and the reports remain the record of how each was found.
