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
//!
//! A hold belongs to the pause run that took it (its manifest id). A
//! superseded run that stands down late releases only its own holds, never
//! the ones its replacement has taken on the same sweeps (Judge finding on
//! #10974).

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

fn held() -> &'static Mutex<BTreeMap<String, String>> {
    static HELD: OnceLock<Mutex<BTreeMap<String, String>>> = OnceLock::new();
    HELD.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, BTreeMap<String, String>> {
    held()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Take over `sweep_id` for pause run `owner`: the reaper leaves it alone
/// from now on. A later run's hold replaces an earlier run's.
pub fn hold(sweep_id: &str, owner: &str) {
    lock().insert(sweep_id.to_string(), owner.to_string());
}

/// Hand `sweep_id` back to the reaper, if `owner` is the run holding it.
pub fn release(sweep_id: &str, owner: &str) {
    let mut held = lock();
    if held.get(sweep_id).is_some_and(|by| by == owner) {
        held.remove(sweep_id);
    }
}

/// Whether a roll's pause has taken over `sweep_id`.
#[must_use]
pub fn is_held(sweep_id: &str) -> bool {
    lock().contains_key(sweep_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_sweep_is_held_until_its_owner_releases_it() {
        let id = "sweep-hold-test-1";
        assert!(!is_held(id));
        hold(id, "rp-old");
        assert!(is_held(id));
        // The replacement run takes the same sweep; the old run's late
        // stand-down must not hand it back.
        hold(id, "rp-new");
        release(id, "rp-old");
        assert!(is_held(id), "a superseded run released its replacement's hold");
        release(id, "rp-new");
        assert!(!is_held(id));
    }
}
