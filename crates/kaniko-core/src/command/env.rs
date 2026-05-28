//! ENV command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// ENV instruction — sets environment variables.
#[derive(Debug)]
pub struct EnvCommand {
    key: String,
    value: String,
}

impl EnvCommand {
    pub fn new(key: String, value: String) -> Self {
        Self { key, value }
    }
}

#[async_trait]
impl BaseCommand for EnvCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        tracing::info!("ENV {}={}", self.key, self.value);
        config.set_env(&self.key, &self.value);
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("ENV {}={}", self.key, self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oci_image::config::ContainerConfig;

    #[tokio::test]
    async fn test_env_command() {
        let mut config = ContainerConfig::default();
        let command = EnvCommand::new("TEST_KEY".to_string(), "test_value".to_string());
        let args = BuildArgs::new();
        
        command.execute_impl(&mut config, &args).await.unwrap();
        
        assert_eq!(config.env.as_ref().unwrap().len(), 1);
        assert_eq!(config.env.as_ref().unwrap()[0], "TEST_KEY=test_value");
    }

    #[tokio::test]
    async fn test_env_command_override() {
        let mut config = ContainerConfig::default();
        let command1 = EnvCommand::new("TEST_KEY".to_string(), "value1".to_string());
        let command2 = EnvCommand::new("TEST_KEY".to_string(), "value2".to_string());
        let args = BuildArgs::new();
        
        command1.execute_impl(&mut config, &args).await.unwrap();
        command2.execute_impl(&mut config, &args).await.unwrap();
        
        assert_eq!(config.env.as_ref().unwrap().len(), 1);
        assert_eq!(config.env.as_ref().unwrap()[0], "TEST_KEY=value2");
    }

    #[tokio::test]
    async fn test_env_command_multiple() {
        let mut config = ContainerConfig::default();
        let command1 = EnvCommand::new("KEY1".to_string(), "value1".to_string());
        let command2 = EnvCommand::new("KEY2".to_string(), "value2".to_string());
        let args = BuildArgs::new();
        
        command1.execute_impl(&mut config, &args).await.unwrap();
        command2.execute_impl(&mut config, &args).await.unwrap();
        
        assert_eq!(config.env.as_ref().unwrap().len(), 2);
        assert!(config.env.as_ref().unwrap().contains(&"KEY1=value1".to_string()));
        assert!(config.env.as_ref().unwrap().contains(&"KEY2=value2".to_string()));
    }
}