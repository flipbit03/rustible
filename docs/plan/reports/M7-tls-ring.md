# M7-ring: the TLS provider moves to `ring`, and the toolkit becomes rustup + clang

Branch `tls-ring`. Implements Cadu's decision on
`docs/plan/reports/C-TOOLCHAIN-SPIKE.md`: **the required toolkit becomes rustup
plus clang, and the TLS crypto provider becomes `ring`.** `rustls-graviola` is
gone from the tree, and so is the CPU pre-flight that existed only to keep it
from aborting mid-playbook.

The thing this is for, demonstrated rather than argued: under
`qemu-x86_64 -cpu Haswell`, the shape of Cadu's Vultr outpost, the graviola
build of an identical probe refuses the request and the ring build fetches
`https://github.com/flipbit03.keys` and prints `OK status 200 keys 4`. Ring also
runs on `qemu64` and `Opteron_G1`, which have no AES-NI at all.

---

## What changed

| | before | after |
|---|---|---|
| `[workspace.dependencies]` | `rustls-graviola = "0.4"`, `rustls` without `ring` | no graviola; `rustls` gains its own `"ring"` feature |
| `rustible-std` | `rustls-graviola.workspace = true` | nothing; the provider comes from `rustls` |
| `rustible-github` | no crypto entry (uses `rustible_std::tls`) | unchanged |
| `rustible_std::tls` | provider + `preflight` + `preflight_url` + `cpu_features` + `missing_cpu_features` + `unsupported_cpu` + 2 hardware constants + 7 tests | `provider()` + 1 test |
| `http::Download::fetch` | `tls::preflight_url("http::Download", &self.url)?` | gone |
| `github::fetch::Https::get` | `tls::preflight_url("github::UserKeys", url)?` | gone |
| `rustible-cli` | — | `src/toolchain.rs`, `build.rs`, `vendor/musl-headers/` |
| `rust-version` | 1.89 | **1.88** |

### Ring is reached through rustls, not as a direct dependency

The brief said `ring` replaces `rustls-graviola` in `[workspace.dependencies]`.
It does not, and the reason is worth stating: a direct `ring` dependency would
mean assembling a `rustls::crypto::CryptoProvider` by hand out of cipher suites,
key-exchange groups and a signature-verification set. That is more surface to
get wrong than this change is worth. `rustls::crypto::ring::default_provider()`
is the constructor rustls itself maintains and keeps in step with its own
version of ring, so there is one version constraint instead of two that can
drift. The `rustls` entry carries `features = ["std", "tls12", "ring"]`, and
`rustible_std::tls::provider()` still names the choice in exactly one place,
which is what the shared module was for.

`rustible-github` still has no crypto dependency of its own, for the same reason
as before: it already depends on `rustible-std`, so the two cannot disagree
about what crypto they speak.

### What survived in `tls.rs`, and why

`provider()`, its module documentation, and one test.

`provider()` earns its place because it is the single point where the choice is
made, shared by `rustible_std::http` and `rustible-github` and copyable by a
third-party collection. Everything else in that file existed only because
graviola asserts its instruction-set requirements instead of detecting them:
`preflight`, `preflight_url`, `is_https`, `cpu_features`,
`missing_cpu_features`, `unsupported_cpu`, `EXCLUDED_HARDWARE` and seven tests.
Ring dispatches on CPUID and falls back to baseline code, so a pre-flight
against it could never fire. Keeping one would be code that lies about the
risk, and a reader would reasonably conclude Rustible has a hardware floor when
it does not.

The one test kept, `the_provider_builds_and_has_cipher_suites`, is cheap and
guards the one thing that would otherwise fail silently: if the `ring` feature
were ever dropped from the `rustls` entry, the module would stop compiling, but
if some future rustls made the feature additive-but-empty the failure would
surface much later as a handshake error. Asserting the provider has cipher
suites and key-exchange groups catches that at `cargo test`.

The `is_https` scheme gate went with the pre-flight it guarded. That was a
review fix on the graviola branch, and it has no job left: there is nothing to
refuse a plain-HTTP request for.

---

## The clang pre-flight

