//! The TLS crypto provider, in one place, and the CPU pre-flight that keeps
//! it from panicking mid-run.
//!
//! Rustible links only pure-Rust crypto (vision 5.3): rustls's own providers
//! (`ring`, `aws-lc-rs`) compile C, so a playbook could not be cross-built for
//! a musl target with nothing but `rustup` and `rust-lld`. Exactly two
//! pure-Rust rustls providers exist. Rustible uses
//! [`rustls-graviola`](https://crates.io/crates/rustls-graviola), whose
//! arithmetic comes from the formally proven s2n-bignum and whose
//! constant-time behaviour is checked in CI, and it is the only one: there is
//! no fallback provider and no runtime downgrade.
//!
//! ## The cost, and why the pre-flight exists
//!
//! Graviola's speed comes from assuming instruction set extensions rather than
//! detecting them, and it enforces that with `assert!` at its first crypto
//! call. On a CPU without them the process **panics inside the handshake**,
//! which in Rustible would abort a running playbook rather than fail one step.
//!
//! So nothing here reaches graviola before [`preflight`] has checked the CPU
//! with `std::arch::is_x86_feature_detected!` /
//! `std::arch::is_aarch64_feature_detected!`. A machine that falls short gets
//! a normal step failure naming the missing extension. Non-network ops are
//! unaffected and keep working.
//!
//! The excluded hardware is pre-Broadwell x86_64 (Haswell has AVX2 and BMI2
//! but no ADX) and, on aarch64, cores without the ARMv8 crypto extensions,
//! which is Raspberry Pi 4 and earlier.
//!
//! ## Using it from a collection
//!
//! ```no_run
//! use rustible_std::tls;
//!
//! # fn main() -> rustible_sdk::error::Result<()> {
//! tls::preflight("mycollection::Fetch")?;
//! let agent = ureq::Agent::config_builder()
//!     .tls_config(
//!         ureq::tls::TlsConfig::builder()
//!             .unversioned_rustls_crypto_provider(tls::provider())
//!             .build(),
//!     )
//!     .build();
//! # let _ = agent;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use rustible_sdk::prelude::*;

/// The crypto provider every Rustible HTTPS request uses.
///
/// Building one is cheap and does no crypto, so this never panics on an
/// unsupported CPU. The panic would come later, at the first handshake, which
/// is why every caller must pass [`preflight`] first.
pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls_graviola::default_provider())
}

/// Refuse an HTTPS operation on a CPU the crypto provider cannot run on.
///
/// `op` names the operation for the error message, for example
/// `"http::Download"`. Returns `Ok(())` on every CPU Rustible supports, which
/// is x86_64 from Intel Broadwell and AMD Zen onwards and aarch64 with the
/// crypto extensions.
///
/// Call this **before** any request. The detection results are cached by the
/// standard library after the first call, so calling it per request is free.
pub fn preflight(op: &str) -> Result<()> {
    let missing = missing_cpu_features();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(unsupported_cpu(op, &missing))
    }
}

/// Every CPU feature the crypto provider requires, each with whether this
/// machine has it. Mirrors graviola's own `verify_cpu_features()`.
///
/// The x86_64 list is the one graviola asserts on, which is narrower than its
/// README: `ssse3` is implied by `avx` (graviola says so in a comment) and
/// `bmi2` is detected as optional through a token type rather than asserted,
/// so neither can be the reason for a panic.
#[cfg(target_arch = "x86_64")]
pub fn cpu_features() -> Vec<(&'static str, bool)> {
    vec![
        ("aes", std::arch::is_x86_feature_detected!("aes")),
        (
            "pclmulqdq",
            std::arch::is_x86_feature_detected!("pclmulqdq"),
        ),
        ("bmi1", std::arch::is_x86_feature_detected!("bmi1")),
        ("adx", std::arch::is_x86_feature_detected!("adx")),
        ("avx", std::arch::is_x86_feature_detected!("avx")),
        ("avx2", std::arch::is_x86_feature_detected!("avx2")),
    ]
}

/// Every CPU feature the crypto provider requires, each with whether this
/// machine has it. Mirrors graviola's own `verify_cpu_features()`.
#[cfg(target_arch = "aarch64")]
pub fn cpu_features() -> Vec<(&'static str, bool)> {
    vec![
        ("neon", std::arch::is_aarch64_feature_detected!("neon")),
        ("aes", std::arch::is_aarch64_feature_detected!("aes")),
        ("pmull", std::arch::is_aarch64_feature_detected!("pmull")),
        ("sha2", std::arch::is_aarch64_feature_detected!("sha2")),
    ]
}

// No arm for other architectures on purpose: graviola itself is
// `compile_error!("This crate only supports x86_64 or aarch64")` there, so a
// build for one never gets this far and a third arm would be unreachable.

/// The required CPU features this machine does **not** have, in the order
/// [`cpu_features`] lists them. Empty on a supported CPU.
pub fn missing_cpu_features() -> Vec<&'static str> {
    cpu_features()
        .into_iter()
        .filter_map(|(name, have)| (!have).then_some(name))
        .collect()
}

