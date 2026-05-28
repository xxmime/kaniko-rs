//! Stage builder for kaniko-rs.
//!
//! Orchestrates the build of a single Dockerfile stage.
//! Analogous to Go: `pkg/executor/build.go` — `stageBuilder`.

use crate::command::{BuildArgs, DockerCommand};
use kaniko_cache::layout::LayoutCache;
use kaniko_snapshot::snapshotter::Snapshotter;
use kaniko_snapshot::layered_map::LayeredMap;
use oci_image::mutate::MutableImage;
use std::path::PathBuf;
use thiserror::Error;

/// Errors during build operations.
#[derive(Debug, Error)]
pub enum BuildError {
    #[error("command error: {0}")]
    Command(#[from] crate::command::CommandError),
    #[error("snapshot error: {0}")]
    Snapshot(#[from] kaniko_snapshot::snapshotter::SnapshotError),
    #[error("layer error: {0}")]
    Layer(#[from] oci_image::layer::LayerError),
    #[error("mutation error: {0}")]
    Mutate(#[from] oci_image::mutate::MutateError),
    #[error("cache error: {0}")]
    Cache(String),
    #[error("build failed: {0}")]
    Failed(String),
    #[error("circular dependency detected: {0}")]
    CycleDetected(String),
    #[error("invalid stage reference: {0}")]
    InvalidStageReference(String),
}

/// Result type for build operations.
pub type Result<T> = std::result::Result<T, BuildError>;

/// Build options for the stage builder.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Whether to use layer caching.
    pub cache: bool,
    /// Cache directory for local layout cache.
    pub cache_dir: Option<String>,
    /// Whether to use single snapshot mode.
    pub single_snapshot: bool,
    /// Whether to force build metadata.
    pub force_build_metadata: bool,
    /// Snapshot mode: "full" or "redo".
    pub snapshot_mode: String,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            cache: false,
            cache_dir: None,
            single_snapshot: false,
            force_build_metadata: false,
            snapshot_mode: "full".to_string(),
        }
    }
}

/// Stage builder — orchestrates building a single Dockerfile stage.
///
/// This is the central orchestrator for building a single Dockerfile stage.
/// It manages the lifecycle of:
/// 1. Executing commands
/// 2. Taking filesystem snapshots
/// 3. Creating OCI layers
/// 4. Caching layers
///
/// Analogous to Go: `pkg/executor/build.go` — `stageBuilder`.
pub struct StageBuilder {
    /// The image being built.
    image: MutableImage,
    /// Build arguments.
    args: BuildArgs,
    /// Commands to execute.
    commands: Vec<Box<dyn DockerCommand>>,
    /// Root directory for file system operations.
    root_dir: PathBuf,
    /// Build options.
    opts: BuildOptions,
    /// Base image digest for cache key computation.
    base_image_digest: String,
}

impl StageBuilder {
    /// Create a new stage builder.
    pub fn new(
        image: MutableImage,
        commands: Vec<Box<dyn DockerCommand>>,
        root_dir: PathBuf,
    ) -> Self {
        let base_image_digest = image.digest().to_string();
        Self {
            image,
            args: BuildArgs::new(),
            commands,
            root_dir,
            opts: BuildOptions::default(),
            base_image_digest,
        }
    }

    /// Set build options.
    pub fn with_opts(mut self, opts: BuildOptions) -> Self {
        self.opts = opts;
        self
    }

    /// Set build arguments.
    pub fn with_args(mut self, args: BuildArgs) -> Self {
        self.args = args;
        self
    }

