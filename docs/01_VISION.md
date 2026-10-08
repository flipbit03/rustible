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
and what must hold, and points at where the real one lives: the source for a
definition (the `Op` trait in `crates/rustible-sdk/src/op.rs`, say), and for
what a playbook, an inventory or a run looks like, the workspace in
`examples/workspace`, which CI builds and so cannot drift, and
`docs/USING_RUSTIBLE.md`. Types, operations, flags and paths are named
inline, as pointers. A goal the code does not meet yet is marked as such in
one sentence, so that a promise is never read as a fact.

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
chosen folder. It refuses only when a file it would itself write is already
there, and names the ones that clash; a directory holding anything else (a
`README.md`, a `LICENSE`, a `.gitignore` from a fresh clone) is written into.
Those files are left alone and never read, with one exception: an existing
`.gitignore` gains the lines it lacks, because a workspace that does not
ignore `target/` is a workspace that commits build output. Refusing on
conflict rather than on non-emptiness is `cargo init`'s rule. `--force` adds
the missing files anyway and keeps the existing ones, rewriting only the two
generated shims. It adds `rustible` (runtime) and `rustible-std` (the base
operations, mirroring Ansible's builtin modules: files, users, groups,
packages, services, ssh keys, and so on) as dependencies, and creates an
opinionated layout: `.gitignore`, an inventory file, a `playbooks/` folder
(with `.gitkeep`), and any config files that turn out to be necessary.

**`rustible playbook create`**, given a path under `playbooks/` such as
`playbooks/ops/ssh_enable_root_user.rs`, scaffolds a playbook file with a
`main` function and the metadata attribute.

**`rustible playbook run`**, given that playbook, with `--check`, `-v` or
`-vv`, and `--var key=value` to set a var for the run, reads the playbook's
metadata (target hosts), validates the inventory vars against the playbook's
typed struct, probes the hosts, compiles per architecture, uploads, runs, and
renders progress. See section 5.2 for the pipeline. `--check` is a dry run.
Each step line carries a one-line summary of its change; `-v` adds the facts,
the full diff, debug logs, and a failed command with its stderr, whether or
not the playbook caught the failure; `-vv` adds every command run (section
5.5).

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
  binary starts, never baked into the binary (see 5.4).
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

1. **Locate the workspace** by walking up from the current directory to the
   nearest `rustible.toml` (section 10.4), load `hosts.kdl`.
2. **Read playbook metadata** by doing a host-native debug build of the playbook
   and running it with `--describe`, which the `#[playbook]` macro generates.
   This yields the target hosts, `escalate`, an optional `ssh_user`, and a JSON
   schema of the typed vars struct (section 10.3). **Accepted trade-off
   (final, 2026-09-06):** the pre-check costs a cold build the first time
   (about a minute) and seconds afterwards. In exchange the schema comes from
   the real compiled types, so there is no source parser of our own to maintain
   and no restriction on var field types. (Alternative considered twice: parse
   the source with `syn`. It is instant but only sound for a closed set of
   canonically spelled types, cannot see through aliases or imports, and needs a
   second parser kept in sync with the proc macro. Rejected.) Mitigations: cache
   describe output by hash of the playbook source plus `Cargo.lock`; dev profile
   with a shared target dir; the describe build shares dependency compilation
   with the target build for same-arch hosts; `rustible inventory check` runs
   only this step.
3. **Resolve hosts and validate vars.** For every resolved host, merge its vars
   (section 10.3) and check them against the schema. Any failure aborts the
   whole run before anything is compiled or uploaded, naming each host and each
   missing or mistyped var.
4. **Connect** to every host in parallel. SSH uses a ControlMaster session
   opened here and reused for everything after (spike 1 measured 20 s for a
   cold Tailscale connection versus 0.4 s for a warm upload, so the connection
   is the expensive part, not the bytes). `connection="local"` hosts run the
   binary as a child process instead. A playbook that sets `ssh_user` logs in
   as that account on every host it targets, in place of the host's `ssh_user`
   (section 6.1).
5. **Probe** each host with one shell command, `uname -sm`, mapped to a
   target triple (musl on Linux, Darwin on a mac, section 5.3). The real
   CLI also resolves `$HOME` here so later paths are absolute. This
   bootstrap probe is the only shell-dependent step; everything after it
   is the static binary.
6. **Compile** once for all needed triples in **one cargo invocation**: a
   `cargo build` with the `dist` profile, the `selected` feature and one
   `--target` per triple, with `RUSTIBLE_PLAYBOOK` naming the playbook. The
   build script includes only that playbook, and the `selected` feature
   keeps the build's output directory apart from the editor's (section 9).
   Cargo accepts several `--target` flags and locks the target directory, so one
   invocation is both simplest and fastest. Per-triple target directories keep
   the caches independent. Use the `dist` profile (section 5.3).
7. **Upload if missing.** SHA-256 the artifact; the target path is
   `~/.cache/rustible/bin/<playbook>-<sha256>`. If it is already there, skip
   the upload; otherwise stream the bytes over the session into a temporary
   file and rename it into place.
8. **Execute** the binary in `--remote` mode, under the host's escalation
   method when the playbook says `escalate = true`; send it the run's start
   (section 5.5) on its stdin, read frames from its stdout until EOF, capture
   stderr separately (panics land there), wait for the exit code.
   When `escalate_user` is neither root nor the login user, the binary is not
   run from the login user's cache: the orchestrator streams it into that
   account's own cache (or a private per-run directory, removed by the end of
   the run) and launches it from there, the places an `as_user` helper uses
   (11.3).
