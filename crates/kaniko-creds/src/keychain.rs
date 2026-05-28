//! System credential chain for Docker registry authentication.
//!
//! Analogous to Go: `pkg/creds.GetKeychain()`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during credential operations.
#[derive(Debug, Error)]
pub enum CredsError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("credential helper error: {0}")]
    Helper(String),
    #[error("no credentials found for registry: {0}")]
    NotFound(String),
}

/// Result type for credential operations.
pub type Result<T> = std::result::Result<T, CredsError>;

/// Docker registry credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Credential {
    /// Username for authentication.
    pub username: String,
    /// Password or access token.
    pub password: String,
    /// Identity token (for token-based auth).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identity_token: Option<String>,
    /// Registry token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_token: Option<String>,
}

impl Credential {
    /// Create an anonymous credential (no auth).
    pub fn anonymous() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            identity_token: None,
            registry_token: None,
        }
    }

    /// Check if this is an anonymous credential.
    pub fn is_anonymous(&self) -> bool {
        self.username.is_empty() && self.password.is_empty() && self.identity_token.is_none()
    }
}

/// Parsed Docker config.json structure.
#[derive(Debug, Deserialize)]
struct DockerConfig {
    /// Registry auth entries (base64-encoded "user:pass").
    #[serde(default)]
    auths: HashMap<String, AuthEntry>,
    /// Credential helpers per registry.
    #[serde(default, rename = "credHelpers")]
    cred_helpers: HashMap<String, String>,
    /// Default credential store.
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

/// An auth entry in Docker config.json.
#[derive(Debug, Deserialize)]
struct AuthEntry {
    /// Base64-encoded "username:password".
    #[serde(default)]
    auth: Option<String>,
}

/// System credential chain that checks multiple sources.
pub struct SystemKeychain {
    docker_config_path: PathBuf,
}

impl SystemKeychain {
    /// Create a new system keychain using the default Docker config path.
    pub fn new() -> Self {
        let config_dir = std::env::var("DOCKER_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                dirs_home().join(".docker")
            });
        Self {
            docker_config_path: config_dir.join("config.json"),
        }
    }

    /// Create a keychain with a custom Docker config path.
    pub fn with_config_path(path: PathBuf) -> Self {
        Self {
            docker_config_path: path,
        }
    }

    /// Get credentials for the given registry.
    pub fn credentials(&self, registry: &str) -> Result<Credential> {
        let config = self.read_docker_config()?;

        // 1. Check auths
        if let Some(auth) = config.auths.get(registry) {
            if let Some(encoded) = &auth.auth {
                if let Some(cred) = decode_auth(encoded) {
                    return Ok(cred);
                }
            }
        }

        // 2. Check credential helpers
        if let Some(helper) = config.cred_helpers.get(registry) {
            return crate::helper::call_credential_helper(helper, registry);
        }

        // 3. Check default credential store
        if let Some(store) = &config.creds_store {
            return crate::helper::call_credential_helper(store, registry);
        }

        // 4. Anonymous
        Ok(Credential::anonymous())
    }

    fn read_docker_config(&self) -> Result<DockerConfig> {
        let content = std::fs::read_to_string(&self.docker_config_path)?;
        let config: DockerConfig = serde_json::from_str(&content)?;
        Ok(config)
    }
}

impl Default for SystemKeychain {
    fn default() -> Self {
        Self::new()
    }
}

/// Decode base64-encoded "username:password" auth string.
fn decode_auth(encoded: &str) -> Option<Credential> {
    let decoded = base64_simple_decode::decode(encoded)?;
    let parts: Vec<&str> = decoded.splitn(2, ':').collect();
    if parts.len() == 2 {
        Some(Credential {
            username: parts[0].to_string(),
            password: parts[1].to_string(),
            identity_token: None,
            registry_token: None,
        })
    } else {
        None
    }
}

/// Minimal base64 decode without external dependency.
mod base64_simple_decode {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn decode(input: &str) -> Option<String> {
        let input = input.trim_end_matches('=');
        let mut bytes = Vec::new();
        let mut buffer: u64 = 0;
        let mut bits = 0;

        for ch in input.chars() {
            let val = TABLE.iter().position(|&b| b == ch as u8)?;
            buffer = (buffer << 6) | val as u64;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                bytes.push((buffer >> bits) as u8);
            }
        }

        String::from_utf8(bytes).ok()
    }
}

/// Get the user's home directory.
fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/root"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anonymous_credential() {
        let cred = Credential::anonymous();
        assert!(cred.is_anonymous());
    }

    #[test]
    fn test_decode_auth() {
        // base64("user:pass") = "dXNlcjpwYXNz"
        let cred = decode_auth("dXNlcjpwYXNz").unwrap();
        assert_eq!(cred.username, "user");
        assert_eq!(cred.password, "pass");
    }
}