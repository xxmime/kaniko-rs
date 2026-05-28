//! Credential helper invocation.
//!
//! Calls `docker-credential-<helper>` subprocesses to obtain credentials.
//! Analogous to Go: `pkg/creds/dockercreds.fromHelper()`.

use crate::keychain::{CredsError, Credential, Result};
use std::collections::HashMap;
use std::sync::Mutex;

/// Response from a credential helper.
#[derive(Debug, serde::Deserialize)]
struct CredentialResponse {
    #[serde(rename = "Username")]
    username: String,
    #[serde(rename = "Secret")]
    secret: String,
    #[serde(rename = "ServerURL", skip_serializing_if = "Option::is_none")]
    server_url: Option<String>,
}

/// Credential helper cache for faster repeated lookups.
pub struct CredentialHelperCache {
    /// Cached credentials keyed by (helper_name, registry).
    cache: Mutex<HashMap<String, Credential>>,
}

impl CredentialHelperCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Get cached credentials or call the helper.
    pub fn get_or_call(&self, helper: &str, registry: &str) -> Result<Credential> {
        let cache_key = format!("{}:{}", helper, registry);

        // Check cache first
        {
            let cache = self.cache.lock().unwrap();
            if let Some(cred) = cache.get(&cache_key) {
                return Ok(cred.clone());
            }
        }

        // Call helper
        let cred = call_credential_helper(helper, registry)?;

        // Store in cache
        {
            let mut cache = self.cache.lock().unwrap();
            cache.insert(cache_key, cred.clone());
        }

        Ok(cred)
    }

    /// Clear the cache.
    pub fn clear(&self) {
        let mut cache = self.cache.lock().unwrap();
        cache.clear();
    }

    /// Get the number of cached entries.
    pub fn cache_size(&self) -> usize {
        let cache = self.cache.lock().unwrap();
        cache.len()
    }
}

impl Default for CredentialHelperCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Call a Docker credential helper to get credentials for a registry.
///
/// The helper binary is expected to be named `docker-credential-<helper>`.
/// It is called with the "get" command and the registry URL on stdin.
/// It returns JSON: `{"Username":"...","Secret":"...","ServerURL":"..."}`.
pub fn call_credential_helper(helper: &str, registry: &str) -> Result<Credential> {
    let binary = format!("docker-credential-{}", helper);

    tracing::debug!("Calling credential helper {} for registry {}", binary, registry);

    let mut child = std::process::Command::new(&binary)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CredsError::Helper(format!("failed to spawn {}: {}", binary, e)))?;

    // Write the registry URL to the helper's stdin.
    // Per the Docker credential helper protocol, the registry server URL
    // is sent on stdin followed by a newline.
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        writeln!(stdin, "{}", registry)
            .map_err(|e| CredsError::Helper(format!("failed to write to {} stdin: {}", binary, e)))?;
        drop(stdin); // Close stdin to signal EOF
    }

    let output = child
        .wait_with_output()
        .map_err(|e| CredsError::Helper(format!("failed to wait for {}: {}", binary, e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("Credential helper {} failed: {}", binary, stderr);
        return Err(CredsError::Helper(format!(
            "{} get failed: {}",
            binary, stderr
        )));
    }

    let resp: CredentialResponse = serde_json::from_slice(&output.stdout)
        .map_err(|e| CredsError::Helper(format!("failed to parse {} output: {}", binary, e)))?;

    tracing::debug!("Successfully obtained credentials from {} for {}", binary, registry);

    Ok(Credential {
        username: resp.username,
        password: resp.secret,
        identity_token: None,
        registry_token: None,
    })
}

/// Call a Docker credential helper asynchronously using tokio.
///
/// This is the async version of `call_credential_helper`, suitable for
/// use in the async build pipeline.
pub async fn call_credential_helper_async(helper: &str, registry: &str) -> Result<Credential> {
    let binary = format!("docker-credential-{}", helper);
    let registry = registry.to_string();

    tracing::debug!("Calling credential helper {} for registry {} (async)", binary, registry);

    let output = tokio::process::Command::new(&binary)
        .arg("get")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| CredsError::Helper(format!("failed to spawn {}: {}", binary, e)))?
        .wait_with_output()
        .await
        .map_err(|e| CredsError::Helper(format!("failed to wait for {}: {}", binary, e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("Credential helper {} failed: {}", binary, stderr);
        return Err(CredsError::Helper(format!(
            "{} get failed: {}",
            binary, stderr
        )));
    }

    let resp: CredentialResponse = serde_json::from_slice(&output.stdout)
        .map_err(|e| CredsError::Helper(format!("failed to parse {} output: {}", binary, e)))?;

    Ok(Credential {
        username: resp.username,
        password: resp.secret,
        identity_token: None,
        registry_token: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: These tests require docker-credential-* binaries to be installed.
    // They are skipped in CI environments without credential helpers.

    #[test]
    fn test_call_nonexistent_helper() {
        let result = call_credential_helper("nonexistent-helper-xyz", "https://example.com");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_call_nonexistent_helper_async() {
        let result = call_credential_helper_async("nonexistent-helper-xyz", "https://example.com").await;
        assert!(result.is_err());
    }

    #[test]
    fn test_credential_helper_cache() {
        let cache = CredentialHelperCache::new();
        assert_eq!(cache.cache_size(), 0);

        // Calling nonexistent helper through cache should still fail
        let result = cache.get_or_call("nonexistent", "https://example.com");
        assert!(result.is_err());
        assert_eq!(cache.cache_size(), 0);
    }

    #[test]
    fn test_cache_clear() {
        let cache = CredentialHelperCache::new();
        let mut inner_cache = cache.cache.lock().unwrap();
        inner_cache.insert("test:key".to_string(), Credential::anonymous());
        assert_eq!(inner_cache.len(), 1);
        drop(inner_cache);

        cache.clear();
        assert_eq!(cache.cache_size(), 0);
    }
}