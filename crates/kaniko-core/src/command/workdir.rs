//! WORKDIR command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use std::path::PathBuf;
use std::sync::Mutex;

/// WORKDIR instruction — sets the working directory.
#[derive(Debug)]
pub struct WorkdirCommand {
    path: String,
    snapshot_files: Mutex<Vec<PathBuf>>,
}

impl WorkdirCommand {
    pub fn new(path: String) -> Self {
        Self {
            path,
            snapshot_files: Mutex::new(vec![]),
        }
    }
}

#[async_trait]
impl BaseCommand for WorkdirCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        let path = &self.path;
        let new_workdir = if std::path::Path::new(path).is_absolute() {
            path.to_string()
        } else {
            match &config.working_dir {
                Some(cwd) if !cwd.is_empty() => format!("{}/{}", cwd.trim_end_matches('/'), path),
                _ => format!("/{}", path),
            }
        };

        tracing::info!("WORKDIR {}", new_workdir);
        config.working_dir = Some(new_workdir.clone());

        // Create the directory if it doesn't exist (on the actual filesystem)
        let rooted = if new_workdir.starts_with('/') {
            new_workdir.clone()
        } else {
            format!("/{}", new_workdir)
        };

        if !std::path::Path::new(&rooted).exists() {
            if let Err(e) = std::fs::create_dir_all(&rooted) {
                // Not fatal — we may be running outside the container root
                tracing::debug!("Could not create workdir {}: {}", rooted, e);
            } else {
                self.snapshot_files.lock().unwrap().push(PathBuf::from(rooted));
            }
        }

        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("WORKDIR {}", self.path)
    }

    fn metadata_only_impl(&self) -> bool {
        false
    }

    fn files_to_snapshot_impl(&self) -> Option<Vec<PathBuf>> {
        let files = self.snapshot_files.lock().unwrap();
        if files.is_empty() {
            None
        } else {
            Some(files.clone())
        }
    }

    fn provides_files_to_snapshot_impl(&self) -> bool {
        true
    }
}