Left alone, a machine without a usable compiler fails like this, which is what
the spike recorded:

```
error occurred in cc-rs: failed to find tool "aarch64-linux-musl-gcc": No such file or directory
```

That names a program nobody should install, leads to a 103 MB third-party
download if followed literally, and never mentions clang. The trap is that this
machine has a perfectly good gcc and still fails, because cc-rs only looks for
the target-prefixed name.

### Where it lives

In `Cargo::build`, the one function every Rustible-driven cargo invocation goes
through: the `dist` builds of `playbook run`, the host-native describe build,
and `inventory check`. Putting it at the funnel rather than at the top of
`playbook run` means no path added later can skip it, and the check sees the
exact triple list it is about to build for instead of guessing from the
inventory. It also sets the environment for that same invocation, so the check
and the fix are one piece of code.

`rustible init` is separate, and warns rather than fails, because creating a
workspace compiles nothing. On this machine, which has gcc and no clang:

```
warning: no `clang` on PATH. Rustible's TLS provider (ring) compiles C, so building a playbook binary needs a C compiler.
         /usr/bin/cc can serve x86_64-unknown-linux-musl, this machine's own architecture, so a
         playbook for hosts like this one will build. Any other architecture needs clang:
             sudo apt install clang   (or: dnf install clang, pacman -S clang, apk add clang)
```

### Being precise about when clang is genuinely required

The spike's section 1.4 found that on an ordinary x86_64 glibc Linux host, plain
`gcc` builds the x86_64 musl target using the host's own headers. Refusing that
machine would be exactly the "refuse a machine that would have worked" failure
the graviola pre-flight was fixed for. So the rule is:

- **clang serves every musl target.**
- **The host's own `cc` serves the musl target of the host's own architecture,
  and only that one, and only on Linux.** The headers it falls back to are then
  both the right architecture and the right operating system; a mac's SDK
  headers are Darwin's and would not serve a musl Linux target, so this is not
  merely an architecture comparison. Nothing is lost on macOS, which always has
  clang. `CC`, else `cc`, else `gcc`.
- **Non-musl triples are left entirely alone.** cc-rs's defaults are right for a
  glibc or Darwin target and guessing would break them.
- **Anything already in `CC_<triple>` or `CFLAGS_<triple>` wins.** An operator
  who wants a different compiler is never overridden.

A host-only build therefore needs no clang. A build for any other architecture
does, and gets this, live from this machine against the ARM VM:

```
[arm]  FAILED: no `clang` on PATH, and this playbook has to be built for
aarch64-unknown-linux-musl. Rustible's TLS provider (ring) compiles a little C,
and building for any architecture but this machine's own needs clang:
/usr/bin/cc serves x86_64-unknown-linux-musl alone. One package covers every
target, Rustible supplies the musl headers and compiler flags itself, and the
target hosts still need nothing.
Install it and run this again:  sudo apt install clang   (or: dnf install clang, pacman -S clang, apk add clang)
```

With `clang` on `PATH` the same command cross-builds and runs on the ARM VM.

### When cc-rs still escapes

The hint is appended to a failed `cargo build` **only when this machine has no
clang**, and it is worded as "if the output above mentions `cc-rs`". Two
alternatives were considered and rejected. Attaching it unconditionally would
put a compiler note under every ordinary Rust type error. Capturing cargo's
stderr and matching `cc-rs` in it would be exact, but it replaces cargo's
terminal with a pipe, and cargo then stops drawing its progress bar; a
15-second build losing its progress indicator to sharpen an already-rare
message is a bad trade. The pre-flight refuses what it knows cannot work, so
what escapes is a compiler that exists and did not do the job, and naming clang
is only useful where there is none.

---

## The x86_64 musl headers

Ring's `build.rs` passes `-nostdlibinc` and defines `RING_CORE_NOSTDLIBINC` for
musl targets that are **not** x86_64, and only when the compiler is clang-like.
On x86_64 it needs real libc headers: `include/ring-core/check.h` includes
`<assert.h>` unguarded, and clang's own `immintrin.h` pulls `mm_malloc.h`, which
pulls `<stdlib.h>`.

