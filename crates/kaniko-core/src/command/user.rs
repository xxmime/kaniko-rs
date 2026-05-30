//! USER command implementation.

use crate::command::base::BaseCommand;
use crate::command::{BuildArgs, Result};
use async_trait::async_trait;
use oci_image::config::ContainerConfig;

/// USER instruction — sets the user for subsequent commands.
///
/// Supports both numeric UID/GID and username lookups.
/// Analogous to Go: `commands.UserCommand.ExecuteCommand()`.
#[derive(Debug)]
pub struct UserCommand {
    user: String,
}

impl UserCommand {
    pub fn new(user: String) -> Self {
        Self { user }
    }

    /// Resolve a user string (name or numeric) to a UID string.
    ///
    /// If the user string is a name (non-numeric), look it up via
    /// `lookup_user()` to find the corresponding UID.
    /// If it's already numeric, return as-is.
    ///
    /// Analogous to Go: `util.LookupUser()`.
    fn resolve_user_str(user_str: &str) -> String {
        // If it's already numeric, return as-is
        if user_str.parse::<u32>().is_ok() {
            return user_str.to_string();
        }

        // Try to look up the username
        match kaniko_snapshot::lookup_user(user_str) {
            Ok(info) => {
                tracing::debug!("Resolved user '{}' to uid {}", user_str, info.uid);
                info.uid.to_string()
            }
            Err(e) => {
                tracing::warn!("Could not look up user '{}': {}", user_str, e);
                user_str.to_string()
            }
        }
    }
}

#[async_trait]
impl BaseCommand for UserCommand {
    async fn execute_impl(&self, config: &mut ContainerConfig, _args: &BuildArgs) -> Result<()> {
        tracing::info!("USER {}", self.user);

        // Parse user:group format
        let parts: Vec<&str> = self.user.split(':').collect();
        let user_str = Self::resolve_user_str(parts[0]);

        let resolved_user = if parts.len() > 1 {
            let group_str = Self::resolve_user_str(parts[1]);
            format!("{}:{}", user_str, group_str)
        } else {
            user_str
        };

        config.user = Some(resolved_user);
        Ok(())
    }

    fn command_string_impl(&self) -> String {
        format!("USER {}", self.user)
    }
}