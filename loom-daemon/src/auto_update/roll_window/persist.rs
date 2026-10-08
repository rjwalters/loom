//! Window consumption across restarts (Issue #10713, #10188 item 2).
//!
//! `consumed` used to start at `None` in every process, so a
//! daemon restarted while its window was still open (most often by the very
//! roll that consumed it) could arm a second roll in the same window. These
//! methods let `persisted_state` save and restore them. A child module so the
//! gate's fields stay private.

use super::WindowGate;
use crate::auto_update::persisted_state::WindowConsumption;

impl WindowGate {
    /// Mark the window this tick's gate let a roll through in as consumed,
    /// **before** the drain is armed: an armed roll can restart the process
    /// before the next tick's `begin_tick` would record it. Returns the previous
    /// `consumed` for [`Self::unmark_armed`], or `None` (and changes nothing)
    /// when no roll was let through or windowing is off.
    pub(in crate::auto_update) fn mark_armed(&mut self) -> Option<Option<i64>> {
        let index = self.armable?;
        Some(self.consumed.replace(index))
    }

    /// Undo [`Self::mark_armed`] when the drain was not armed after all, so a
    /// failed or refused roll leaves the window exactly as it was.
    pub(in crate::auto_update) fn unmark_armed(&mut self, prior: Option<i64>) {
        self.consumed = prior;
    }

    /// The consumption to persist, or `None` when windowing is off.
    pub(in crate::auto_update) fn consumption(&self) -> Option<WindowConsumption> {
        let (period, offset, _) = self.enabled()?;
        Some(WindowConsumption {
            period_secs: period.as_secs(),
            offset_secs: offset.as_secs(),
            consumed: self.consumed,
        })
    }

    /// Restore persisted consumption. Window indices are only meaningful for the
    /// schedule that produced them, so a record from a different period or
    /// offset (or any record while windowing is off) is ignored. Returns whether
    /// it was applied.
    pub(in crate::auto_update) fn restore_consumption(
        &mut self,
        saved: &WindowConsumption,
    ) -> bool {
        let Some((period, offset, _)) = self.enabled() else {
            return false;
        };
        if saved.period_secs != period.as_secs() || saved.offset_secs != offset.as_secs() {
            return false;
        }
        self.consumed = saved.consumed;
        true
    }
}
