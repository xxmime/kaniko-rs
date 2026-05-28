//! ENTRYPOINT command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// ENTRYPOINT instruction — sets the entrypoint for the image.
#[derive(Debug)]
pub struct EntrypointCommand {
    args: Vec<String>,
    is_exec_form: bool,
}

impl EntrypointCommand {
    pub fn new_exec(args: Vec<String>) -> Self {
        Self { args, is_exec_form: true }
    }
    pub fn new_shell(command: String) -> Self {
        Self { args: vec![command], is_exec_form: false }
    }
}

#[async_trait]
impl BaseCommand for EntrypointCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        if self.is_exec_form {
            config.entrypoint = Some(self.args.clone());
        } else {
            // Shell form: ["/bin/sh", "-c", <command>]
            config.entrypoint = Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                self.args.first().cloned().unwrap_or_default(),
            ]);
        }
        tracing::info!("ENTRYPOINT {:?}", config.entrypoint);
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        if self.is_exec_form {
            format!("ENTRYPOINT {:?}", self.args)
        } else {
            format!("ENTRYPOINT {}", self.args.first().unwrap_or(&String::new()))
        }
    }
}