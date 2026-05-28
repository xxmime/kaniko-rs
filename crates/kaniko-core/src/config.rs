//! Build configuration for kaniko-rs.
//!
//! Re-exports OCI image config types and provides kaniko-specific
//! build configuration structures.

// Re-export OCI image config types for convenience
pub use oci_image::config::{ContainerConfig, HealthConfig, HistoryEntry, ImageConfig, RootFs};

/// Dockerignore patterns for excluding files from the build context.
#[derive(Debug, Clone, Default)]
pub struct DockerIgnoreConfig {
    /// Patterns to exclude from the build context.
    pub patterns: Vec<String>,
}

/// Build context configuration.
#[derive(Debug, Clone)]
pub struct BuildContextConfig {
    /// The build context directory.
    pub context_dir: String,
    /// Dockerignore patterns.
    pub dockerignore: DockerIgnoreConfig,
    /// Path to the Dockerfile within the context.
    pub dockerfile_path: String,
}

impl Default for BuildContextConfig {
    fn default() -> Self {
        Self {
            context_dir: ".".to_string(),
            dockerignore: DockerIgnoreConfig::default(),
            dockerfile_path: "Dockerfile".to_string(),
        }
    }
}