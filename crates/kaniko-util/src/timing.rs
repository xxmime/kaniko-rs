//! Timing module for kaniko-rs.
//!
//! Tracks elapsed time for build phases, analogous to Go: `pkg/timing/timing.go`.
//!
//! Usage:
//! ```ignore
//! use kaniko_util::timing::DEFAULT_TIMER;
//!
//! DEFAULT_TIMER.start("unpack_fs");
//! // ... do work ...
//! DEFAULT_TIMER.stop("unpack_fs");
//!
//! // Print all timings
//! DEFAULT_TIMER.format_all();
//! ```

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Instant;

/// A single timing record for a labelled phase.
#[derive(Debug, Clone)]
pub struct TimingRecord {
    /// Human-readable label for this phase.
    pub label: String,
    /// When `start(label)` was called.
    pub start: Instant,
    /// When `stop(label)` was called (`None` if still running).
    pub end: Option<Instant>,
}

impl TimingRecord {
    /// Return the elapsed duration.
    ///
    /// If the timer is still running (no `stop` called yet), returns
    /// the duration from `start` to **now**.
    pub fn duration(&self) -> std::time::Duration {
        match self.end {
            Some(end) => end.duration_since(self.start),
            None => self.start.elapsed(),
        }
    }
}

/// A collection of named timing records.
///
/// Thread-safe wrapper around the inner map so that it can be used
/// from multiple parts of the build pipeline without passing
/// mutable references around.
///
/// Analogous to Go: `timing.Timing`.
#[derive(Debug)]
pub struct Timing {
    records: std::sync::Mutex<HashMap<String, TimingRecord>>,
}

impl Timing {
    /// Create a new empty timing collector.
    pub fn new() -> Self {
        Self {
            records: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Start a timer for `label`.
    ///
    /// Overwrites any previous record with the same label.
    pub fn start(&self, label: &str) {
        let record = TimingRecord {
            label: label.to_string(),
            start: Instant::now(),
            end: None,
        };
        self.records.lock().unwrap().insert(label.to_string(), record);
    }

    /// Stop the timer for `label`.
    ///
    /// Does nothing if no timer was started for `label`.
    pub fn stop(&self, label: &str) {
        if let Some(record) = self.records.lock().unwrap().get_mut(label) {
            record.end = Some(Instant::now());
        }
    }

    /// Get the elapsed duration for `label`.
    ///
    /// Returns `None` if no timer was started for `label`.
    /// Returns the duration so far if the timer is still running.
    pub fn get(&self, label: &str) -> Option<std::time::Duration> {
        self.records
            .lock()
            .unwrap()
            .get(label)
            .map(|r| r.duration())
    }

    /// Format all timing records as a human-readable string.
    ///
    /// Each line is: `<label>: <duration>`
    pub fn format_all(&self) -> String {
        let records = self.records.lock().unwrap();
        let mut lines: Vec<String> = records
            .values()
            .map(|r| format!("{}: {:.3}s", r.label, r.duration().as_secs_f64()))
            .collect();
        lines.sort();
        lines.join("\n")
    }

    /// Log all timing records at INFO level.
    pub fn log_all(&self) {
        let records = self.records.lock().unwrap();
        tracing::info!("=== Build Timing Summary ===");
        for r in records.values() {
            tracing::info!("  {}: {:.3}s", r.label, r.duration().as_secs_f64());
        }
    }

    /// Clear all timing records.
    pub fn clear(&self) {
        self.records.lock().unwrap().clear();
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::new()
    }
}

/// Global default timer used across the build pipeline.
///
/// Analogous to Go: `timing.DefaultRun`.
pub static DEFAULT_TIMER: LazyLock<Timing> = LazyLock::new(Timing::new);

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_start_stop() {
        let timing = Timing::new();
        timing.start("phase1");
        thread::sleep(Duration::from_millis(50));
        timing.stop("phase1");

        let dur = timing.get("phase1").unwrap();
        assert!(dur >= Duration::from_millis(40), "duration was {:?}", dur);
    }

    #[test]
    fn test_get_nonexistent() {
        let timing = Timing::new();
        assert!(timing.get("nope").is_none());
    }

    #[test]
    fn test_running_timer() {
        let timing = Timing::new();
        timing.start("bg");
        // Not stopped yet — should still return a duration
        let dur = timing.get("bg");
        assert!(dur.is_some());
        timing.stop("bg");
    }

    #[test]
    fn test_stop_without_start() {
        let timing = Timing::new();
        // Should not panic
        timing.stop("never_started");
    }

    #[test]
    fn test_format_all() {
        let timing = Timing::new();
        timing.start("a");
        timing.stop("a");
        timing.start("b");
        timing.stop("b");

        let output = timing.format_all();
        assert!(output.contains("a:"));
        assert!(output.contains("b:"));
    }

    #[test]
    fn test_clear() {
        let timing = Timing::new();
        timing.start("x");
        timing.stop("x");
        timing.clear();
        assert!(timing.get("x").is_none());
    }

    #[test]
    fn test_default_timer_is_usable() {
        DEFAULT_TIMER.start("global_test");
        DEFAULT_TIMER.stop("global_test");
        assert!(DEFAULT_TIMER.get("global_test").is_some());
        DEFAULT_TIMER.clear();
    }

    #[test]
    fn test_overwrite_label() {
        let timing = Timing::new();
        timing.start("dup");
        thread::sleep(Duration::from_millis(30));
        timing.stop("dup");
        let first = timing.get("dup").unwrap();

        timing.start("dup");
        timing.stop("dup");
        let second = timing.get("dup").unwrap();

        // Second should be shorter since no sleep
        assert!(second < first);
    }
}