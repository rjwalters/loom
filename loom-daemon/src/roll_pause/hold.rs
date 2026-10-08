//! Sweeps a roll's H4 pause has taken over (issue #10831; design
//! `docs/design/daemon-roll-pause-resume.md` §7 step 9, §8).
//!
//! H4 stops each agent's process tree itself and must leave the sweep's claim
//! lock (`owner.json`), journal entry and checkpoint in place: they are the
//! fallback for a binary that cannot read the pause manifest. The sweep
//! reaper would undo that. It sees the dead child on its next tick and runs
//! the ordinary terminal transition: release the lock, restore the label, or
//! crash-resume the sweep. So H4 lists every manifest item here **before it
//! signals anything**, and the reaper skips a listed sweep. The process exits
//! at the end of H4, which is what clears the set; a pause that stands down
//! (an operator abort or promotion before anything was stopped) releases its
//! holds so the reaper handles those sweeps normally again.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};

fn held() -> &'static Mutex<BTreeSet<String>> {
    static HELD: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn lock() -> std::sync::MutexGuard<'static, BTreeSet<String>> {
    held()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Take over `sweep_id`: the reaper leaves it alone from now on.
pub fn hold(sweep_id: &str) {
    lock().insert(sweep_id.to_string());
}

/// Hand `sweep_id` back to the reaper (a pause that stood down).
pub fn release(sweep_id: &str) {
    lock().remove(sweep_id);
}

/// Whether a roll's pause has taken over `sweep_id`.
#[must_use]
pub fn is_held(sweep_id: &str) -> bool {
    lock().contains(sweep_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_sweep_is_held_until_released() {
        let id = "sweep-hold-test-1";
        assert!(!is_held(id));
        hold(id);
        assert!(is_held(id));
        release(id);
        assert!(!is_held(id));
    }
}
