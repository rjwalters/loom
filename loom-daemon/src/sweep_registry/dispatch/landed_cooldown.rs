//! The landed cooldown (Issue #11853): a long dispatch window for an issue
//! whose work already landed.
//!
//! An issue that stays open after its closing PR merged used to be refused
//! on every tick for 30 minutes. This window rides the #4485 backoff map, so
//! every consumer of [`SweepRegistry::dispatch_backoff_issues`] (the work
//! finder's pre-dispatch skip, step 2.8 inside `dispatch()`) honours it
//! without a new lane. The peer-claim broadcast that once carried it to other
//! hosts is retired (#11112); the window is per host. What differs from the
//! ladder:
//!
//! - its length is the caller's, not `backoff_delay(..)`'s: hours, where the
//!   ladder stops at 900 s;
//! - it never shortens a window already in force, so a re-arm can only
//!   extend; and
//! - it is tagged [`DispatchBackoffCause::Landed`], so its log lines say it is
//!   a deliberate skip rather than a failed dispatch.
//!
//! The window is lifted early when the issue is reopened after its landing or
//! Curator keeps it ([`SweepRegistry::clear_landed_cooldown`], #11901).

use std::collections::HashSet;
use std::time::Duration;

use chrono::Utc;

use super::{backoff_cause, DispatchBackoffCause, DispatchBackoffState};
use crate::sweep_registry::SweepRegistry;

/// Ceiling on one landed cooldown: a caller's mistake costs a day, not an
/// issue that never dispatches again.
const MAX_LANDED_COOLDOWN: Duration = Duration::from_secs(24 * 3600);

impl SweepRegistry {
    /// Arm `issue`'s landed cooldown for `window` (#11853). A no-op when the dispatch backoff is disabled or `window` is
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
    }

    /// Every issue whose local dispatch-backoff window in force now is a
    /// landed cooldown armed by this host (#11901): the ones the work finder
    /// may lift early.
    #[must_use]
    pub(crate) fn landed_cooldown_issues(&self) -> HashSet<u32> {
        let now = Utc::now();
        self.dispatch_backoff
            .iter()
            .filter(|(_, s)| s.cause == DispatchBackoffCause::Landed && s.until > now)
            .map(|(issue, _)| *issue)
            .collect()
    }

    /// Lift `issue`'s landed cooldown (#11901): the issue was reopened after
    /// its landing, or Curator kept it, so the window has nothing left to
    /// protect. Only a window of the `Landed` cause is removed; a
    /// failure-ladder window stays. Returns whether a landed window was
    /// removed.
    pub(crate) fn clear_landed_cooldown(&mut self, issue: u32) -> bool {
        let removed = self
            .dispatch_backoff
            .get(&issue)
            .is_some_and(|s| s.cause == DispatchBackoffCause::Landed);
        if removed {
            self.dispatch_backoff.remove(&issue);
        }
        log::info!("sweep_registry: landed cooldown on issue #{issue} lifted (#11901)");
        removed
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
