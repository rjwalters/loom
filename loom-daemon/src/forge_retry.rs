//! Bounded in-process retry for the forge reads a composite write depends on
//! (#13975).
//!
//! `post-verdict.sh`'s composite write reads several forge leaves first
//! (the trusted-comment listing behind the verdict gates, a base branch's
//! required contexts); in the 2026-11-16 reproduction one transient bounded
//! `gh` read failure per invocation refused four whole passes over ~12
//! minutes, though each endpoint answered within a second between attempts.
//! Retrying at the shell level would re-run the whole composite — the cost
//! being removed — and raising the read timeout only lengthens every refusal;
//! so the retry lives **in-process at the leaf**, bounded, and only for the
//! failures a short pause can plausibly clear.
//!
//! Contract:
//!
//! - at most [`MAX_ATTEMPTS`] attempts total, the first included;
//! - a short fixed backoff between attempts, never more than
//!   [`MAX_ADDED_WAIT`] of added wait, so a caller's own budget (e.g.
//!   `post-verdict.sh`'s `--timeout`) keeps its headroom;
//! - the caller's predicate decides which errors may clear — a refusal that
//!   cannot (a permission, a plan gate) is returned on the first attempt;
//! - once attempts are exhausted the **last** error is returned unchanged:
//!   the caller's refusal text, exit code and fail-closed direction are
//!   exactly what a single attempt gave it before this module existed.

use std::time::Duration;

/// Total attempts per read, the first included: two retries.
pub const MAX_ATTEMPTS: u32 = 3;

/// Pause before the second attempt.
pub const RETRY_BACKOFF_1: Duration = Duration::from_secs(1);

/// Pause before the third attempt.
pub const RETRY_BACKOFF_2: Duration = Duration::from_secs(2);

/// Hard cap on the total added wait one read's retries may cost a caller:
/// the sum of the two backoffs. No read ever blocks longer than this beyond
/// its own attempts.
pub const MAX_ADDED_WAIT: Duration = Duration::from_secs(3);

/// The production sleeper: park the thread for `d`. Tests inject a no-op or
/// a recording closure instead, so no test waits out a backoff.
pub fn thread_sleep(d: Duration) {
    std::thread::sleep(d);
}

/// The pause before retrying after failed attempt `n` (1-based).
#[must_use]
fn backoff(failed_attempt: u32) -> Duration {
    if failed_attempt <= 1 {
        RETRY_BACKOFF_1
    } else {
        RETRY_BACKOFF_2
    }
}

/// Run `attempt` at most [`MAX_ATTEMPTS`] times, pausing (`sleep`) between
/// attempts and retrying only while `retryable` accepts the error. The last
/// error is returned unchanged once attempts are exhausted — a caller's
/// failure handling is byte-for-byte what a single attempt gave it.
pub fn bounded<T, E>(
    mut attempt: impl FnMut() -> Result<T, E>,
    retryable: impl Fn(&E) -> bool,
    mut sleep: impl FnMut(Duration),
) -> Result<T, E> {
    let mut n = 1;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(err) => {
                if n >= MAX_ATTEMPTS || !retryable(&err) {
                    return Err(err);
                }
                sleep(backoff(n));
                n += 1;
            }
        }
    }
}

/// [`bounded`] for a leaf that reports failure as `None` (the `run_gh`
/// leaves, where a failed exit and a spawn failure are one `None`): every
/// `None` is treated as retryable, and any `Some` is returned on the attempt
/// that produced it — a successful read, empty or not, is never retried.
pub fn bounded_option<T>(
    mut attempt: impl FnMut() -> Option<T>,
    mut sleep: impl FnMut(Duration),
) -> Option<T> {
    let mut n = 1;
    loop {
        match attempt() {
            Some(value) => return Some(value),
            None => {
                if n >= MAX_ATTEMPTS {
                    return None;
                }
                sleep(backoff(n));
                n += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_that_clears_is_retried_to_success() {
        let mut calls = 0;
        let mut sleeps = Vec::new();
        let out = bounded(
            || {
                calls += 1;
                if calls == 1 {
                    Err("blip")
                } else {
                    Ok(7)
                }
            },
            |_| true,
            |d| sleeps.push(d),
        );
        assert_eq!(out, Ok(7));
        assert_eq!(calls, 2, "one retry, not a fresh composite");
        assert_eq!(sleeps, vec![RETRY_BACKOFF_1]);
    }

    #[test]
    fn exhausted_attempts_return_the_last_error_after_exactly_max_attempts_calls() {
        let mut calls = 0;
        let mut sleeps = 0;
        let out = bounded(
            || {
                calls += 1;
                Err::<(), _>(format!("fail {calls}"))
            },
            |_| true,
            |_| sleeps += 1,
        );
        assert_eq!(out, Err("fail 3".to_string()), "the last error, unchanged");
        assert_eq!(calls, usize::try_from(MAX_ATTEMPTS).unwrap());
        assert_eq!(sleeps, usize::try_from(MAX_ATTEMPTS - 1).unwrap());
    }

    #[test]
    fn an_error_the_predicate_rejects_is_returned_after_one_call() {
        let mut calls = 0;
        let mut slept = 0;
        let out = bounded(
            || {
                calls += 1;
                Err::<(), _>("permission")
            },
            |e| *e != "permission",
            |_| slept += 1,
        );
        assert_eq!(out, Err("permission"));
        assert_eq!(calls, 1, "a refusal that cannot clear is not retried");
        assert_eq!(slept, 0);
    }

    #[test]
    fn the_option_twin_retries_only_none_and_returns_the_first_some() {
        let mut calls = 0;
        let mut sleeps = Vec::new();
        let out = bounded_option(
            || {
                calls += 1;
                (calls > 1).then(|| calls * 10)
            },
            |d| sleeps.push(d),
        );
        assert_eq!(out, Some(20));
        assert_eq!(calls, 2);
        assert_eq!(sleeps, vec![RETRY_BACKOFF_1]);

        let mut calls = 0;
        assert_eq!(
            bounded_option(
                || {
                    calls += 1;
                    None::<Vec<u8>>
                },
                |_| {}
            ),
            None
        );
        assert_eq!(calls, usize::try_from(MAX_ATTEMPTS).unwrap());

        let mut calls = 0;
        assert_eq!(
            bounded_option(
                || {
                    calls += 1;
                    Some(Vec::<u8>::new())
                },
                |_| {}
            ),
            Some(Vec::new())
        );
        assert_eq!(calls, 1, "a successful read, empty or not, is never retried");
    }

    #[test]
    fn the_backoffs_and_the_added_wait_cap_agree() {
        assert_eq!(backoff(1), RETRY_BACKOFF_1);
        assert_eq!(backoff(2), RETRY_BACKOFF_2);
        assert_eq!(MAX_ADDED_WAIT, RETRY_BACKOFF_1 + RETRY_BACKOFF_2);
    }
}
