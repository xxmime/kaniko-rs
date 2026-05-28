//! Credential management for kaniko-rs.
//!
//! Handles Docker config.json parsing and credential helper invocation.
//! Analogous to Go: `pkg/creds`.

pub mod keychain;
pub mod helper;

pub use keychain::{CredsError, Credential, SystemKeychain};
pub use helper::{CredentialHelperCache, call_credential_helper, call_credential_helper_async};