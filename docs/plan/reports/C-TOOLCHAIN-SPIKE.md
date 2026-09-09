# The C toolchain path for TLS

Spike, 2026-09-08. Follow-up to `TLS-OPTIONS.md`, prompted by `github::UserKeys`
failing on `outpost.flipbit03.com` because graviola aborts on a CPU without `adx`.

Everything below was measured on `cadu-cogram-vm-x86` (Ubuntu 24.04, x86_64,
16 cores) by compiling and running. Where I inferred rather than demonstrated, it
says so in the line.

---

## Verdict

**The C path is cheap, and it is much cheaper than the hypothesis assumed. Take
it, with `ring`, and make the single user-visible requirement "a clang on PATH".**

Three findings drive that:

1. **`ring` needs no musl headers on aarch64 at all.** Its `build.rs` passes
   `-nostdlibinc` and defines `RING_CORE_NOSTDLIBINC` for musl targets that are
   not x86_64, whenever the compiler is clang-like, and ships pure-C fallbacks
   for the libc functions it drops. The comment above that branch reads "Allow
   cross-compiling without a target sysroot for these targets." Cross-compiling
   `aarch64-unknown-linux-musl` therefore needs clang and nothing else.
2. **On x86_64 the headers are needed, but they are 1.2 MB of MIT-licensed text**
   that Rustible can vendor, so the user still installs nothing beyond clang. On
   an ordinary glibc Linux box the host's own headers already work, with no
   vendoring at all.
3. **Nothing about the linking story changes.** `rust-lld` with
   `link-self-contained=yes` links ring's C objects without complaint and the
   binary is still fully static. `.cargo/config.toml` needs no edit.

