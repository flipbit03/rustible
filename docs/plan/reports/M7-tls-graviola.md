# M7-tls: the TLS crypto provider moves to `rustls-graviola`

Branch `tls-graviola`. Implements Cadu's decision on
`docs/plan/reports/TLS-OPTIONS.md`: **switch to `rustls-graviola`, with no
fallback.** `rustls-rustcrypto` is gone from the tree.

## What changed

`rustls-rustcrypto` 0.0.2-alpha is removed from `[workspace.dependencies]`,
from `crates/rustible-std/Cargo.toml` and from `crates/rustible-github/Cargo.toml`,
and `rustls-graviola` 0.4 takes its place. There is no fallback provider, no
feature flag and no runtime downgrade. One crypto path.

A new module, `crates/rustible-std/src/tls.rs`, owns both the provider and a
CPU pre-flight, and both network ops go through it:

| | before | after |
|---|---|---|
| `rustible-std::http` | `fn tls_provider()` returning `rustls_rustcrypto::provider()` | `tls::provider()`, and `tls::preflight_url("http::Download", &self.url)?` at the top of `fetch` |
| `rustible-github::fetch` | inline `Arc::new(rustls_rustcrypto::provider())` in `build_agent` | `tls::provider()`, and `tls::preflight_url("github::UserKeys", url)?` at the top of `Https::get` |

`rustible-github` now has **no crypto dependency of its own at all**. It
already depended on `rustible-std`, so the shared module cost no new
dependency edge, and the two crates cannot drift apart on what the CPU floor
is. `rustible-std` keeps its direct `rustls` entry, because `tls` names
`rustls::crypto::CryptoProvider`.

## Why the pre-flight exists

Graviola gets its speed by assuming instruction set extensions rather than
detecting them, and it enforces that with a chain of `assert!` in
`verify_cpu_features()`, called from `Entry::new_public()` and
`Entry::new_secret()` at every public function. So on a CPU below the floor a
bare swap panics **inside the handshake**. The runtime's `catch_unwind` sits at
the playbook entry, so the process survives, but the panic unwinds past any
per-step error handling and kills every remaining task. A failed step is
recoverable; a panic in the middle of a run is not.

`tls::preflight` therefore detects the requirements first, with
`std::arch::is_x86_feature_detected!` and
`std::arch::is_aarch64_feature_detected!`, and returns a normal step error. It
names the missing extension, the hardware that excludes, the op, and the fact
that the pure-Rust rule is why there is no alternative:

```
http::Download: this CPU lacks the adx instruction set extension(s) that
Rustible's TLS provider (rustls-graviola) requires, so the HTTPS request
cannot be made. That is an x86_64 CPU older than Intel Broadwell (Haswell and
earlier have avx2 and bmi2 but no adx) or an AMD part from before Zen.
Rustible links only pure-Rust crypto (vision 5.3), and rustls-graviola is the
only pure-Rust rustls provider that is not alpha software, so there is no
fallback to select and no way to enable one. Ops that do not use the network
are unaffected; run http::Download from a newer machine, or fetch the data
elsewhere and pass it in.
```

## The exact CPU requirements, verified against graviola's source

Read from `graviola-0.4.1/src/low/x86_64/cpu.rs:226` and
`.../aarch64/cpu.rs:207`, not from the README.

| arch | required | excludes |
|---|---|---|
| x86_64 | `aes`, `pclmulqdq`, `bmi1`, `adx`, `avx`, `avx2` | Intel before Broadwell (Haswell has avx2 and bmi2 but no adx); AMD before Zen |
| aarch64 | `neon`, `aes`, `pmull`, `sha2` | cores without the ARMv8 crypto extensions, so Raspberry Pi 4 and earlier; Pi 5 is fine |

**The README is wrong in two directions and the source is what matters.** It
lists `ssse3` and `bmi2` as required. Neither is asserted: a comment at
`cpu.rs:263` says `ssse3` is implied by `avx`, and `bmi2` is detected through
an optional `HaveBmi2` token type whose absence falls back to
`generic::sha512` rather than panicking. Including them would refuse machines
that would have worked. Conversely `bmi1` **is** asserted and the README omits
it. The list in `tls::cpu_features()` is graviola's assertion list exactly, in
the same order, pinned by `tls::tests::required_features_are_the_asserted_set`.

One nuance, deliberately not modelled: graviola's `adx` assert has an
`|| option_env!("VALGRIND_BUG_494162").is_some()` escape. In that non-default
valgrind-only build graviola would run without ADX while the pre-flight still
refuses. Strictly-stronger in a configuration nobody ships.

## Tests

Seven unit tests in `crates/rustible-std/src/tls.rs`.

- **`detection_agrees_with_cpuinfo`** checks every detected feature against
  `/proc/cpuinfo`, an independent source, rather than the macro checking
  itself. It maps `neon` to the kernel's `asimd` spelling on aarch64 and
  returns quietly where the kernel publishes no such line.
- **`this_cpu_is_supported`** asserts the test host is above the floor, so a
  future failure of the network suite on a sub-floor CI runner is explained by
  a named test rather than by a confusing timeout.
