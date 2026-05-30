//! OCI Registry interaction module for kaniko-rs.
//!
//! Handles push/pull operations against OCI-compliant registries.

pub mod push;
pub mod pull;
pub mod auth;
pub mod transport;

pub use auth::RegistryAuth;
pub use push::{Reference, PushError, PushOptions, push_image, push_image_with_options};
pub use pull::{PullError, pull_image, pull_image_with_platform};
pub use transport::{RetryConfig, RegistryOptions, TransportError, build_client, build_client_with_options, retry_request, set_new_registry, set_new_repository};