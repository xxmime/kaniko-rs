//! SHA-256 digest computation for OCI content-addressable storage.
//!
//! OCI uses SHA-256 digests as content identifiers in the format
//! `sha256:<hex-encoded-hash>`. This module provides the [`Sha256Digest`]
//! type that encapsulates this convention.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

/// A SHA-256 digest in OCI format: `sha256:<hex>`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Sha256Digest {
    hex: String,
}

impl Sha256Digest {
    /// Compute SHA-256 digest from raw bytes.
    ///
    /// Returns a digest in the OCI format `sha256:<64-hex-chars>`.
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let result = hasher.finalize();
        let hex = hex::encode(result);
        Self {
            hex: format!("sha256:{}", hex),
        }
    }

    /// Create a digest from an existing hex string (must include `sha256:` prefix).
    pub fn from_hex(hex: &str) -> Result<Self, DigestError> {
        if !hex.starts_with("sha256:") {
            return Err(DigestError::MissingPrefix(hex.to_string()));
        }
        let hash_part = &hex[7..];
        if hash_part.len() != 64 {
            return Err(DigestError::InvalidLength(hash_part.len()));
        }
        if !hash_part.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(DigestError::InvalidHex(hash_part.to_string()));
        }
        Ok(Self { hex: hex.to_string() })
    }

    /// Returns the full digest string including the `sha256:` prefix.
    pub fn as_str(&self) -> &str {
        &self.hex
    }

    /// Returns only the hex portion (without `sha256:` prefix).
    pub fn hex_only(&self) -> &str {
        &self.hex[7..]
    }

    /// Create an empty/zero digest.
    pub fn zero() -> Self {
        Self {
            hex: "sha256:".to_string() + &"0".repeat(64),
        }
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.hex)
    }
}

impl FromStr for Sha256Digest {
    type Err = DigestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_hex(s)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.hex)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::from_hex(&s).map_err(serde::de::Error::custom)
    }
}

/// Errors that can occur when parsing OCI digests.
#[derive(Debug, thiserror::Error)]
pub enum DigestError {
    #[error("digest missing 'sha256:' prefix: {0}")]
    MissingPrefix(String),
    #[error("digest has invalid length: expected 64 hex chars, got {0}")]
    InvalidLength(usize),
    #[error("digest contains invalid hex characters: {0}")]
    InvalidHex(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_digest_from_bytes() {
        let digest = Sha256Digest::from_bytes(b"hello world");
        assert!(digest.as_str().starts_with("sha256:"));
        assert_eq!(digest.hex_only().len(), 64);
    }

    #[test]
    fn test_digest_from_hex() {
        let hex = "sha256:2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        let digest = Sha256Digest::from_hex(hex).unwrap();
        assert_eq!(digest.as_str(), hex);
    }

    #[test]
    fn test_digest_from_hex_invalid_prefix() {
        let result = Sha256Digest::from_hex("md5:abc");
        assert!(result.is_err());
    }

    #[test]
    fn test_digest_from_hex_invalid_length() {
        let result = Sha256Digest::from_hex("sha256:abc");
        assert!(result.is_err());
    }

    #[test]
    fn test_digest_deterministic() {
        let d1 = Sha256Digest::from_bytes(b"test");
        let d2 = Sha256Digest::from_bytes(b"test");
        assert_eq!(d1, d2);
    }
}