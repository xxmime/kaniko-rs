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
    ///
    /// Searches for Docker config in multiple locations (matching Go kaniko
    /// and Docker CLI behavior):
    /// 1. $DOCKER_CONFIG/config.json (if DOCKER_CONFIG is set)
    /// 2. $HOME/.docker/config.json
    /// 3. /kaniko/.docker/config.json (kaniko Docker image)
    /// 4. /root/.docker/config.json (common in containers)
    /// 5. $XDG_RUNTIME_DIR/containers/auth.json (Podman)
    pub fn new() -> Self {
        // Collect all candidate paths with their source description for debugging.
        // Priority order (matching Go kaniko's fixed loadDir logic):
        // 1. /kaniko/.docker/config.json (kaniko Docker image — user-mounted)
        // 2. $DOCKER_CONFIG/config.json (if file actually exists)
        // 3. $HOME/.docker/config.json
        // 4. /root/.docker/config.json
        // 5. Podman / XDG paths
        //
        // IMPORTANT: We check /kaniko/.docker BEFORE DOCKER_CONFIG because
        // CI tools (kaniko-action, etc.) often set DOCKER_CONFIG to a path
        // that does not contain config.json, while the user manually mounts
        // credentials at /kaniko/.docker/config.json. This was a bug in the
        // Go version where DOCKER_CONFIG overrode the actual credential path.

        // 1. /kaniko/.docker/config.json (highest priority — user-mounted in container)
        let kaniko_path = PathBuf::from("/kaniko/.docker/config.json");
        if kaniko_path.exists() {
            tracing::debug!("Found Docker config at: /kaniko/.docker/config.json");
            return Self { docker_config_path: kaniko_path };
        }

        // 2. $DOCKER_CONFIG/config.json (only if file actually exists)
        if let Ok(config_dir) = std::env::var("DOCKER_CONFIG") {
            let path = PathBuf::from(&config_dir).join("config.json");
            if path.exists() {
                tracing::debug!("Using Docker config from DOCKER_CONFIG: {}", path.display());
                return Self { docker_config_path: path };
            }
            tracing::debug!("DOCKER_CONFIG set but config not found at: {}", path.display());
            // Fall through to other locations instead of returning non-existent path
        }

        // 3. $HOME/.docker/config.json
        if let Ok(home) = std::env::var("HOME") {
            let path = PathBuf::from(&home).join(".docker/config.json");
            if path.exists() {
                tracing::debug!("Found Docker config at: {} (HOME)", path.display());
                return Self { docker_config_path: path };
            }
        }

        // 4. /root/.docker/config.json
        let root_path = PathBuf::from("/root/.docker/config.json");
        if root_path.exists() {
            tracing::debug!("Found Docker config at: /root/.docker/config.json");
            return Self { docker_config_path: root_path };
        }

        // 5. Check Podman auth file
        if let Ok(xdg_runtime) = std::env::var("XDG_RUNTIME_DIR") {
            let podman_path = PathBuf::from(xdg_runtime).join("containers/auth.json");
            if podman_path.exists() {
                tracing::debug!("Found Podman auth at: {}", podman_path.display());
                return Self { docker_config_path: podman_path };
            }
        }

        // 6. Check REGISTRY_AUTH_FILE (Podman alternative)
        if let Ok(auth_file) = std::env::var("REGISTRY_AUTH_FILE") {
            let path = PathBuf::from(&auth_file);
            if path.exists() {
                tracing::debug!("Found registry auth at: {}", path.display());
                return Self { docker_config_path: path };
            }
        }

        // Default: use /kaniko/.docker/config.json even if it doesn't exist yet
        // This matches the Go kaniko default behavior
        let default = PathBuf::from("/kaniko/.docker/config.json");
        tracing::debug!("No Docker config found, using default path: {}", default.display());
        Self { docker_config_path: default }
    }

    /// Create a keychain with a custom Docker config path.
    ///
    /// Accepts either a directory (containing `config.json`) or a direct
    /// path to the config file itself. This matches Go's `DockerConfLocation()`
    /// behavior where `DOCKER_CONFIG` can be either a directory or a file.
    pub fn with_config_path(path: PathBuf) -> Self {
        let config_path = if path.is_dir() {
            path.join("config.json")
        } else {
            path
        };
        tracing::debug!("Using custom Docker config path: {}", config_path.display());
        Self {
            docker_config_path: config_path,
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
        tracing::debug!("Looking for Docker config at: {}", self.docker_config_path.display());
        if !self.docker_config_path.exists() {
            tracing::warn!("Docker config not found at: {}", self.docker_config_path.display());
            return Err(CredsError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("Docker config not found at: {}", self.docker_config_path.display()),
            )));
        }
        let content = std::fs::read_to_string(&self.docker_config_path)?;
        let config: DockerConfig = serde_json::from_str(&content)?;
        tracing::debug!(
            "Docker config loaded: {} auths, {} credHelpers, credsStore={}",
            config.auths.len(),
            config.cred_helpers.len(),
            config.creds_store.as_deref().unwrap_or("none"),
        );
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