//! Bytes that must not outlive their use: the escalation password and what
//! `ctx.local_secret` returns (vision doc 5.6, 11). Zeroized on drop, never
//! written to disk by the SDK, redacted in `Debug`.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use zeroize::{Zeroize, Zeroizing};

/// In-memory secret bytes. `Clone` copies the bytes into another zeroized
/// buffer; `Debug` prints only the length.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(Zeroizing<Vec<u8>>);

impl Secret {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        Secret(Zeroizing::new(bytes.into()))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The bytes as text, with a trailing newline stripped (a password or
    /// token file usually ends with one). Errors when not UTF-8.
    pub fn as_str(&self) -> crate::Result<&str> {
        let s = std::str::from_utf8(&self.0)
            .map_err(|e| crate::Error::msg(format!("secret is not utf-8: {e}")))?;
        Ok(s.strip_suffix('\n').unwrap_or(s))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Append bytes (used while a secret streams in chunk by chunk).
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
}

/// Explicit early wipe; the same happens on drop.
impl Zeroize for Secret {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Secret({} bytes)", self.0.len())
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Secret(Zeroizing::new(s.into_bytes()))
    }
}

/// On the wire a secret is a plain JSON string (the escalation password in
/// `Start`). Frame buffers are zeroized after decoding, see `protocol`.
impl Serialize for Secret {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match std::str::from_utf8(&self.0) {
            Ok(text) => s.serialize_str(text),
            Err(_) => Err(serde::ser::Error::custom("secret is not utf-8")),
        }
    }
}

impl<'de> Deserialize<'de> for Secret {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = Zeroizing::new(String::deserialize(d)?);
        Ok(Secret(Zeroizing::new(text.as_bytes().to_vec())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_str_are_safe() {
        let s = Secret::new("tok3n\n");
        assert_eq!(format!("{s:?}"), "Secret(6 bytes)");
        assert_eq!(s.as_str().unwrap(), "tok3n");
        assert_eq!(s.as_bytes(), b"tok3n\n");
        assert!(Secret::new(vec![0xff, 0xfe]).as_str().is_err());
    }

    #[test]
    fn bytes_are_zeroized() {
        // Reading memory after `drop` is undefined behaviour (the allocator
        // writes free-list metadata into it), so the wipe is checked on the
        // explicit path; drop goes through the same `Zeroizing` buffer.
        let mut secret = Secret::new(vec![0x5a; 64]);
        secret.push(&[0x5a; 8]);
        assert_eq!(secret.len(), 72);
        secret.zeroize();
        assert!(secret.as_bytes().iter().all(|&b| b == 0));
        assert_eq!(secret.len(), 0, "zeroize also truncates");
    }

    #[test]
    fn serde_round_trip_as_json_string() {
        let s = Secret::new("p@ss");
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"p@ss\"");
        let back: Secret = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }
}
