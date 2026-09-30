//! Retrying the backend, but only where retrying helps (PLAN.md §4b, §7).
//!
//! The four statuses BitRPC returns split cleanly in two. A 401 or a 403 means
//! the key is wrong or the method is not on the allowlist: retrying that is
//! just spending the budget to be told no again. A 429 means we are early, and
//! a 502 means the node blinked — both are worth another attempt.
//!
//! That distinction is the whole reason `BackendError` has four variants rather
//! than one (§4b).

use crate::error::{BackendError, CoreError, Result};
use std::time::Duration;

/// How many attempts in total, including the first.
pub const MAX_ATTEMPTS: u32 = 4;

/// The first backoff step. Doubles each attempt, so 250ms, 500ms, 1s.
pub const BASE_BACKOFF: Duration = Duration::from_millis(250);

/// How long a `Retry-After` we will actually honour before giving up — past
/// this, a user waiting on a command deserves an answer instead.
pub const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Whether this failure is worth trying again, and how long to wait first.
///
/// `None` means "do not retry", which is a decision and not an omission: a 403
/// retried three times is three calls spent learning the same thing.
pub fn backoff_for(error: &CoreError, attempt: u32) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS {
        return None;
    }

    match error {
        CoreError::Backend(BackendError::RateLimited { retry_after }) => {
            // The server's own figure wins when it gave one: it knows when the
            // window rolls over and we are guessing.
            Some(
                retry_after
                    .unwrap_or_else(|| exponential(attempt))
                    .min(MAX_BACKOFF),
            )
        }
        CoreError::Backend(BackendError::NodeUnavailable) => Some(exponential(attempt)),
        // A transport fault is usually a blip; a JSON-RPC error object is the
        // node's considered answer and will not change.
        CoreError::Backend(BackendError::Transport(_)) => Some(exponential(attempt)),
        _ => None,
    }
}

fn exponential(attempt: u32) -> Duration {
    BASE_BACKOFF
        .checked_mul(1u32 << attempt.min(8))
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

/// Run a blocking backend call with the policy above.
///
/// Takes a closure rather than a future because every RPC path here is
/// blocking; the caller has already put it on a blocking thread.
pub fn with_retry<T>(mut call: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempt = 0;
    loop {
        match call() {
            Ok(value) => return Ok(value),
            Err(e) => match backoff_for(&e, attempt) {
                Some(wait) => {
                    tracing::debug!(
                        attempt,
                        wait_ms = wait.as_millis(),
                        error = %e,
                        "retrying a backend call"
                    );
                    std::thread::sleep(wait);
                    attempt += 1;
                }
                None => return Err(e),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn rate_limited(retry_after: Option<Duration>) -> CoreError {
        CoreError::Backend(BackendError::RateLimited { retry_after })
    }

    /// §4b: retry only where retrying helps.
    #[test]
    fn an_invalid_key_is_never_retried() {
        let forbidden = CoreError::Backend(BackendError::Forbidden {
            method: "estimatesmartfee".into(),
        });
        assert_eq!(backoff_for(&forbidden, 0), None);

        let missing = CoreError::Backend(BackendError::MissingApiKey);
        assert_eq!(backoff_for(&missing, 0), None);
    }

    #[test]
    fn a_node_error_object_is_not_retried_either() {
        // The node considered the request and said no; asking again is rude.
        let rpc = CoreError::Backend(BackendError::Rpc {
            code: -26,
            message: "min relay fee not met".into(),
        });
        assert_eq!(backoff_for(&rpc, 0), None);
    }

    #[test]
    fn a_rate_limit_and_an_unavailable_node_are_both_retried() {
        assert!(backoff_for(&rate_limited(None), 0).is_some());
        assert!(backoff_for(&CoreError::Backend(BackendError::NodeUnavailable), 0).is_some());
    }

    #[test]
    fn the_servers_own_retry_after_wins_over_our_guess() {
        let theirs = Duration::from_secs(7);
        assert_eq!(backoff_for(&rate_limited(Some(theirs)), 0), Some(theirs));
    }

    #[test]
    fn an_absurd_retry_after_is_capped_so_a_user_still_gets_an_answer() {
        let absurd = Duration::from_secs(3_600);
        assert_eq!(
            backoff_for(&rate_limited(Some(absurd)), 0),
            Some(MAX_BACKOFF)
        );
    }

    #[test]
    fn backoff_grows_and_then_gives_up() {
        let first = backoff_for(&rate_limited(None), 0).expect("retryable");
        let second = backoff_for(&rate_limited(None), 1).expect("retryable");
        assert!(second > first, "backoff must grow");
        assert_eq!(
            backoff_for(&rate_limited(None), MAX_ATTEMPTS),
            None,
            "it gives up rather than retrying forever"
        );
    }

    #[test]
    fn a_call_that_recovers_returns_its_value() {
        let attempts = AtomicU32::new(0);
        let value = with_retry(|| {
            if attempts.fetch_add(1, Ordering::Relaxed) < 2 {
                Err(CoreError::Backend(BackendError::NodeUnavailable))
            } else {
                Ok(42)
            }
        })
        .expect("recovers");

        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn a_call_that_cannot_be_retried_is_tried_exactly_once() {
        let attempts = AtomicU32::new(0);
        let result: Result<()> = with_retry(|| {
            attempts.fetch_add(1, Ordering::Relaxed);
            Err(CoreError::Backend(BackendError::MissingApiKey))
        });

        assert!(result.is_err());
        assert_eq!(attempts.load(Ordering::Relaxed), 1);
    }
}
