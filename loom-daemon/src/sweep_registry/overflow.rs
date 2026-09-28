//! The `loom:operator-priority` overflow flag on a sweep record (#9244).
//!
//! The work finder may admit one starred issue over the concurrency caps as
//! this host's single over-limit sweep (`work_finder::OverflowSlot`). The
//! registry records that on the sweep's own [`SweepInfo::overflow`], so the
//! flag survives into `list_sweeps` / `get_sweep_status` / `loom-daemon
//! status`, and so the next tick can see the slot is still taken.
//!
//! Its own file: `sweep_registry/mod.rs` is frozen by the file-size ratchet.

use super::SweepRegistry;

impl SweepRegistry {
    /// Mark `sweep_id` as the host's overflow sweep. Returns `false` when no
    /// such sweep is registered (it already finished, or was never recorded).
    pub fn mark_overflow(&mut self, sweep_id: &str) -> bool {
        match self.entries.get_mut(sweep_id) {
            Some(info) => {
                info.overflow = true;
                true
            }
            None => false,
        }
    }

    /// Whether a non-terminal sweep in this registry is marked overflow.
    #[must_use]
    pub fn overflow_in_flight(&self) -> bool {
        self.entries
            .values()
            .any(|info| info.overflow && !info.state.is_terminal())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::test_support::{fixture_registry, insert_running_at, insert_terminal_issue};

    #[test]
    fn a_marked_live_sweep_holds_the_slot_until_it_is_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, _) = fixture_registry(dir.path());
        let live = insert_running_at(&mut reg, 7, 1, chrono::Utc::now());
        assert!(!reg.overflow_in_flight());
        assert!(reg.mark_overflow(&live));
        assert!(reg.overflow_in_flight());
        assert!(reg.get(&live).unwrap().overflow);
        assert!(!reg.mark_overflow("sweep-issue-404-1"), "unknown sweep");

        // A finished overflow sweep no longer holds the slot.
        insert_terminal_issue(&mut reg, "sweep-issue-8-1", 8, None);
        assert!(reg.mark_overflow("sweep-issue-8-1"));
        reg.entries.remove(&live);
        assert!(!reg.overflow_in_flight());
    }

    #[test]
    fn overflow_is_on_the_wire_only_when_true() {
        let dir = tempfile::tempdir().unwrap();
        let (mut reg, _) = fixture_registry(dir.path());
        let id = insert_running_at(&mut reg, 7, 1, chrono::Utc::now());
        let plain = serde_json::to_value(reg.get(&id).unwrap()).unwrap();
        assert!(plain.get("overflow").is_none(), "{plain}");
        reg.mark_overflow(&id);
        let over = serde_json::to_value(reg.get(&id).unwrap()).unwrap();
        assert_eq!(over["overflow"], true);
        // Older payloads (no field) decode as not-overflow.
        let mut legacy = plain.clone();
        legacy.as_object_mut().unwrap().remove("overflow");
        let decoded: crate::types::SweepInfo = serde_json::from_value(legacy).unwrap();
        assert!(!decoded.overflow);
    }
}
