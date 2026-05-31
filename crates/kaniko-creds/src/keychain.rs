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
    ///
    /// Checks multiple sources in order (analogous to Go: `GetKeychain()`
    /// with `authn.NewMultiKeychain`):
    /// 1. Docker config auths entries (with key normalization)
    /// 2. Docker config credHelpers entries
    /// 3. Docker config credsStore (default credential store)
    /// 4. Cloud provider credential helpers (ECR, ACR, GitLab)
    /// 5. Anonymous
    pub fn credentials(&self, registry: &str) -> Result<Credential> {
        let config = self.read_docker_config()?;

        // 1. Check auths — try multiple key variations matching Go's DefaultKeychain:
        //    - registry as-is (e.g. "gcr.io")
        //    - with https:// prefix (e.g. "https://gcr.io")
        //    - with https:// and /v1/ suffix (e.g. "https://index.docker.io/v1/")
        //    - DockerHub special case: "index.docker.io" -> "https://index.docker.io/v1/"
        for key in registry_keys(registry) {
            if let Some(auth) = config.auths.get(&key) {
                if let Some(encoded) = &auth.auth {
                    if let Some(cred) = decode_auth(encoded) {
                        tracing::debug!("Found credentials in auths for key: {}", key);
                        return Ok(cred);
                    }
                }
            }
        }

        // 2. Check credential helpers (also try key variations)
        for key in registry_keys(registry) {
            if let Some(helper) = config.cred_helpers.get(&key) {
                tracing::debug!("Trying credential helper '{}' for key: {}", helper, key);
                return crate::helper::call_credential_helper(helper, registry);
            }
        }

        // 3. Check default credential store
        if let Some(store) = &config.creds_store {
            tracing::debug!("Trying default credential store: {}", store);
            if let Ok(cred) = crate::helper::call_credential_helper(store, registry) {
                return Ok(cred);
            }
        }

        // 4. Cloud provider credential helpers
        if let Some(cred) = self.try_cloud_helpers(registry) {
            return Ok(cred);
        }

        // 5. Anonymous
        tracing::debug!("No credentials found for {}, using anonymous", registry);
        Ok(Credential::anonymous())
    }

    /// Try cloud provider credential helpers based on registry URL patterns.
    ///
    /// Analogous to Go: `GetKeychain()` combining ECR/ACR/GitLab helpers
    /// via `authn.NewMultiKeychain`.
    fn try_cloud_helpers(&self, registry: &str) -> Option<Credential> {
        // ECR: *.dkr.ecr.*.amazonaws.com
        if registry.contains(".dkr.ecr.") && registry.contains(".amazonaws.com") {
            tracing::debug!("Trying ECR credential helper for {}", registry);
            if let Ok(cred) = crate::helper::call_credential_helper("ecr-login", registry) {
                return Some(cred);
            }
        }

        // ACR (Azure): *.azurecr.io
        if registry.ends_with(".azurecr.io") {
            tracing::debug!("Trying ACR credential helper for {}", registry);
            if let Ok(cred) = crate::helper::call_credential_helper("acr-env", registry) {
                return Some(cred);
            }
        }

        // GitLab: registry.gitlab.*
        if registry.contains("registry.gitlab") {
            tracing::debug!("Trying GitLab credential helper for {}", registry);
            if let Ok(cred) = crate::helper::call_credential_helper("gitlabci", registry) {
                return Some(cred);
            }
        }

        // Google: gcr.io, *.pkg.dev, *.gcr.io
        if registry.ends_with("gcr.io") || registry.contains(".gcr.io")
            || registry.ends_with(".pkg.dev") || registry.contains("artifacts.")
        {
            tracing::debug!("Trying Google credential helper for {}", registry);
            // Google uses gcloud as credential helper
            if let Ok(cred) = crate::helper::call_credential_helper("gcloud", registry) {
                return Some(cred);
            }
        }

        None
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

/// Generate possible Docker config.json key variations for a registry.
///
/// Docker config.json can store auth keys in multiple formats:
/// - `gcr.io` (bare hostname)
/// - `https://gcr.io` (with https prefix)
/// - `https://index.docker.io/v1/` (DockerHub with /v1/ suffix)
///
/// This function generates all possible key variations to try when
/// looking up credentials, matching Go's DefaultKeychain behavior.
fn registry_keys(registry: &str) -> Vec<String> {
    let mut keys = Vec::new();

    // 1. Registry as-is
    keys.push(registry.to_string());

    // 2. With https:// prefix
    let with_https = format!("https://{}", registry);
    keys.push(with_https.clone());

    // 3. With https:// prefix and /v1/ suffix
    keys.push(format!("{}/v1/", with_https));

    // 4. DockerHub special case: "index.docker.io" -> "https://index.docker.io/v1/"
    if registry == "index.docker.io" || registry == "docker.io" {
        keys.push("https://index.docker.io/v1/".to_string());
    }

    // 5. If the registry already has https://, also try without it
    if let Some(bare) = registry.strip_prefix("https://") {
        keys.push(bare.trim_end_matches('/').to_string());
        keys.push(bare.trim_end_matches('/').to_string());
    }

    keys
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