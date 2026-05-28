//! VOLUME command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

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
}