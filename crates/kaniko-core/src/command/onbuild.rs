//! ONBUILD command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// ONBUILD instruction — registers a trigger to be executed in downstream builds.
#[derive(Debug)]
pub struct OnBuildCommand {
    /// The trigger instruction string (e.g. "RUN pip install -r requirements.txt").
    trigger: String,
}

impl OnBuildCommand {
    pub fn new(trigger: String) -> Self {
        Self { trigger }
    }
}

#[async_trait]
impl BaseCommand for OnBuildCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        // ONBUILD doesn't execute the trigger now — it stores it in the image config
        // so it runs when a child image's FROM references this image.
        if let Some(ref mut onbuild) = config.on_build {
            onbuild.push(self.trigger.clone());
        } else {
            config.on_build = Some(vec![self.trigger.clone()]);
        }
        tracing::info!("ONBUILD {}", self.trigger);
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("ONBUILD {}", self.trigger)
    }
}