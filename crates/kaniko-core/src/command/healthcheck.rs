//! HEALTHCHECK command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::{ContainerConfig, HealthConfig};

/// HEALTHCHECK instruction — defines a health check for the container.
#[derive(Debug)]
pub struct HealthCheckCommand {
    cmd: HealthCheckCmd,
}

#[derive(Debug)]
enum HealthCheckCmd {
    Check {
        test: Vec<String>,
        interval: Option<String>,
        timeout: Option<String>,
        start_period: Option<String>,
        retries: Option<u32>,
    },
    None,
}

impl HealthCheckCommand {
    pub fn new(
        test: Vec<String>,
        interval: Option<String>,
        timeout: Option<String>,
        start_period: Option<String>,
        retries: Option<u32>,
    ) -> Self {
        Self {
            cmd: HealthCheckCmd::Check { test, interval, timeout, start_period, retries },
        }
    }

    pub fn none() -> Self {
        Self { cmd: HealthCheckCmd::None }
    }
}

#[async_trait]
impl BaseCommand for HealthCheckCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        match &self.cmd {
            HealthCheckCmd::Check { test, interval, timeout, start_period, retries } => {
                tracing::info!("HEALTHCHECK {:?}", test);
                let healthcheck = HealthConfig {
                    test: Some(test.clone()),
                    interval: interval.as_ref().and_then(|v| parse_nanoseconds(v)),
                    timeout: timeout.as_ref().and_then(|v| parse_nanoseconds(v)),
                    start_period: start_period.as_ref().and_then(|v| parse_nanoseconds(v)),
                    retries: *retries,
                };
                config.healthcheck = Some(healthcheck);
            }
            HealthCheckCmd::None => {
                tracing::info!("HEALTHCHECK NONE");
                config.healthcheck = Some(HealthConfig {
                    test: Some(vec!["NONE".to_string()]),
                    interval: None,
                    timeout: None,
                    start_period: None,
                    retries: None,
                });
            }
        }
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        match &self.cmd {
            HealthCheckCmd::Check { test, .. } => format!("HEALTHCHECK {:?}", test),
            HealthCheckCmd::None => "HEALTHCHECK NONE".into(),
        }
    }
}

/// Parse Docker duration string (e.g. "30s", "1m30s") to nanoseconds.
fn parse_nanoseconds(s: &str) -> Option<u64> {
    let mut total_ns: u64 = 0;
    let mut num = String::new();
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else {
            let n: u64 = num.parse().ok()?;
            num.clear();
            match ch {
                'h' => total_ns += n * 3_600_000_000_000,
                'm' => total_ns += n * 60_000_000_000,
                's' => total_ns += n * 1_000_000_000,
                _ => return None,
            }
        }
    }
    Some(total_ns)
}