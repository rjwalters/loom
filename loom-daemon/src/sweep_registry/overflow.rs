//! The `loom:operator-priority` overflow flag on a sweep record (#9244).
//!
//! The work finder may admit one starred issue over the concurrency caps as
//! this host's single over-limit sweep (`work_finder::OverflowSlot`). The
//! registry records that on the sweep's own [`SweepInfo::overflow`], so the
//! flag survives into `list_sweeps` / `get_sweep_status` / `loom-daemon
//! status`, and so the next tick can see the slot is still taken.
//!
//! The flag is also stamped onto the sweep's claim lock (Issue #9314) so it
//! survives a **daemon restart**: `reconstruct()` adopts a live sweep from its
//! `owner.json`, and before the lock carried the flag every adopted sweep came
//! back as `overflow: false` — silently freeing the host's single slot while
//! the over-limit sweep was still running.
//!
//! Its own file: `sweep_registry/mod.rs` is frozen by the file-size ratchet.

use super::SweepRegistry;
use crate::types::SweepKind;

impl SweepRegistry {
    /// Mark `sweep_id` as the host's overflow sweep. Returns `false` when no
    /// such sweep is registered (it already finished, or was never recorded).
    ///
    /// Also stamps the flag onto the sweep's `issue-<N>` claim lock so a daemon
    /// restart restores it (#9314). The stamp is **best-effort**: a lock that
    /// is already gone (the sweep finished between dispatch and this call) or
    /// unwritable is logged and ignored, because the in-memory mark is what
    /// this tick's accounting reads and losing the durable copy degrades to
    /// exactly the pre-#9314 behaviour rather than to anything unsafe. A
    /// non-`Issue` sweep (a `PrSet`) has no `issue-<N>` lock to stamp, and no
    /// `PrSet` dispatch ever takes the slot — the work finder only admits
    /// starred *issues* — so that arm is a silent no-op.
    ///
    /// The stamp is one small local read-modify-write of a file this registry
    /// already owns, made under the registry mutex the caller holds and with no
    /// `.await` in sight — the same shape (and the same cost) as
    /// `record_child_pid_in_lock`'s dispatch-time stamp.
    pub fn mark_overflow(&mut self, sweep_id: &str) -> bool {
        let Some(info) = self.entries.get_mut(sweep_id) else {
            return false;
        };
        info.overflow = true;
        let issue = match info.kind {
            SweepKind::Issue(n) => Some(n),
            _ => None,
        };
        if let Some(issue) = issue {
            if let Err(e) = self.stamp_overflow_in_lock(issue) {
                log::warn!(
                    "sweep_registry: could not persist the overflow flag for issue #{issue} \
                     ({e}) — the mark holds for this daemon's lifetime, but a restart will \
                     adopt the sweep without it (#9314)"
                );
            }
        }
        true
    }

    /// Whether a non-terminal sweep in this registry is marked overflow.
    #[must_use]
    pub fn overflow_in_flight(&self) -> bool {
        self.entries
            .values()
            .any(|info| info.overflow && !info.state.is_terminal())
    }

    /// The ids of the non-terminal sweeps marked overflow, i.e. the sweep
    /// holding the host's overflow slot (`fleet.state`'s `slot`, #10196).
    #[must_use]
    pub fn overflow_sweep_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, info)| info.overflow && !info.state.is_terminal())
            .map(|(id, _)| id.clone())
            .collect()
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
        assert!(reg.overflow_sweep_ids().is_empty());
        assert!(reg.mark_overflow(&live));
        assert!(reg.overflow_in_flight());
        assert_eq!(reg.overflow_sweep_ids(), vec![live.clone()]);
        assert!(reg.get(&live).unwrap().overflow);
        assert!(!reg.mark_overflow("sweep-issue-404-1"), "unknown sweep");

        // A finished overflow sweep no longer holds the slot.
        insert_terminal_issue(&mut reg, "sweep-issue-8-1", 8, None);
        assert!(reg.mark_overflow("sweep-issue-8-1"));
        reg.entries.remove(&live);
        assert!(!reg.overflow_in_flight());
        assert!(reg.overflow_sweep_ids().is_empty(), "a terminal sweep holds no slot");
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
