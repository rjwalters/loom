//! The host-wide cooldown gate shared by the cleanup passes.
//!
//! Three reclaim passes — [`crate::docker_image_clean`],
//! [`crate::tmpfs_reclaim`] and [`crate::native_state_reclaim`] — each gate
//! their pass on one process-global "when did a pass last actually evaluate"
//! timestamp: the resource each walks (the Docker inventory, the mount
//! table, the native session bases) is not scoped to any one registered
//! repo, so a host with several repos ticking on the same reaper cadence
//! must not re-walk it once per repo per tick.
//!
//! Each pass owns its own [`HostWideCooldown`] instance; the instances share
//! no state. A single shared timestamp would let one pass's job suppress
//! the other two (#14861).

use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};

/// One pass's host-wide cooldown: the "last actually evaluated at"
/// timestamp plus the elapsed/record logic that gates a pass on it.
///
/// [`Self::new`] is `const`, so each module names one `static` instance and
/// delegates its check/record calls to it.
pub(crate) struct HostWideCooldown {
    last_evaluated_at: OnceLock<Mutex<Option<DateTime<Utc>>>>,
}

impl HostWideCooldown {
    pub(crate) const fn new() -> Self {
        Self {
            last_evaluated_at: OnceLock::new(),
        }
    }

    fn slot(&self) -> &Mutex<Option<DateTime<Utc>>> {
        self.last_evaluated_at.get_or_init(|| Mutex::new(None))
    }

    /// Whether enough time has passed since the last evaluation to run
    /// another. A pass with no recorded evaluation yet is eligible, a clock
    /// that jumped backwards since the last evaluation is eligible, and the
    /// exact `min_interval_secs` boundary counts as elapsed.
    #[must_use]
    pub(crate) fn elapsed(&self, now: DateTime<Utc>, min_interval_secs: u64) -> bool {
        let guard = self
            .slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *guard {
            None => true,
            Some(last) => {
                let since = (now - last).num_seconds();
                since < 0 || since >= i64::try_from(min_interval_secs).unwrap_or(i64::MAX)
            }
        }
    }

    /// Record that a pass evaluated at `now`, starting a new window.
    pub(crate) fn record(&self, now: DateTime<Utc>) {
        let mut guard = self
            .slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = Some(now);
    }

    /// Drop the recorded timestamp. Test-only seam (a module's static
    /// instance would otherwise leak between `#[serial]` tests in the same
    /// binary).
    pub(crate) fn reset(&self) {
        *self
            .slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn elapsed_is_true_before_any_evaluation() {
        let cooldown = HostWideCooldown::new();
        assert!(cooldown.elapsed(t(0), 1_800));
    }

    #[test]
    fn a_recent_evaluation_holds_the_cooldown() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(0));
        assert!(!cooldown.elapsed(t(60), 1_800));
    }

    #[test]
    fn the_exact_interval_boundary_is_eligible() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(0));
        assert!(cooldown.elapsed(t(1_800), 1_800));
    }

    #[test]
    fn the_cooldown_expires() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(0));
        assert!(cooldown.elapsed(t(1_801), 1_800));
    }

    #[test]
    fn a_backwards_clock_is_eligible() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(600));
        assert!(cooldown.elapsed(t(0), 1_800));
    }

    #[test]
    fn recording_one_instance_leaves_another_immediately_eligible() {
        let one = HostWideCooldown::new();
        let other = HostWideCooldown::new();
        one.record(t(0));
        assert!(!one.elapsed(t(60), 1_800));
        assert!(other.elapsed(t(60), 1_800));
    }

    #[test]
    fn reset_restores_first_evaluation() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(0));
        cooldown.reset();
        assert!(cooldown.elapsed(t(0), 1_800));
    }
}
