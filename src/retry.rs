use crate::{DlmError, ProgressBarManager};
use std::future::Future;
use std::time::Duration;

/// Wait used as-is for the first `FIXED_RETRIES` attempts, then multiplied by
/// `2^k` on each subsequent attempt.
const BASE_WAIT: Duration = Duration::from_millis(500);

/// Retries that use the fixed `BASE_WAIT` before exponential backoff kicks
/// in. Lets short transient failures recover quickly without immediately
/// stretching out delays.
const FIXED_RETRIES: u32 = 3;

/// Upper bound on a single retry's wait — without this, large `--retry`
/// values produce absurd delays (2^N grows fast).
const MAX_WAIT: Duration = Duration::from_secs(10 * 60);

/// Builds a "polite" retry schedule: `BASE_WAIT` for the first
/// `FIXED_RETRIES` retries, then `BASE_WAIT * 2^k` for each subsequent
/// retry, capped at `MAX_WAIT`. Yields exactly `max_retries` durations —
/// `with_retries` consumes one per retry, so the action is called at most
/// `1 + max_retries` times.
///
/// With the defaults (500ms / 3 fixed / 10-min cap) and `max_retries = 10`
/// the schedule is:
/// 500ms, 500ms, 500ms, 1s, 2s, 4s, 8s, 16s, 32s, 64s.
pub fn retry_strategy(max_retries: u32) -> impl Iterator<Item = Duration> {
    FixedThenExponential {
        max: max_retries,
        fixed: FIXED_RETRIES,
        base: BASE_WAIT,
        cap: MAX_WAIT,
        next_index: 0,
    }
}

struct FixedThenExponential {
    max: u32,
    fixed: u32,
    base: Duration,
    cap: Duration,
    next_index: u32,
}

impl Iterator for FixedThenExponential {
    type Item = Duration;

    fn next(&mut self) -> Option<Duration> {
        if self.next_index >= self.max {
            return None;
        }
        let i = self.next_index;
        self.next_index += 1;

        let wait = if i < self.fixed {
            self.base
        } else {
            let exp = i - self.fixed + 1;
            let factor = 2u32.checked_pow(exp).unwrap_or(u32::MAX);
            self.base.saturating_mul(factor)
        };

        Some(wait.min(self.cap))
    }
}

/// Runs `action` and retries on errors deemed retryable by `should_retry`,
/// sleeping between attempts according to `strategy`. Bails immediately on a
/// non-retryable error; returns the last error if the strategy is exhausted.
pub async fn with_retries<T, E, A, Fut, R>(
    mut strategy: impl Iterator<Item = Duration>,
    mut action: A,
    mut should_retry: R,
) -> Result<T, E>
where
    A: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    R: FnMut(&E) -> bool,
{
    loop {
        match action().await {
            Ok(v) => return Ok(v),
            Err(e) => {
                if !should_retry(&e) {
                    return Err(e);
                }
                match strategy.next() {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => return Err(e),
                }
            }
        }
    }
}

pub fn retry_handler(e: &DlmError, pbm: &ProgressBarManager, link: &str) -> bool {
    let should_retry = is_retryable_error(e);
    if should_retry {
        let msg = format!("Scheduling retry for {link} after error {e}");
        pbm.log_above_progress_bars(&msg);
    }
    should_retry
}

