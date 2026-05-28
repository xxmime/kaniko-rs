//! LABEL command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;
use std::collections::BTreeMap;

/// LABEL instruction — sets image metadata labels.
#[derive(Debug)]
pub struct LabelCommand {
    labels: Vec<(String, String)>,
}

impl LabelCommand {
    pub fn new(labels: Vec<(String, String)>) -> Self {
        Self { labels }
    }
}

#[async_trait]
impl BaseCommand for LabelCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        let existing = config.labels.get_or_insert_with(BTreeMap::new);
        for (key, value) in &self.labels {
            tracing::info!("LABEL {}={}", key, value);
            existing.insert(key.clone(), value.clone());
        }
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        let pairs: Vec<String> = self.labels.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        format!("LABEL {}", pairs.join(" "))
    }
}