- **`required_features_are_the_asserted_set`** pins the list per architecture.
- **`the_error_names_the_feature_the_op_and_the_rule`** and
  **`the_error_lists_every_missing_feature`** pin the message: the feature, the
  op, `rustls-graviola`, "pure-Rust", "no fallback", and the hardware family.
- **`only_https_urls_are_gated`** and **`plain_http_is_never_refused`** cover
  the scheme gate, including the multi-byte cases that would panic under
  `&url[..8]`.

**The unsupported branch is tested at the function, not end to end.** The brief
asked to use graviola's debug toggle if one existed, and one does
(`GRAVIOLA_CPU_DISABLE_<feature>`), but it is the wrong instrument here: it
suppresses *graviola's* detection and not `std::arch`'s, so setting it makes
graviola panic while the pre-flight happily passes. That is, it produces
exactly the failure the pre-flight exists to prevent, and would test nothing.
So `unsupported_cpu` is called directly with a fabricated missing list, and the
detection half is covered independently against `/proc/cpuinfo`.

## The pure-Rust rule still holds

Cross-built with stock rustup, the repo's own `.cargo/config.toml`
(`linker = "rust-lld"`, `link-self-contained=yes`) and **no C toolchain**: this
VM has no `clang`, no `musl-gcc` and no `aarch64-linux-musl-gcc`.

The probe is a `--profile dist` binary linking both `rustible-std`
(`http::Download`) and `rustible-github` (`Https`), i.e. the full TLS surface a
real playbook binary carries.

```
x86_64-unknown-linux-musl   ELF 64-bit LSB pie executable, static-pie linked, stripped
aarch64-unknown-linux-musl  ELF 64-bit LSB executable, ARM aarch64, statically linked, stripped
```

Both were run against `https://github.com/flipbit03.keys` for a genuine TLS 1.3
handshake, not just built:

- the x86_64 binary on this VM: `status 200 keys 4`
- the aarch64 binary on `cadu@cadu-cogram-vm-arm`: `status 200 keys 4`

The ARM run was not asked for and is worth having: it is the only end-to-end
exercise of the aarch64 detection path, on a machine whose `Features` line
really does carry `asimd aes pmull sha2`.

### Binary size

Same probe, same `dist` profile, before and after the swap.

| target | rustls-rustcrypto | rustls-graviola | delta |
|---|---|---|---|
| `x86_64-unknown-linux-musl` | 1,702,728 | 2,290,344 | +587,616 (+34.5%) |
| `aarch64-unknown-linux-musl` | 1,406,136 | 1,939,056 | +532,920 (+37.9%) |

