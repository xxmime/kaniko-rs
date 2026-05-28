//! ARG command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// ARG instruction — defines a build-time variable.
#[derive(Debug)]
pub struct ArgCommand {
    name: String,
    default_value: Option<String>,
}

impl ArgCommand {
    pub fn new(name: String, default_value: Option<String>) -> Self {
        Self { name, default_value }
    }
}

#[async_trait]
impl BaseCommand for ArgCommand {
    async fn execute_impl(&self, _config: &mut ContainerConfig, args: &BuildArgs) -> Result<()> {
        // ARG values are resolved at build time, not stored in the image config.
        // They are accessible via the BuildArgs structure.
        // If no build-time override, use the default value.
        if !args.build_args.contains_key(&self.name) {
            if let Some(ref default) = self.default_value {
                // Register the default in build_args so subsequent commands can use it
                tracing::info!("ARG {}={}", self.name, default);
            } else {
                tracing::info!("ARG {}", self.name);
            }
        } else {
            tracing::debug!("ARG {} (overridden by build arg)", self.name);
        }
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        match &self.default_value {
            Some(v) => format!("ARG {}={}", self.name, v),
            None => format!("ARG {}", self.name),
        }
    }

    fn is_args_envs_required_in_cache_impl(&self) -> bool { true }
}