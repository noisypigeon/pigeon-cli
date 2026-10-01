use std::future::Future;
use std::time::Duration;

/// Retries `f` up to `attempts` times, sleeping `backoff * attempt_number`
/// between tries (linear: `backoff`, `2*backoff`, ...) before giving up --
/// absorbs a transient failure (e.g. a burst of concurrent workers all
/// connecting within the same instant at job start) instead of failing on
/// the first timeout. Generic over `f` so it's testable without any real
/// I/O. Originally `email_sync::worker`'s own helper (ADR-0021 §6 addendum);
/// hoisted here once a second (`upload`) and third (`pull_transform`,
/// ADR-0074) consumer needed the exact same retry shape.
pub(crate) async fn retry_with_backoff<T, F, Fut>(
    attempts: usize,
    backoff: Duration,
    mut f: F,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, String>>,
{
    let mut last_err = None;
    for attempt in 0..attempts.max(1) {
        if attempt > 0 {
            tokio::time::sleep(backoff * attempt as u32).await;
        }
        match f().await {
            Ok(value) => return Ok(value),
            Err(err) => {
                tracing::warn!(
                    attempt = attempt + 1,
                    attempts,
                    error = %err,
                    "retrying after error"
                );
                last_err = Some(err);
            }
        }
    }
    if let Some(err) = &last_err {
        tracing::error!(attempts, error = %err, "retries exhausted");
    }
    Err(last_err.unwrap_or_else(|| "retry_with_backoff called with zero attempts".to_string()))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    const TEST_RETRIES: usize = 3;

    #[tokio::test]
    async fn retry_with_backoff_succeeds_after_transient_failures() {
        let attempts = AtomicUsize::new(0);
        let result: Result<&str, String> =
            retry_with_backoff(TEST_RETRIES, Duration::from_millis(1), || {
                let count = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                async move {
                    if count < 3 {
                        Err(format!("attempt {count} failed"))
                    } else {
                        Ok("connected")
                    }
                }
            })
            .await;

        assert_eq!(result, Ok("connected"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retry_with_backoff_returns_the_last_error_after_exhausting_attempts() {
        let attempts = AtomicUsize::new(0);
        let result: Result<(), String> =
            retry_with_backoff(TEST_RETRIES, Duration::from_millis(1), || {
                let count = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                async move { Err(format!("attempt {count} failed")) }
            })
            .await;

        assert_eq!(result, Err("attempt 3 failed".to_string()));
        assert_eq!(attempts.load(Ordering::SeqCst), TEST_RETRIES);
    }

    #[tokio::test]
    async fn retry_with_backoff_does_not_retry_a_first_success() {
        let attempts = AtomicUsize::new(0);
        let result: Result<&str, String> =
            retry_with_backoff(TEST_RETRIES, Duration::from_millis(1), || {
                attempts.fetch_add(1, Ordering::SeqCst);
                async move { Ok("connected") }
            })
            .await;

        assert_eq!(result, Ok("connected"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
