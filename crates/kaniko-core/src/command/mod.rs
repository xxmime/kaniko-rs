//! Dockerfile command trait and implementations.
//!
//! Analogous to Go: `pkg/commands/commands.go` — `DockerCommand` interface.

use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use oci_image::layer::Layer;
use oci_image::mutate::MutableImage;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use thiserror::Error;

/// Errors during command execution.
#[derive(Debug, Error)]
pub enum CommandError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("walk error: {0}")]
    Walk(#[from] walkdir::Error),
    #[error("command failed: {0}")]
    Failed(String),
}

/// Result type for command operations.
pub type Result<T> = std::result::Result<T, CommandError>;

/// Build arguments passed to commands.
#[derive(Debug, Clone, Default)]
pub struct BuildArgs {
    /// ARG key-value pairs.
    pub args: Vec<(String, Option<String>)>,
    /// Resolved environment variables.
    pub env: Vec<(String, String)>,
    /// Build-time ARG overrides (--build-arg KEY=VALUE).
    pub build_args: HashMap<String, String>,
}

impl BuildArgs {
    pub fn new() -> Self {
        Self::default()
    }
}

/// Dockerfile command execution trait.
///
/// Analogous to Go: `commands.DockerCommand`.
#[async_trait]
pub trait DockerCommand: Send + Sync + fmt::Debug {
    /// Execute the command: modify filesystem + update image config.
    async fn execute(&self, config: &mut ContainerConfig, args: &BuildArgs) -> Result<()>;

    /// String representation of the command.
    fn command_string(&self) -> String;

    /// Files that need to be snapshotted after execution.
    fn files_to_snapshot(&self) -> Option<Vec<PathBuf>>;

    /// Whether this command can provide a list of files to snapshot.
    fn provides_files_to_snapshot(&self) -> bool;

    /// Return a cache-aware implementation of this command, if available.
    fn cache_command(&self, _cached_image: &MutableImage) -> Option<Box<dyn DockerCommand>> {
        None
    }

    /// Files used from the build context.
    fn files_used_from_context(
        &self,
        _config: &ContainerConfig,
        _args: &BuildArgs,
    ) -> Result<Vec<PathBuf>> {
        Ok(vec![])
    }

    /// Whether this command only modifies metadata (no filesystem changes).
    fn metadata_only(&self) -> bool;

    /// Whether this command requires an unpacked filesystem.
    fn requires_unpacked_fs(&self) -> bool;

    /// Whether the output layer should be cached.
    fn should_cache_output(&self) -> bool;

    /// Whether this command could delete files.
    fn should_detect_deleted_files(&self) -> bool;

    /// Whether cache key computation needs ARGs/ENVs.
    fn is_args_envs_required_in_cache(&self) -> bool;
}

/// Marker trait for cached commands.
pub trait CachedCommand: DockerCommand {
    fn layer(&self) -> Result<Layer>;
}

/// Composite cache key for layer caching.
#[derive(Debug, Clone)]
pub struct CompositeCache {
    base_image_digest: String,
    command_hash: String,
    files_hash: String,
    args_hash: String,
}

impl CompositeCache {
    /// Create a new composite cache key.
    pub fn new(base_image_digest: &str) -> Self {
        Self {
            base_image_digest: base_image_digest.to_string(),
            command_hash: String::new(),
            files_hash: String::new(),
            args_hash: String::new(),
        }
    }

    /// Update the cache key with new command information.
    pub fn update(
        self,
        command_str: &str,
        files: Vec<PathBuf>,
        args: &BuildArgs,
    ) -> Self {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // Hash the command string
        let mut command_hasher = DefaultHasher::new();
        command_str.hash(&mut command_hasher);
        let command_hash = command_hasher.finish().to_string();

        // Hash the files
        let mut files_hasher = DefaultHasher::new();
        for file in files {
            file.hash(&mut files_hasher);
        }
        let files_hash = files_hasher.finish().to_string();

        // Hash the build args
        let mut args_hasher = DefaultHasher::new();
        // Hash the build_args HashMap
        for (key, value) in &args.build_args {
            key.hash(&mut args_hasher);
            value.hash(&mut args_hasher);
        }
        // Hash the env Vec
        for (key, value) in &args.env {
            key.hash(&mut args_hasher);
            value.hash(&mut args_hasher);
        }
        let args_hash = args_hasher.finish().to_string();

        Self {
            base_image_digest: self.base_image_digest,
            command_hash,
            files_hash,
            args_hash,
        }
    }

    /// Generate the final cache key hash.
    pub fn hash(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        self.base_image_digest.hash(&mut hasher);
        self.command_hash.hash(&mut hasher);
        self.files_hash.hash(&mut hasher);
        self.args_hash.hash(&mut hasher);
        hasher.finish().to_string()
    }
}

// Sub-modules with individual command implementations.
mod env;
mod label;
mod expose;
mod user;
mod workdir;
mod copy;
mod add;
mod run;
mod cmd;
mod entrypoint;
mod volume;
mod arg;
mod shell;
mod stopsignal;
mod healthcheck;
mod onbuild;
mod base;
mod cache_command;
mod mount;
mod run_marker;

pub use base::BaseCommand;
pub use cache_command::{CachingCopyCommand, CachingRunCommand};
pub use mount::{MountSpec, MountType, NetworkMode, parse_mount, parse_network, apply_mount};
pub use run_marker::RunMarkerCommand;
pub use env::EnvCommand;
pub use label::LabelCommand;
pub use expose::ExposeCommand;
pub use user::UserCommand;
pub use workdir::WorkdirCommand;
pub use copy::CopyCommand;
pub use add::AddCommand;
pub use run::RunCommand;
pub use cmd::CmdCommand;
pub use entrypoint::EntrypointCommand;
pub use volume::VolumeCommand;
pub use arg::ArgCommand;
pub use shell::ShellCommand;
pub use stopsignal::StopSignalCommand;
pub use healthcheck::HealthCheckCommand;
pub use onbuild::OnBuildCommand;