And the thing the whole spike was for: a ring-backed binary **runs on the
hardware graviola refuses**, demonstrated, not inferred. Under
`qemu-x86_64 -cpu Haswell` (aes, pclmulqdq, bmi1, avx, avx2, no `adx` — the
outpost's shape) graviola aborts and ring completes a real HTTPS request to
`api.github.com`.

Ring is also **smaller and only 3 seconds slower to build** than graviola. The
"C costs you size" intuition is backwards here.

The one real cost is that a user with a bare rustup gets a build failure whose
text does not name the fix. That is fixable with a preflight check, and it is a
build-time failure on the controller, not a runtime failure on a target. Today's
alternative is a `SIGABRT` on the target host, mid-playbook.

---

## The cost table

`ring`, `aws-lc-rs` and `graviola` compared on what a new user must do before
`rustible init` works. "Install" is the controller only; no target host is
affected either way.

| | graviola (today) | **ring (recommended)** | aws-lc-rs |
|---|---|---|---|
| **User must install** | nothing | **clang** | clang |
| **Extra files Rustible ships** | none | **musl headers, 1.2 MB/arch, MIT** | musl headers, both arches |
| **x86_64-musl builds** | yes | yes | yes |
| **aarch64-musl builds** | yes | yes (no headers needed) | yes (headers needed) |
| **x86_64 CPU floor** | **Broadwell / Zen 1 (2014)** | **baseline x86-64, no AES-NI** | baseline x86-64 |
| **aarch64 CPU floor** | Cortex-A57 + crypto ext. | no floor found | no floor found |
| **Binary, x86_64 `dist`** | 2.06 MiB | **1.58 MiB** | 2.32 MiB |
| **Binary, aarch64 `dist`** | 1.72 MiB | **1.30 MiB** | 1.79 MiB |
| **Clean build, x86_64** | 10.2 s | 13.0 s | 19.5 s |
| **Clean build, aarch64** | 10.8 s | 12.9 s | 20.1 s |
| **What breaks when missing** | nothing to miss | build fails on controller, error names `aarch64-linux-musl-gcc` | same |
| **What breaks on old CPU** | **`SIGABRT` on target, mid-run** | nothing | nothing |
| **cmake / perl / go needed** | n/a | no | **no** (cmake is not installed here) |

Sizes are `--profile dist` (`opt-level="z"`, LTO, one codegen unit,
`panic="abort"`, stripped) on an identical probe crate: `ureq` 3 +
`rustls` 0.23 + one HTTPS GET. Times are `cargo clean` then build, 16 cores.

---

## 1. Can `ring` build for both musl targets with rustup + clang + musl headers, no gcc cross-toolchain, no zig?

**Yes. Demonstrated, for both targets, and for less than the question assumes.**

### Is clang on this machine?

No. `which clang` returns nothing; the box has gcc 13.3.0 and `/usr/bin/cc`
only. `rust-lld` is also not on `PATH` — it lives inside the rustup sysroot and
rustc resolves the name itself.

### Is clang on the mac?

**Not established.** `cadu@cadumac.local` timed out on port 22, and no `cadumac`
appears in `tailscale status` (the sibling `cadu-cogram-vm-arm` is listed but
offline, last seen 7 minutes before the check). Inferred: macOS Command Line
Tools ship clang and the macOS SDK, so the aarch64 case would work there
unchanged. The x86_64 case would need the vendored musl headers, because a mac's
SDK headers are Darwin's, and test A in section 1.3 shows what happens when the
headers on hand are not usable for the target.

### Getting musl headers

From the upstream source tarball, `musl-1.2.5.tar.gz`, 1.0 MB, **MIT** (its
`COPYRIGHT` file: "musl as a whole is licensed under the following standard MIT
license"). No root:

```sh
curl -LO https://musl.libc.org/releases/musl-1.2.5.tar.gz
tar xzf musl-1.2.5.tar.gz
mkdir build-aarch64 && cd build-aarch64
CC=gcc ../musl-1.2.5/configure --target=aarch64-linux-musl --prefix=$PREFIX
make install-headers
```

Result: 218 headers, **1.2 MB for x86_64, 1.1 MB for aarch64**. The two differ
(`bits/alltypes.h` is arch-specific), so both must be generated. `make
install-headers` compiles nothing — it is sed over `.h.in` templates — but
`configure` insists on finding a C compiler, hence the explicit `CC=gcc`. Without
that it fails with `cannot find a C compiler`, because `--target` makes it look
for `aarch64-linux-musl-gcc`.

Simplest for Rustible: run this once and **commit the two header trees**, 2.3 MB
total, MIT, attribution-only. Then no user ever runs the above.

### 1.1 The demonstrated builds

Obtaining clang without root, to prove no system package is needed: `apt-get
download` (needs no root) for `clang-18 libclang-cpp18 libllvm18
libclang-common-18-dev llvm-18-linker-tools`, **43 MB of debs**, then `dpkg-deb
-x` into a prefix, **198 MB on disk**. It runs from there directly. A full
`apt install clang-18` would pull 22 packages, 96 MB.

Both of these produced a working, fully static binary that made a real HTTPS
request:

```sh
# x86_64: clang + musl sysroot
CC_x86_64_unknown_linux_musl=$CLANG \
CFLAGS_x86_64_unknown_linux_musl="--sysroot=$PREFIX/sysroot-x86_64" \
cargo build --profile dist --target x86_64-unknown-linux-musl

# aarch64: clang, no sysroot, no headers, no CFLAGS at all
CC_aarch64_unknown_linux_musl=$CLANG \
cargo build --profile dist --target aarch64-unknown-linux-musl
```

The aarch64 binary was run under `qemu-aarch64-static` and printed
`OK 200 OK Speak like a human.` from `api.github.com`. Both are
`statically linked, stripped`; `ldd` on the x86_64 one says `statically linked`.

### 1.2 Why aarch64 needs no headers

`ring` 0.17.14 has 17 C files. Across all of them the only libc includes are one
`<stdlib.h>` and one `<string.h>`, both in `crypto/internal.h`:

- `<stdlib.h>` sits inside `#elif defined(_MSC_VER)`. Never reached on Linux.
- `<string.h>` sits inside `#if !defined(RING_CORE_NOSTDLIBINC)`, and every use
  site has a hand-written fallback loop under the `#else`.

Everything else it includes — `stddef.h`, `stdint.h`, `stdalign.h`, `limits.h`,
`immintrin.h` — is a **freestanding** header supplied by the compiler's own
resource directory, not by libc. All five are present in the extracted clang
prefix.

`build.rs` lines 594-603 then do this:

```rust
// Allow cross-compiling without a target sysroot for these targets.
if (target.arch == WASM32)
    || (target.os == "linux" && target.env == "musl" && target.arch != X86_64)
{
    if compiler.is_like_clang() {
        let _ = c.flag("-nostdlibinc");
        let _ = c.define("RING_CORE_NOSTDLIBINC", "1");
    }
}
```

Two conditions matter to us: **clang only** (a cross-gcc gets nothing), and
**not x86_64**.

### 1.3 Why x86_64 does need them

I forced the same treatment on x86_64 to check whether the exclusion is
conservative or load-bearing. It is load-bearing, in two stages:

- **A.** `-nostdlibinc` alone →
  `include/ring-core/check.h:27:11: fatal error: 'assert.h' file not found`
- **B.** `-nostdlibinc -DRING_CORE_NOSTDLIBINC=1` →
  `clang/18/include/mm_malloc.h:13:10: fatal error: 'stdlib.h' file not found`

So on x86_64, `assert.h` is unguarded and clang's own `immintrin.h` pulls
`mm_malloc.h` which pulls `stdlib.h`. Real libc headers are required. Either the
vendored musl set, or — on an x86_64 glibc Linux host — the host's own.

### 1.4 The cheapest x86_64 path of all

On this machine, with no clang and no musl headers, only the host gcc that every
dev box already has:

```sh
CC_x86_64_unknown_linux_musl=gcc cargo build --profile dist --target x86_64-unknown-linux-musl
```

Builds, static, runs, and completes a real HTTPS request — including under
`qemu -cpu Haswell`, where graviola dies. Setting `CC_*` is required; without it
cc-rs looks for `x86_64-linux-musl-gcc` and stops, even with gcc right there.

This works because the host is x86_64 and the headers it falls back to are the
right architecture. I would not build on it: it silently compiles musl-targeted
code against glibc declarations. Ring only touches `memcpy`, `memset` and
`assert` so the risk is small in practice, but the vendored musl headers make it
principled for one megabyte. Worth knowing it exists as a fallback.

---

## 2. If that fails, what is the cheapest thing that works?

It did not fail, so this is a ranking of alternatives rather than a rescue.
Ordered by what a new user must do before `rustible init` works.

1. **clang, from the OS package manager.** One package. Covers **both**
   architectures with one binary. macOS gets it with Command Line Tools, which
   any Rustible-on-mac user has already. Ubuntu: `apt install clang`, 96 MB.
   This is the recommendation.
2. **Host `cc` only, x86_64 target only.** Zero installs on any Linux dev box.
   Does not reach aarch64 — the host gcc cannot cross-compile — so it cannot be
   the whole story.
3. **Distro cross-gcc, `gcc-aarch64-linux-gnu`.** 22 debs, **43 MB**.
   Demonstrated working: with the musl sysroot it built ring for aarch64-musl and
   the binary ran under qemu. Two costs against clang. It is glibc-targeted, so
   it **must** have the musl headers (ring's `-nostdlibinc` branch is clang-only).
   And rootless extraction is fiddly — the bundled `as` failed with
   `libopcodes-2.42-arm64.so: cannot open shared object file` until I set
   `LD_LIBRARY_PATH`; a normal `apt install` would not hit that. It also only
   solves one architecture, so a two-arch user installs two toolchains.
4. **Prebuilt musl cross toolchains from musl.cc.** `aarch64-linux-musl-cross.tgz`
   is **103 MB**, `x86_64-linux-musl-cross.tgz` **109 MB** (HTTP 200, sizes read
   from `content-length`; not downloaded). Installs without root, being a tarball.
   Twice the download of clang for half the coverage, and it is a third-party
   binary distribution with no distro provenance.
5. **Official LLVM release tarball.** `LLVM-20.1.8-Linux-X64.tar.xz` is
   **1.9 GB**. Only worth it where no package manager exists.
6. **`cross`.** Needs docker. Rejected without measurement: requiring a container
   runtime to build a config-management tool is a larger ask than a compiler.

On distro musl packages specifically: Ubuntu's `musl-dev` (615 kB) is
**amd64-only** in content — extracted, it contains `usr/include/x86_64-linux-musl`
and nothing else. It cannot serve the aarch64 target. Building headers from the
musl tarball is both smaller and arch-complete, which is why section 1 uses it.

---

## 3. Does a `ring`-backed binary run where graviola refuses?

**Yes. Demonstrated.** I could not reach the outpost, so I reproduced its CPU
under `qemu-x86_64-static` (extracted from `qemu-user-static`, 14.7 MB, no root)
with `-cpu` models chosen for their feature sets. Same probe program, same
request, only the provider differs.

| `-cpu` | graviola | ring |
|---|---|---|
| `qemu64` (baseline x86-64, no AES-NI) | not tested | **OK 200** |
| `Opteron_G1` | not tested | **OK 200** |
| `core2duo`, `Penryn`, `Westmere` | not tested | **OK 200** |
| `Nehalem` | abort: `graviola requires aes CPU support` | **OK 200** |
| `SandyBridge` | abort: `graviola requires bmi1 CPU support` | **OK 200** |
| `IvyBridge` | abort: `graviola requires bmi1 CPU support` | **OK 200** |
| **`Haswell`** (the outpost's shape) | **abort: `graviola requires adx CPU support`** | **OK 200** |
| `Broadwell` | OK 200 | OK 200 |
| `max` | OK 200 | OK 200 |

The Haswell line is the whole spike in one row. Graviola's exact output:

```
thread 'main' panicked at graviola-0.4.1/src/low/x86_64/cpu.rs:248:5:
graviola requires adx CPU support (rebuild with VALGRIND_BUG_494162 for valgrind compatibility)
qemu: uncaught target signal 6 (Aborted) - core dumped
```

Note it panics **after** `default_provider()` returns — the probe printed
`provider ciphersuites: 9` first. The check fires during the handshake, not at
construction, which is why a preflight had to be written by hand in `M7`.

Graviola's floor is exactly as documented: it starts working at `Broadwell`.
Ring's floor is **the x86-64 baseline** — it works on `qemu64` and
`Opteron_G1`, which have no AES-NI, no SSE4, no AVX. That is runtime detection
doing its job, and it is a floor about twenty years below graviola's.

**On aarch64 there is no difference.** All three providers ran on `cortex-a53`,
`cortex-a57`, `cortex-a72` and `max`. The CPU-floor problem is x86_64-only.

I did not read ring's detection source line by line; the empirical range from
`qemu64` upward is stronger evidence than the source would be, and the
`cpu_intel.c` file plus `#include <immintrin.h>` confirms the mechanism is
CPUID-based dispatch rather than compile-time gating.

---

## 4. The same, for `aws-lc-rs`

Briefly, since it loses on every axis that matters here.

- **Builds?** Yes, both targets, same flags. **It does not need cmake** — none is
  installed on this machine and `aws-lc-sys` 0.45.0 fell back to its cc-based
  builder. No perl or go needed either. That is better than its reputation.
- **Headers?** **Needed on both architectures**, unlike ring. Without a sysroot
  the aarch64 build fails with
  `/usr/include/stdlib.h:26:10: fatal error: 'bits/libc-header-start.h' file not found`,
  clang having fallen into the host's glibc tree which has no aarch64 `bits/`.
  So aws-lc-rs forfeits ring's best property.
- **CPU floor?** Same as ring, demonstrated: `OK 200` on `qemu64`, `Haswell`
  and `max`.
- **Cost?** Biggest binary of the three (2.32 MiB x86_64, 1.79 MiB aarch64) and
  slowest build (~20 s versus ring's ~13 s). It is also a far larger C surface
  than ring's 17 files.

Same benefit as ring, strictly more cost. No reason to prefer it.

---

## 5. What happens to the linking story?

**Nothing. `.cargo/config.toml` is unchanged, and the binary is still fully
static.** Demonstrated three ways:

- A verbose build shows the flags are live: `-C linker=rust-lld` and
  `link-self-contained=yes`.
- Overriding with a bogus linker name fails loudly
  (``error: linker `definitely-not-a-linker` not found``), proving the setting is
  being honoured rather than ignored.
- `file` on the products: x86_64 is
  `ELF 64-bit LSB pie executable, x86-64, static-pie linked, stripped`; aarch64
  is `ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped`.
  `ldd` on the x86_64 binary: `statically linked`.

`rust-lld` linked ring's C objects against the rustup-supplied musl `libc.a`
without a single flag change. The `self-contained` directory rustup ships
(`crt1.o`, `crti.o`, `crtn.o`, `libc.a`, `libunwind.a`, …) has everything the
link needs; only the **headers** were ever missing, never the library.

---

## 6. Binary size and build time

Identical probe crate, `--profile dist`, `cargo clean` before each, 16 cores.

| provider | target | build | binary |
|---|---|---|---|
| graviola | x86_64-musl | 10.2 s | 2,161,608 B (2.06 MiB) |
| graviola | aarch64-musl | 10.8 s | 1,809,256 B (1.72 MiB) |
| **ring** | x86_64-musl | 13.0 s | **1,654,408 B (1.58 MiB)** |
| **ring** | aarch64-musl | 12.9 s | **1,365,536 B (1.30 MiB)** |
| aws-lc-rs | x86_64-musl | 19.5 s | 2,434,856 B (2.32 MiB) |
| aws-lc-rs | aarch64-musl | 20.1 s | 1,875,808 B (1.78 MiB) |

**Ring is smaller than graviola on both targets** — 0.48 MiB smaller on x86_64,
0.42 MiB on aarch64, around 23%. Graviola is pure Rust but ships wide assembly
for a fixed high baseline; ring's C is compact and its cold paths compress well
under `opt-level="z"` and LTO. The build-time difference is 3 seconds.

---

## 7. What the failure looks like when the toolchain is absent

A user with a bare rustup, running a playbook that has to build a target binary,
sees a build-script failure. With `PATH` cut to cargo plus `/usr/bin` and no
`CC_*` set at all:

```
error: failed to run custom build command for `ring v0.17.14`

Caused by:
  process didn't exit successfully: `.../build-script-build` (exit status: 1)
  --- stderr
  error occurred in cc-rs: failed to find tool "aarch64-linux-musl-gcc": No such file or directory (os error 2)
```

preceded by a `cargo:warning=Compiler family detection failed due to error:
ToolNotFound: failed to find tool "aarch64-linux-musl-gcc"`.

**This is a cliff as it stands, but a shallow and fixable one.** Three things
make it so:

- The error names `aarch64-linux-musl-gcc`, a program the user should **not**
  install — following it literally leads to the 103 MB musl.cc download, the
  worst of the options in section 2. It never mentions clang.
- It fires **on the controller, at build time**, where the user is sitting and
  can act. Graviola's failure fires on the **target host, mid-playbook**, as a
  `SIGABRT` after some ops have already run.
- The gcc-is-present case is the trap: this box has a perfectly good gcc and
  still fails, because cc-rs only looks for the target-prefixed name.

The fix is a preflight in `rustible init` and before any target build: probe for
a usable compiler, and on failure print the actual instruction — "install clang"
plus the one-line package command per platform — rather than letting cc-rs's
message through. Rustible would set `CC_*` and `CFLAGS_*` itself from the
vendored headers, so no user ever exports an environment variable. With that,
the C path is a good experience, not a cliff.

---

## Is the C path cheap enough to take, and which flavour?

**Yes, and the flavour is `ring` with clang as the sole requirement.**

The rule Vision 5.3 protects is that cross-compiling needs nothing but stock
rustup. `ring` costs exactly one addition to that: **a clang on `PATH`**. On
macOS that is already there with Command Line Tools. On Linux it is one package
from the distro's own repository, no third-party binaries, no docker, no root
beyond the user's normal package manager. Rustible ships 2.3 MB of MIT-licensed
musl headers in-tree and wires the flags itself, so nothing else is asked of
anyone. `rust-lld`, `link-self-contained=yes` and full static linking all survive
untouched.

What that buys: the x86_64 floor drops from Broadwell (2014) to the x86-64
baseline (2003), and Cadu's outpost can do HTTPS. What it costs: 3 seconds of
build time, and a build-time error on a controller without clang that Rustible
should intercept and reword. It **saves** half a megabyte of binary.

The framing in the brief is confirmed: this was never a cost of static linking.
It was a cost of the no-C rule, and the rule is buying a CPU floor eleven years
younger than the alternative, in exchange for a dependency most of the target
audience already has installed.

Two amendments to Vision 5.3 would follow, and both are Cadu's call:

1. Narrow "no C in the dependency graph" to what it was actually protecting —
   "no toolchain a stock rustup cannot drive, and no cross-gcc, zig or docker" —
   which clang satisfies while `ring` sits in the graph.
2. If the rule is kept whole instead, the honest consequence is that
   `http::Download` and `github::UserKeys` stay unavailable on pre-2014 x86_64,
   and the outpost stays broken. The `rustible_std::tls::preflight` guard already
   written in M7 makes that a named error rather than a panic, which is the right
   behaviour either way and should stay regardless of which branch is taken.

Recommendation: (1). The measured cost of the C path is one package, and the
measured benefit is that Rustible works on hardware it currently aborts on.

---

## Reproduction

Everything lives in
`/tmp/claude-1000/-home-cadu-w-cadu-rustible/5e9fe8ac-d62a-43bd-93c7-43bd950ce259/scratchpad/`:
`probe-ring/`, `probe-grav/`, `probe-awslc/` (identical crates, one line
different), `sysroot-x86_64/` and `sysroot-aarch64/` (musl headers),
`clang-prefix/` (clang 18.1.3 from extracted debs), `xgcc/prefix/` (aarch64
cross-gcc), `qemu/prefix/` (qemu-user-static).

**No system packages were installed and `sudo` was never used.** Every tool was
obtained with `apt-get download` plus `dpkg-deb -x`, or `curl` plus `tar`, into
the scratch directory. Nothing was written to `~/.cargo` or `~/.rustup` beyond
the normal crate registry cache. No repository file other than this one was
touched, and no code was changed.