9. **Render** the per-host, per-step view from the event stream as it arrives.
   Facts gathering is the first thing the binary does and is reported as an
   event. What happens between a step's start and its finish (commands run,
   debug logs) belongs to that step; the renderer buffers it under the step.

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
  operations that make sense there; `docs/plan/reports/MACOS-TARGET-SPIKE.md`
  is the measurement and section 6 is why the ops that read `/etc/passwd`
  refuse it by name. Windows targets are deferred. FreeBSD and NetBSD binaries
  build (zig carries their libc; measured 2026-09-13) and wait for operations.

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

**What crosses the channel.** Down, the orchestrator first sends the run's
start: the vars, what the target needs to know about its host (11.1), whether
this is a dry run, the verbosity, and the escalation password when the host's
method needs one (11.3). After that it may send a cancel, and it answers
the binary's file requests chunk by chunk, or with a refusal. Up, the
binary first says hello, naming the protocol version and the playbook (the
name comes from the macro, not `argv[0]`), and then streams events:

- the facts it gathered (13);
- a block opening and closing (11.1);
- a step starting, finishing, or deliberately skipped. A finished step
  carries its status (`ok`, `changed`, `would change`, `failed`), its diff, a
  short note (`action` for an always-changing op, for instance), how long it
  took, and the identity it ran as, so the renderer can mark escalated steps
  (`as root`);
- logs, at debug, info and warning level;
- every command run, with the identity it ran as and its exit status;
- the host's failure (14), and the run's summary: ok, changed, would change,
  skipped, failed, recovered, warnings.

A failed step carries the command that failed, with its exit status and
stderr, whenever its error holds one, so `-v` can show the command and its
stderr whether the playbook caught the failure or not (14).

Files move both ways in chunks (5.6): the binary asks for a workspace file
and receives it a chunk at a time, and sends a fetched file up the same way.
Requests are correlated by id and may overlap: the binary can request a file
while a step runs, and the orchestrator can serve several hosts from one
local read. A cancel (ctrl-c on the orchestrator) stops the run between
steps: the binary never abandons a step half way through its `apply`, and no
further step starts. A binary that has not stopped within a grace period is
killed, and that can cut a long `apply` short.

**Reserved, not built.** Frames for multi-host coordination, a barrier the
binary waits at until the orchestrator releases it and the facts of other
hosts, belong to section 11's tier 3. The channel is bidirectional from day
one so that adding them changes no playbook.

The frames and everything they carry are defined in
`crates/rustible-sdk/src/protocol.rs` and `crates/rustible-sdk/src/event.rs`,
which are the authority on their fields.

**What the operator sees.** The renderer turns the event stream into one
line per step, prefixed with the host and the blocks open around it, ending
in the step's status and a one-line summary of its change: how many lines
changed, which attributes, or the first line of a longer summary. `-v` adds
the facts each host reported, the full diff under every step that carries
one, debug logs, and a failed step's command with its stderr, once per
failure, whether or not the playbook caught it. `-vv` adds every command
run, with the identity it ran as, its exit status and its time. `--json`
prints the frames themselves as JSON lines instead of rendering them, for a
program to read; their content is the protocol's (above).

**Protocol versioning.** The binary's hello names a protocol version, and
the orchestrator compares it with its own before the first step, refusing a
mismatch and saying how to bring the workspace to the CLI's release. An
incompatible change to the frames, or to anything they carry, bumps it. A
field added with a default leaves an older peer able to read the frames and
needs no bump, but may take one on purpose when an older peer would read them
and show less than it should. The escalation helper's wire (11.3) is outside
the version: a helper is always the same build as its parent, so it never
meets an older peer.

The goal is that any skew between the CLI and a workspace's crates means
"rebuild", never silent breakage. **That is not yet true.** Today only the
protocol version is compared, so two releases that speak the same version
but behave differently are not told apart, and the identity that would tell
them apart, a hash of the SDK and op-crate set the binary was built from, is
not built. `docs/plan/DECISIONS.md` records the gap.

**The binary's modes.** One playbook binary answers to four flags: none
(local pretty run, for development), `--remote` (driven by an orchestrator),
`--describe` (print metadata and vars schema as JSON, section 5.2), and
`--helper` (serve `Backend` primitives to a sibling process, section 11.3).

### 5.6 Getting local files to the target (DECIDED: both ways)

Ansible's `copy` and `template` ship controller-side files to the target. Rustible
supports two mechanisms, each for a different need:

1. **Embed at compile time** via `include_bytes!` / `include_str!`, or a
   compile-time template engine (askama-style). Use for small, fixed files and
   templates. The binary stays self-contained. An embedded file is handed to
   `file::Copy` as its content, with the destination and mode the step sets.
   A compile-time template renders a struct whose fields the template names,
   so a misspelt field in the template is a compile error, and
   `file::Template` writes the result. That op is wave two (6.9) and not
   built; today a playbook builds the text in Rust and copies it
   (`docs/USING_RUSTIBLE.md`, "Putting a variable into a config file").

2. **Stream over the channel at run time.** `ctx.local_file`, given a
   workspace path, asks the orchestrator for that file, receives it a chunk
   at a time, writing each as it arrives, and returns a temp path on the
   target, removed when the run ends; anything outside the workspace is
   denied. `ctx.local_secret` returns a secret's bytes in memory only. Use
   for large files, files generated right before the run, and secrets that
   must not sit inside a binary in a build cache.
   `examples/workspace/playbooks/demo/streaming.rs` streams a file, loads a
   secret and fetches a file back.

Rule of thumb: embed by default, stream when large, dynamic, or secret.

**Large stays large end to end.** Nothing on the way holds a file whole. The
channel carries it in chunks, the escalation helper does the same (11.3), and
the operations that move file contents (`file::Copy`, `http::Download`,
`archive::Extracted`) read and write as they go. So a file's size is bounded
by the target's disk, not by memory, as root or as any account, and no
operation imposes a default size limit; a playbook that wants one asks for
it. A streamed write lands whole or not at all: it is staged beside its
destination with its mode and owner already set, and renamed into place only
when complete, so a failure part way leaves the destination as it was
(though a process killed part way can leave its staged file beside it).

