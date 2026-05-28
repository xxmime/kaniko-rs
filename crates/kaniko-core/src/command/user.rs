//! USER command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// USER instruction — sets the user for subsequent commands.
#[derive(Debug)]
pub struct UserCommand {
    user: String,
}

impl UserCommand {
    pub fn new(user: String) -> Self {
        Self { user }
    }
}

#[async_trait]
impl BaseCommand for UserCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        tracing::info!("USER {}", self.user);
        config.user = Some(self.user.clone());
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("USER {}", self.user)
    }
}