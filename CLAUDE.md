# Rustible

An independent replacement for Ansible. A playbook is an ordinary Rust file
with typed operations and typed outputs. Running one compiles it into a static
musl binary per target architecture, ships it over SSH, runs it *there*, and
streams `changed / ok / failed` back with dry run and diffs. The target needs
nothing installed: no Python, no agent, no runtime.

Read this file first, then `docs/01_VISION.md` for whatever you are about to
touch. This file says how to work here; the vision doc says what the thing is
and why.

## The contract

**`docs/01_VISION.md` is the source of truth.** It is long and it is vetted.
When code and vision disagree, the vision wins and the code is wrong, unless
Cadu says otherwise in the session you are in.

**Never edit it unattended.** The rule exists so a machine cannot rewrite the
contract at 3am. If your work requires an amendment, write the exact
replacement prose in your report and hand it over. If Cadu is present and
decides the amendment, applying it is fine and normal.

`docs/plan/DECISIONS.md` is the running log of every decision made while
building, each with a `Reverse:` clause saying how to undo it. Add to it, do
not rewrite it. Entries tagged `PROPOSED AMENDMENT` are questions for Cadu;
`KNOWN GAP` and `RECOMMENDED` are things deliberately left.

`docs/plan/PROGRESS.md` is the resume point. It is read first on every resume,
so keep it true: a stale line there sends the next session hunting for work
that is done, or repeating it.

## Rules that are not negotiable

- **`escalate`, never `become`.** `become` is a reserved Rust keyword and the
  name is gone everywhere: the attribute, the inventory, the CLI, the code.
- **No toolchain stock rustup cannot drive**: no cross-gcc, no zig, no docker
  for builds. `rustup target add <triple>` plus **clang** is the whole setup,
  and clang is needed only on the machine running `rustible`. Target hosts
  need nothing, ever. A crate that bundles a C *library* (`openssl-sys`,
  `libgit2-sys`) is unsupported; the fix is the pure-Rust alternative.
  `ring` is the one C dependency, for TLS, and Rustible carries musl's headers
  itself.
- **Never publish.** Releases are Cadu's, cut by tagging `vX.Y.Z` and
  publishing a GitHub release, which fires `.github/workflows/release.yml`.
- **Never force-push.**
- `docs/07_UNATTENDED.md` holds the full hard limits for unattended runs
  (which hosts may be touched, sudo scope, and so on). Read it before any long
  autonomous session.

## Layout

| crate | what |
|---|---|
| `rustible-cli` | the `rustible` binary: workspace, inventory, the run pipeline, rendering |
| `rustible` | the facade a playbook imports (`use rustible::prelude::*`) |
| `rustible-sdk` | `Op`, `Ctx`, `System`, `Backend`, facts, protocol, events, testing harness |
| `rustible-std` | the standard operations |
| `rustible-macros` | `#[playbook]`, `#[vars]`, `#[integration_test]` |
| `rustible-build` | playbook discovery for the generated `build.rs` |
| `rustible-github` | the first collection, and the worked example of one |

`examples/workspace` is a user's workspace, **excluded from the cargo
workspace on purpose**. Nothing else compiles it, which is why CI builds it as
its own job. It has rotted twice from an API change nobody noticed.

## How to work

Every change goes through a branch and a pull request, even a one-line doc
fix. CI runs five jobs on each: the fmt/clippy/test/rustdoc gate, an MSRV
check on **1.88**, the example-workspace build, a macOS controller job, and
the Docker harness. All five must be green.

Before pushing, run what CI runs:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib
cargo build --manifest-path examples/workspace/Cargo.toml
RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --tests   # needs docker
```

`#![deny(missing_docs)]` is on in every library crate, so a new public item
without documentation does not compile.

**Pushing anything under `.github/` fails over the https remote**, because the
`gh` OAuth token has no `workflow` scope. Push over
`git@github.com:flipbit03/rustible.git` instead.

## Writing an operation

The shape matters more than the code. Read `crates/rustible-std/src/systemd.rs`
and `crates/rustible-std/src/user.rs` before writing a new one; they are the
reference for voice and structure.

- **One type per desired state, named for it**: `apt::Present`, `apt::Absent`,
  `systemd::Enabled`. Never a `state:` enum parameter. Things that are
  genuinely actions get verbs and always report changed: `systemd::Restart`,
  `shell::Command`.
- **`check` does all the thinking** and produces the diff. **`apply` executes
  that diff**, rather than inspecting the system again. That is what lets the
  `Fake` tests plant a tool's effect and check the result.
- **Predict an output only when every field is honestly knowable.** A new user
  needs an explicit primary group; an apt install needs a candidate version.
  Otherwise return `Plan::change` with no prediction, and check mode reports
  the output as unavailable rather than handing out a plausible lie.
- **Refuse, do not invent.** An operation that manages a user does not create
  the group it references, and `authorized_keys` does not create `~/.ssh`'s
  parent. Fail naming the operation the author wanted.
- **Every message is read by someone at 2am.** Name the thing, say why, say
  what to do. `rustible_std::tls`'s refusal and `toolchain.rs`'s clang message
  are the standard.
- Everything the operation does to the machine goes through `sys`, including
  reads, so the `Fake` is meaningful.

## Testing tiers

Three, and they catch different things:

1. **Pure functions** for parsers and planners.
2. **`Fake` backend** for operation behaviour: satisfied, change, apply,
   failure, refusals.
3. **Containers** (`#[rustible::integration_test]`, fourteen files in
   `crates/rustible-std/tests/`) against real distributions. These are the
   source of truth and they have earned it: they caught that `useradd` refuses
   to create a private group when one already carries the name, and that
   `chown` clears setuid, neither of which a fake can model.

They only run with `RUSTIBLE_INTEGRATION=1`; without it they skip themselves,
so a plain `cargo test` stays offline and Docker-free.

**A test that pins a deadlock or a hang needs a time bound**, or a regression
hangs instead of failing and wedges CI until the workflow timeout.

## Things that have cost time before

- **macOS is a supported controller, never a target.** It cross-builds for
  both Linux targets; the local probe refuses `Darwin arm64` by name. Apple
  patches clang's own `stddef.h` to delegate to the system header when the
  target is musl, and ring's `-nostdlibinc` removes it, so cross-builds need
  musl's headers offered with `-idirafter`. That is handled in
  `rustible-cli/src/toolchain.rs`; do not "simplify" it away.
- **`rustible toolchain check`** answers what a machine can build for, and
  `--print-env` prints the compiler environment a build is given. Use it
  rather than setting `CC_*` by hand.
- The CPU floor is gone with ring, and it was never about old hardware: the
  machine that exposed it is a modern Xeon whose hypervisor masks one flag.
- **Ops run on the target, including lookups** (vision 5.1). So
  `github::UserKeys` needs egress from the target, not from the controller.

## Talking to Cadu

He is a backend engineer and does not need Rust explained. Say what changed,
what it cost, and what is unverified. If something was not tested, say so in
the same breath as the result; a green suite is not a verified one. When a
decision is his (a trade-off, an amendment, anything irreversible), put the
options and a recommendation in front of him rather than choosing quietly.
