//! Registry authentication.

use kaniko_creds::Credential;

/// Authentication configuration for a registry.
#[derive(Debug, Clone)]
pub struct RegistryAuth {
    /// The registry hostname.
    pub registry: String,
    /// The credentials (if any).
    pub credential: Credential,
    /// Whether to use insecure (HTTP) connection.
    pub insecure: bool,
}

impl RegistryAuth {
    /// Create auth for a registry with credentials.
    pub fn new(registry: &str, credential: Credential) -> Self {
        Self {
            registry: registry.to_string(),
            credential,
            insecure: false,
        }
    }

    /// Create anonymous auth.
    pub fn anonymous(registry: &str) -> Self {
        Self {
            registry: registry.to_string(),
            credential: Credential::anonymous(),
            insecure: false,
        }
    }

    /// Set insecure mode.
    pub fn insecure(mut self, insecure: bool) -> Self {
        self.insecure = insecure;
        self
    }
}