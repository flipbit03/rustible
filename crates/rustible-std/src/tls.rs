//! The TLS crypto provider, chosen in one place.
//!
//! Rustible uses [`ring`](https://crates.io/crates/ring), reached through
//! rustls's own `ring` feature, and it is the only provider in the tree: there
//! is no fallback and no runtime downgrade.
//!
//! `ring` compiles a small amount of C, which is why an operator needs a
//! `clang` on `PATH` to cross-build a playbook binary (vision 5.3). What that
//! buys is runtime CPU feature detection: ring dispatches on CPUID and falls
//! back to baseline x86-64 code, so a playbook binary runs on any x86-64 or
//! aarch64 machine rather than requiring a fixed instruction-set floor. The
//! provider Rustible used before this one, `rustls-graviola`, asserted its
//! extensions instead and aborted mid-handshake on anything older than Intel
//! Broadwell or AMD Zen; `docs/plan/reports/C-TOOLCHAIN-SPIKE.md` measures
//! both. Nothing here needs a CPU pre-flight, and there is none.
//!
//! The compiler requirement is a *build-time* one, on the operator's machine.
//! The `rustible` CLI checks for it before the build and supplies the C
//! compiler and, on `x86_64-unknown-linux-musl`, the libc headers itself, so
//! nobody exports an environment variable by hand.
//!
//! ## Using it from a collection
//!
//! One call, when the agent is built. There is nothing to do per request.
//!
//! ```no_run
//! use rustible_std::tls;
//!
//! let agent = ureq::Agent::config_builder()
//!     .tls_config(
//!         ureq::tls::TlsConfig::builder()
//!             .unversioned_rustls_crypto_provider(tls::provider())
//!             .build(),
//!     )
//!     .build();
//! # let _: ureq::Agent = agent.into();
//! ```

use std::sync::Arc;

/// The crypto provider every Rustible HTTPS request uses.
///
/// Both `rustible_std::http` and `rustible-github` call this, so the two
/// cannot drift apart on which crypto they speak; a third-party collection
/// should call it too rather than naming a provider of its own.
pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Building the provider works and it offers cipher suites to negotiate
    /// with. Cheap, but it is the one thing that would break silently if the
    /// `ring` feature were dropped from the `rustls` entry: without a provider
    /// the failure would surface much later, as a handshake error.
    #[test]
    fn the_provider_builds_and_has_cipher_suites() {
        let p = provider();
        assert!(!p.cipher_suites.is_empty());
        assert!(!p.kx_groups.is_empty());
    }
}
