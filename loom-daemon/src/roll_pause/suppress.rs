//! Paused agents the next start must not treat as dead (issue #10832; design
//! `docs/design/daemon-roll-pause-resume.md` §7 H5 "Entry", §8).
//!
//! H4 stops every agent and leaves its claim lock, journal entry and checkpoint
//! in place, with a dead pid. To the restart-recovery passes that is exactly a
//! crashed daemon-owned sweep, and they would act on it at once: `reconstruct`
//! drops the stale lock and turns the checkpoint into a `Crashed` entry the
//! reaper resumes from scratch or requeues; claim reconciliation reclaims the
//! `loom:building` claim on the dead journal pid; the orphan-process and
//! worktree reapers see a worktree nobody owns. Each of those would beat H5 to
//! the item and lose the same-session resume the roll promised.
//!
//! So a process that starts and finds a live pause manifest **arms** this gate
//! before any of those passes run, and H5 **disarms** it when every item has
//! been resumed or requeued. While it is armed:
//!
//! | Pass | What it does for a held item |
//! |---|---|
//! | `SweepRegistry::reconstruct` | leaves the stale lock in place; no `Crashed` entry |
//! | `live_claim::probe` | reports the item as claimed, which vetoes claim reconciliation's reclaim, refuses a fresh dispatch of the issue and makes the orphan-process reaper protect the worktree |
//! | `worktree_ops::liveness::active_spawn_loop_issues` | lists the issue as active, so the worktree reaper keeps the worktree |
//! | sweep reaper | skips the item's sweep id ([`super::hold`]) |
//!
//! The gate is process-global because the passes it guards are reached from
//! many call sites that share no handle. It is keyed by manifest id so two
//! holders (in practice: tests) never clear each other's items. Every lookup
//! returns at once, without taking the lock, while nothing is armed, which is
//! every process that did not start from a pause.
//!
//! [`host_verified`] is the "this host is back at H0" signal: `false` from the
//! moment a live manifest is found until H5 has passed health probation and
//! finished with the manifest. A startup step that must not run on an
//! unverified binary (the per-repo resync of #10971) waits on it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// One manifest item the gate holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldItem {
    /// The manifest item id (the sweep id for a sweep).
    pub id: String,
    /// The owning workspace root.
    pub repo: PathBuf,
    /// The claimed issue, for an issue sweep.
    pub issue: Option<u32>,
}

static ARMED: AtomicUsize = AtomicUsize::new(0);
static LIVE: Mutex<BTreeMap<String, Vec<HeldItem>>> = Mutex::new(BTreeMap::new());

fn live() -> std::sync::MutexGuard<'static, BTreeMap<String, Vec<HeldItem>>> {
    LIVE.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Canonical form of a workspace root, for comparison. A root that no longer
/// resolves compares by its spelling.
fn canon(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Arm the gate for `manifest_id`'s items, replacing any earlier set under the
/// same id. Each sweep item is also taken over from the sweep reaper.
pub fn arm(manifest_id: &str, items: Vec<HeldItem>) {
    let items: Vec<HeldItem> = items
        .into_iter()
        .map(|i| HeldItem {
            repo: canon(&i.repo),
            ..i
        })
        .collect();
    for item in &items {
        super::hold::hold(&item.id, manifest_id);
    }
    let mut map = live();
    map.insert(manifest_id.to_string(), items);
    ARMED.store(map.len(), Ordering::SeqCst);
}

/// Hand one item back to ordinary recovery (H5 resumed or requeued it).
pub fn release_item(manifest_id: &str, item_id: &str) {
    let mut map = live();
    if let Some(items) = map.get_mut(manifest_id) {
        items.retain(|i| i.id != item_id);
    }
    drop(map);
    super::hold::release(item_id, manifest_id);
}

/// Disarm the gate for `manifest_id`: every item it still holds goes back to
/// ordinary recovery.
pub fn disarm(manifest_id: &str) {
    let mut map = live();
    let items = map.remove(manifest_id).unwrap_or_default();
    ARMED.store(map.len(), Ordering::SeqCst);
    drop(map);
    for item in items {
        super::hold::release(&item.id, manifest_id);
    }
}

/// Whether any manifest is armed.
#[must_use]
pub fn is_armed() -> bool {
    ARMED.load(Ordering::SeqCst) > 0
}

/// Whether `manifest_id` is armed.
#[must_use]
pub fn is_armed_for(manifest_id: &str) -> bool {
    is_armed() && live().contains_key(manifest_id)
}

/// The host is back at H0: no pause manifest is waiting to be verified and
/// resumed by this process. `false` from startup until H5 completes.
#[must_use]
pub fn host_verified() -> bool {
    !is_armed()
}

/// The manifest holding `issue` of the workspace at `root`, if any.
#[must_use]
pub fn held_issue(root: &Path, issue: u32) -> Option<String> {
    if !is_armed() {
        return None;
    }
    let root = canon(root);
    live().iter().find_map(|(id, items)| {
        items
            .iter()
            .any(|i| i.issue == Some(issue) && i.repo == root)
            .then(|| id.clone())
    })
}

/// Every issue of the workspace at `root` that a live manifest holds.
#[must_use]
pub fn held_issues(root: &Path) -> Vec<u32> {
    if !is_armed() {
        return Vec::new();
    }
    let root = canon(root);
    live()
        .values()
        .flatten()
        .filter(|i| i.repo == root)
        .filter_map(|i| i.issue)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn item(id: &str, repo: &Path, issue: u32) -> HeldItem {
        HeldItem {
            id: id.to_string(),
            repo: repo.to_path_buf(),
            issue: Some(issue),
        }
    }

    #[test]
    fn an_armed_item_is_held_until_it_is_released_or_the_manifest_is_disarmed() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let id = "rp-suppress-test-1";
        arm(id, vec![item("s-sup-1", a.path(), 7), item("s-sup-2", a.path(), 8)]);
        assert!(is_armed_for(id) && !host_verified());
        assert_eq!(held_issue(a.path(), 7).as_deref(), Some(id));
        assert_eq!(held_issue(b.path(), 7), None, "another workspace's #7 is not held");
        assert_eq!(held_issue(a.path(), 9), None);
        assert!(crate::roll_pause::hold::is_held("s-sup-1"), "the reaper is held off");
        let mut held = held_issues(a.path());
        held.sort_unstable();
        assert_eq!(held, vec![7, 8]);

        release_item(id, "s-sup-1");
        assert_eq!(held_issue(a.path(), 7), None);
        assert!(!crate::roll_pause::hold::is_held("s-sup-1"));
        assert_eq!(held_issue(a.path(), 8).as_deref(), Some(id));

        disarm(id);
        assert!(!is_armed_for(id));
        assert_eq!(held_issue(a.path(), 8), None);
        assert!(!crate::roll_pause::hold::is_held("s-sup-2"));
    }

    #[test]
    fn two_manifests_do_not_clear_each_other() {
        let a = tempfile::tempdir().unwrap();
        arm("rp-suppress-test-2a", vec![item("s-sup-3", a.path(), 1)]);
        arm("rp-suppress-test-2b", vec![item("s-sup-4", a.path(), 2)]);
        disarm("rp-suppress-test-2a");
        assert_eq!(held_issue(a.path(), 1), None);
        assert_eq!(held_issue(a.path(), 2).as_deref(), Some("rp-suppress-test-2b"));
        disarm("rp-suppress-test-2b");
    }
}
