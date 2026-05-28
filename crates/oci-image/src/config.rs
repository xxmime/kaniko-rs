//! OCI Image Config data model.
//!
//! Implements the OCI Image Configuration specification:
//! <https://github.com/opencontainers/image-spec/blob/main/config.md>
//!
//! Analogous to `go-containerregistry/pkg/v1.ConfigFile`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// OCI Image Configuration.
///
/// The configuration object for a container image, including
/// the rootfs, history, and container runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfig {
    /// An combined date and time at which the image was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,

    /// The author of the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    /// The CPU architecture.
    #[serde(default = "default_architecture")]
    pub architecture: String,

    /// The operating system.
    #[serde(default = "default_os")]
    pub os: String,

    /// The container runtime configuration.
    #[serde(default)]
    pub config: ContainerConfig,

    /// The rootfs section describes the layer content.
    #[serde(default)]
    pub rootfs: RootFs,

    /// The history of each layer.
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
}

fn default_architecture() -> String {
    "amd64".to_string()
}

fn default_os() -> String {
    "linux".to_string()
}

impl ImageConfig {
    /// Create a new scratch (empty) image config.
    ///
    /// Analogous to `go-containerregistry/pkg/v1/empty.Image`.
    pub fn scratch() -> Self {
        Self {
            created: Some(chrono::Utc::now().to_rfc3339()),
            author: None,
            architecture: default_architecture(),
            os: default_os(),
            config: ContainerConfig::default(),
            rootfs: RootFs {
                r#type: "layers".to_string(),
                diff_ids: vec![],
            },
            history: vec![],
        }
    }

    /// Get the diff IDs of all layers.
    pub fn diff_ids(&self) -> &[String] {
        &self.rootfs.diff_ids
    }

    /// Get the number of layers.
    pub fn layer_count(&self) -> usize {
        self.rootfs.diff_ids.len()
    }
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self::scratch()
    }
}

/// Container runtime configuration.
///
/// This section specifies the execution parameters which should be used
/// as a base when running a container using this image.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct ContainerConfig {
    /// The user to run the container as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// Ports to expose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposed_ports: Option<BTreeMap<String, ()>>,

    /// Environment variables in KEY=VALUE format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<Vec<String>>,

    /// The entrypoint for the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,

    /// Default arguments to the entrypoint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd: Option<Vec<String>>,

    /// Volumes to create.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volumes: Option<BTreeMap<String, ()>>,

    /// Working directory inside the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,

    /// Labels attached to the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<BTreeMap<String, String>>,

    /// Stop signal for the container.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,

    /// Healthcheck configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthConfig>,

    /// Default shell for RUN commands.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<Vec<String>>,

    /// Onbuild triggers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_build: Option<Vec<String>>,

    /// Args passed during build (for ARG substitution).
    #[serde(skip_serializing)]
    pub args: Option<BTreeMap<String, String>>,
}

impl ContainerConfig {
    /// Set an environment variable.
    ///
    /// If the variable already exists, it is replaced.
    /// Otherwise, it is appended.
    pub fn set_env(&mut self, key: &str, value: &str) {
        let entry = format!("{}={}", key, value);
        if let Some(env) = &mut self.env {
            // Check if key already exists
            let prefix = format!("{}=", key);
            if let Some(pos) = env.iter().position(|e| e.starts_with(&prefix)) {
                env[pos] = entry;
            } else {
                env.push(entry);
            }
        } else {
            self.env = Some(vec![entry]);
        }
    }

    /// Get an environment variable value.
    pub fn get_env(&self, key: &str) -> Option<String> {
        let prefix = format!("{}=", key);
        self.env.as_ref()?.iter()
            .find(|e| e.starts_with(&prefix))
            .map(|e| e[prefix.len()..].to_string())
    }

    /// Add an exposed port.
    pub fn add_exposed_port(&mut self, port: String) {
        let ports = self.exposed_ports.get_or_insert_with(BTreeMap::new);
        ports.insert(port, ());
    }

    /// Set a label.
    pub fn set_label(&mut self, key: &str, value: &str) {
        let labels = self.labels.get_or_insert_with(BTreeMap::new);
        labels.insert(key.to_string(), value.to_string());
    }

