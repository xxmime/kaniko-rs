//! STOPSIGNAL command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// STOPSIGNAL instruction — sets the system call signal for stopping the container.
#[derive(Debug)]
pub struct StopSignalCommand {
    signal: String,
}

impl StopSignalCommand {
    pub fn new(signal: String) -> Self {
        Self { signal }
    }
}

#[async_trait]
impl BaseCommand for StopSignalCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        config.stop_signal = Some(self.signal.clone());
        tracing::info!("STOPSIGNAL {}", self.signal);
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("STOPSIGNAL {}", self.signal)
    }
}