**Vendored: musl 1.2.5, x86_64 only, MIT.** 218 files, 470,198 bytes of text
(1.2 MB as the filesystem accounts for it, one block per small header), at
`crates/rustible-cli/vendor/musl-headers/x86_64/include/`, with upstream's
`COPYRIGHT` beside it unmodified and a `README.md` recording the provenance and
the two commands that regenerate it. The tree is upstream's `make
install-headers` output exactly as produced; nothing is edited and nothing is
hand-trimmed. Trimming to the handful of headers ring reaches today would save
about a megabyte and produce a libc header set that is silently incomplete the
moment ring adds an include.

**aarch64 is deliberately not vendored.** It takes ring's `-nostdlibinc` path
and needs no headers at all, which this branch verified rather than assumed: the
ARM cross-build below ran with `CC_aarch64_unknown_linux_musl=clang` and no
`CFLAGS` of any kind.

### Making it work from an installed binary

`crates/rustible-cli/build.rs` walks the vendored tree and writes
`$OUT_DIR/musl_headers.rs`, one `include_bytes!` per file, so the headers are
**inside the `rustible` binary**. On first use they are written to
`<workspace>/.rustible/musl-headers/1.2.5-x86_64/`, and a `.complete` stamp is
written last and checked first, so an unpack that died half way is redone rather
than trusted. `CC_x86_64_unknown_linux_musl` and
`CFLAGS_x86_64_unknown_linux_musl=--sysroot=<that directory>` are then set on
the cargo invocation.

Embedding is what makes `cargo install rustible-cli` sufficient. An installed
binary has no checkout to read from, and the CLI is what drives the playbook
build, so it has to supply the compiler and the include path itself. Verified by
renaming the vendored directory out of the repository and running an installed
binary against it (below).

The cost is **+516,976 bytes on the `rustible` CLI binary**, 6,837,896 to
7,354,872, a little over the raw header text because each file also carries its
path. It is **not** on the playbook binaries that ship to targets.

### The Docker harness needed the same nudge

`rustible_sdk::testing` cross-compiles the running test binary for
`<host arch>-unknown-linux-musl`. Under graviola nothing there compiled C;
under ring, every one of the fourteen container integration tests would have failed with
`cc-rs: failed to find tool "x86_64-linux-musl-gcc"` on a box that has gcc.

It gets a ten-line local helper, not a share of `rustible_cli::toolchain`. The
harness only ever builds for the host's own architecture, so it needs no clang,
no sysroot and no vendored headers; the SDK cannot depend on the CLI; and
hoisting the module into `rustible-build` would put compiler discovery in a
crate documented as a playbook-discovery build-script helper. The duplication is
`CC`, else `cc`, else `gcc`, else `clang`.

---

## The MSRV came back down: 1.88

`rust-version` in the workspace manifest and CI's MSRV job both return to 1.88.
1.89 was graviola's floor and nothing else's; ring and rustls both declare far
lower. `cargo +1.88 check --workspace --all-targets` passes. 1.88 is the real
floor now, set by let-chains in the CLI.

---

## Binary size and the dependency graph

Same probe both times: a `--profile dist` binary linking `rustible-std`
(`http::Download`) and `rustible-github` (`Https`) and making one real HTTPS
request, which is the same shape the graviola report measured. Built with clang
for both targets.

| target | graviola | **ring** | delta |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | 2,291,720 | **1,779,688** | **-512,032 (-22.3%)** |
| `aarch64-unknown-linux-musl` | 1,940,680 | **1,482,856** | **-457,824 (-23.6%)** |

Ring is smaller, as the spike found. Graviola is pure Rust but ships wide
unrolled assembly for a fixed high baseline; ring's C is compact and its cold
paths compress under `opt-level="z"` and LTO. This reverses the +34% that the
graviola swap cost, and then some.

The workspace lock goes from **175 packages to 169**. Nothing new enters it:
`ring`, `untrusted`, `cc` and `cfg-if` were already locked entries, `ring` as an
optional dependency of `rustls-webpki` that graviola's build left switched off.
What leaves is `graviola`, `rustls-graviola`, and `getrandom` 0.2 with its
`r-efi`, `wasip2` and `wit-bindgen` wasi-target companions. What newly
*compiles* is `ring` and `untrusted`, plus `cc` as a build dependency.

---

## Proof

Everything here was run, not reasoned about. This machine has gcc 13.3.0 and
**no clang**; the clang used below is 18.1.3 extracted from Ubuntu debs into the
spike's scratch prefix with `dpkg-deb -x`, on `PATH` for the runs that say so.
No system package was installed and `sudo` was never used.

### 1. Ring runs where graviola refuses

`qemu-x86_64-static` from the spike's prefix, same probe, only the provider
differs.

| `-cpu` | graviola | ring |
|---|---|---|
| `qemu64` (baseline x86-64, no AES-NI) | refused: lacks `aes, pclmulqdq, bmi1, adx, avx, avx2` | **OK 200, 4 keys** |
| `Opteron_G1`, `core2duo`, `Nehalem` | refused: lacks all six | **OK 200, 4 keys** |
| `SandyBridge`, `IvyBridge` | refused: lacks `bmi1, adx, avx2` | **OK 200, 4 keys** |
| **`Haswell`** (the outpost's shape) | **refused: lacks `adx`** | **OK 200, 4 keys** |
| `Broadwell`, `max` | OK 200, 4 keys | **OK 200, 4 keys** |

On aarch64 under `qemu-aarch64-static`, ring returned `OK 200 keys 4` on
`cortex-a53`, `cortex-a57`, `cortex-a72` and `max`.

The graviola line at `Haswell` is the pre-flight it shipped with, which turns
the abort into a step failure. Without that pre-flight the same CPU gets
`SIGABRT` mid-playbook, which is what the spike recorded.

### 2. Both musl targets cross-build with clang and a real handshake

The probe above was cross-built for both targets with clang alone, plus the
vendored sysroot on x86_64 and nothing at all on aarch64:

```
x86_64-unknown-linux-musl   ELF 64-bit LSB pie executable, static-pie linked, stripped
aarch64-unknown-linux-musl  ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped
```

Both fetched `https://github.com/flipbit03.keys` and reported `status 200 keys
4`, the x86_64 one natively and under every qemu CPU model above, the aarch64
one under `qemu-aarch64-static`.

### 3. A real playbook, both architectures, through the CLI

A generated workspace, an inventory of `local` and `arm`
(`cadu@cadu-cogram-vm-arm`), and a playbook that downloads a file over HTTPS.
With clang on `PATH`:

```
[local]  temp dir ................................... changed         exists=yes
[local]  LICENSE over https ......................... changed         GET https://raw.githubusercontent.com/... (missing)
[local]    downloaded 1071 bytes
[arm  ]  temp dir ................................... changed         exists=yes
[arm  ]  LICENSE over https ......................... changed         GET https://raw.githubusercontent.com/... (missing)
[arm  ]    downloaded 1071 bytes