    /// Add a volume.
    pub fn add_volume(&mut self, path: String) {
        let volumes = self.volumes.get_or_insert_with(BTreeMap::new);
        volumes.insert(path, ());
    }

    /// Set a build argument.
    pub fn set_arg(&mut self, key: &str, value: &str) {
        let args = self.args.get_or_insert_with(BTreeMap::new);
        args.insert(key.to_string(), value.to_string());
    }

    /// Get a build argument value.
    pub fn get_arg(&self, key: &str) -> Option<String> {
        self.args.as_ref()?.get(key).cloned()
    }
}

/// Health configuration for the container.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HealthConfig {
    /// The test to perform.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<Vec<String>>,

    /// Time between health checks (nanoseconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>,

    /// Timeout for each health check (nanoseconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,

    /// Number of consecutive failures before marking unhealthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,

    /// Start period for the container to bootstrap (nanoseconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_period: Option<u64>,
}

/// RootFS section of the image configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RootFs {
    /// The type of the rootfs (always "layers").
    pub r#type: String,

    /// An array of layer diff_ids (SHA-256 of uncompressed layer).
    #[serde(default)]
    pub diff_ids: Vec<String>,
}

impl Default for RootFs {
    fn default() -> Self {
        Self {
            r#type: "layers".to_string(),
            diff_ids: vec![],
        }
    }
}

/// A history entry describing a layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// The creation time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<String>,

    /// The author of the layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    /// The command that created the layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    /// A comment for the layer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,

    /// Whether this is an empty (non-layer-producing) entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub empty_layer: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_image_config_scratch() {
        let config = ImageConfig::scratch();
        assert_eq!(config.architecture, "amd64");
        assert_eq!(config.os, "linux");
        assert!(config.rootfs.diff_ids.is_empty());
        assert!(config.history.is_empty());
    }

    #[test]
    fn test_container_config_set_env() {
        let mut config = ContainerConfig::default();
        config.set_env("FOO", "bar");
        assert_eq!(config.get_env("FOO"), Some("bar".to_string()));

        // Override
        config.set_env("FOO", "baz");
        assert_eq!(config.get_env("FOO"), Some("baz".to_string()));
        assert_eq!(config.env.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn test_container_config_multiple_envs() {
        let mut config = ContainerConfig::default();
        config.set_env("A", "1");
        config.set_env("B", "2");
        assert_eq!(config.env.as_ref().unwrap().len(), 2);
        assert_eq!(config.get_env("A"), Some("1".to_string()));
        assert_eq!(config.get_env("B"), Some("2".to_string()));
    }

    #[test]
    fn test_container_config_add_exposed_port() {
        let mut config = ContainerConfig::default();
        config.add_exposed_port("8080/tcp".to_string());
        assert!(config.exposed_ports.as_ref().unwrap().contains_key("8080/tcp"));
    }

    #[test]
    fn test_container_config_set_label() {
        let mut config = ContainerConfig::default();
        config.set_label("version", "1.0");
        assert_eq!(
            config.labels.as_ref().unwrap().get("version"),
            Some(&"1.0".to_string())
        );
    }

    #[test]
    fn test_container_config_add_volume() {
        let mut config = ContainerConfig::default();
        config.add_volume("/data".to_string());
        assert!(config.volumes.as_ref().unwrap().contains_key("/data"));
    }

    #[test]
    fn test_image_config_serde_roundtrip() {
        let config = ImageConfig::scratch();
        let json = serde_json::to_string(&config).unwrap();
        let deserialized: ImageConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_history_entry() {
        let entry = HistoryEntry {
            created: Some("2025-01-01T00:00:00Z".to_string()),
            created_by: Some("/bin/sh -c echo hello".to_string()),
            empty_layer: Some(false),
            ..Default::default()
        };
        let json = serde_json::to_string(&entry).unwrap();
        let deserialized: HistoryEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, deserialized);
    }

    #[test]
    fn test_root_fs_default() {
        let rootfs = RootFs::default();
        assert_eq!(rootfs.r#type, "layers");
        assert!(rootfs.diff_ids.is_empty());
    }

    #[test]
    fn test_container_config_set_arg() {
        let mut config = ContainerConfig::default();
        config.set_arg("VERSION", "1.0");
        assert_eq!(config.get_arg("VERSION"), Some("1.0".to_string()));
    }
}