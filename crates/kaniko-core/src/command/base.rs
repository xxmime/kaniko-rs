//! Base command with default trait method implementations.

use crate::command::{BuildArgs, DockerCommand, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use oci_image::mutate::MutableImage;
use std::path::PathBuf;

/// Base command providing default implementations for DockerCommand trait.
///
/// Most commands only need to override `execute` and `command_string`.
/// Metadata-only commands (ENV, LABEL, etc.) can use defaults as-is.
#[async_trait]
pub trait BaseCommand: Send + Sync + std::fmt::Debug {
    /// Execute the command logic.
    async fn execute_impl(&self, config: &mut ContainerConfig, args: &BuildArgs) -> Result<()>;

    /// String representation.
    fn command_string_impl(&self) -> String;

    /// Whether this command only modifies metadata.
    fn metadata_only_impl(&self) -> bool {
        true
    }

    /// Whether this command requires an unpacked filesystem.
    fn requires_unpacked_fs_impl(&self) -> bool {
        false
    }

    /// Whether the output layer should be cached.
    fn should_cache_output_impl(&self) -> bool {
        false
    }

    /// Whether this command could delete files.
    fn should_detect_deleted_files_impl(&self) -> bool {
        false
    }

    /// Whether cache key needs ARGs/ENVs.
    fn is_args_envs_required_in_cache_impl(&self) -> bool {
        false
    }

    /// Files to snapshot after execution.
    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        None
    }

    /// Whether this command provides files to snapshot.
    fn provides_files_to_snapshot_impl(&self) -> bool {
        self.metadata_only_impl()
    }

    /// Files used from the build context.
    /// Analogous to Go: `DockerCommand.FilesUsedFromContext`.
    fn files_used_from_context_impl(
        &self,
        _config: &ContainerConfig,
        _args: &BuildArgs,
    ) -> Result<Vec<PathBuf>> {
        Ok(vec![])
    }

    /// Return a cache-aware implementation of this command, if available.
    /// Analogous to Go: `DockerCommand.CacheCommand`.
    fn cache_command_impl(&self, _cached_image: &MutableImage) -> Option<Box<dyn DockerCommand>> {
        None
    }
}

/// Blanket implementation of DockerCommand for any type implementing BaseCommand.
#[async_trait]
impl<T: BaseCommand + 'static> DockerCommand for T {
    async fn execute(&self, config: &mut ContainerConfig, args: &BuildArgs) -> Result<()> {
        self.execute_impl(config, args).await
    }

    fn command_string(&self) -> String {
        self.command_string_impl()
    }

    fn files_to_snapshot(&self) -> Option<Vec<PathBuf>> {
        self.files_to_snapshot_impl()
    }

    fn provides_files_to_snapshot(&self) -> bool {
        self.provides_files_to_snapshot_impl()
    }

    fn cache_command(&self, cached_image: &MutableImage) -> Option<Box<dyn DockerCommand>> {
        self.cache_command_impl(cached_image)
    }

    fn files_used_from_context(
        &self,
        config: &ContainerConfig,
        args: &BuildArgs,
    ) -> Result<Vec<PathBuf>> {
        self.files_used_from_context_impl(config, args)
    }

    fn metadata_only(&self) -> bool {
        self.metadata_only_impl()
    }

    fn requires_unpacked_fs(&self) -> bool {
        self.requires_unpacked_fs_impl()
    }

    fn should_cache_output(&self) -> bool {
        self.should_cache_output_impl()
    }

    fn should_detect_deleted_files(&self) -> bool {
        self.should_detect_deleted_files_impl()
    }

    fn is_args_envs_required_in_cache(&self) -> bool {
        self.is_args_envs_required_in_cache_impl()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}