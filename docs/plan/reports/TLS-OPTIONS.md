# TLS options for the target binary

## Decision

**Cadu, 2026-09-08: switch to `rustls-graviola`, with no fallback.** **Reversed the same day**, after `docs/plan/reports/C-TOOLCHAIN-SPIKE.md`: the provider is now `ring` and the toolkit is rustup plus clang. See `docs/plan/reports/M7-tls-ring.md`. What follows described the graviola decision and is kept as written.
`rustls-rustcrypto` leaves the tree entirely.

That is option (2) of section 7, not the hybrid this report leans towards. The
reasoning for taking the stricter branch is that one crypto path is one thing
to reason about, and an opt-in fallback would keep alpha code in the graph for
a hardware population Rustible has no user on. The cost is accepted knowingly:
`http::Download` and `github::UserKeys` cannot run at all on pre-Broadwell
x86_64 or on Raspberry Pi 4 and earlier. Every non-network op still works
there.

The pre-flight this report calls not optional is built, as
`rustible_std::tls::preflight`, and both ops call it before any handshake, so
an unsupported CPU gets a named error instead of a panic mid-playbook.

Two things this report did not predict:

- **graviola 0.4.1 declares `rust-version = 1.89`**, so the workspace MSRV
  moved from 1.88 to 1.89. Every graviola from 0.3.0 onwards is on 1.89, and
  `rustls-graviola` 0.4 requires graviola 0.4, so pinning back is not
  available.
- **The static musl binary grows by about a third**, not shrinks, despite
  graviola having a far smaller dependency graph and despite the duplicate
  `rustls-webpki` copy going away.

Implemented on branch `tls-graviola`. See
`docs/plan/reports/M7-tls-graviola.md` and the `[M7-tls]` entries in
`docs/plan/DECISIONS.md`. Everything below is the original research,
unchanged.

---

Research report. No code changed. Everything marked **verified** was executed or
read on this machine on 2026-09-08; everything marked **inferred** is reasoning
from a source I could read but not run.

---

## Verdict

**Switch the crypto provider to `rustls-graviola`, and keep `rustls-rustcrypto`
only as a runtime fallback on CPUs graviola refuses to run on.**

This keeps the pure-Rust rule completely intact. Graviola has zero `.c` files,
zero `.S` files, and no `build.rs` anywhere in its tree. Its only dependencies
are `cfg-if` and `getrandom`. I cross-built `ureq` + `rustls` + `rustls-graviola`
for both `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` with
nothing but stock rustup and the repo's existing `rust-lld` config, and ran the
x86_64 musl binary against `https://github.com/flipbit03.keys` for a real
handshake. It worked on the first try.

The trust root improves a lot. Graviola's field arithmetic, ECDSA, X25519 and
big-integer code come from **s2n-bignum**, which is formally proven to implement
the intended mathematical operation. Its constant-time behaviour is checked in
CI with ctgrind on both x86_64 and aarch64. That is a materially better story
than a crate whose own README says do not use it in production.

The one real cost is hardware reach. Graviola `assert!`s on required CPU
features and **panics** if they are missing. On x86_64 it needs ADX, which
excludes everything before Intel Broadwell, Haswell included. On aarch64 it
needs AES and PMULL, which excludes Raspberry Pi 4 and earlier. That panic fires
at the first crypto call inside a running playbook, which is the worst possible
place for it.

So the recommendation has two parts, and the second part is not optional:
detect the required CPU features yourself before touching graviola, and on a
CPU that lacks them either fall back to `rustls-rustcrypto` or fail with a clear
error. I built and ran that hybrid. Both providers coexist in one static musl
binary, both handshake successfully, and the release binary is 5.6 MB.

If you would rather ship no alpha code at all, drop the fallback and let the
pre-flight check produce a proper op error saying the target CPU is too old for
Rustible's TLS. That is a clean, honest answer, and it is a supported-hardware
decision rather than a cryptographic one.

**Do not pursue the orchestrator-fetches-it architecture for this problem.** It
is a legitimate feature for a different requirement, but as a fix for TLS trust
it costs far more than it saves. Reasoning in section 5.

---

## 1. `rustls-rustcrypto` — what the alpha label actually means

**Verified.** Latest published version is **0.0.2-alpha, published 2024-04-24**.
There has been no release in roughly two and a half years. That is the version
the workspace pins today (`Cargo.toml:39`).

