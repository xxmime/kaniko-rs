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
    /// Snapshot mode: "full", "redo", or "time".
    pub snapshot_mode: String,
    /// Whether to cache COPY layers.
    pub cache_copy_layers: bool,
    /// Whether to cache RUN layers.
    pub cache_run_layers: bool,
    /// Whether to use RunV2 (RunMarkerCommand).
    pub run_v2: bool,
    /// Compression algorithm for layers.
    pub compression: Option<String>,
    /// Compression level (0-9).
    pub compression_level: u32,
    /// Whether to use compressed caching.
    pub compressed_caching: bool,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            cache: false,
            cache_dir: None,
            single_snapshot: false,
            force_build_metadata: false,
            snapshot_mode: "full".to_string(),
            cache_copy_layers: false,
            cache_run_layers: false,
            run_v2: false,
            compression: None,
            compression_level: 0,
            compressed_caching: false,
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

        // Resolve ONBUILD triggers from the base image.
        // When a Dockerfile uses `FROM base_image`, any ONBUILD triggers
        // registered in the base image must be prepended to the command list.
        // Analogous to Go: `executor.resolveOnBuild()`.
        self.resolve_onbuild_triggers();

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

            // Determine if we should take a snapshot.
            // Analogous to Go: `stageBuilder.shouldTakeSnapshot(index, isMetadataCmd)`.
            let is_last_command = index == self.commands.len() - 1;
            if !self.should_take_snapshot(index, is_last_command, command.metadata_only()) {
                tracing::debug!("Skipping snapshot for: {}", cmd_str);
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

    /// Resolve ONBUILD triggers from the base image.
    ///
    /// When a Dockerfile uses `FROM base_image`, any ONBUILD triggers
    /// registered in the base image's config must be prepended to the
    /// current stage's command list.
    ///
    /// Analogous to Go: `executor.resolveOnBuild()`.
    fn resolve_onbuild_triggers(&mut self) {
        if let Some(ref onbuild_triggers) = self.image.config.config.on_build {
            if onbuild_triggers.is_empty() {
                return;
            }

            tracing::info!("Resolving {} ONBUILD trigger(s) from base image", onbuild_triggers.len());

            // Parse each ONBUILD trigger string into a DockerCommand
            // and prepend them to the command list.
            // ONBUILD triggers are strings like "RUN pip install -r requirements.txt"
            let mut trigger_commands: Vec<Box<dyn DockerCommand>> = Vec::new();
            for trigger in onbuild_triggers {
                if let Some(cmd) = parse_trigger_to_command(trigger) {
                    tracing::info!("  ONBUILD trigger: {}", trigger);
                    trigger_commands.push(cmd);
                } else {
                    tracing::warn!("  Could not parse ONBUILD trigger: {}", trigger);
                }
            }

            // Prepend triggers before existing commands
            let mut new_commands = trigger_commands;
            new_commands.append(&mut self.commands);
            self.commands = new_commands;

            // Clear the on_build field so triggers don't propagate further
            self.image.config.config.on_build = None;
        }
    }

    /// Initialize the image config with default values.
    ///
    /// Analogous to Go: `executor.initConfig()`.
    /// Sets default environment variables and applies CLI labels.
    pub fn init_config(&mut self, labels: &[(String, String)]) {
        // Set default environment variables if not present
        let default_env = vec![
            ("PATH".to_string(), "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string()),
        ];

        for (key, value) in default_env {
            if self.image.config.config.get_env(&key).is_none() {
                self.image.config.config.set_env(&key, &value);
            }
        }

        // Apply CLI labels
        if !labels.is_empty() {
            let label_map = self.image.config.config.labels.get_or_insert_with(Default::default);
            for (key, value) in labels {
                label_map.insert(key.clone(), value.clone());
            }
        }
    }

    /// Determine whether a snapshot should be taken after a command.
    ///
    /// Analogous to Go: `stageBuilder.shouldTakeSnapshot(index, isMetadataCmd)`.
    ///
    /// Rules:
    /// - In single snapshot mode, only snapshot the very last command.
    /// - If caching is enabled, always take snapshots.
    /// - Skip metadata-only commands unless it's the last command.
    fn should_take_snapshot(&self, index: usize, is_last_command: bool, is_metadata_cmd: bool) -> bool {
        if self.opts.single_snapshot {
            return is_last_command;
        }

        // Always take snapshots if caching is enabled
        if self.opts.cache {
            return true;
        }

        // Skip metadata-only commands (unless it's the last command or forced)
        !is_metadata_cmd
    }
}

/// Parse an ONBUILD trigger string into a DockerCommand.
///
/// ONBUILD triggers are stored as strings like "RUN pip install -r requirements.txt"
/// or "COPY . /app". This function parses them back into executable commands.
fn parse_trigger_to_command(trigger: &str) -> Option<Box<dyn DockerCommand>> {
    let trigger = trigger.trim();
    let (directive, rest) = trigger.split_once(' ')?;
    let directive = directive.to_uppercase();

    match directive.as_str() {
        "RUN" => {
            let cmd = crate::command::RunCommand::new_shell(rest.to_string(), false);
            Some(Box::new(cmd))
        }
        "COPY" => {
            let cmd = crate::command::CopyCommand::new(
                rest.split_whitespace().map(String::from).collect(),
                String::new(), // Will be set properly during execution
                None,
                std::path::PathBuf::from("."),
                false,
            );
            Some(Box::new(cmd))
        }
        "ADD" => {
            let cmd = crate::command::AddCommand::new(
                rest.split_whitespace().map(String::from).collect(),
                String::new(),
                std::path::PathBuf::from("."),
                false,
            );
            Some(Box::new(cmd))
        }
        "ENV" => {
            if let Some((key, value)) = rest.split_once('=') {
                let cmd = crate::command::EnvCommand::new(key.to_string(), value.to_string());
                Some(Box::new(cmd))
            } else if let Some((key, value)) = rest.split_once(' ') {
                let cmd = crate::command::EnvCommand::new(key.to_string(), value.to_string());
                Some(Box::new(cmd))
            } else {
                None
            }
        }
        "LABEL" => {
            // Simplified: treat the rest as a single label
            if let Some((key, value)) = rest.split_once('=') {
                let cmd = crate::command::LabelCommand::new(vec![(key.to_string(), value.to_string())]);
                Some(Box::new(cmd))
            } else {
                None
            }
        }
        "WORKDIR" => {
            let cmd = crate::command::WorkdirCommand::new(rest.to_string());
            Some(Box::new(cmd))
        }
        "USER" => {
            let cmd = crate::command::UserCommand::new(rest.to_string());
            Some(Box::new(cmd))
        }
        "EXPOSE" => {
            let ports = rest.split_whitespace().map(String::from).collect();
            let cmd = crate::command::ExposeCommand::new(ports);
            Some(Box::new(cmd))
        }
        "VOLUME" => {
            let paths = rest.split_whitespace().map(String::from).collect();
            let cmd = crate::command::VolumeCommand::new(paths);
            Some(Box::new(cmd))
        }
        "ARG" => {
            let (name, default) = if let Some((n, v)) = rest.split_once('=') {
                (n.to_string(), Some(v.to_string()))
            } else {
                (rest.to_string(), None)
            };
            let cmd = crate::command::ArgCommand::new(name, default);
            Some(Box::new(cmd))
        }
        "CMD" => {
            let cmd = crate::command::CmdCommand::new_shell(rest.to_string());
            Some(Box::new(cmd))
        }
        "ENTRYPOINT" => {
            let cmd = crate::command::EntrypointCommand::new_shell(rest.to_string());
            Some(Box::new(cmd))
        }
        "SHELL" => {
            let shell = rest.split_whitespace().map(String::from).collect();
            let cmd = crate::command::ShellCommand::new(shell);
            Some(Box::new(cmd))
        }
        "STOPSIGNAL" => {
            let cmd = crate::command::StopSignalCommand::new(rest.to_string());
            Some(Box::new(cmd))
        }
        "ONBUILD" => {
            // Nested ONBUILD is not allowed by Docker
            tracing::warn!("Nested ONBUILD is not allowed, skipping: {}", trigger);
            None
        }
        _ => {
            tracing::warn!("Unknown ONBUILD directive: {}", directive);
            None
        }
    }
}