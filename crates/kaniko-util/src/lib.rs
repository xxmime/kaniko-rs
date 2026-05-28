//! Utility types and functions for kaniko-rs.
//!
//! Provides the unified error type, logging helpers, and common
//! utilities shared across all kaniko-rs crates.

pub mod error;
pub mod logging;

pub use error::KanikoError;
pub use logging::init_tracing;