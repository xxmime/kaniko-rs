//! Image reference parsing for Docker/OCI images.
//!
//! Supports various reference formats:
//! - "ubuntu:22.04" -> registry-1.docker.io/library/ubuntu:22.04
//! - "library/ubuntu:22.04" -> registry-1.docker.io/library/ubuntu:22.04
//! - "registry.io/repo:tag" -> registry.io/repo:tag

/// Parsed reference: registry / repository / tag.
#[derive(Debug, Clone)]
pub struct Reference {
    pub registry: String,
    pub repository: String,
    pub tag: String,
}

/// Errors during reference parsing.
#[derive(Debug, thiserror::Error)]
pub enum ReferenceError {
    #[error("invalid reference: {0}")]
    Invalid(String),
}

/// Parse a reference string, returning a Reference or ReferenceError.
impl Reference {
    /// Parse a reference string like "gcr.io/my-project/my-app:latest".
    /// Supports various formats:
    /// - "ubuntu:22.04" -> registry-1.docker.io/library/ubuntu:22.04
    /// - "library/ubuntu:22.04" -> registry-1.docker.io/library/ubuntu:22.04
    /// - "registry.io/repo:tag" -> registry.io/repo:tag
    pub fn parse(reference: &str) -> Result<Self, ReferenceError> {
        let reference = reference.trim();
        
        // Handle empty reference
        if reference.is_empty() {
            return Err(ReferenceError::Invalid("empty reference".to_string()));
        }

        // Determine registry and repository
        let (registry, repository, tag) = if reference.contains('/') {
            // Case 1: Contains '/', so it might be registry/repo or library/repo
            let parts: Vec<&str> = reference.splitn(2, '/').collect();
            let first_part = parts[0];
            let rest = parts[1];
            
            // Check if first_part looks like a registry (contains . or : or is localhost)
            let is_registry = first_part.contains('.') || first_part.contains(':') || first_part == "localhost";
            
            if is_registry {
                // Case: registry.io/repo:tag
                let (repo, tag) = if let Some((repo, t)) = rest.rsplit_once(':') {
                    (repo.to_string(), t.to_string())
                } else {
                    (rest.to_string(), "latest".to_string())
                };
                (first_part.to_string(), repo, tag)
            } else {
                // Case: library/repo:tag -> registry-1.docker.io/library/repo:tag
                let (repo, tag) = if let Some((repo, t)) = reference.rsplit_once(':') {
                    (repo.to_string(), t.to_string())
                } else {
                    (reference.to_string(), "latest".to_string())
                };
                ("registry-1.docker.io".to_string(), repo, tag)
            }
        } else {
            // Case 2: No '/', so it's just repo:tag -> registry-1.docker.io/library/repo:tag
            let (repo, tag) = if let Some((repo, t)) = reference.rsplit_once(':') {
                (repo.to_string(), t.to_string())
            } else {
                (reference.to_string(), "latest".to_string())
            };
            ("registry-1.docker.io".to_string(), format!("library/{}", repo), tag)
        };

        Ok(Self {
            registry,
            repository,
            tag,
        })
    }

    /// Base URL for the v2 API.
    pub fn base_url(&self, insecure: bool) -> String {
        let scheme = if insecure { "http" } else { "https" };
        format!("{}://{}/v2", scheme, self.registry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reference_parse_docker_hub_official() {
        let ref1 = Reference::parse("ubuntu:22.04").unwrap();
        assert_eq!(ref1.registry, "registry-1.docker.io");
        assert_eq!(ref1.repository, "library/ubuntu");
        assert_eq!(ref1.tag, "22.04");

        let ref2 = Reference::parse("nginx").unwrap();
        assert_eq!(ref2.registry, "registry-1.docker.io");
        assert_eq!(ref2.repository, "library/nginx");
        assert_eq!(ref2.tag, "latest");
    }

    #[test]
    fn test_reference_parse_docker_hub_library() {
        let ref1 = Reference::parse("library/ubuntu:22.04").unwrap();
        assert_eq!(ref1.registry, "registry-1.docker.io");
        assert_eq!(ref1.repository, "library/ubuntu");
        assert_eq!(ref1.tag, "22.04");
    }

    #[test]
    fn test_reference_parse_custom_registry() {
        let ref1 = Reference::parse("gcr.io/my-project/my-app:latest").unwrap();
        assert_eq!(ref1.registry, "gcr.io");
        assert_eq!(ref1.repository, "my-project/my-app");
        assert_eq!(ref1.tag, "latest");

        let ref2 = Reference::parse("localhost:5000/my-app:v1").unwrap();
        assert_eq!(ref2.registry, "localhost:5000");
        assert_eq!(ref2.repository, "my-app");
        assert_eq!(ref2.tag, "v1");
    }

    #[test]
    fn test_reference_base_url() {
        let ref1 = Reference::parse("gcr.io/my-app:latest").unwrap();
        assert_eq!(ref1.base_url(false), "https://gcr.io/v2");
        assert_eq!(ref1.base_url(true), "http://gcr.io/v2");
    }
}