//! The landed cooldown (Issue #11853): a long, fleet-wide dispatch window for
//! an issue whose work already landed.
//!
//! An issue that stays open after its closing PR merged used to be refused
//! on every tick for 30 minutes on the host that landed it, and dispatched
//! for real on every other host. This window rides the #4485 backoff map and
//! the #7477 `DispatchBackoffArmed` broadcast, so every consumer of
//! [`SweepRegistry::dispatch_backoff_issues`] (the work finder's pre-dispatch
//! skip, step 2.8 inside `dispatch()`) honours it here and on every peer
//! without a new lane or wire kind. What differs from the ladder:
//!
//! - its length is the caller's, not `backoff_delay(..)`'s: hours, where the
//!   ladder stops at 900 s;
//! - it never shortens a window already in force, so a re-arm can only
//!   extend; and
//! - it is tagged [`DispatchBackoffCause::Landed`], so its log lines say it is
//!   a deliberate skip rather than a failed dispatch.
//!
//! A peer receiving the broadcast installs `now + remaining` without a cap
//! (`PeerClaimView::observe_dispatch_backoff_at`), so the length carries.

use std::time::Duration;

use chrono::Utc;

use super::{backoff_cause, DispatchBackoffCause, DispatchBackoffState};
use crate::peer_claims;
use crate::sweep_registry::SweepRegistry;

/// Ceiling on one landed cooldown: a caller's mistake costs a day, not an
/// issue that never dispatches again.
const MAX_LANDED_COOLDOWN: Duration = Duration::from_secs(24 * 3600);

impl SweepRegistry {
    /// Arm `issue`'s landed cooldown for `window` here and on every peer
    /// (#11853). A no-op when the dispatch backoff is disabled or `window` is
    /// zero; a longer window already in force is kept.
    pub(crate) fn arm_landed_cooldown(&mut self, issue: u32, window: Duration) {
        if !self.dispatch_backoff_config.enabled || window.is_zero() {
            return;
        }
        let now = Utc::now();
        let window = window.min(MAX_LANDED_COOLDOWN);
        let mut until =
            now + chrono::Duration::from_std(window).unwrap_or_else(|_| chrono::Duration::zero());
        if let Some(prev) = self.dispatch_backoff.get(&issue) {
            until = until.max(prev.until);
        }
        self.dispatch_backoff.insert(
            issue,
            DispatchBackoffState {
                // Not a failure: `dispatch_failure_count` stays 0.
                consecutive: 0,
                last_failure_at: now,
                until,
                cause: DispatchBackoffCause::Landed,
            },
        );
        let remaining = (until - now).to_std().unwrap_or(window);
        backoff_cause::log_armed(issue, 0, remaining, DispatchBackoffCause::Landed);
        self.publish_peer_cooldown_claim(
            peer_claims::ClaimKind::DispatchBackoffArmed,
            issue,
            remaining,
        );
    }

    /// Whether `issue`'s local dispatch-backoff window in force now is a
    /// landed cooldown (#11853). Test/inspection helper.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn landed_cooldown_active(&self, issue: u32) -> bool {
        let now = Utc::now();
        self.dispatch_backoff
            .get(&issue)
            .is_some_and(|s| s.cause == DispatchBackoffCause::Landed && s.until > now)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "landed_cooldown_tests.rs"]
mod tests;