const fn is_retryable_error(e: &DlmError) -> bool {
    matches!(
        e,
        DlmError::ConnectError
            | DlmError::ConnectionTimeout
            | DlmError::ResponseBodyError
            | DlmError::DeadLineElapsedTimeout
            | DlmError::IncompleteDownload { .. }
            | DlmError::ResponseStatusNotSuccess {
                status_code: 429 | 500..=599
            }
    )
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn retry_strategy_default_schedule() {
        let mut s = retry_strategy(10);
        // 3 fixed at 500ms, then doubling from 1s.
        assert_eq!(s.next(), Some(Duration::from_millis(500)));
        assert_eq!(s.next(), Some(Duration::from_millis(500)));
        assert_eq!(s.next(), Some(Duration::from_millis(500)));
        assert_eq!(s.next(), Some(Duration::from_secs(1)));
        assert_eq!(s.next(), Some(Duration::from_secs(2)));
        assert_eq!(s.next(), Some(Duration::from_secs(4)));
        assert_eq!(s.next(), Some(Duration::from_secs(8)));
        assert_eq!(s.next(), Some(Duration::from_secs(16)));
        assert_eq!(s.next(), Some(Duration::from_secs(32)));
        assert_eq!(s.next(), Some(Duration::from_secs(64)));
        assert_eq!(s.next(), None);
    }

    #[test]
    fn retry_strategy_zero_attempts_yields_nothing() {
        assert_eq!(retry_strategy(0).next(), None);
    }

    #[test]
    fn retry_strategy_below_fixed_count_only_yields_fixed() {
        let s: Vec<_> = retry_strategy(2).collect();
        assert_eq!(
            s,
            vec![Duration::from_millis(500), Duration::from_millis(500)]
        );
    }

    #[test]
    fn retry_strategy_caps_at_max_wait() {
        let s: Vec<_> = retry_strategy(20).collect();
        assert_eq!(s.len(), 20);
        assert!(s.iter().all(|d| *d <= MAX_WAIT));
        assert_eq!(*s.last().unwrap(), MAX_WAIT);
    }

    #[test]
    fn retry_strategy_handles_huge_attempts_without_overflow() {
        // 2^32 would overflow u32::pow; saturating_mul + checked_pow keep us sane.
        let s: Vec<_> = retry_strategy(64).collect();
        assert_eq!(s.len(), 64);
        assert_eq!(*s.last().unwrap(), MAX_WAIT);
    }

    #[tokio::test]
    async fn with_retries_returns_first_success_without_touching_strategy() {
        // Empty strategy: if anything but the first call happened, the test
        // would still see it (the `should_retry` path returns Err immediately
        // when the strategy is empty). Successful first call must short-circuit.
        let calls = Cell::new(0u32);
        let result: Result<u32, &str> = with_retries(
            std::iter::empty::<Duration>(),
            || {
                calls.set(calls.get() + 1);
                async { Ok(42) }
            },
            |_| true,
        )
        .await;
        assert_eq!(result, Ok(42));
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn with_retries_retries_until_success() {
        let calls = Cell::new(0u32);
        let result: Result<&str, &str> = with_retries(
            [Duration::ZERO; 5].into_iter(),
            || {
                let n = calls.get();
                calls.set(n + 1);
                async move { if n < 2 { Err("transient") } else { Ok("done") } }
            },
            |_| true,
        )
        .await;
        assert_eq!(result, Ok("done"));
        // initial attempt + 2 retries.
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn with_retries_bails_immediately_on_non_retryable_error() {
        let calls = Cell::new(0u32);
        let result: Result<u32, &str> = with_retries(
            [Duration::ZERO; 5].into_iter(),
            || {
                calls.set(calls.get() + 1);
                async { Err("fatal") }
            },
            |_| false,
        )
        .await;
        assert_eq!(result, Err("fatal"));
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn with_retries_returns_last_error_when_strategy_exhausted() {
        let calls = Cell::new(0u32);
        let result: Result<u32, &str> = with_retries(
            [Duration::ZERO; 2].into_iter(),
            || {
                calls.set(calls.get() + 1);
                async { Err("transient") }
            },
            |_| true,
        )
        .await;
        assert_eq!(result, Err("transient"));
        // initial attempt + 2 strategy entries = 3 calls before giving up.
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn with_retries_empty_strategy_runs_once() {
        let calls = Cell::new(0u32);
        let result: Result<u32, &str> = with_retries(
            std::iter::empty::<Duration>(),
            || {
                calls.set(calls.get() + 1);
                async { Err("boom") }
            },
            |_| true,
        )
        .await;
        assert_eq!(result, Err("boom"));
        assert_eq!(calls.get(), 1);
    }
}
