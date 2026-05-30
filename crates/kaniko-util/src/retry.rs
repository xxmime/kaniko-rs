//! Retry utility functions for kaniko-rs.
//!
//! Analogous to Go: `pkg/util/util.go` Retry() and RetryWithResult().
//!
//! Provides exponential backoff retry for fallible operations.

use std::future::Future;
use std::time::Duration;
use tracing;

/// Retry a synchronous operation with exponential backoff.
///
/// The operation is retried up to `retry_count` times with exponential backoff
/// starting from `initial_delay_ms` milliseconds.
///
/// Analogous to Go: `util.Retry()`.
pub fn retry<F>(operation: F, retry_count: u32, initial_delay_ms: u64) -> Result<(), String>
where
    F: Fn() -> Result<(), String>,
{
    if let Ok(()) = operation() {
        return Ok(());
    }

    for i in 0..retry_count {
        let delay_ms = initial_delay_ms * 2u64.pow(i);
        let sleep_duration = Duration::from_millis(delay_ms);
        tracing::warn!("Retrying operation after {:?} due to previous error", sleep_duration);
        std::thread::sleep(sleep_duration);

        if let Ok(()) = operation() {
            return Ok(());
        }
    }

    Err(format!(
        "unable to complete operation after {} attempts",
        retry_count + 1
    ))
}

/// Retry an async operation with exponential backoff.
///
/// The operation is retried up to `retry_count` times with exponential backoff
/// starting from `initial_delay_ms` milliseconds.
///
/// Analogous to Go: `util.Retry()` but async.
pub async fn retry_async<F, Fut, T>(
    operation: F,
    retry_count: u32,
    initial_delay_ms: u64,
) -> Result<T, String>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    match operation().await {
        Ok(result) => return Ok(result),
        Err(e) => {
            if retry_count == 0 {
                return Err(format!("operation failed: {}", e));
            }
            tracing::warn!("Operation failed: {}, will retry", e);
        }
    }

    for i in 0..retry_count {
        let delay_ms = initial_delay_ms * 2u64.pow(i);
        let sleep_duration = Duration::from_millis(delay_ms);
        tracing::warn!("Retrying operation after {:?} due to previous error", sleep_duration);
        tokio::time::sleep(sleep_duration).await;

        match operation().await {
            Ok(result) => return Ok(result),
            Err(e) => {
                if i == retry_count - 1 {
                    return Err(format!(
                        "unable to complete operation after {} attempts, last error: {}",
                        retry_count + 1,
                        e
                    ));
                }
                tracing::warn!("Operation failed: {}, will retry", e);
            }
        }
    }

    Err("unexpected retry loop exit".to_string())
}

/// Retry an operation that returns a result with a custom error type.
///
/// Analogous to Go: `util.RetryWithResult()`.
pub fn retry_with_result<F, T, E>(
    operation: F,
    retry_count: u32,
    initial_delay_ms: u64,
) -> Result<T, E>
where
    F: Fn() -> Result<T, E>,
    E: std::fmt::Display,
{
    match operation() {
        Ok(result) => return Ok(result),
        Err(e) => {
            if retry_count == 0 {
                return Err(e);
            }
            tracing::warn!("Operation failed: {}, will retry", e);
        }
    }

    for i in 0..retry_count {
        let delay_ms = initial_delay_ms * 2u64.pow(i);
        let sleep_duration = Duration::from_millis(delay_ms);
        tracing::warn!("Retrying operation after {:?} due to previous error", sleep_duration);
        std::thread::sleep(sleep_duration);

        match operation() {
            Ok(result) => return Ok(result),
            Err(e) => {
                if i == retry_count - 1 {
                    return Err(e);
                }
                tracing::warn!("Operation failed: {}, will retry", e);
            }
        }
    }

    // This should not be reached, but just in case
    operation()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_retry_success_first_try() {
        let result = retry(|| Ok(()), 3, 10);
        assert!(result.is_ok());
    }

    #[test]
    fn test_retry_success_after_failures() {
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = count.clone();
        let result = retry(
            move || {
                let c = count_clone.fetch_add(1, Ordering::SeqCst);
                if c < 2 {
                    Err("not yet".to_string())
                } else {
                    Ok(())
                }
            },
            5,
            1,
        );
        assert!(result.is_ok());
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_retry_all_failures() {
        let result = retry(|| Err("always fail".to_string()), 2, 1);
        assert!(result.is_err());
    }

    #[test]
    fn test_retry_with_result_success() {
        let result: Result<i32, &str> = retry_with_result(|| Ok(42), 3, 10);
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn test_retry_with_result_after_failure() {
        let count = Arc::new(AtomicU32::new(0));
        let count_clone = count.clone();
        let result: Result<i32, String> = retry_with_result(
            move || {
                let c = count_clone.fetch_add(1, Ordering::SeqCst);
                if c < 1 {
                    Err("not yet".to_string())
                } else {
                    Ok(99)
                }
            },
            3,
            1,
        );
        assert_eq!(result.unwrap(), 99);
    }

    #[tokio::test]
    async fn test_retry_async_success() {
        let result = retry_async(|| async { Ok::<i32, String>(7) }, 3, 1).await;
        assert_eq!(result.unwrap(), 7);
    }

    #[tokio::test]
    async fn test_retry_async_all_failures() {
        let result = retry_async(|| async { Err::<i32, String>("fail".to_string()) }, 2, 1).await;
        assert!(result.is_err());
    }
}