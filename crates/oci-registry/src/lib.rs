//! OCI Registry interaction module for kaniko-rs.
//!
//! Handles push/pull operations against OCI-compliant registries.

pub mod push;
pub mod pull;
pub mod auth;
pub mod transport;

pub use auth::RegistryAuth;
pub use push::{Reference, PushError, push_image};
pub use pull::{PullError, pull_image};
pub use transport::{RetryConfig, TransportError, build_client, retry_request};