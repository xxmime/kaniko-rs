//! CMD command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// CMD instruction — sets the default command for the image.
#[derive(Debug)]
pub struct CmdCommand {
    /// Command args (exec form) or single shell string.
    args: Vec<String>,
    is_exec_form: bool,
}

impl CmdCommand {
    pub fn new_exec(args: Vec<String>) -> Self {
        Self { args, is_exec_form: true }
    }
    pub fn new_shell(command: String) -> Self {
        Self { args: vec![command], is_exec_form: false }
    }
}

#[async_trait]
impl BaseCommand for CmdCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        if self.is_exec_form {
            config.cmd = Some(self.args.clone());
        } else {
            // Shell form: ["/bin/sh", "-c", <command>]
            config.cmd = Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                self.args.first().cloned().unwrap_or_default(),
            ]);
        }
        tracing::info!("CMD {:?}", config.cmd);
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        if self.is_exec_form {
            format!("CMD {:?}", self.args)
        } else {
            format!("CMD {}", self.args.first().unwrap_or(&String::new()))
        }
    }
}