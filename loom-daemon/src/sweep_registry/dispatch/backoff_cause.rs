//! What a dispatch-backoff window means for each [`DispatchBackoffCause`]
//! (Issue #8865).
//!
//! The #4485 ladder has two users: a dispatch that failed, and (since #7606)
//! the #4123 open-PR guard refusing an issue that is correctly parked behind
//! its own open PR. They share the doubling math and nothing else. Before
//! #8865 they also shared one streak and one log line, so an issue waiting on
//! review was logged as "N consecutive failed dispatch(es)" and a genuine
//! failure after it inherited the refusals' count. This module keeps the two
//! apart:
//!
//! - [`continues`]: a streak only grows under the cause that started it. The
//!   first event of the other cause starts at 1.
//! - [`log_armed`] / [`log_refused`]: the open-PR cause is worded as the
//!   deliberate skip it is. The generic lines are unchanged.

use super::{DispatchBackoffCause, DispatchBackoffState};
use chrono::{DateTime, Utc};
use std::time::Duration;

/// Whether an event of `cause` at `now` extends `prev`'s streak: `prev` was
/// armed by the same cause, no longer ago than `max_secs` (a streak older
/// than the ladder's cap has gone cold, as before).
pub(in crate::sweep_registry) fn continues(
    prev: &DispatchBackoffState,
    cause: DispatchBackoffCause,
    now: DateTime<Utc>,
    max_secs: i64,
) -> bool {
    prev.cause == cause && (now - prev.last_failure_at).num_seconds() <= max_secs
}

/// The ceiling for every cause but [`DispatchBackoffCause::FastFailure`]: the
/// 900 s `max` default from before #11854.
pub(crate) const TRANSIENT_BACKOFF_CAP: Duration = Duration::from_secs(900);

/// The ceiling a `cause` streak doubles up to (Issue #11854). Only an
/// issue-attributable fast failure climbs to the configured `max` (6 h by
/// default). Every other cause is transient and the window is broadcast to
/// every peer, so it stays at 900 s: a lost lease race, a token-pool death or
/// an environmental class says nothing about the issue, and an open-PR
/// refusal must not outlive the open-PR memo (`OPEN_PR_MEMO_FRESH`).
pub(super) fn cap(cause: DispatchBackoffCause, max: Duration) -> Duration {
    match cause {
        // The landed cooldown (#11853) bypasses the ladder; never capped here.
        DispatchBackoffCause::FastFailure | DispatchBackoffCause::Landed => max,
        DispatchBackoffCause::Generic => max.min(TRANSIENT_BACKOFF_CAP),
        DispatchBackoffCause::OpenPrGuard => max.min(super::super::guards::OPEN_PR_MEMO_FRESH),
    }
}

/// The INFO line for a window just armed for `issue`.
pub(super) fn log_armed(
    issue: u32,
    consecutive: u32,
    delay: Duration,
    cause: DispatchBackoffCause,
) {
    log::info!("{}", armed_line(issue, consecutive, delay, cause));
}

/// The INFO line for a dispatch of `issue` refused inside the window `state`
/// (step 2.8), with `remaining` of it left.
pub(super) fn log_refused(issue: u32, state: Option<&DispatchBackoffState>, remaining: Duration) {
    log::info!("{}", refused_line(issue, state, remaining));
}

fn armed_line(
    issue: u32,
    consecutive: u32,
    delay: Duration,
    cause: DispatchBackoffCause,
) -> String {
    let secs = delay.as_secs();
    match cause {
        DispatchBackoffCause::Generic | DispatchBackoffCause::FastFailure => format!(
            "sweep_registry: issue #{issue} dispatch backoff armed — {consecutive} consecutive \
             failed dispatch(es), next attempt allowed in {secs}s (#4485; cause={cause:?})"
        ),
        DispatchBackoffCause::OpenPrGuard => format!(
            "sweep_registry: issue #{issue} dispatch backoff armed — open-PR guard refusal \
             {consecutive} in a row (a deliberate skip; nothing failed), next attempt allowed in \
             {secs}s (#4485, #7606; cause={cause:?})"
        ),
        DispatchBackoffCause::Landed => format!(
            "sweep_registry: issue #{issue} landed cooldown armed — its work already landed (a \
             deliberate skip; nothing failed), next attempt allowed in {secs}s, fleet-wide \
             (#11853; cause={cause:?})"
        ),
    }
}

fn refused_line(issue: u32, state: Option<&DispatchBackoffState>, remaining: Duration) -> String {
    let secs = remaining.as_secs();
    let consecutive = state.map_or(0, |s| s.consecutive);
    match state.map(|s| s.cause) {
        Some(DispatchBackoffCause::OpenPrGuard) => format!(
            "sweep_registry: refusing to dispatch issue #{issue} — inside the window its open-PR \
             guard refusal(s) armed ({consecutive} in a row; nothing failed), {secs}s of backoff \
             remaining (#4485, #7606)"
        ),
        Some(DispatchBackoffCause::Landed) => format!(
            "sweep_registry: refusing to dispatch issue #{issue} — its work already landed, \
             {secs}s of landed cooldown remaining (#11853)"
        ),
        Some(DispatchBackoffCause::Generic | DispatchBackoffCause::FastFailure) | None => format!(
            "sweep_registry: refusing to dispatch issue #{issue} — {consecutive} consecutive \
             failed dispatch(es), {secs}s of backoff remaining (#4485)"
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "backoff_cause_tests.rs"]
mod tests;