/// What the missing extensions say about the machine. Separate per
/// architecture so the message names hardware the reader can recognise.
#[cfg(target_arch = "x86_64")]
const EXCLUDED_HARDWARE: &str = "That is an x86_64 CPU older than Intel Broadwell \
    (Haswell and earlier have avx2 and bmi2 but no adx) or an AMD part from before Zen";

/// What the missing extensions say about the machine. Separate per
/// architecture so the message names hardware the reader can recognise.
#[cfg(target_arch = "aarch64")]
const EXCLUDED_HARDWARE: &str = "That is an aarch64 core without the ARMv8 crypto \
    extensions, which includes Raspberry Pi 4 and earlier but not Raspberry Pi 5";

/// The failure a machine below the floor gets. Split out from [`preflight`] so
/// the message can be tested without lying about the running CPU.
fn unsupported_cpu(op: &str, missing: &[&str]) -> Error {
    Error::msg(format!(
        "{op}: this CPU lacks the {} instruction set extension(s) that Rustible's \
         TLS provider (rustls-graviola) requires, so the HTTPS request cannot be \
         made. {EXCLUDED_HARDWARE}. Rustible links only pure-Rust crypto \
         (vision 5.3), and rustls-graviola is the only pure-Rust rustls provider \
         that is not alpha software, so there is no fallback to select and no \
         way to enable one. Ops that do not use the network are unaffected; run \
         {op} from a newer machine, or fetch the data elsewhere and pass it in.",
        missing.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names `/proc/cpuinfo` uses for the features we detect, where they
    /// differ from the `is_*_feature_detected!` spelling.
    fn cpuinfo_name(feature: &str) -> &str {
        match feature {
            // aarch64 advertises NEON as `asimd` in the `Features` line.
            "neon" => "asimd",
            other => other,
        }
    }

    fn cpuinfo_flags() -> Option<Vec<String>> {
        let text = std::fs::read_to_string("/proc/cpuinfo").ok()?;
        let line = text
            .lines()
            .find(|l| l.starts_with("flags") || l.starts_with("Features"))?;
        let (_, values) = line.split_once(':')?;
        Some(values.split_whitespace().map(str::to_owned).collect())
    }

    /// The detection reports what this CPU actually has, checked against an
    /// independent source rather than against itself.
    #[test]
    fn detection_agrees_with_cpuinfo() {
        let Some(flags) = cpuinfo_flags() else {
            // Not Linux, or a kernel that does not publish the line. The rest
            // of the suite still covers the message.
            return;
        };
        for (feature, detected) in cpu_features() {
            let in_cpuinfo = flags.iter().any(|f| f == cpuinfo_name(feature));
            assert_eq!(
                detected, in_cpuinfo,
                "{feature}: is_*_feature_detected! says {detected}, /proc/cpuinfo says {in_cpuinfo}"
            );
        }
    }

    /// The machine running the suite is one Rustible supports, so the
    /// pre-flight lets HTTPS through. If this ever fails, the test host is
    /// itself below the floor and the whole network suite is expected to fail.
    #[test]
    fn this_cpu_is_supported() {
        assert_eq!(missing_cpu_features(), Vec::<&str>::new());
        preflight("tls::tests").expect("the test host should support the provider");
    }

    /// The list is exactly graviola's `verify_cpu_features()` assertions, and
    /// nothing detects as an unknown name.
    #[test]
    fn required_features_are_the_asserted_set() {
        let names: Vec<&str> = cpu_features().into_iter().map(|(n, _)| n).collect();
        #[cfg(target_arch = "x86_64")]
        assert_eq!(names, ["aes", "pclmulqdq", "bmi1", "adx", "avx", "avx2"]);
        #[cfg(target_arch = "aarch64")]
        assert_eq!(names, ["neon", "aes", "pmull", "sha2"]);
    }

    /// The unsupported branch. Tested at the function rather than end to end:
    /// graviola's own `GRAVIOLA_CPU_DISABLE_*` toggle suppresses *its*
    /// detection, not `std::arch`'s, so setting it would produce the panic
    /// this check exists to prevent instead of this error.
    #[test]
    fn the_error_names_the_feature_the_op_and_the_rule() {
        let e = unsupported_cpu("http::Download", &["adx"]).to_string();
        assert!(e.contains("adx"), "{e}");
        assert!(e.contains("http::Download"), "{e}");
        assert!(e.contains("rustls-graviola"), "{e}");
        assert!(e.contains("pure-Rust"), "{e}");
        assert!(e.contains("no fallback"), "{e}");
        #[cfg(target_arch = "x86_64")]
        assert!(e.contains("Broadwell"), "{e}");
        #[cfg(target_arch = "aarch64")]
        assert!(e.contains("Raspberry Pi 4"), "{e}");
    }

    /// Several missing extensions are all named, not just the first.
    #[test]
    fn the_error_lists_every_missing_feature() {
        let e = unsupported_cpu("github::UserKeys", &["adx", "avx2"]).to_string();
        assert!(e.contains("adx, avx2"), "{e}");
        assert!(e.contains("github::UserKeys"), "{e}");
    }
}
