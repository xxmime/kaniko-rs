//! Logging initialization for kaniko-rs.
//!
//! Provides structured logging via `tracing` with configurable
//! log levels and output formats.

use tracing::Level;
use tracing_subscriber::EnvFilter;

/// Initialize the tracing subscriber with default settings.
///
/// Uses the `RUST_LOG` environment variable for configuration,
/// defaulting to `info` level if not set.
/// Safe to call multiple times (subsequent calls are no-ops).
pub fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .try_init();
}

/// Initialize tracing with a specific log level.
/// Safe to call multiple times (subsequent calls are no-ops).
pub fn init_tracing_with_level(level: Level) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level.to_string()));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .try_init();
}

/// Initialize tracing with JSON output format (for structured logging).
/// Safe to call multiple times (subsequent calls are no-ops).
pub fn init_tracing_json() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .json()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_tracing_does_not_panic() {
        // Should not panic even if called multiple times in tests
        // (tracing handles this internally)
        init_tracing();
    }

    #[test]
    fn test_init_tracing_with_level() {
        init_tracing_with_level(Level::DEBUG);
    }
}