host    ok  changed  would change  skipped  failed  warnings
local    0        2             0        0       0         0
arm      0        2             0        0       0         0
```

`.rustible/musl-headers/1.2.5-x86_64/` appeared during that run, 1.2 MB, and the
aarch64 binary was built with no headers.

**Without clang**, the same workspace targeting `local` alone still ran end to
end, building the x86_64 musl binary with `/usr/bin/cc`. That is the case the
pre-flight must not refuse, and it does not.

### 4. It works from an installed binary with no repository

`cargo install --root <scratch> --path crates/rustible-cli`, then
`crates/rustible-cli/vendor/` **moved out of the repository entirely**, then a
playbook run driven by the installed binary in a fresh workspace. It unpacked
`.rustible/musl-headers/1.2.5-x86_64/include/stdlib.h`, built the x86_64 musl
binary and downloaded over HTTPS, with no vendored tree anywhere on disk. The
directory was moved back afterwards.

---

## What the spike got wrong, or did not reach

Very little. Its three headline findings all held: aarch64 needs no headers,
x86_64 does, and the linking story is untouched. Four corrections and additions:

1. **The spike did not notice the integration harness.** `rustible_sdk::testing`
   cross-compiles for musl too, outside the CLI, and every one of the fifteen
   integration tests would have broken on the cc-rs error. Its section 7 treats
   the failure as something a user meets on a controller; it also meets a
   contributor running `RUSTIBLE_INTEGRATION=1 cargo test`.
2. **`ring` was already in the lock file.** Section 6 and the cost table read as
   though ring arrives with its dependencies; it was already a locked entry, as
   an optional dependency of `rustls-webpki` that the graviola configuration
   left switched off. The lock shrinks by six rather than growing.
3. **The recommendation to vendor both header sets is more than is needed.**
   Section "Getting musl headers" says "run this once and commit the two header
   trees, 2.3 MB". Only x86_64 is required, which halves it; the spike's own
   section 1.2 is what shows aarch64 does not need them.
4. **The build-time gap is smaller here than the spike measured.** It reported
   ring 3 seconds slower on its probe. On this workspace, a clean `dist`
   cross-build of both targets took 19.2 s under ring against 17.4 s under
   graviola, so under two seconds for both targets together.

Two things it left open are now closed: macOS was not established and still is
not, since no mac was reachable, but the README states the command-line-tools
route; and section 7's "the fix is a preflight" is now written.

---

## Documentation and CI

**README** gains an `## Install` section before `## Status`: rustup and clang,
the one-line Debian/Ubuntu command, the note that macOS command line tools
already ship clang, why clang is there at all, and the point that a build only
needs it to reach an architecture other than the machine's own.