The reverse (Ansible's `fetch`) uses the same channel upward, in chunks, and
whatever receives the file stages it beside the destination and renames it
into place when the last chunk has arrived.
Cross-host copy (Ansible's `synchronize` with `delegate_to`) is deferred; it is a
coordination feature, not a backend concern.

## 6. Playbook programming model

### 6.1 Playbook file shape

A playbook is a Rust file under `playbooks/`. It imports the prelude and the
modules of the operations it uses, and marks one function, `main`, with
`#[rustible::playbook(...)]`; `main` receives the run's `Ctx` and returns a
`Result`. Its body is ordinary Rust, and each thing it does to the machine
is a `ctx.step`. Two things it shows that YAML could not:

- **Typed outputs flow into the next step.** A step returns its op's typed
  output, and the next op is built from that value directly. The account
  `user::Present` returns is what `ssh::authorized_keys::Present` is told to
  manage, so the keys op receives the account's uid, gid and home, not a
  name to look up again; it creates `~/.ssh` itself (6.7). A misspelt field,
  or an output handed to an op it does not fit, is a compile error.
- **Reacting to change is an `if`.** Every step's result says whether it
  changed, and the playbook branches on that, with no handler mechanism
  (6.6).

`examples/workspace/playbooks/hello.rs` is the smallest playbook there is,
and `examples/workspace/playbooks/vagrant.rs` hands the account from
`user::Present` to `authorized_keys::Present`, among much else; CI compiles
both.

**What the operator sees.** The orchestrator renders the event stream as one
line per step: the host in brackets, the step's name, its status (`ok`,
`changed`, `would change`, `skipped` or `FAILED`), and a one-line summary of
its change, such as the uid a new account received or how many keys were
added. The run ends with a recap, one row per host counting ok, changed,
would change, skipped, failed, recovered and warnings. Section 5.5 has what
each verbosity adds; `crates/rustible-cli/src/render.rs` is the renderer.

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

**The contract.** An op has two halves and a value between them. It names
two types of its own, its output and its intent; `check` looks at the
machine and returns a plan, and `apply` takes the intent from that plan and
returns the output. Both reach the machine only through the `System` they
are handed (section 7). The definition is the `Op` trait in
`crates/rustible-sdk/src/op.rs`, with `Plan` and the `Intent` trait beside
it.

- **`check` observes and decides, and does not mutate** (7.3 has the one
  deliberate exception). It answers either that the system is already in
  the desired state, with the op's output, or that something must change,
  with the op's **intent**: a type of the op's own whose typed fields say
  what `check` observed and what it decided to do. An intent never holds
  what `apply` will produce (section 12).
- **`apply` executes that intent.** It does not inspect the system again or
  plan again; it may read for itself whatever its output needs beyond the
  intent (a gid to report, a digest), because a read is not a decision.
- **The report is rendered from the intent.** `ctx.step` asks the intent for
  the step's `Diff`, so what is reported is what runs, and the diff shown in
  check mode is exactly the change that would be applied. That is what makes
  a dry run trustworthy.
- **`Diff` is opaque.** It can be built and rendered, never matched or read
  field by field, and an intent never contains one. An `apply` that read its
  instruction out of the report would let rewording a report change what
  runs.
- A read-only op's intent is a type with no values, so its `apply` is proved
  unreachable by the compiler.

**What `ctx.step` does.** It announces the step and runs `check` with the
system marked as checking, so a file mutation there is refused (7.3). When
`check` is satisfied, the step reports `ok` and returns the output. When it
asks for a change under `--check`, the step renders the diff, reports `would
change`, drops the intent unexecuted and returns without an output (12).
Otherwise it renders the diff, runs `apply` with the intent, and reports
`changed`, or `ok` for an action that ran and changed nothing (6.4). What it
returns is the op's typed output together with whether the step changed and
its diff; it derefs to the output, so `account.home` and `account.changed`
both work. `crates/rustible-sdk/src/op.rs` and `ctx.rs` are the authority on
the trait and the driver.

**Policies learned in spike 3, now rules for the stdlib:**
- **No predictions.** Spike 3 found that both of its ops computed their
  post-apply output while planning, so handing it over as a prediction cost
  nothing, and "every stdlib op predicts unless it genuinely cannot" became
  the rule. Two waves of the stdlib showed where the cost lands: not in the
  code but in the judgement. Every op that creates something had to rule on
  which fields it may honestly claim before the tool has run (a uid it has
  not allocated, the shell BusyBox picks from an environment the op cannot
  see, a version apt has not resolved), each ruling needed its own
  decision-log entry, and the report never distinguished a prediction from a
  fact. The rule was reversed: a would-change step has no output in check
  mode, and section 12 has the rule.
- **Builders end in a finishing call for the one mandatory piece of desired
  state.** `file::Line` is built from its path, then the pattern and options,
  and finished by `set`, which takes the line and returns the `Op`, so a
  `Line` without a line cannot be constructed.
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
  stop a purge being asked of a package meant to be present at compile time,
  which is YAML hell in Rust clothing.
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

An action need not name a subject, and its output type is whatever it can
honestly report. `systemd::DaemonReload` makes the manager re-read its unit
files: it names no unit, so it cannot return the `UnitState` the rest of that
module returns, and there is nothing else to read back afterwards. Its output is
`()`. An output that only echoes the op's own inputs is an input, not an output.

`shell::Command` can be promoted toward state-like behavior with Ansible's
escape hatches as builder methods: `.creates(path)` makes `check` return
`Satisfied` when the path exists; `.removes(path)` likewise. `changed_when` maps
to a closure over the output.

An op whose `check` always returns `Change` can mark itself as always
changing, so the orchestrator can mark those steps in the output. This
preserves the "where does this playbook stop being idempotent" scan without a
second verb.

An action that can only tell after running whether anything happened reports
`ok` when nothing did, rather than `changed`: `shell::Command` with
`.changed_when(..)`, `http::Request` for a `GET`, `HEAD` or `OPTIONS` unless
the playbook says otherwise, and a lookup of remote state (6.5). Under
`--check` it has not run, so it reports `would change`.

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

A second shape, showing removal and loops: a playbook looks an account up
with `user::Existing`, revokes compromised keys with
`ssh::authorized_keys::Absent` for that account and logs how many it
removed, then loops over a list of group names; each iteration ensures the
group with `group::Present` and the account's membership in it with
`user::Membership`, which is built from the account and the group step's
output, and each step's name carries the group's name.
`examples/workspace/playbooks/vagrant.rs` loops over its tarballs the same
way, one named step per iteration.

Note the three shapes on one resource, following rule 6.3:
`authorized_keys::Present` ("ensure these"), `authorized_keys::Present` with
its `exclusive` option ("ensure exactly these": still the present state, with
the option of removing strangers, and the output gains a `removed` list), and
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
`apt::Present`, `apt::Absent` and `apt::Latest`. Each takes a list of
packages. `Present` and `Latest` can refresh the package lists when they are
older than a given age, and `Present`'s output says which packages the step
installed and which were already there; `Absent` can purge and autoremove.
`examples/workspace/playbooks/vagrant.rs` installs a package with a refresh
age. All three refuse on a host whose package manager is not apt, and when not
running as root. Two defaults differ from Ansible's: recommended packages
are not installed unless the playbook asks for them, where Ansible follows
the system's apt configuration; and `Present` refreshes the lists only when
it is about to install something, so it is not a way to refresh them for
later steps, as Ansible's `update_cache` is.

`Latest` compares each installed version against the *candidate* apt would
install, and the candidates come from the package lists, so with
`.update_cache(max_age)` a real run refreshes stale lists in `check`, before
it decides: the one op that changes the machine from `check` (7.3). Under
`--check` the refresh is not run: a dry run contacts no mirror and writes
nothing (12). So when the lists are older than the max age the step cannot
know the candidates, and says so: it reports `would change`, with a diff
saying the lists were not refreshed, and has no output. Lists within the max
age need no refresh, and the dry run plans from them exactly as the real run
will. Without `.update_cache(...)` neither mode refreshes, and both compare
against whatever the lists already say. Ansible's `apt` also skips the
refresh under check mode (`apt.py`, `if not module.check_mode:
cache.update()`), but then plans against the stale lists, so its dry run can
call a package current that the real run upgrades; Rustible says it does not
know instead.

**`ansible.builtin.lineinfile`** becomes `file::Line`. It is given a file, a
regular expression for the line to replace, and the line to set, with an
optional backup; disabling password authentication in `sshd_config` is one
step, and its result says whether the file changed and where the backup
went when one was made. `check` plans the rewritten text and returns
`Satisfied` when it is already there; `apply` backs up if asked and writes
exactly that text, atomically (7.3). `examples/workspace/playbooks/mac.rs`
edits a line this way.

**`ansible.builtin.systemd`** becomes one op per state and one per action.
Enabling `sshd` is a `systemd::Enabled` step, and restarting it after its
config changed is a `systemd::Restart` step inside an `if` on the config
step's result (6.6), optionally reloading the manager first.
`Enabled`, `Running` and `Stopped` are states. `Restart` is an action (6.4):
its `check` always plans it, and the step fails unless the unit is running,
or on its way up, afterwards. `DaemonReload` is the reload on its own, for a
playbook that writes a unit file and wants systemd to notice it without
bouncing anything: it names no unit and returns `()` (6.4).

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

`System` is concrete: an op's `check` and `apply` take it as it is and never
see a generic or `dyn`. It knows the target's facts, the identity it acts as,
whether the run is a dry run, and where to report what it does; behind it is
a swappable backend: `Local` on a real machine, `Fake` in unit tests, and
`Elevated` (section 11.3) for another identity, with a `Chroot` or
`Container` backend possible later. What must hold:

- **Every effect on the machine goes through `sys`, reads included**: files,
  directories, attributes, commands. Otherwise the `Fake` is one nobody hits
  (7.2).
- **Commands** run as the `System`'s identity, with `LANG=C` and `LC_ALL=C`
  forced, and each one is reported as an event the renderer shows at `-vv`.
  A non-zero exit is an error carrying the command, its exit status and its
  stderr (14), unless the op says that a failure is an answer, as a "does
  this succeed" probe does.
- **Writes are atomic.** A write is staged as a temporary file beside its
  target and renamed over it, so the target is never seen half written.
  Content of any size streams rather than being held whole (5.6). A
  requested mode and owner are applied to the staged file before the
  rename, so the file never appears with the wrong ones; without one, a
  rewrite keeps the existing file's mode and, where the identity may give
  it, its owner, as Ansible's `atomic_move` does (7.1), so rewriting a
  `0440` file leaves it `0440`. A new file with no requested mode gets the
  mode any new file gets.
- `System` has no temp-directory or common-file-attributes helper; an op
  that needs one builds it from the primitives.
- Ops are synchronous. The event channel is the only concurrent thing in the
  binary and is owned by `Ctx`, not `System`.
- **SSH is not a backend.** The binary runs on the target, so every file is local.
  SSH is the orchestrator's transport only. In production the backend is always
  `Local` (or `Elevated`, section 11.3, which proxies to a `Local` in a helper
  process). (In a local-brain design SSH would have been a backend; this is one
  of the payoffs of remote-brain.)
- **`check` cannot change the machine, with one deliberate exception.** The
  mutation guard covers files, not commands (spike 3): `sys.cmd()` must work
  inside `check` (an op reads state by asking a tool), so nothing can stop
  a `check` that runs `apt-get install`. A file mutation through `sys` during
  `check` is refused, in the main process and again inside an escalation
  helper; process honesty is the op author's. The exception is
  `apt::Latest`: in a real run it refreshes the package lists from `check`
  when asked to, by design, because its answer is read from those lists
  (6.8). Under `--check` it does nothing of the kind (12). A test in which a
  deliberately bad op writes in `check` verifies that it is refused.

`crates/rustible-sdk/src/system.rs` and `crates/rustible-sdk/src/backend/`
are the authority on the methods and the primitives.

### 7.4 What is deliberately not on `System`

**Users and groups are not backend primitives.** `user::Present` reads the
account database and runs the distribution's account tool, both through
`sys`. Putting
`create_user` on `System` would force `System` to know that Alpine uses `adduser`
with BusyBox flags, which is distro knowledge that belongs in the op, chosen via
`facts.distro`. Layering:

- `System`: primitives identical on every Unix (files, directories,
  attributes, processes, identity; `backend/` is the authority).
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
   no harness. It gives what the container harness cannot. Structurally: its
   own kernel and `/proc/sys` (a container shares the host kernel, so a write
   is refused or reaches the host), and a real boot (a harness container runs
   systemd as pid 1 only in `systemd_images` mode, privileged, on the host's
   kernel and cgroups). By the harness's choice: every body runs as root, so
   no non-root login escalates through the host's sudoers, and nothing
   connects over SSH. And a target no container can be, macOS (there is no
   macOS container), which only the macOS runner covers: the Darwin probe,
   the Mach-O build through zig, launchd and brew. In CI the machine is a
   Linux VM over SSH, on both architectures, or the macOS runner itself over
   a local connection; only the VM exercises the SSH transport. Each
   playbook that converges a machine is run twice, and the second run is
   the test: on the VM it must report nothing changed, and on the macOS
   runner its recap must match the counts expected of it (an always-changing
   action still reports `changed`), because a first run reporting `changed`
   proves only that the op did something. The VM is
   distribution-specific in a way T2 is not — one guest is one distro — so
   its CI job names the distribution it covers. `CLAUDE.md` ("The machine
   tier") and `docs/DEVELOPING.md` name the playbooks, the guests and the
   jobs.

## 9. Project layout and ecosystem

- **A Rustible project is one Cargo package.** `rustible init` creates it.
- **A playbook is any `.rs` file under `playbooks/` that carries
  `#[rustible::playbook]`. A build script finds them. Nothing is ever indexed
  by hand (DECIDED 2026-09-07).** The requirement, from vetting: the Ansible
  experience, where a playbook file simply exists and gets used, with no
  manifest entry to add on create or remove on delete, while keeping full
  rust-analyzer, clippy, types, and completion in every playbook file.

  **How it works.** The workspace has one bin target, `src/main.rs`, and a
  `build.rs`, both written once by `rustible init` and both **shims that
  must stay shims**: each is a doc comment plus a call into a crate (the
  `rustible` runtime, and the `rustible-build` scanner), so all logic lives
  in crates and a fix ships as a version bump, never as "edit your
  main.rs". Each file opens with a header saying that `rustible init`
  generated it, that it is not to be edited, where playbooks and shared code
  go, and that `rustible init --refresh` regenerates it. The CLI does **not**
  hash-check or police these files: a power user may edit them, at their own
  risk, and the header comment is the whole safeguard. (A hash-and-warn
  scheme was proposed during vetting and rejected as unnecessary nannying.)
  The build script parses every file under `playbooks/` (a real parse, so
  the attribute in a comment or a string does not count) and compiles each
  file with a function marked `#[rustible::playbook(..)]` into the bin crate
  as a module, registered under its name; the runtime picks the entry by
  name and speaks the protocol. Files without the marker are not playbooks:
  they are ignored unless a playbook pulls them in with a `mod` declaration
  or `#[path]`, so helper code may live next to playbooks. Two marked
  functions in one file is a build error naming the file. The same scan
  backs `rustible playbook list`.

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

  **Isolation.** The CLI sets `RUSTIBLE_PLAYBOOK=ops/x` when building for a
  run, and the build script (with `rerun-if-env-changed`) includes only that
  playbook. Verified: a playbook with a type error elsewhere in the tree does
  not affect `rustible playbook run` of another one. The shipped binary
  contains exactly one playbook, stays small, and is hashed per playbook for
  the target-side cache. With the variable unset, as in the IDE, `cargo
  check`, and CI, every playbook is included, so every broken playbook is
  visible while editing and fails CI, which is the desired behaviour, not a
  wart.

  **The `selected` feature, and why it must exist (DECIDED 2026-09-07).** The
  selected build and the editor's own `cargo check` are the same package with
  the same feature set and profile, so Cargo gives them the **same build-script
  output directory**. The discovery spike showed what that does in practice:
  with `playbooks/top.rs` open and healthy in the editor, a terminal
  `RUSTIBLE_PLAYBOOK=ops/a cargo build` rewrote the registry file
  rust-analyzer was reading down to `['ops/a']`, and `top.rs` immediately
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
  (verified; an earlier draft of this paragraph said `ops/x/helpers.rs`,
  which is wrong), as `examples/workspace/playbooks/demo/mc.rs` and its
  `helpers.rs` show. A playbook that wants a subfolder layout puts a
  `#[path]` attribute naming `x/helpers.rs` on the declaration. Two
  playbooks in one directory that both declare `mod helpers` each compile
  the same file as a private module, which works. Code shared across
  playbooks lives in the package's `src/lib.rs` and is reached by the
  **package name**, `myinfra::helper()`, not `crate::helper()`, because
  playbooks are modules of the bin crate (verified both ways);
  `examples/workspace/playbooks/hello.rs` calls its workspace's
  `src/lib.rs` this way. A playbook's name is its path under `playbooks/`
  without the extension (`ops/x`); generated module identifiers carry the
  needed `#[allow]`s and leak only into test names and backtraces. The scanner
  matches the attribute path textually (`rustible::playbook` or bare
  `playbook`), which is acceptable for a marker. An unmarked file nobody
  references is silently ignored (rust-analyzer greys it out); `rustible
  playbook list` may warn about such orphans.

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
    Re-exports the SDK, the macros, and the std prelude, so a playbook
    imports `rustible::prelude` and is marked `#[rustible::playbook(..)]`.
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
- **Publishing.** Seven crates ship from this repository: `rustible`,
  `rustible-cli`, `rustible-sdk`, `rustible-macros`, `rustible-build`,
  `rustible-std`, `rustible-github`. They are versioned in lockstep through
  `workspace.package.version`, so one tag releases all of them.
- **A playbook binary has four modes** (section 5.5): plain local run,
  `--remote`, `--describe`, `--helper`. The macro generates all of them.
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
the built-in default. For `ssh_user` only, a playbook's `ssh_user` attribute
outranks all four (section 6.1); `inventory show` describes the inventory and
does not apply it. Parameters never come from `vars` and vars never from
properties.

### 10.2.2 Full example

`examples/workspace/hosts.kdl` is a complete inventory that shows every kind
of node above: workspace-wide `vars` (the "all" level), `defaults` for the
workspace-wide parameters, a local host, groups carrying parameters and
vars, a host that overrides its group's var, a list var given as positional
arguments, a group of groups, a group that cherry-picks hosts by name, and
a host disabled with slash-dash, children included. The CLI's tests load it
(`example_workspace_inventory_loads`), so it cannot drift from the parser.
`docs/HOSTS_KDL_REFERENCE.md` is the reference for the format.

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

In a playbook, the vars struct sits beside `main`: a plain field is
required, an `Option` is optional, and a `#[default]` attribute gives a
field its default. The playbook attribute names the struct, and `main`
receives it as its second parameter. `examples/workspace/playbooks/vagrant.rs`
and `examples/workspace/playbooks/demo/mc.rs` declare vars with defaults.

When validation fails, the error says how many of the targeted hosts do not
satisfy the playbook's vars and names the target and the playbook; it lists
every host, `ok` or the var it lacks or mistypes; it recalls the order vars
are resolved from `hosts.kdl` in; and it says where to add a missing var: on
each host that lacks it, or on the group when it is shared.
`crates/rustible-cli/src/inventory/validate.rs` writes it.

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

`Ctx` is what `main` receives: the playbook's handle on the run. Through it a
playbook runs steps and learns about the host it is on, and everything it
does is reported to the orchestrator. Behind it are the `System` the steps
run against, what the target knows about its host (11.1), and the link to
the orchestrator; every `Ctx` of a run shares one step sequence and one
stack of open blocks. What it offers comes in three tiers:

- **Tier 1, the core.** `ctx.step(name, op)` (6.2); the host it runs on, its
  facts (13) and whether this is a dry run; `log`, `warn` and `debug` lines
  in the report; `ctx.local_file(..)` and `ctx.local_secret(..)` for a
  workspace file or a secret streamed at run time (5.6), the file removed
  when the run ends and the secret held in memory and zeroized; and
  `ctx.sys()`, the machine directly (11.1).
- **Tier 2.** `ctx.skip(name, reason)` for a step deliberately not run;
  `ctx.block`, given a name and a closure, to group steps, returning what
  its closure returns; `ctx.as_user(..)`, `ctx.as_root()` and
  `ctx.as_escalated()` for another identity (11.3); and `ctx.fetch(..)`, a
  file from the target back to the operator's machine (5.6).
- **Tier 3, reserved, not built.** `ctx.barrier(name)`, where every host
  waits until all have arrived; `ctx.run_once`, a named closure run on one
  host the orchestrator chooses; and `ctx.peer_facts(host)`, another host's
  facts. They are blocking calls over the channel (5.5), and the channel is
  bidirectional from day one so that adding them changes no playbook.

`crates/rustible-sdk/src/ctx.rs` is the authority on the signatures.

### 11.1 Decisions embedded

- **`vars` is a parameter of `main`**, its second after the `Ctx`, not
  a method on `Ctx`. The macro deserializes it from the run's start (5.5)
  before `main` runs; failure is a clean per-host message. `Ctx` is untyped
  w.r.t. vars.
- **`sys()` is exposed.** Playbooks are programs; authors are responsible. Reads
  are unreported. Mutations outside a step still go through the backend (logged
  at `-v`) but do not appear as steps; if it should be in the report, make it a
  step (`shell::Command` exists for that).
- **`skip` is explicit and optional.** An `if` that does not run a step makes it
  vanish from the output; `skip` records it with a reason and the summary counts
  it, for the "twelve steps, three did not apply, here is why" report.
- **`block` is a grouping, not an operation.** It draws no step id, moves no
  counter and has no line of its own; every step inside is printed with a
  `[outer][inner] ` prefix, so a line stands on its own however hosts
  interleave. A step belongs to every block open while it runs, through any
  `Ctx` value, an `as_root()` bound earlier among them. It returns
  `Block<T>`, which is only the closure's value: a block has no result of
  its own, so the closure returns what a later step needs. Under `--check`
  it is where a read of a missing output in playbook code ends (section 12);
  a read inside an op's `check` fails that step instead. Blocks nest.
- **`bail!`** (re-exported) is Ansible's `fail` module.
- **Tier 3 calls are blocking calls over the channel** (remote-brain): `barrier`
  tells the orchestrator it has arrived and waits to be released; `run_once`
  is a barrier plus an election by the orchestrator. Reserved, not built.
- **What the target learns about its host** is what a playbook or an
  escalation needs there: the host's inventory name and groups, how it is
  connected, and the escalation parameters (`escalate_user` is what
  `as_escalated` follows). Never its address or port; those are the
  orchestrator's. `HostInfo` in `crates/rustible-sdk/src/ctx.rs` is the
  authority on what it carries.
- **`step` numbering and the summary are shared** across `as_user`
  contexts (they share one counter) and inside blocks (which pass the
  same `Ctx`), so a run has one step sequence regardless of how many `Ctx`
  values exist.
- **In check mode `changed` means "would change".** A playbook that logs after
  a changed step should branch on `ctx.check_mode()` to word it honestly
  (spike 2 caught the `mc` playbook logging "installed mc" in a dry run).

### 11.2 Example

A playbook for a `web` group shows most of `Ctx` at once. It declares a
domain and a defaulted worker count as its vars. It reads the facts first
and, with `bail!`, refuses a host that has no apt, naming the distribution
it found. It ensures nginx with `apt::Present`. It then opens a
`ctx.block` around two steps, writing the site's config from the vars and
enabling the site with `file::Symlink`, and the block's closure returns
whether either changed. When one did, it restarts nginx; otherwise it
records the restart with `ctx.skip` and the reason "config unchanged", so
the report says why the step did not run. It warns, with `ctx.warn`, when
the host has a single CPU. Last, it takes a TLS certificate from the
workspace with `ctx.local_secret`, so the key never sits in a binary, and
installs it with `file::Copy` at mode 0600.

No compiled example shows all of this together.
`examples/workspace/playbooks/vagrant.rs` gates on facts with `ensure!`, and
`examples/workspace/playbooks/demo/streaming.rs` loads a secret.

### 11.3 Per-step privilege escalation (DECIDED)

Escalation is a property of how a step runs, not of the op, so it lives on
`Ctx`: a step is run through `ctx.as_root()`, or that context is bound once
and used for several steps, or `ctx.as_user` steps down to an account such
as `postgres`. `examples/workspace/playbooks/demo/escalation.rs` runs one
step as root from an unescalated playbook, and
`examples/workspace/playbooks/vagrant.rs` steps into two unprivileged
accounts. Playbook-level
`escalate = true` remains for the common case and means the binary is launched
via the inventory's escalation method as `escalate_user` (default root), from
whichever account logged in: the inventory's `ssh_user`, or the playbook's when
it sets one.

**Three identity methods (DECIDED):**
- `as_user(name)`: explicit user.
- `as_root()`: literally `as_user("root")`. It never follows the inventory; a
  method named `as_root` that might run as `admin` would be hidden indirection.
- `as_escalated()`: `as_user` with the host's `escalate_user`, i.e. the
  privileged account the inventory chose for this host (root by default, or
  a shared admin account where direct root is not allowed). This is what
  `escalate = true` uses at launch, exposed per step. Output marks steps
  whose identity differs from the binary's own (`as root`, `as postgres`).

**How it works.** A running process cannot change identity per call, and a
write to `/etc/...` from an unprivileged process gets EACCES. Ansible's answer
is shell tricks (`sudo tee`, chmod dances). Ours: a step under another
identity does its I/O through a helper, which is **the same binary** started
in helper mode as that identity, through the host's escalation method, on
first use, so the helper is always the same build as its parent. The
account has to be able to run that binary, and an account other than root
usually cannot reach the login user's cache (homes are 0750 or 0700 by
default on Ubuntu and Debian, and `~/.cache` is 0700 on macOS), so the
binary is put in a place the account owns: its own cache, kept for later
runs of the same build, or a private per-run directory when it has no
usable home (none, not writable, or `noexec`), which is removed by the end
of the run. A step is refused only when neither is usable, naming both
causes. A helper copies itself there on the target, so nothing extra is
uploaded from the controller. `escalate = true` with an `escalate_user`
other than root or the login lands the binary in the same places (5.2 step
8), streamed there by the orchestrator over the session.
`crates/rustible-sdk/src/launch.rs` and `backend/elevated.rs` are the
authority on how.

The helper serves the same primitives `Local` does, to its parent over its
stdin and stdout, framed like the main channel; it is `Local` wrapped in a
request loop. Anything that can be large, a file read or written, a
directory listing, a command's stdin and output, crosses in chunks, so the
helper never holds a file whole either (5.6), and a write through it lands
whole or not at all, as any streamed write does. A helper is always the
same build as its parent, which is why its wire needs no version (5.5). One
helper per identity, spawned lazily, kept alive for the run, killed at exit.
Properties:

- No extra upload from the controller: the helper is the binary already on the
  target, copied over a local pipe into an unprivileged account's own cache
  once per build.
- Ops know nothing: this is the payoff of routing all I/O through `sys`.
- Stepping down (`as_user("postgres")`) is the same mechanism.
- The check-mode mutation guard holds in the helper (same code).
- Helper commands are reported back and forwarded up as commands run,
  tagged with the identity.
- Cost: one spawn per identity, then a pipe round trip per primitive, or per
  chunk of a large one.

Sudo passwords: `-n` fails rather than prompts. If the inventory's `escalate`
needs a password, the orchestrator sends it with the run's start as a secret
and the helper spawn hands it to sudo on stdin. In memory only, zeroized after
use. A rejected password is refused as soon as sudo says so, and a spawn that
does not answer in bounded time is killed, so a wrong password cannot hang a
run.

## 12. Check-mode semantics (DECIDED)

Problem: `Plan::Satisfied(T)` carries an output, `Plan::Change(intent)` does
not, so in a dry run a step that *would* change has nothing to return, and a
later step that chains from it has no value.

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
  something had to rule on which fields it may claim before the tool has run,
  and the rulings piled up: a new account predicts only with an explicit uid,
  gid *and* shell; apt only when `apt-cache policy` names a candidate; a
  download never, except when only its permissions change. Each ruling was a
  decision-log entry, a guide paragraph and a test.
- The predictions rarely fired where a dry run matters most. On a fresh host
  nearly every step creates something, and the honest answer for a created
  thing was usually "cannot predict", so the playbook author was told to add
  an explicit uid and gid for the dry run's sake.
- The report never showed which values were predictions. The distinction
  existed for the playbook and not for the person reading the run.
- To let a dry run get past a step that *needs* what an earlier step would
  create, `System` grew a registry of planned resources
  (`note_would_create`, 2026-09-08). One op ever wrote to it. Every later gap
  of the same shape — the unit a package ships, the directory a step would
  make, the home an account would get — was answered by declining to extend
  it and adding a check-mode branch in the op instead, so three different
  answers to one question were in the tree at once.

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
- A would-change step's output does not exist. Reading it, through
  `.output()` or through `Deref`, raises an error naming the step.
  `.changed` and `.diff` remain readable.
  Under `--check` that read ends the innermost enclosing `ctx.block` — or,
  outside any block, the playbook body for that host — with a warning naming
  the block and the step whose output was read. (Read inside an operation's
  own `check`, the missing output is that step's failure instead: the step
  fails like any other, `failed` when its error leaves the playbook and
  `recovered` when the playbook catches it (14), and nothing is absorbed.)
  The block yields no value and the run continues after it. This is not a
  failure: nothing failed, the dry run could not see further, and it is
  counted neither `failed` nor `recovered`. Playbooks are written as if
  every output exists, without guards; `.is_available()` remains for a
  playbook that wants to branch inside a block rather than end it. In a real
  run every step has applied and the read cannot fail. Ansible carries on
  with silent garbage; Rustible says where the dry run stopped seeing.
- **Prerequisites are verified when the run is about to act.** An op whose
  `check` would refuse for want of a resource another op in the same run
  could create — a group for an account not there yet, that account, its
  home, a parent directory, a unit; within the two limits Ansible draws above
  — reports `would change` under check mode instead; its diff shows the state
  it would set, or says what it waits for, and names the prerequisite when
  the op knows it by name (a group, an account, a unit). The tolerance is
  gated on check mode, so a real run's `check` takes the refusal, and a dry
  run's plan never reaches `apply` (the step ends after `check`): the
  refusal is never skipped on a run that can act. What a dry run therefore
  does not catch is
  a forgotten prerequisite step: the real run refuses at that step, before
  that step touches anything, with the steps before it already applied.
  That is the trade this rule accepts, and 6.7 still holds at the step.
- That deferral covers only what another step could supply. A refusal about
  the machine or the request itself — wrong platform, not root, the tool the
  op drives is absent, a malformed key, a sysctl key this kernel does not
  have — stands in check mode as in a real run, because no earlier step
  changes it.
- **A dry run touches nothing outside the target.** Under `--check` no
  operation opens a connection to anything beyond the machine it runs on — no
  request of any method, to any service, however read-only its author
  believes it to be. A request can be logged, counted against a quota,
  billed, or have effects its method does not advertise, and none of that is
  visible from the client; a dry run is only worth running if nobody has to
  wonder what it did. An operation whose answer depends on remote state
  reports `would change` under check mode, with a diff saying what it would
  send and that the remote state was not read; its output is unavailable, as
  for any would-change step. What this costs is a dry run that cannot report
  such a step `ok`, and that cost is accepted.
- **`check` still cannot mutate a file through `sys`** (7.3), in a dry run
  or a real one. The one op that changes the machine from `check` by design,
  `apt::Latest` refreshing its package lists in a real run because its
  answer is read from them, does it through commands and runs none of them
  under `--check` (6.8).

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

What the core covers: the operating system, the distribution and its
version, the CPU architecture, the kernel, the hostname, every package
manager present (a playbook asks whether one is there, rather than being told
the one), the init system, the CPU count and memory, and the account the
binary runs as, with whether it is root. The operating system, distribution,
architecture and init system are typed enums with an `Other(String)` variant,
so an unknown value degrades to a string instead of failing; a package
manager Rustible does not know is simply not listed. Deliberately excluded
from the core: mounts, network interfaces, users and groups, installed
packages, environment. Each is an op when a playbook needs it. Facts cross
the wire, so changing their shape is a protocol change (5.5).
`crates/rustible-sdk/src/facts.rs` is the authority on the fields and their
variants.

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

**Typed values inside the chain.** The SDK's own signals remain concrete
types that the orchestrator can downcast for rendering: a file mutation
during `check` (`MutationDuringCheck`), a read of a would-change step's
output (`OutputUnavailable`), and a failed command (`CmdFailed`, with its
argv, exit status and stderr). Playbooks never need them.

**On the wire.** A host's failure carries the rendered chain as text, the
failed step's id and block path, and the failed command when the chain holds
one; a failed step's own report carries the same command, so `-v` can show
the command and its stderr separately whether the playbook caught the
failure or not, once per failure (5.5). Step names repeat, so the id is what
ties the failure to the step line it closes.

**What the operator sees.** A failure that ends a host closes it with one
line: the host, `FAILED at` and the step's name, then the error's context
chain, from the step down to the failed command and its exit status; `-v`
adds the command and its stderr beneath it (5.5). Inside a block, the
step's own line carries the block path and `FAILED`, and the closing line
names the block path as the step line does.
`crates/rustible-cli/src/render.rs` renders both, and its tests hold the
exact text.

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
- **Remote mode / describe mode / helper mode**: the playbook binary's
  `--remote`, `--describe`, `--helper` flags (section 5.5).
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