    /// Build the stage.
    ///
    /// Executes all commands in sequence, taking snapshots and appending layers.
    /// Analogous to Go: `stageBuilder.build()`.
    pub async fn build(&mut self) -> Result<()> {
        tracing::info!("Building stage...");

        // Initialize snapshotter
        let layered_map = LayeredMap::new();
        let mut snapshotter = Snapshotter::new(layered_map, self.root_dir.clone());

        // Initialize the snapshotter with the current filesystem state
        if !self.opts.single_snapshot {
            snapshotter.init()?;
        }

        // Initialize composite cache key
        let mut composite_key = crate::command::CompositeCache::new(&self.base_image_digest);

        // Initialize local cache if enabled
        let layout_cache = if self.opts.cache {
            if let Some(ref cache_dir) = self.opts.cache_dir {
                let cache = LayoutCache::new(cache_dir);
                if let Err(e) = cache.init() {
                    tracing::warn!("Failed to initialize cache: {}", e);
                    None
                } else {
                    Some(cache)
                }
            } else {
                None
            }
        } else {
            None
        };

        let mut init_snapshot_taken = false;

        for (index, command) in self.commands.iter().enumerate() {
            let cmd_str = command.command_string();
            tracing::info!("[{}/{}] {}", index + 1, self.commands.len(), cmd_str);

            // Update composite cache key
            if self.opts.cache {
                composite_key = composite_key.update(
                    &cmd_str,
                    command.files_to_snapshot().unwrap_or_default(),
                    &self.args,
                );

                // Try cache hit
                if let Some(ref cache) = layout_cache {
                    let cache_key = composite_key.hash();
                    if cache.exists(&cache_key) {
                        tracing::info!("Cache hit for: {}", cmd_str);
                        match cache.retrieve_layer(&cache_key) {
                            Ok(cached_image) => {
                                // Use cached layers
                                for layer in &cached_image.layers {
                                    self.image = oci_image::mutate::append_layer(
                                        self.image.clone(),
                                        layer.clone(),
                                    )?;
                                }
                                tracing::debug!("Applied {} cached layers", cached_image.layers.len());
                                continue;
                            }
                            Err(e) => {
                                tracing::debug!("Cache retrieve failed: {}", e);
                            }
                        }
                    }
                }
            }

            // Take initial snapshot if not yet taken
            if !init_snapshot_taken
                && !command.provides_files_to_snapshot()
                && !command.metadata_only()
            {
                snapshotter.init()?;
                init_snapshot_taken = true;
            }

            // Execute the command
            command
                .execute(&mut self.image.config.config, &self.args)
                .await?;

            // Skip snapshot for metadata-only commands (unless forced)
            if command.metadata_only() && !self.opts.force_build_metadata {
                tracing::debug!(
                    "Skipping snapshot for metadata-only command: {}",
                    cmd_str
                );
                continue;
            }

            // Take snapshot
            if self.opts.single_snapshot {
                // Defer snapshot to the end
                continue;
            }

            let layer = if let Some(files) = command.files_to_snapshot() {
                if !files.is_empty() || self.opts.force_build_metadata {
                    snapshotter.take_snapshot(
                        &files,
                        command.should_detect_deleted_files(),
                        self.opts.force_build_metadata,
                    )?
                } else {
                    None
                }
            } else if command.should_detect_deleted_files() {
                // Full filesystem snapshot needed (e.g., after RUN)
                Some(snapshotter.take_snapshot_fs()?)
            } else {
                // Metadata-only or no files to snapshot
                if self.opts.force_build_metadata {
                    let layer = oci_image::layer::Layer::empty()?;
                    Some(layer)
                } else {
                    None
                }
            };

            // Append layer to image
            if let Some(layer) = layer {
                self.image = oci_image::mutate::append_layer(self.image.clone(), layer)?;

                // Cache the layer if caching is enabled
                if self.opts.cache {
                    if let Some(ref cache) = layout_cache {
                        let cache_key = composite_key.hash();
                        if let Err(e) = cache.push_layer(&cache_key, &self.image) {
                            tracing::warn!("Failed to cache layer: {}", e);
                        }
                    }
                }

                tracing::debug!(
                    "Appended layer {} for command: {}",
                    self.image.layer_count(),
                    cmd_str
                );
            }
        }

        // Single snapshot mode: take one final snapshot
        if self.opts.single_snapshot {
            let layer = snapshotter.take_snapshot_fs()?;
            self.image = oci_image::mutate::append_layer(self.image.clone(), layer)?;
        }

        tracing::info!("Stage build complete with {} layers", self.image.layer_count());
        Ok(())
    }

    /// Get the built image.
    pub fn into_image(self) -> MutableImage {
        self.image
    }

    /// Get a reference to the image.
    pub fn image(&self) -> &MutableImage {
        &self.image
    }
}