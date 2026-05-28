//! Integration tests for kaniko-rs.
//!
//! These tests verify end-to-end behavior by running the full
//! build pipeline: Dockerfile parsing → command execution →
//! image construction.

mod dockerfile_build;