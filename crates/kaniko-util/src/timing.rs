//! Timing module for kaniko-rs.
//!
//! Provides timing instrumentation for build stages, analogous to Go:
//! `pkg/timing/timing.go`.
//!
//! Usage:
//! ```ignore
//! let t = Timing::start("FS Unpacking");
//! // ... do work ...
//! DEFAULT_RUN.stop(t);
//! println!("{}", DEFAULT_RUN.format_all());
//! ```

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Instant;

/// A timing entry representing a measured duration.
#[derive(Debug, Clone)]
pub struct TimingEntry {
    /// Label for this timing entry.
    pub label: String,
    /// Duration in milliseconds.
    pub duration_ms: u64,
}

/// A started timer. Call `stop()` to record the duration.
#[derive(Debug)]
pub struct Timer {
    label: String,
    start: Instant,
}

impl Timer {
    /// Create a new timer with the given label.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            start: Instant::now(),
        }
    }

    /// Stop the timer and return the label and elapsed milliseconds.
    pub fn stop(self) -> (String, u64) {
        let elapsed = self.start.elapsed();
        (self.label, elapsed.as_millis() as u64)
    }
}

/// Global timing collector.
///
/// Thread-safe via `Mutex`. Use `start()` to begin a timer and `stop()`
/// to record its duration.
#[derive(Debug)]
pub struct Timing {
    entries: BTreeMap<String, u64>,
}

impl Timing {
    /// Create a new empty timing collector.
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// Start a new timer with the given label.
    /// Analogous to Go: `timing.Start(label)`.
    pub fn start(&self, label: impl Into<String>) -> Timer {
        Timer::new(label)
    }

    /// Stop a timer and record its duration.
    /// Analogous to Go: `timing.DefaultRun.Stop(t)`.
    pub fn stop(&mut self, timer: Timer) {
        let (label, duration_ms) = timer.stop();
        self.entries.insert(label, duration_ms);
    }

    /// Get the duration for a given label, if recorded.
    pub fn get(&self, label: &str) -> Option<u64> {
        self.entries.get(label).copied()
    }

    /// Format all timing entries as a human-readable string.
    pub fn format_all(&self) -> String {
        if self.entries.is_empty() {
            return "No timing data recorded".to_string();
        }

        let mut output = String::new();
        output.push_str("=== Timing Summary ===\n");
        let total: u64 = self.entries.values().sum();
        for (label, duration) in &self.entries {
            output.push_str(&format!("  {:40} {:>8} ms\n", label, duration));
        }
        output.push_str(&format!("  {:40} {:>8} ms\n", "TOTAL", total));
        output
    }

    /// Get all timing entries.
    pub fn entries(&self) -> &BTreeMap<String, u64> {
        &self.entries
    }

    /// Clear all timing entries.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

impl Default for Timing {
    fn default() -> Self {
        Self::new()
    }
}

/// Global timing collector instance.
/// Analogous to Go: `timing.DefaultRun`.
pub static DEFAULT_RUN: once_cell::sync::Lazy<Mutex<Timing>> =
    once_cell::sync::Lazy::new(|| Mutex::new(Timing::new()));

/// Convenience function to start a timer on the global timing collector.
pub fn start(label: impl Into<String>) -> Timer {
    Timer::new(label)
}

/// Convenience function to stop a timer on the global timing collector.
pub fn stop(timer: Timer) {
    if let Ok(mut timing) = DEFAULT_RUN.lock() {
        timing.stop(timer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timing_basic() {
        let mut timing = Timing::new();
        let t = timing.start("test_op");
        // Simulate work
        std::thread::sleep(std::time::Duration::from_millis(10));
        timing.stop(t);

        assert!(timing.get("test_op").unwrap() >= 10);
    }

    #[test]
    fn test_timing_multiple() {
        let mut timing = Timing::new();
        let t1 = timing.start("op1");
        timing.stop(t1);
        let t2 = timing.start("op2");
        timing.stop(t2);

        assert!(timing.get("op1").is_some());
        assert!(timing.get("op2").is_some());
    }

    #[test]
    fn test_format_all() {
        let mut timing = Timing::new();
        let t = timing.start("fast_op");
        timing.stop(t);

        let output = timing.format_all();
        assert!(output.contains("fast_op"));
        assert!(output.contains("TOTAL"));
    }

    #[test]
    fn test_timer_stop() {
        let timer = Timer::new("test");
        let (label, ms) = timer.stop();
        assert_eq!(label, "test");
        // Should be very fast, < 100ms
        assert!(ms < 100);
    }

    #[test]
    fn test_clear() {
        let mut timing = Timing::new();
        let t = timing.start("temp");
        timing.stop(t);
        assert!(timing.get("temp").is_some());
        timing.clear();
        assert!(timing.get("temp").is_none());
    }

    #[test]
    fn test_global_timing() {
        let t = start("global_test");
        stop(t);
        if let Ok(timing) = DEFAULT_RUN.lock() {
            assert!(timing.get("global_test").is_some());
        }
    }
}