**This went the other way from what the shape of the change suggests.**
Graviola has a far smaller dependency graph (`cfg-if` and `getrandom` against
RustCrypto's twenty-odd crates), and dropping `rustls-rustcrypto` also removes
the duplicate `rustls-webpki` copy the research report noted. The binary still
grows by about a third, presumably because graviola carries large unrolled
`asm!` bodies and precomputed tables where RustCrypto uses compact generic
code. Half a megabyte per playbook binary is a real cost worth knowing, though
it is not obviously a reason to revisit the decision.

### The dependency graph

The workspace lock goes from **217 crates to 165**, a net 52 fewer. Graviola
brings `graviola` and `rustls-graviola` (plus `wasip2` and `wit-bindgen`, which
are `getrandom`'s wasi-target entries and are never compiled for a Linux
target). Leaving are the whole RustCrypto stack and its `rand`/`zeroize`/`der`
supporting cast: `aead`, `aes`, `aes-gcm`, `chacha20`, `chacha20poly1305`,
`crypto-bigint`, `curve25519-dalek`, `ecdsa`, `ed25519-dalek`,
`elliptic-curve`, `ghash`, `hmac`, `p256`, `p384`, `pkcs1`, `pkcs8`,
`poly1305`, `primeorder`, `rsa`, `sec1`, `spki`, `x25519-dalek` and thirty
more.

Two of those matter beyond the count. **`rsa` 0.9.10 leaves the graph**, and
with it RUSTSEC-2023-0071 (the Marvin timing attack, unpatched, no fixed
version available) that the research report flagged. And the unaudited
`p256`/`p384` field arithmetic that sat on the ECDHE and certificate-verify hot
path is replaced by s2n-bignum code with machine-checked proofs. Whatever else
this change costs, the trust story for the path from a network response to a
user's `authorized_keys` is materially better.

## What did not go as the research predicted

1. **MSRV.** `graviola` 0.4.1 declares `rust-version = "1.89"`, so
   `cargo +1.88 check --workspace` refuses the workspace outright. The research
   report does not mention this. Every graviola from 0.3.0 (2025-08-30) onwards
   declares 1.89; only 0.2.1 and earlier are on 1.72, and `rustls-graviola` 0.4
   requires graviola 0.4, so pinning back is not an option. `rust-version` in
   the workspace manifest and CI's MSRV job both move 1.88 → 1.89. Rust 1.89
   shipped 2025-08-04, over a year before this change, so the practical cost is
   small, but it is a visible project-wide change that was not in the brief.
2. **Binary size**, above.
3. **Everything else matched.** Purity, both cross-builds, the real handshake,
   the panic behaviour and the feature lists were all as reported.

## Review

Reviewed at high effort against graviola's own source. One confirmed bug,
fixed; the rest of the diff came back clean.

**Fixed — the pre-flight was scheme-blind.** Both call sites ran the check on
every request, but `http::Download` documents `http://` as well as `https://`,
`validate_url` accepts it, and the op's own container test downloads over plain
HTTP from a loopback listener. `github::UserKeys::base_url(..)` likewise
accepts a plain-HTTP mirror. On a sub-floor machine a plain-HTTP download that
never touches the crypto provider was therefore refused, with a message about
HTTPS that did not describe the request. That is precisely the "refuse a
machine that would have worked" failure the check exists to avoid. The gate is
now `tls::preflight_url(op, url)`, matching `https://` byte-wise with
`split_at_checked` and `eq_ignore_ascii_case` (slicing `&url[..8]` would panic
on a URL whose eighth byte is inside a multi-byte character).

**Confirmed clean, by reading graviola and by probe.** No path reaches graviola
crypto without the pre-flight: `rustls_graviola::default_provider()` is three
static references and two `to_vec()` calls and does no crypto,
`TlsConfig::builder().build()` and `Agent::config_builder().build()` do no
crypto, and the panic fires only at `.call()`. Both was checked by building a
debug probe with `GRAVIOLA_CPU_DISABLE_adx=1` and watching which step panicked.
`Download::check` never calls `fetch`. Redirects and pooled connections reuse
the already-checked agent. Nothing else in `crates/` builds a `ureq` agent.
There are no `unwrap`s, slices or arithmetic in `tls.rs` outside its tests. The
Broadwell and Pi 4 claims in the message are correct.

**Dismissed — `Https::get` hardcodes `"github::UserKeys"` as the op name.**
`Https` is public, so a third party fetching something else would see an error
naming an op they never ran. Fixing it properly means an op-label field and a
constructor on a public type, which is more surface than the inaccuracy is
worth at 0.0.1: `Https` is documented as the transport for `UserKeys`, and the
step name in the run report already tells the operator which step failed.

**Dismissed — fold `preflight` and the agent into one `tls::agent(op, url)`.**
The reviewer is right that the pairing is currently a hand-discipline rather
than a structural guarantee, and that the two `agent` builders are near
duplicates. They are not identical, though: the two ops differ in user agent
and in whether `timeout_recv_body` is set, so unifying them would silently
change `github`'s timeout behaviour. The two call sites are the entire
population, the module doc now teaches the pairing, and this branch should stay
tight. Worth revisiting if a third collection appears.

## Verification

Everything below is in `docs/plan/logs/M7-tls-graviola-done.txt`, run twice:
once on the merged tree and once again after the review fix. All green both
times.

| command | result |
|---|---|
| `cargo fmt --all --check` | clean |
| `cargo clippy --workspace --all-targets -- -D warnings` | clean |
| `cargo test --workspace` | all pass; `rustible-std` 333 unit tests pass, 1 ignored |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --lib` | clean |
| `cargo build --manifest-path examples/workspace/Cargo.toml` | ok |
| `cargo +1.89 check --workspace --all-targets` | ok (the new MSRV floor) |
| `cargo +1.88 check --workspace --all-targets` | fails, as expected: `graviola@0.4.1 requires rustc 1.89` |
| `cargo test -p rustible-github -- --ignored` | live GitHub fetch over graviola TLS, passes |
| `cargo test -p rustible-std --lib https_ -- --ignored` | live HTTPS download, passes |
| `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --test it_http_download` | passes |
| `RUSTIBLE_INTEGRATION=1 cargo test -p rustible-std --tests` | 13 container test files, all pass |

`main` was merged into the branch mid-way (it had gained PR 19, the three
decided amendments). `docs/plan/DECISIONS.md` conflicted and keeps both sides.

## Decisions recorded

Six `[M7-tls]` entries in `docs/plan/DECISIONS.md`, each with a Reverse clause:
the swap itself, the no-fallback decision and its hardware cost, the pre-flight,
the shared-module placement, the MSRV bump, and how the unsupported branch is
tested. A seventh records the review fix. The `[M6-gh]` proposed amendment
about the alpha provider is marked decided by Cadu on 2026-09-08 and points at
them. `docs/plan/reports/TLS-OPTIONS.md` gains a Decision section at the top
recording what was chosen and the two things it did not predict.

## The cost, stated plainly

`http::Download` and `github::UserKeys` **cannot run at all** on x86_64 older
than Intel Broadwell or AMD Zen, or on aarch64 without the crypto extensions
(Raspberry Pi 4 and earlier). Those machines get a failed step naming the
missing extension. Every op that does not use the network is unaffected, and
plain-HTTP downloads still work there. This is the accepted price of one
crypto path.
