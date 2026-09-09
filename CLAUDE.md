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
the author decides otherwise in the session you are in.

**Do not edit it on your own initiative.** The rule exists so the contract is
not rewritten by whoever happens to be passing. If your work requires an
amendment, write the exact replacement prose in your report and hand it over.
When the author decides the amendment in the session, applying it is normal.

`docs/plan/DECISIONS.md` is the running log of every decision made while
building, each with a `Reverse:` clause saying how to undo it. Add to it, do
not rewrite it. Entries tagged `PROPOSED AMENDMENT` are open questions;
`KNOWN GAP` and `RECOMMENDED` are things deliberately left undone.

`docs/plan/PROGRESS.md` is the resume point. Keep it true: a stale line there
sends the next session hunting for work that is already done, or repeating it.

## The dependency rule

**`rustup target add <triple>` plus `clang` is the entire set of dependencies
for running Rustible, and that must never grow.** Not a preference, not a
default to be revisited: it is the property the project exists to have. Target
hosts need nothing at all, ever.

This is what Ansible lost. Its modules need a Python interpreter on every
target, and anything interesting needs more Python on top, so managing Docker
or a cloud API turns into a dependency negotiation with every machine you own.
Rustible ships one static binary and asks the target for nothing. Every
addition to what a user must install moves us back toward that, and hurts
adoption more than any feature repays.

So: a crate that bundles a C *library* (`openssl-sys`, `libgit2-sys`) is
unsupported, and the fix is the pure-Rust alternative. No cross-gcc, no zig,
no docker for builds. `ring` is the single C dependency, for TLS, and even
there Rustible carries musl's headers itself so nothing else is installed by
hand. If a change appears to require another tool, that is a design problem to
solve, not a requirement to document.

## Other rules

- **`escalate`, never `become`.** `become` is a reserved Rust keyword and the
  name is gone everywhere: the attribute, the inventory, the CLI, the code.
- **Never publish to crates.io.** Releases are cut by tagging and publishing a
  GitHub release, which fires `.github/workflows/release.yml`.
- **Release names are exactly `vX.Y.Z`.** No description, no suffix, no
  "v0.1.0 — the streaming release". The tag and the release title are the
  version and nothing else.
- **Never force-push.**
- **SSH for git.** The remote is `git@github.com:flipbit03/rustible.git`.

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

`examples/workspace` is a workspace of the shape `rustible init` generates,
kept in the repository so the generated layout is exercised by something. It
is **excluded from the cargo workspace on purpose**: it depends on the crates
by path and builds its playbooks through the build script, exactly as a user's
workspace does, and folding it in would change that. The consequence is that
`cargo test --workspace` never compiles it, so a change to an op's builder can
break it silently. CI builds it as its own job for that reason. If you change
a public API, check that workspace too.

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

or `make` for the first five and `make integration` for the last.

`#![deny(missing_docs)]` is on in every library crate, so a new public item
without documentation does not compile.

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
  what to do about it.
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

4. **Machines** (`make vm-test`), the Vagrant guests in `dev/vagrant/`: a real
   SSH transport, a real `sudo`, a live `/proc/sys` and a real init system,
   none of which a container has. Not in CI, optional day to day, and
   **expected of a new operation before it merges** — say in the pull request
   which architecture you ran it on. `docs/DEVELOPING.md` is the setup.

**A test that pins a deadlock or a hang needs a time bound**, or a regression
hangs instead of failing and wedges CI until the workflow timeout.

## TLS, and why clang

Rustible speaks TLS in two places: `http::Download` and the `rustible-github`
collection. The provider is `ring`, reached through rustls, and it is the
reason `clang` is in the dependency rule above.

Ring compiles a small amount of C, and cargo's C helper will not use the
host's compiler for a musl target unless it is named, so `rustible-cli` names
it and supplies the compiler flags itself (`crates/rustible-cli/src/toolchain.rs`).
For `x86_64` musl it also supplies musl's libc headers, which it carries
vendored and unpacks into the workspace cache. For other musl targets ring
needs no libc headers, except that Apple's clang patches its own `stddef.h` to
delegate to the system header when the target is musl, so those get musl's
headers offered as a last-resort include. None of this is visible to a user,
and none of it may grow into a second thing to install.

`rustible toolchain check` reports what a machine can build for, and
`--print-env` prints the compiler environment a build is given. Use it rather
than setting `CC_*` by hand.

## Platforms

Rustible **runs from** Linux (x86_64, aarch64) and macOS on Apple silicon. It
**manages** Linux hosts, x86_64 and aarch64, any libc.

macOS is a controller and never a target: the local probe refuses `Darwin
arm64` by name, because the operations speak apt, systemd and `/etc/passwd`.
A Linux controller cannot build macOS binaries at all, since linking Mach-O
needs an Apple SDK that rustup does not ship.

Operations run on the target, including lookups (vision 5.1), so
`github::UserKeys` needs network egress from the target rather than from the
controller.
