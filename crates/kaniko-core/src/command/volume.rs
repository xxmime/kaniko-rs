//! VOLUME command implementation.
//!
//! VOLUME creates mount points and marks them as holding externally mounted volumes.
//! The directories are created if they don't exist.
//!
//! Analogous to Go: `pkg/commands/volume.go` — `VolumeCommand`.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use std::path::PathBuf;

/// VOLUME instruction — creates mount points for volumes.
#[derive(Debug)]
pub struct VolumeCommand {
    paths: Vec<String>,
}

impl VolumeCommand {
    pub fn new(paths: Vec<String>) -> Self {
        Self { paths }
    }
}

#[async_trait]
impl BaseCommand for VolumeCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        for path in &self.paths {
            // Add to volumes map (key with unit value in OCI spec)
            if let Some(ref mut volumes) = config.volumes {
                volumes.insert(path.clone(), ());
            } else {
                let mut map = std::collections::BTreeMap::new();
                map.insert(path.clone(), ());
                config.volumes = Some(map);
            }
            // Also create the directory in the filesystem
            let _ = std::fs::create_dir_all(path);
        }
        tracing::info!("VOLUME {:?}", self.paths);
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("VOLUME {:?}", self.paths)
    }

    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        // Return the volume paths as files to snapshot.
        // Analogous to Go: VolumeCommand.FilesToSnapshot() returns volume paths.
        if self.paths.is_empty() {
            None
        } else {
            Some(self.paths.iter().map(|p| PathBuf::from(p)).collect())
        }
    }

    fn provides_files_to_snapshot_impl(&self) -> bool {
        true
    }
}