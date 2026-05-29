//! Utility types and functions for kaniko-rs.
//!
//! Provides the unified error type, logging helpers, timing, and common
//! utilities shared across all kaniko-rs crates.

pub mod error;
pub mod logging;
pub mod timing;

pub use error::KanikoError;
pub use logging::init_tracing;
pub use timing::{Timing, TimingRecord, DEFAULT_TIMER};