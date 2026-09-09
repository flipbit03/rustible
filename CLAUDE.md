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
fix. CI runs five jobs on each, and all five must be green:

| job | what it protects |
|---|---|
| Format, clippy, test | the gate: `fmt`, `clippy -D warnings`, the suite, rustdoc |
| Minimum supported Rust version | the floor stays **1.88** |
| Example workspace builds | `examples/workspace`, which the cargo workspace never compiles |
| macOS controller | the suite on macOS, and a cross-build for both Linux targets |
| Container ops (Docker harness) | tier 3, the 27 container runs |

Before pushing, run what CI runs:

```sh
make                # fmt, clippy, test, rustdoc, example workspace
make integration    # the container tier; needs docker
```

which is these, in the order that fails soonest:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib
cargo build --manifest-path examples/workspace/Cargo.toml
RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --tests   # needs docker
```

`make vm-test` is the one tier CI does not run. It is not part of this list
and it is not optional when an op needs it; see "The machine tier".

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

Four. They are not redundant: each one can see something the tier below it
cannot, and each costs more to run than the tier below it.

| tier | what it is | where it runs | cost |
|---|---|---|---|
| 1. pure | functions with no I/O | `cargo test` | free |
| 2. fake | ops against the `Fake` backend | `cargo test` | free |
| 3. container | ops against real distributions | `make integration`, **and CI** | seconds, needs docker |
| 4. machine | a playbook against a real VM over SSH | `make vm-test`, **not CI** | a minute, needs vagrant |

Today that is 579 tests in tiers 1 and 2, **27 container runs** across
`debian:12`, `ubuntu:24.04`, `alpine:3.20` and two `jrei/systemd-*` images,
and one playbook on each of two architectures.

**Tiers 1–3 run in CI. Tier 4 does not** — see "The machine tier" below for
why, and for what it is nonetheless expected to catch before you merge.

### Choosing a tier

Put a test in the *lowest* tier that can actually fail for the right reason.
A test in too high a tier is slow and flaky; a test in too low a tier passes
while the thing is broken.

- **Parsing, planning, diffing, any decision made from data** → tier 1.
- **An op's behaviour**: satisfied, change, apply, failure, refusal → tier 2.
  The `Fake` lets you plant a tool's output and assert on the op's reaction,
  which is why `check` must do all the thinking and `apply` must execute the
  plan rather than re-inspecting.
- **Anything where the answer comes from a real tool** → tier 3. This is the
  source of truth for how `useradd`, `apt-get`, `systemctl` and friends
  behave, and it has earned it: it caught that `useradd` refuses to create a
  private group when one already carries the name, and that `chown` clears
  setuid. A fake models what you *believe*; a container shows what is.
- **Anything a container structurally cannot do** → tier 4. That list is
  short and specific: writes to `/proc/sys` (a container shares the host
  kernel, so the write is refused or hits the *host*), a real init system, a
  real `sudo`, and the SSH transport itself. `it_sysctl_present.rs` says so in
  its own header — it runs with `.apply_now(false)` and asserts only on the
  drop-in file, because the live write is not available to it.

If a new op needs nothing from tier 4, it does not need a tier-4 test. Say so
in the pull request rather than adding a step to the playbook for symmetry.

### The machine tier

`dev/vagrant/` holds two Debian 12 guests, `x86` and `arm`, from one
multi-architecture box: vagrant-libvirt on Linux, vagrant-qemu on macOS.
`docs/DEVELOPING.md` is the per-platform setup. The loop:

```sh
make vm-up          # the guest whose architecture matches this host
make vm-ssh         # a shell in it, passwordless sudo
make vm-test        # the playbook, twice
make vm-status      # what is up, and the inventory naming it
make vm-destroy     # give the disk back
make vm-orphans     # domains left behind by a deleted checkout
```

`make vm-up` brings up **only** the guest matching this host's architecture,
because that is the one marked `autostart` — and the one `vm-ssh` reaches
without being told which. The other architecture is always emulated, so it is
opt-in by name: `make vm-up-arm`, `make vm-up-x86`. Both spawn fast; the
emulated one is slow to *work in*, not slow to start.

`make vm-test` runs `examples/workspace/playbooks/vagrant.rs` **twice** and
fails unless the second run reports nothing changed and nothing failed. The
second run is the test. A first run reporting `changed` proves only that the
op did something; an op that rewrites a correct file every pass reports
`changed` too. Limit it with `make vm-test HOSTS=vagrant-arm`.

To drive the machines yourself — a dry run, more verbosity, a playbook of your
own — use the inventory `vagrant up` generates. Do not paste it into a tracked
file; that is what `--inventory` is for:

```sh
rustible --workspace examples/workspace \
         --inventory dev/vagrant/hosts.vagrant.kdl \
         playbook run vagrant --check -v
```

Three things that will bite you:

- **Under libvirt, only one of the two machines may exist at a time.** They
  share one box volume in libvirt's storage pool, and each machine's disk is a
  copy-on-write overlay on it, so giving one machine the other architecture's
  image corrupts the one you left behind. `vagrant up` refuses and names the
  `vagrant destroy` to run. macOS has no shared pool and no restriction.
- **Destroy the machines before deleting a checkout or a git worktree.** The
  state tying a libvirt domain to Vagrant lives in `dev/vagrant/.vagrant/`;
  remove that first and the domain keeps running with nothing able to stop it.
  `make vm-orphans` finds them and prints the `virsh` commands.
- **It is not in CI, and it is still expected of a new operation that needs
  it.** Nothing enforces this. Say in the pull request which architecture you
  ran it on, or say why the op needs nothing tier 4 provides.

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
