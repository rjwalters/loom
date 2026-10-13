//! [`WorkspaceHealthStates::find`] — the exact-key-then-canonical-path lookup
//! (#11565).
//!
//! Extracted from `main_health_gate.rs` because that file is over
//! `.loom/docs/file-size-policy.md`'s threshold and therefore FROZEN — it may
//! shrink, not grow. This is the same pattern [`super::owned_paths`] set:
//! new surface gets its own sibling module instead of re-growing a
//! 1,800-line file.
//!
//! Inherent impls may live in any module of the defining crate, so callers
//! are unaffected — the method stays `WorkspaceHealthStates::find`.

use std::path::Path;
use std::sync::Arc;

use super::{MainHealthState, WorkspaceHealthStates};

impl WorkspaceHealthStates {
    /// `root`'s [`MainHealthState`] if the gate has ever seen it, else `None`
    /// (#11565). Tries the exact key first, then falls back to comparing
    /// canonical paths, so a reader whose root differs from the gate loop's
    /// only by a symlink, `..` or trailing slash (the registry stores
    /// normalized roots, the workspace pool keys by the caller's literal
    /// value) still finds the state. Never creates an entry.
    #[must_use]
    pub fn find(&self, root: &Path) -> Option<Arc<MainHealthState>> {
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(s) = map.get(root) {
            return Some(s.clone());
        }
        let wanted = std::fs::canonicalize(root).ok()?;
        map.iter()
            .find(|(k, _)| std::fs::canonicalize(k).is_ok_and(|c| c == wanted))
            .map(|(_, s)| s.clone())
    }
}
