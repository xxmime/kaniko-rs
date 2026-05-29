//! Core kaniko-rs library
//!
//! This crate provides the core functionality for building OCI images
//! from Dockerfiles in a rootless environment.

pub mod buildcontext;
pub mod builder;
pub mod command;
pub mod composite_key;
pub mod config;
pub mod multistage_builder;

pub use builder::{BuildOptions, BuildError, Result};
pub use composite_key::CompositeCache;

/// Version of the kaniko-rs core library
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Re-export commonly used types
pub use builder::StageBuilder;
pub use multistage_builder::MultiStageBuilder;
pub use buildcontext::{BuildContext, BuildContextError, GitBuildOptions, resolve_build_context};