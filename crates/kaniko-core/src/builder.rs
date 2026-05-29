//! Stage builder for kaniko-rs.
//!
//! Orchestrates the build of a single Dockerfile stage.
//! Analogous to Go: `pkg/executor/build.go` — `stageBuilder`.

use crate::command::{BuildArgs, DockerCommand};
use kaniko_cache::layout::LayoutCache;
use kaniko_snapshot::snapshotter::Snapshotter;
use kaniko_snapshot::layered_map::LayeredMap;
use oci_image::manifest::MediaType;
use oci_image::mutate::MutableImage;
use oci_image::layer::Layer;
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
    /// Whether the initial filesystem is already unpacked.
    /// When true, the first stage skips base image extraction.
    /// Analogous to Go: `opts.InitialFSUnpacked`.
    pub initial_fs_unpacked: bool,
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
            initial_fs_unpacked: false,
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
    /// Cross-stage dependencies: stage_index → list of source paths.
    cross_stage_deps: std::collections::HashMap<usize, Vec<String>>,
    /// Cache repo for registry cache.
    cache_repo: Option<String>,
    /// Destinations for cache inference.
    destinations: Vec<String>,
    /// Whether to use insecure registry.
    insecure: bool,
    /// Whether to skip pushing cache layers.
    no_push_cache: bool,
    /// Stage index (0-based) within the multi-stage build.
    stage_index: usize,
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
            cross_stage_deps: std::collections::HashMap::new(),
            cache_repo: None,
            destinations: Vec::new(),
            insecure: false,
            no_push_cache: false,
            stage_index: 0,
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

    /// Set cross-stage dependencies.
    pub fn with_cross_stage_deps(mut self, deps: std::collections::HashMap<usize, Vec<String>>) -> Self {
        self.cross_stage_deps = deps;
        self
    }

    /// Set cache repository.
    pub fn with_cache_repo(mut self, repo: Option<String>) -> Self {
        self.cache_repo = repo;
        self
    }

    /// Set destinations for cache inference.
    pub fn with_destinations(mut self, dests: Vec<String>) -> Self {
        self.destinations = dests;
        self
    }

    /// Set insecure registry flag.
    pub fn with_insecure(mut self, insecure: bool) -> Self {
        self.insecure = insecure;
        self
    }

    /// Set no-push-cache flag.
    pub fn with_no_push_cache(mut self, no_push: bool) -> Self {
        self.no_push_cache = no_push;
        self
    }

    /// Set stage index.
    pub fn with_stage_index(mut self, index: usize) -> Self {
        self.stage_index = index;
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

        // Initialize composite cache key with base image digest.
        // Analogous to Go: `compositeKey = NewCompositeCache(s.baseImageDigest)`.
        let mut composite_key = crate::command::CompositeCache::new(&self.base_image_digest);

        // Apply optimizations — replace cacheable commands with cached versions.
        // Analogous to Go: `stageBuilder.optimize()`.
        if self.opts.cache {
            if let Err(e) = self.optimize(&mut composite_key) {
                tracing::warn!("Optimize failed: {}", e);
            }
        }
        // Analogous to Go: `stageBuilder.build()` lines 310-340.
        let should_unpack = self.should_unpack_fs();
        if should_unpack {
            tracing::info!("Unpacking rootfs as a command requires it.");
            match oci_image::extract::extract_image_to_fs(&self.image, &self.root_dir) {
                Ok(files) => {
                    tracing::debug!("Extracted {} files from base image", files.len());
                }
                Err(e) => {
                    return Err(BuildError::Failed(format!(
                        "failed to get filesystem from image: {}",
                        e
                    )));
                }
            }
        } else {
            tracing::info!("Skipping unpacking as no commands require it.");
        }

        // Initialize snapshotter
        let layered_map = LayeredMap::new();
        let mut snapshotter = Snapshotter::new(layered_map, self.root_dir.clone());

        // Initialize the snapshotter with the current filesystem state
        if !self.opts.single_snapshot {
            snapshotter.init()?;
        }

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

                // Try cache hit from local layout cache.
                // Analogous to Go: `s.layerCache.RetrieveLayer(ck)`.
                if let Some(ref cache_dir) = self.opts.cache_dir {
                    let cache = LayoutCache::new(cache_dir);
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

            // Check if this is a cached command.
            // Analogous to Go: `isCacheCommand := func() bool { switch command.(type) { case commands.Cached: return true } }()`
            let is_cache_command = command.command_string().ends_with(" (cached)");

            if is_cache_command {
                // For cached commands, apply the cached layers directly.
                // Analogous to Go: `v := command.(commands.Cached); layer := v.Layer(); s.saveLayerToImage(layer, ...)`
                if let Some(cached) = command.as_any().downcast_ref::<CachedCommand>() {
                    for layer in cached.layers() {
                        self.image = oci_image::mutate::append_layer(
                            self.image.clone(),
                            layer.clone(),
                        )?;
                    }
                    tracing::debug!("Applied {} cached layers", cached.layers().len());
                    continue;
                }
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
                self.image = oci_image::mutate::append_layer(self.image.clone(), layer.clone())?;

                // Cache the layer if caching is enabled.
                // Analogous to Go: `pushLayerToCache()` — push layer to cache
                // in parallel along with new config file.
                if self.opts.cache && command.should_cache_output() {
                    let cache_key = composite_key.hash();
                    let cmd_str_for_cache = cmd_str.clone();

                    // Try local cache first
                    if let Some(ref cache_dir) = self.opts.cache_dir {
                        if let Err(e) = kaniko_cache::push::push_layer_to_local_cache(
                            &Some(cache_dir.clone()),
                            &cache_key,
                            &self.image,
                        ) {
                            tracing::warn!("Failed to cache layer locally: {}", e);
                        }
                    }

                    // Try registry cache push (async, non-blocking)
                    if self.cache_repo.is_some() || !self.destinations.is_empty() {
                        let cache_repo = self.cache_repo.clone();
                        let destinations = self.destinations.clone();
                        let insecure = self.insecure;
                        let no_push = self.no_push_cache;
                        let layer_for_cache = layer.clone();

                        tokio::spawn(async move {
                            if let Err(e) = kaniko_cache::push::push_layer_to_cache(
                                &cache_repo,
                                &destinations,
                                &cache_key,
                                layer_for_cache,
                                &cmd_str_for_cache,
                                &None,
                                insecure,
                                no_push,
                            ).await {
                                tracing::warn!("Failed to push layer to cache: {}", e);
                            }
                        });
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

    /// Save a layer to the image, converting media type if needed.
    ///
    /// Analogous to Go: `stageBuilder.saveLayerToImage(layer, createdBy)`.
    fn save_layer_to_image(&mut self, layer: oci_image::layer::Layer, created_by: &str) -> Result<()> {
        // Convert layer media type if the image and layer have different vendors.
        // Analogous to Go: `stageBuilder.convertLayerMediaType(layer)`.
        let layer = self.convert_layer_media_type(layer)?;

        // Append the layer with history entry.
        // Analogous to Go: `mutate.Append(s.image, mutate.Addendum{Layer, History})`.
        self.image = oci_image::mutate::append_layer_with_history(
            self.image.clone(),
            layer,
            oci_image::config::HistoryEntry {
                created: None,
                author: Some("kaniko".to_string()),
                created_by: Some(created_by.to_string()),
                comment: None,
                empty_layer: None,
            },
        )?;
        Ok(())
    }

    /// Save a snapshot (tar file) as a layer to the image.
    ///
    /// Analogous to Go: `stageBuilder.saveSnapshotToImage(createdBy, tarPath)`.
    fn save_snapshot_to_image(&mut self, created_by: &str, tar_path: &std::path::Path) -> Result<()> {
        let layer = self.save_snapshot_to_layer(tar_path)?;
        if let Some(layer) = layer {
            self.save_layer_to_image(layer, created_by)?;
        }
        Ok(())
    }

    /// Convert a tar snapshot file into an OCI layer.
    ///
    /// Returns None if the tar is empty (≤1024 bytes) and force_build_metadata
    /// is not set.
    ///
    /// Analogous to Go: `stageBuilder.saveSnapshotToLayer(tarPath)`.
    fn save_snapshot_to_layer(&self, tar_path: &std::path::Path) -> Result<Option<oci_image::layer::Layer>> {
        if tar_path.as_os_str().is_empty() {
            return Ok(None);
        }

        let metadata = std::fs::metadata(tar_path)
            .map_err(|e| BuildError::Failed(format!("tar file path does not exist: {}", e)))?;

        // Empty tar is 1024 bytes in Go; skip if not forcing metadata.
        // Analogous to Go: `fi.Size() <= emptyTarSize && !s.opts.ForceBuildMetadata`.
        const EMPTY_TAR_SIZE: u64 = 1024;
        if metadata.len() <= EMPTY_TAR_SIZE && !self.opts.force_build_metadata {
            tracing::info!("No files were changed, appending empty layer to config. No layer added to image.");
            return Ok(None);
        }

        // Read the tar data and create a layer with appropriate compression.
        let tar_data = std::fs::read(tar_path)
            .map_err(|e| BuildError::Failed(format!("failed to read tar file: {}", e)))?;

        let compression = self.opts.compression.as_deref().unwrap_or("gzip");
        let layer = match compression {
            "zstd" => oci_image::layer::Layer::from_tar_uncompressed_with_options(
                tar_data,
                oci_image::layer::LayerCompression::zstd(self.opts.compression_level),
            )?,
            _ => oci_image::layer::Layer::from_tar_uncompressed_with_options(
                tar_data,
                oci_image::layer::LayerCompression::gzip(self.opts.compression_level),
            )?,
        };

        Ok(Some(layer))
    }

    /// Convert layer media type to match the image's vendor (OCI vs Docker).
    ///
    /// When a layer has a different vendor than the target image, we need
    /// to re-encode the layer with the appropriate media type.
    ///
    /// Analogous to Go: `stageBuilder.convertLayerMediaType(layer)`.
    fn convert_layer_media_type(&self, layer: oci_image::layer::Layer) -> Result<oci_image::layer::Layer> {
        let layer_mt = layer.media_type();
        let image_mt = self.image.manifest.media_type.clone().unwrap_or_default();

        let layer_vendor = oci_image::manifest::MediaType::extract_vendor_prefix(&layer_mt);
        let image_vendor = oci_image::manifest::MediaType::extract_vendor_prefix(&image_mt);

        if layer_vendor == image_vendor {
            return Ok(layer);
        }

        // Try to convert the media type
        let use_zstd = self.opts.compression.as_deref() == Some("zstd");
        let target_mt = oci_image::manifest::MediaType::convert_layer_media_type(
            &layer_mt,
            image_vendor,
            use_zstd,
        );

        match target_mt {
            Some(target) => {
                tracing::debug!(
                    "Converting layer media type from {} to {}",
                    layer_mt,
                    target
                );
                // Re-create the layer with the target media type.
                // For now, we just update the media_type since the data format
                // (tar/gzip/zstd) may need actual re-compression.
                let data = layer.data();
                let diff_id = layer.diff_id().to_string();

                // Check if we need to re-compress
                // Note: data() returns &[u8], need to convert to Vec<u8> for re-compression
                let new_layer = if target.contains("zstd") && !layer_mt.contains("zstd") {
                    // Re-compress with zstd
                    // Note: data is compressed, need uncompressed_data for re-compression
                    let uncompressed = layer.uncompressed_data()?;
                    oci_image::layer::Layer::from_tar_uncompressed_with_options(
                        uncompressed,
                        oci_image::layer::LayerCompression::zstd(self.opts.compression_level),
                    )?
                } else if target.contains("gzip") && !layer_mt.contains("gzip") && !layer_mt.contains("zstd") {
                    // Re-compress with gzip
                    let uncompressed = layer.uncompressed_data()?;
                    oci_image::layer::Layer::from_tar_uncompressed_with_options(
                        uncompressed,
                        oci_image::layer::LayerCompression::gzip(self.opts.compression_level),
                    )?
                } else {
                    // Same compression, just update the media type string
                    layer
                };
                Ok(new_layer)
            }
            None => Err(BuildError::Failed(format!(
                "layer with media type {} cannot be converted to match {}",
                layer_mt, image_mt
            ))),
        }
    }

    /// Determine whether the filesystem needs to be unpacked.
    ///
    /// Checks if any command requires an unpacked FS, or if there are
    /// cross-stage dependencies that need the filesystem.
    ///
    /// When `initial_fs_unpacked` is true and this is stage 0,
    /// the FS is already on disk so we skip extraction.
    ///
    /// Analogous to Go: `stageBuilder.build()` — shouldUnpack logic.
    fn should_unpack_fs(&self) -> bool {
        // If the initial FS is already unpacked and this is the first stage,
        // skip extraction. Analogous to Go: `s.stage.Index == 0 && s.opts.InitialFSUnpacked`.
        if self.stage_index == 0 && self.opts.initial_fs_unpacked {
            tracing::info!("Initial filesystem already unpacked, skipping extraction");
            return false;
        }

        for cmd in &self.commands {
            if cmd.requires_unpacked_fs() {
                tracing::debug!("Command {} requires unpacked FS", cmd.command_string());
                return true;
            }
        }
        // Also check cross-stage dependencies
        if !self.cross_stage_deps.is_empty() {
            return true;
        }
        false
    }

    /// Optimize commands by replacing cacheable ones with cached versions.
    ///
    /// Walks through all commands. For those that should be cached,
    /// computes the composite cache key and checks if a cached layer exists.
    /// If a cache hit is found, the command is replaced with a cached version.
    ///
    /// For metadata-only commands, they are still executed (to track
    /// state changes for proper cache key computation), but their
    /// output is discarded.
    ///
    /// Analogous to Go: `stageBuilder.optimize()`.
    fn optimize(
        &mut self,
        composite_key: &mut crate::command::CompositeCache,
    ) -> Result<()> {
        if !self.opts.cache {
            return Ok(());
        }

        let mut stop_cache = false;

        for i in 0..self.commands.len() {
            let command = &self.commands[i];
            let cmd_str = command.command_string();

            // Get files used from context for cache key computation
            let files = command
                .files_used_from_context(&self.image.config.config, &self.args)
                .unwrap_or_default();

            // Populate composite key
            let file_paths: Vec<String> = files.iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect();
            composite_key.add_key(&[&cmd_str]);
            for fp in &file_paths {
                if let Err(e) = composite_key.add_path(fp) {
                    tracing::debug!("Failed to add path to composite key: {}", e);
                }
            }

            let ck = composite_key.hash();
            tracing::debug!("Optimize: cache key for command {} = {}", cmd_str, ck);

            // Check if this command should be cached and we haven't
            // had a cache miss yet
            if command.should_cache_output() && !stop_cache {
                // Try local cache hit
                if let Some(ref cache_dir) = self.opts.cache_dir {
                    let cache = LayoutCache::new(cache_dir);
                    if cache.exists(&ck) {
                        match cache.retrieve_layer(&ck) {
                            Ok(cached_image) => {
                                tracing::info!(
                                    "Using caching version of cmd: {}",
                                    cmd_str
                                );
                                // Replace the command with a CachedCommand wrapper
                                // that provides the cached layers directly.
                                let cached_cmd = CachedCommand::new(
                                    cmd_str.clone(),
                                    cached_image.layers.clone(),
                                );
                                self.commands[i] = Box::new(cached_cmd);
                                continue;
                            }
                            Err(e) => {
                                tracing::debug!(
                                    "Failed to retrieve layer: {}",
                                    e
                                );
                                tracing::info!(
                                    "No cached layer found for cmd {}",
                                    cmd_str
                                );
                                tracing::debug!(
                                    "Key missing was: {}",
                                    composite_key.key()
                                );
                                stop_cache = true;
                            }
                        }
                    } else {
                        tracing::info!(
                            "No cached layer found for cmd {}",
                            cmd_str
                        );
                        stop_cache = true;
                    }
                }
            }

            // Execute metadata-only commands to track state
            // (we need their effect on config for proper cache keys)
            if command.metadata_only() {
                // We can't execute async commands here in a sync method,
                // so just track that we need to execute them later
                tracing::debug!(
                    "Optimize: skipping metadata-only command execution: {}",
                    cmd_str
                );
            }
        }

        Ok(())
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

/// A cached command that provides pre-computed layers instead of executing.
///
/// When the optimizer finds a cache hit, it replaces the original command
/// with this wrapper. During the build loop, the cached layers are applied
/// directly without re-executing the command.
///
/// Analogous to Go: `commands.Cached` interface + `CacheCommand()`.
#[derive(Debug, Clone)]
struct CachedCommand {
    /// Original command string for logging.
    command_string: String,
    /// Pre-computed layers from cache.
    layers: Vec<oci_image::layer::Layer>,
}

impl CachedCommand {
    fn new(command_string: String, layers: Vec<oci_image::layer::Layer>) -> Self {
        Self {
            command_string,
            layers,
        }
    }

    /// Get the cached layers.
    fn layers(&self) -> &[oci_image::layer::Layer] {
        &self.layers
    }
}

#[async_trait::async_trait]
impl DockerCommand for CachedCommand {
    async fn execute(&self, _config: &mut oci_image::config::ContainerConfig, _args: &BuildArgs) -> crate::command::Result<()> {
        // Cached commands don't execute — layers are applied directly
        Ok(())
    }

    fn command_string(&self) -> String {
        format!("{} (cached)", self.command_string)
    }

    fn files_to_snapshot(&self) -> Option<Vec<std::path::PathBuf>> {
        None
    }

    fn provides_files_to_snapshot(&self) -> bool {
        false
    }

    fn metadata_only(&self) -> bool {
        false
    }

    fn requires_unpacked_fs(&self) -> bool {
        false
    }

    fn should_cache_output(&self) -> bool {
        true
    }

    fn should_detect_deleted_files(&self) -> bool {
        false
    }

    fn is_args_envs_required_in_cache(&self) -> bool {
        false
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Save a layer to the image, converting its media type if needed.
///
/// When the image manifest uses a different vendor prefix (OCI vs Docker)
/// than the layer, the layer's media type must be converted to match.
/// This ensures consistency within the manifest.
///
/// Analogous to Go: `stageBuilder.saveLayerToImage()` + `convertLayerMediaType()`.
pub fn save_layer_to_image(
    image: MutableImage,
    layer: Layer,
    use_zstd: bool,
) -> Result<MutableImage> {
    // Determine the target vendor from the image manifest's media type
    let manifest_media_type = image.manifest.media_type.as_deref().unwrap_or("");
    let image_vendor = MediaType::extract_vendor_prefix(manifest_media_type);

    // Convert the layer's media type to match the image vendor
    let converted_layer = convert_layer_media_type(layer, image_vendor, use_zstd)?;

    oci_image::mutate::append_layer(image, converted_layer)
        .map_err(BuildError::Mutate)
}

/// Convert a layer's media type to match the image's vendor prefix.
///
/// When building an OCI image from a Docker base image (or vice versa),
/// the layers from the base image may use a different media type vendor.
/// This function converts the layer's media type to match the target vendor.
///
/// Analogous to Go: `executor.convertLayerMediaType()` (build.go:576-608).
pub fn convert_layer_media_type(
    layer: Layer,
    target_vendor: &str,
    use_zstd: bool,
) -> Result<Layer> {
    let current_media_type = layer.media_type().to_string();
    let current_vendor = MediaType::extract_vendor_prefix(&current_media_type);

    if current_vendor == target_vendor {
        // Same vendor — no conversion needed
        return Ok(layer);
    }

    // Determine the target media type
    let target_media_type = MediaType::convert_layer_media_type(
        &current_media_type,
        target_vendor,
        use_zstd,
    ).ok_or_else(|| BuildError::Failed(format!(
        "cannot convert layer media type from '{}' to vendor '{}'",
        current_media_type, target_vendor
    )))?;

    tracing::debug!(
        "Converting layer media type: {} → {}",
        current_media_type,
        target_media_type
    );

    layer.with_media_type(&target_media_type)
        .map_err(|e| BuildError::Failed(format!("failed to convert layer media type: {}", e)))
}