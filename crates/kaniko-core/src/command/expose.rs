//! EXPOSE command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use std::collections::BTreeMap;

/// EXPOSE instruction — exposes network ports.
#[derive(Debug)]
pub struct ExposeCommand {
    ports: Vec<String>,
}

impl ExposeCommand {
    pub fn new(ports: Vec<String>) -> Self {
        Self { ports }
    }
}

#[async_trait]
impl BaseCommand for ExposeCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        let exposed = config.exposed_ports.get_or_insert_with(BTreeMap::new);
        for port in &self.ports {
            tracing::info!("EXPOSE {}", port);
            exposed.insert(port.clone(), ());
        }
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("EXPOSE {}", self.ports.join(" "))
    }
}