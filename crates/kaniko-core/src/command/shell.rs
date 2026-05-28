//! SHELL command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// SHELL instruction — sets the default shell for subsequent RUN commands.
#[derive(Debug)]
pub struct ShellCommand {
    shell: Vec<String>,
}

impl ShellCommand {
    pub fn new(shell: Vec<String>) -> Self {
        Self { shell }
    }
}

#[async_trait]
impl BaseCommand for ShellCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        config.shell = Some(self.shell.clone());
        tracing::info!("SHELL {:?}", config.shell);
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("SHELL {:?}", self.shell)
    }
}