**Rustdoc** on both network ops is rewritten where it described the provider or
the CPU floor: the module header of `rustible_std::http`, the `Https` docs and
crate header in `rustible-github`, and `rustible_std::tls` entirely. The two
ignored network tests are renamed from `..._with_graviola_tls`.

**CI needs no change.** GitHub's `ubuntu-24.04` image ships clang 16, 17 and 18
with `/usr/bin/clang` set through `update-alternatives`, so any job that needed
it would find it. None does: the only musl build in CI is the harness job, which
builds `x86_64-unknown-linux-musl` on an x86_64 runner and therefore takes the
host-`cc` path. The one workflow edit is the MSRV job, 1.89 to 1.88; because
that touches `.github/`, the branch was pushed over
`git@github.com:flipbit03/rustible.git` rather than through the `gh` OAuth
token, which has no `workflow` scope.

---

## Verification

All in `docs/plan/logs/M7-tls-ring-done.txt`.

| command | result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | all pass |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib` | clean |
| `cargo build --manifest-path examples/workspace/Cargo.toml` | ok |
| `cargo +1.88 check --workspace --all-targets` | ok, the MSRV is back down |
| `cargo test -p rustible-github -- --ignored` | live GitHub fetch over ring TLS, passes |
| `cargo test -p rustible-std --lib https_ -- --ignored` | live HTTPS download, passes |
| `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --tests` | 14 container test binaries plus the lib, all pass |

`deny(missing_docs)` is on in every library crate and the new module documents
every item.

---

## The vision 5.3 amendment, for the lead to apply

`docs/01_VISION.md` is untouched (docs/07_UNATTENDED.md rule 1). This is the
replacement prose for the clause that currently forbids C outright:

> Cross-compiling needs **no toolchain stock rustup cannot drive: no cross-gcc,
> no zig, no docker; clang on the operator's machine is required and is the only
> addition**. `rustup target add <triple>` plus a clang is the whole setup. The
> linker is the bundled `rust-lld` with `link-self-contained=yes`, against the
> musl crt rustup ships, and playbook binaries are fully static. The one C
> dependency is `ring`, the TLS crypto provider; Rustible carries musl's libc
> headers for the targets that need them and sets the compiler flags itself, so
> nothing beyond clang is ever installed by hand. The rule this replaces
> forbade C outright, and what it was protecting was the toolchain, not the
> language: `docs/plan/reports/C-TOOLCHAIN-SPIKE.md` measured that the pure-Rust
> provider bought an x86_64 floor at Intel Broadwell (2014) and a mid-playbook
> abort below it, in exchange for one package the target audience mostly has
> already. Target hosts remain untouched by this: they need nothing, as before.