**Verified.** The repository is *not* abandoned. Recent commits on
`RustCrypto/rustls-rustcrypto` master include 2026-08-28, 2026-08-24, 2026-07-17
and 2026-07-16. But the activity is almost entirely dependency bumps by
Dependabot, plus the occasional move of a RustCrypto dependency from release
candidate to stable. There are 19 open issues and 17 open pull requests.

**Verified.** Issue
[#107, "Are there any plans to cut new release?"](https://github.com/RustCrypto/rustls-rustcrypto/issues/107),
opened 2026-01-30, is still open. The release tracker,
[#51](https://github.com/RustCrypto/rustls-rustcrypto/issues/51), opened
2024-03-16, still has unchecked boxes. **A 0.1 or stable release does not look
close.** Nothing in the repository suggests a date.

**Verified.** The README warning, verbatim:

> ## ⚠️USE THIS AT YOUR OWN RISK! DO NOT USE THIS IN PRODUCTION⚠️
>
> Not only that this is incomplete that only few selected TLS suites implemented
> (it should be well enough to cover 70% of the usage), but the elephant in the
> room is that neither did rustls nor RustCrypto packages were formally verified
> and certified with FIPS compliance.

Read carefully, that warning is about two things: incompleteness of cipher-suite
coverage, and absence of formal verification and FIPS certification. It is not a
claim of a known break. The nine supported suites are the modern ECDHE and
TLS 1.3 ones, which is fine for talking to GitHub and to release-tarball hosts.

### Is the risk the glue or the primitives?

**Mostly the glue, but the primitives are not uniformly strong either.**

**Verified** dependency tree of `rustls-rustcrypto` in this workspace, via
`cargo tree -p rustls-rustcrypto --target x86_64-unknown-linux-musl`:

```
aead 0.5.2, aes-gcm 0.10.3, chacha20poly1305 0.10.1, crypto-common 0.1.7,
der 0.7.10, digest 0.10.7, ecdsa 0.16.9, ed25519-dalek 2.2.0, hmac 0.12.1,
p256 0.13.2, p384 0.13.1, pkcs8 0.10.2, rand_core 0.6.4, rsa 0.9.10,
sec1 0.7.3, sha2 0.10.9, signature 2.2.0, x25519-dalek 2.0.1,
rustls-webpki 0.102.8
```

Audit status of the primitives that matter, from the crates' own documentation:

- **`aes-gcm`, `chacha20`, `poly1305`** — each received one NCC Group security
  audit with no significant findings, funded by MobileCoin. Constant-time
  execution is a design goal and is claimed. The Poly1305 audit predates the
  AVX2 backend, which is unaudited.
- **`p256` / `p384`** — the elliptic-curve arithmetic **has never been
  independently audited**. The crates state they are designed so secret-dependent
  operations run in constant time, but explicitly say they have *not* been
  assessed to confirm the generated assembly is constant time on common CPU
  architectures. For a TLS client this is the ECDHE key exchange and ECDSA
  certificate verification, so it is on the hot path.
- **`rsa 0.9.10`** — carries
  [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html),
  the Marvin attack, a timing side channel, CVSS 5.9, **with no patched version
  available**. **Inferred, and I am fairly confident:** the practical exposure
  here is small, because a TLS *client* uses RSA only to verify a certificate
  signature, which is a public-key operation with no private key present, and
  the negotiated suites are all ECDHE so there is no RSA key exchange. It is
  still an unfixed advisory sitting in the graph, and `cargo audit` would flag
  it. Note that `cargo-audit` is not installed on this machine, so I did not run
  it.

Two artifacts of the stale release worth knowing. First, `rustls-rustcrypto`
0.0.2-alpha pins `rustls-webpki 0.102.8` while `rustls 0.23.44` uses
`rustls-webpki 0.103.x`, so the binary carries **two copies of webpki**. Second,
the dependency versions in the published crate are two years behind the ones the
maintainers have already updated on master.

**Summary of this option:** the glue is unreleased and self-described as
not-for-production, and the highest-value primitive under it, the P-curve
arithmetic, is unaudited. It works, and it is not known to be broken, but it is
not something to point at when someone asks what protects the path from a
network response to a user's `authorized_keys`.

---

## 2. `rustls-graviola` — verified in full

Crate: [`rustls-graviola` 0.4.0](https://crates.io/crates/rustls-graviola),
published **2026-06-17**, 127k downloads. Underlying
[`graviola` 0.4.1](https://crates.io/crates/graviola). Author Joe Birr-Pixton,
who is also the author of rustls. Repository <https://github.com/ctz/graviola>.

### Purity — verified by inspection of a fresh clone

```
find . -name '*.c'      -> 0
find . -name '*.S'      -> 0
find . -name 'build.rs' -> 0
```

`graviola`'s dependencies are `cfg-if` and `getrandom` (plus an optional
`crabgrind` behind the non-default `__ctgrind` feature).
`rustls-graviola`'s dependencies are `graviola` and `rustls`. That is the entire
graph. The assembly it uses is written as Rust `core::arch::asm!` blocks and
intrinsics inside `.rs` files, so it needs no assembler and no external
toolchain. The README states the goal directly:

> *Easy and fast to build*: no C compiler, assembler or other tooling needed:
> just the Rust compiler. Compiles in less than one second.

### Cross-build — verified by execution

I built a probe crate with the repo's own `.cargo/config.toml` (`linker =
"rust-lld"`, `rustflags = ["-C", "link-self-contained=yes"]`) depending on
`ureq` 3 with `rustls-no-provider` + `rustls-webpki-roots`, `rustls` 0.23, and
`rustls-graviola` 0.4.

Both targets compiled clean. `file` on the outputs:

```
aarch64-unknown-linux-musl/debug/tlsprobe: ELF 64-bit LSB executable, ARM aarch64, statically linked
x86_64-unknown-linux-musl/debug/tlsprobe:  ELF 64-bit LSB pie executable, x86-64, static-pie linked
```

Running the x86_64 musl binary performed a real TLS 1.3 handshake with
github.com and returned 4 SSH keys. This machine has no clang, no `musl-gcc`,
and no `aarch64-linux-musl-gcc`.

### CPU requirements — the claim is correct, and it is a panic

**Verified.** The README's Limitations section, verbatim:

> `aarch64` and `x86_64` architectures only.
>
> - `aarch64` requires `aes`, `sha2`, `pmull`, and `neon` CPU features.
>   (This notably excludes Raspberry PI 4 and earlier, but covers Raspberry Pi 5.)
> - `x86_64` requires `aes`, `ssse3`, `avx`, `avx2`, `adx`, `bmi2`, and
>   `pclmulqdq` CPU features.
>   (This is most x86_64 CPUs made since around ~2014.)

**Verified in source.** `graviola/src/low/x86_64/cpu.rs:226` and
`graviola/src/low/aarch64/cpu.rs:207` define `verify_cpu_features()`, which is a
chain of `assert!` calls. It is called from `Entry::new_public()` and
`Entry::new_secret()` in `graviola/src/low/entry.rs:21` and `:35`, and the
comment there says one of these is made at **every public function** of the
library. So the check runs on the first crypto operation of the process.

**It builds fine and panics at runtime.** Verified using graviola's own
debug-build test toggle:

```
$ GRAVIOLA_CPU_DISABLE_avx2=1 ./tlsprobe
thread 'main' panicked at graviola-0.4.1/src/low/x86_64/cpu.rs:258:5:
graviola requires avx2 CPU support

$ GRAVIOLA_CPU_DISABLE_adx=1 ./tlsprobe
thread 'main' panicked at graviola-0.4.1/src/low/x86_64/cpu.rs:248:5:
graviola requires adx CPU support
```

The message is clear, but it is a panic, not a `Result`, and it would abort a
playbook mid-run rather than failing one step.

Note the x86 cut is stricter than "2014". The repository's `COMPATIBILITY.md`
tracks CPU families, and the binding requirement is ADX: **Intel Haswell is not
supported** (it has AVX2 and BMI2 but no ADX). Broadwell is the first supported
Intel generation. AMD Zen 3 and Zen 4 are supported.

### Is there a fallback inside graviola?

**No, and it is deliberate.** The README says AES and GHASH always use
intrinsics and there are no fallbacks. The one exception is SHA-256, which does
fall back to a pure Rust version on x86_64 when SHA-NI is absent. AVX-512 is
detected at runtime and is genuinely optional. The mandatory set is mandatory.

**Verified.** I scanned graviola's open and closed issues for anything about
older-CPU support or a software fallback. There is nothing. The open issues are
about adding algorithms (ML-DSA, SHA-1) and about faster aarch64 paths. Nobody
is building a fallback path, upstream or elsewhere that I could find.

### Is anyone doing provider-level fallback?

**Not that I found published.** But it is straightforward, and I verified it
works. rustls's `CryptoProvider` is a plain struct you choose at runtime, so
selecting between two providers is an `if`. I built this and ran it:

```rust
fn graviola_supported() -> bool {
    #[cfg(target_arch = "x86_64")]
    { std::arch::is_x86_feature_detected!("aes")
      && std::arch::is_x86_feature_detected!("pclmulqdq")
      && std::arch::is_x86_feature_detected!("bmi1")
      && std::arch::is_x86_feature_detected!("bmi2")
      && std::arch::is_x86_feature_detected!("adx")
      && std::arch::is_x86_feature_detected!("avx")
      && std::arch::is_x86_feature_detected!("avx2") }
    #[cfg(target_arch = "aarch64")]
    { std::arch::is_aarch64_feature_detected!("neon")
      && std::arch::is_aarch64_feature_detected!("aes")
      && std::arch::is_aarch64_feature_detected!("pmull")
      && std::arch::is_aarch64_feature_detected!("sha2") }
}
```

**Verified:** this compiles for both musl targets, both provider branches
handshake successfully with github.com, and the combined release binary is
5,586,792 bytes. `is_aarch64_feature_detected!` is stable and works in a static
musl build. The feature list mirrors `verify_cpu_features()` exactly, including
`bmi1`, which the README omits but the source asserts.

### Maturity

The README says: *"This project is very new, so exercise due caution."* That is
a real caveat and it should not be waved away. But it is a different kind of
caveat from rustls-rustcrypto's. Graviola is at 0.4.0, released regularly since
September 2024, actively developed (commits through 2026-08-24), written by the
rustls author, built on formally-proven s2n-bignum assembly, tested against
Wycheproof and CAVP vectors, and constant-time-tested with ctgrind in CI on both
architectures (issues #167 and #184 cover ctgrind on aarch64). It has not had a
third-party security audit that I could find.

---

## 3. Other pure-Rust providers, and rustls's provider story

**Verified.** rustls 0.23.44 (published 2026-09-07) documents exactly two
built-in providers: `aws-lc-rs` (default) and `ring` (optional feature), plus
`default_fips_provider`. The provider architecture is unchanged from 0.23's
original design. There is no new pure-Rust option from upstream and no sign of
one coming.

The full third-party provider list from rustls's own documentation:
`rustls-rustcrypto`, `rustls-graviola`, `boring-rustls-provider`,
`rustls-mbedtls-provider`, `rustls-openssl`, `rustls-symcrypt`,
`rustls-wolfcrypt-provider`, and `rustls-ccm`.

**Every one of those except `rustls-rustcrypto` and `rustls-graviola` wraps a C
library**: BoringSSL, mbedTLS, OpenSSL, SymCrypt, wolfCrypt. `rustls-ccm` is not
a full provider; it adds AES-CCM suites on top of RustCrypto for constrained
devices.

I also checked `portable-rustls`, which surfaced in searching. **It is not
relevant.** It is a fork of rustls by Chris J. Brody aimed at targets without
atomic pointers, last released 0.0.2 in February 2025, and it still requires you
to pick `aws-lc-rs` or `ring` as the provider. It solves a different problem and
is less maintained than what we already have.

**So the field of pure-Rust rustls providers is exactly two crates, and we have
now evaluated both.**

---

## 4. Can `ring` or `aws-lc-rs` cross-build with only rustup?

**No. Verified by execution, for both.**

I built probe crates depending on `rustls 0.23` with the `ring` and `aws-lc-rs`
features respectively, targeting `aarch64-unknown-linux-musl` with the repo's
own linker config. Both failed identically:

```
error occurred in cc-rs: failed to find tool "aarch64-linux-musl-gcc":
  No such file or directory (os error 2)
```

That reproduces exactly the failure the team saw. Now, precisely what each
needs.

### `ring` (0.17.14, present in the local registry)

**Verified by inspecting the vendored crate:**

- 73 `.S` assembly files, all **pregenerated**, shipped in the crate. No perl,
  no assembler needed. The assembly is not the problem.
- 17 `.c` files, and `build.rs` compiles a fixed list of them for every target
  (`curve25519.c`, `aes_nohw.c`, `montgomery.c`, `limbs.c`, `mem.c`,
  `poly1305.c`, and target-specific ones such as `p256-nistz.c` for AARCH64).
  There is no feature or configuration that skips them.
- The C sources include `<stddef.h>`, `<stdint.h>`, `<stdlib.h>`, `<string.h>`,
  `<stdalign.h>` and `<immintrin.h>`.

So the requirement is **a hosted C compiler targeting aarch64-musl**, not just
an assembler and not just a freestanding compiler.

**Could an already-present clang with `--target` satisfy it?** **No, not on its
own, and this is the important detail.** Clang ships its own freestanding
headers, which covers `stddef.h`, `stdint.h`, `stdalign.h` and `immintrin.h`,
but `stdlib.h` and `string.h` are libc headers. **Verified:** the rustup
self-contained directory for `aarch64-unknown-linux-musl` ships `libc.a`
(6.2 MB), the crt objects and `libunwind.a`, and **zero `.h` files**. Rustup
gives you a musl library to link against but no musl headers to compile
against. So even with clang installed, you would additionally need a set of
aarch64 musl headers from somewhere, which means a package or a vendored copy.
That is a genuine cross-toolchain dependency, not a flag.

This machine has no clang at all, so I could not test the clang path directly.
The header analysis above is **verified** from the sources and the rustup
directory listing; the conclusion that clang alone fails is **inferred** from it,
with high confidence.

### `aws-lc-rs`

Same failure, same cause, and worse in degree. `aws-lc-sys` vendors a large
BoringSSL-derived C codebase. Its `prebuilt-nasm` feature is Windows-only and
does not help here. It does ship **pregenerated bindgen bindings** for
`aarch64_unknown_linux_musl`, which removes the need for libclang at build time,
but that only skips binding generation. The C still has to be compiled. The
upstream guidance for musl cross-compilation is to install `musl-tools` and
`musl-dev` and set `CC_aarch64_unknown_linux_musl` and
`AR_aarch64_unknown_linux_musl`. That is a cross-toolchain.

### True cost of going this way

You would be telling every Rustible user that `rustible init` requires a
per-architecture musl cross-compiler before it can build a playbook. On Debian
and Ubuntu there is no packaged `aarch64-linux-musl-gcc`; people use
`musl.cc` tarballs, `cross` with Docker, or zig, all of which section 5.3 of the
vision rules out. This is not a small tax, and it lands on first use.

---

## 5. The architectural way out: let the orchestrator fetch

The idea: `http::Download` and `github::UserKeys` stop speaking TLS. They send a
request up the existing framed channel, the orchestrator performs the HTTPS GET
on the operator's machine where a C toolchain is normal, and the bytes come back
down as chunks.

**It would work, and the machinery is closer than you might think.** Verified on
the `m5-elevated-streaming` branch: `crates/rustible-sdk/src/stream.rs` already
has `Chunk`, `chunks()`, `write_chunks()`, and a `WorkspaceFiles` type with
`resolve`, `open`, `resolve_dest` and `write_chunk`, including symlink-escape
confinement in both directions. Vision 5.5 reserves `FileRequest`, `FileChunk`,
`FileDenied` and `FetchChunk` frames, and 5.6 documents `ctx.local_file(..)` as
the streaming-down mechanism. Adding a `FetchUrl` request frame that reuses the
same chunk plumbing is a modest amount of work.

**But it is the wrong fix for this problem, for four reasons.**

**It contradicts a decision already recorded.** Vision 5.1 says lookups run on
the target, calls it a known semantic difference from Ansible, and cites the
exact case at issue: the eleven `set_fact` tasks in `my_infra` that fetch
`https://github.com/<user>.keys`. Reversing that under pressure from a TLS
dependency problem is letting a build constraint rewrite a semantic decision.

**It changes observable behaviour in ways users will hit.** A target with egress
to a private artifact host that the operator's laptop cannot reach stops working.
So does a target behind a proxy the operator is not behind. So does the reverse.
Ansible's own `get_url` runs on the target for exactly this reason, and the
escape hatch is explicit: you write `delegate_to: localhost` when you want the
controller to fetch, and then `copy` the result. Ansible makes controller-side
fetching opt-in and visible in the playbook. Silently making it the only mode
would be a worse design than Ansible's, not a better one.

**It does not remove TLS from the project.** It moves it into `rustible-cli`.
That is a real gain, because the orchestrator is built natively and can use
`aws-lc-rs` without any cross-compilation problem. But it is a much larger change
than swapping a provider, and it buys the same end state that graviola buys
directly.

**The alternative costs almost nothing.** The provider is confined to two
functions:

- `crates/rustible-std/src/http.rs:442` — `fn tls_provider()`, a one-line body,
  and its doc comment already says *"One place, so swapping it..."*
- `crates/rustible-github/src/fetch.rs:95` — `Https::build_agent`, one inline
  `Arc::new(rustls_rustcrypto::provider())`

Swapping to graviola touches those two sites plus three workspace manifest lines.
Adding CPU pre-flight and fallback is maybe thirty more lines in one shared
place.

**What I would do with the delegation idea instead.** Keep it, but as a future
feature answering a different requirement: air-gapped targets, or targets with no
egress. A `.via_controller()` modifier on `http::Download`, opt-in and visible in
the playbook, matching Ansible's `delegate_to` in spirit. That is worth building
when someone actually needs it. It is not the answer to "our TLS provider is
alpha".

---

## 6. Comparison

| | Pure Rust | Cross-builds with stock rustup | Runtime CPU requirement | Maturity and audit | Cost to switch |
|---|---|---|---|---|---|
| **`rustls-rustcrypto` 0.0.2-alpha** (today) | Yes | **Yes, verified** | None; runs anywhere Rust runs | Alpha, no release since 2024-04. README says do not use in production. AEADs NCC-audited; **P-256/P-384 arithmetic never audited**; `rsa 0.9.10` carries unfixed RUSTSEC-2023-0071 | Zero, it is what we ship |
| **`rustls-graviola` 0.4.0** | **Yes** — 0 `.c`, 0 `.S`, 0 `build.rs`; deps are `cfg-if`, `getrandom` | **Yes, verified on both musl targets; real handshake confirmed** | x86_64: aes, ssse3, avx, avx2, adx, bmi1, bmi2, pclmulqdq (**Broadwell+**, excludes Haswell). aarch64: neon, aes, pmull, sha2 (**excludes Pi 4**). **Panics if absent** | 0.4.x, regular releases since 2024-09, active. s2n-bignum **formally proven** arithmetic; ctgrind constant-time CI; Wycheproof + CAVP vectors. No third-party audit. README: "very new" | **Two call sites + 3 manifest lines**, plus ~30 lines of CPU pre-flight |
| **Both, runtime-selected** | Yes | **Yes, verified; both branches handshake; 5.6 MB release binary** | None. Graviola on capable CPUs, RustCrypto elsewhere | Best available on modern hardware; alpha only on hardware that has no other pure-Rust option | Above, plus keeping `rustls-rustcrypto` in the graph |
| **`ring`** | No. 17 `.c` files compiled by `build.rs` every build (asm is pregenerated) | **No, verified failure**: `failed to find tool "aarch64-linux-musl-gcc"`. Needs a hosted C compiler **and** musl headers, which rustup does not ship | None | Mature, widely deployed, BoringSSL-derived | Breaks the rule. Every user needs a musl cross-toolchain per arch |
| **`aws-lc-rs`** | No. Vendors a large BoringSSL-derived C tree | **No, verified, same error.** Pregenerated bindings for the target exist, so no libclang, but the C still needs compiling | None | Most mature option; FIPS-validated builds available | Same as ring, larger build |
| **Orchestrator fetches (delegation)** | Yes on the target, since the target stops speaking TLS | Yes, trivially | None | N/A | Large. Contradicts vision 5.1, changes egress semantics, needs the streaming protocol landed, and still needs TLS in `rustible-cli` |
| **`boring` / `mbedtls` / `openssl` / `symcrypt` / `wolfcrypt` providers** | No, all wrap C libraries | No | Varies | Varies | Breaks the rule |

---

## 7. The direct answer

**"Is there a way to keep the pure-Rust rule and not ship alpha TLS?"**

**Yes, for any target CPU from roughly 2015 onward.** `rustls-graviola` is fully
pure Rust with no build script and no C anywhere in its graph, cross-builds for
both musl targets with nothing but rustup, and I confirmed a real handshake from
a statically linked musl binary built exactly the way Rustible builds playbooks.
It is a 0.4 release from the rustls author, built on formally-proven arithmetic,
with constant-time testing in CI. It is not alpha and its README does not tell
you to stay away from production. That is a genuine and complete answer for
modern hardware.

**No, not for older hardware, and here is the least-bad option.** Graviola will
not run on pre-Broadwell x86_64 or on a Raspberry Pi 4 and earlier, and there is
no pure-Rust provider other than `rustls-rustcrypto` that will. On that
hardware the choice is:

1. **Fall back to `rustls-rustcrypto`.** You keep the alpha crate in the tree,
   but it stops being the trust root for every download and becomes the path
   taken only on hardware where the alternative is nothing. Verified to build and
   run alongside graviola in one binary.
2. **Refuse.** Declare pre-Broadwell x86_64 and pre-Pi-5 aarch64 unsupported for
   TLS-using ops, and fail those steps with a clear message naming the missing
   CPU feature. Non-TLS ops keep working. Zero alpha code ships.

I lean towards (1) because a config-management tool meeting an old machine
should degrade rather than refuse, and because RustCrypto on an old Xeon is not
worse than what every Rustible target gets today. But (2) is defensible and
simpler, and it is your call which risk you would rather carry.

**Either way, write the CPU pre-flight check.** A bare graviola swap turns an old
CPU into a panic in the middle of a run, which is worse than the honest error
you get from checking first. The detection code is needed for both options, and
once it exists the fallback is a few extra lines.

---

## Appendix: how to reproduce

Every build below used the repo's existing `.cargo/config.toml` (`linker =
"rust-lld"`, `rustflags = ["-C", "link-self-contained=yes"]`) on a machine with
rustc 1.97.1, both musl targets added via rustup, and no clang, no `musl-gcc`,
and no `aarch64-linux-musl-gcc`.

```toml
# probe Cargo.toml
[dependencies]
ureq = { version = "3", default-features = false, features = ["rustls-no-provider", "rustls-webpki-roots"] }
rustls = { version = "0.23", default-features = false, features = ["std", "tls12"] }
rustls-graviola = "0.4"
rustls-rustcrypto = "0.0.2-alpha"
```

```rust
fn main() {
    let p = if graviola_supported() { rustls_graviola::default_provider() }
            else { rustls_rustcrypto::provider() };
    p.install_default().unwrap();
    let agent = ureq::Agent::new_with_defaults();
    let body = agent.get("https://github.com/flipbit03.keys")
        .call().unwrap().into_body().read_to_string().unwrap();
    println!("keys fetched: {}", body.lines().count());
}
```

```
cargo build --target x86_64-unknown-linux-musl     # ok
cargo build --target aarch64-unknown-linux-musl    # ok
./target/x86_64-unknown-linux-musl/debug/probe     # "keys fetched: 4"
```

Graviola's panic behaviour is reproducible in a debug build with its own test
toggle, `GRAVIOLA_CPU_DISABLE_avx2=1` or `GRAVIOLA_CPU_DISABLE_adx=1`.

The `ring` and `aws-lc-rs` failures reproduce with the same config and
`rustls = { version = "0.23", features = ["ring"] }` or `["aws-lc-rs"]`.

### Sources

- [rustls-rustcrypto on crates.io](https://crates.io/crates/rustls-rustcrypto) — 0.0.2-alpha, 2024-04-24
- [RustCrypto/rustls-rustcrypto](https://github.com/RustCrypto/rustls-rustcrypto) — README warning, commit activity
- [Issue #107, plans for a new release](https://github.com/RustCrypto/rustls-rustcrypto/issues/107)
- [Issue #51, release tracker](https://github.com/RustCrypto/rustls-rustcrypto/issues/51)
- [rustls-graviola on crates.io](https://crates.io/crates/rustls-graviola) — 0.4.0, 2026-06-17
- [ctz/graviola](https://github.com/ctz/graviola) — README, `COMPATIBILITY.md`, `src/low/*/cpu.rs`, `src/low/entry.rs`
- [s2n-bignum](https://github.com/awslabs/s2n-bignum) — formally proven assembly
- [rustls crypto module docs](https://docs.rs/rustls/latest/rustls/crypto/index.html) — 0.23.44, provider list
- [RUSTSEC-2023-0071](https://rustsec.org/advisories/RUSTSEC-2023-0071.html) — Marvin attack, `rsa`, unpatched
- [chacha20 docs](https://docs.rs/chacha20/latest/chacha20/), [aes-gcm docs](https://docs.rs/aes-gcm) — NCC Group audits
- [ring #2127, aarch64-musl build](https://github.com/briansmith/ring/issues/2127)
- [aws-lc-sys README](https://github.com/aws/aws-lc-rs/blob/main/aws-lc-sys/README.md) — build prerequisites
