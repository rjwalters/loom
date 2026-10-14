//! Which step roles this workspace's token pool is exhausted for (#11909).
//!
//! The draw gives a step weight 0 when its role's pool has no spawnable
//! account (`item_draw::WeighContext::pool_exhausted_roles`), because the
//! spawn would only be refused and burn the redraw budget. The fact lives in
//! the registry's token-selection failure map, keyed
//! `TokenSelectionFailureSource::RoleTick { root, role }`: a role tick or a
//! step that hit an empty pool records it
//! ([`SweepRegistry::record_role_tick_pool_exhausted`]), and any dispatch
//! that gets past token selection clears the map. The map is private to the
//! registry and `dispatch.rs` is frozen by the size ratchet, so the read is
//! here.

use super::*;

impl SweepRegistry {
    /// The roles of this workspace's live `RoleTick` entries at `now`: those
    /// inside the empty-pool breaker's trailing window, the same window the
    /// breaker counts them in.
    #[must_use]
    pub fn pool_exhausted_roles(&self, now: DateTime<Utc>) -> std::collections::BTreeSet<String> {
        let window = chrono::Duration::seconds(resolve_empty_pool_breaker_window_secs());
        let root = &self.config.workspace_root;
        self.token_selection_failures
            .iter()
            .filter(|(_, at)| now - **at <= window)
            .filter_map(|(source, _)| match source {
                TokenSelectionFailureSource::RoleTick { root: r, role } if r == root => {
                    Some(role.clone())
                }
                _ => None,
            })
            .collect()
    }
}
