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

impl SweepRegistry {
    /// A `role` step got past token selection with a named account: its
    /// pool hands out credentials again. Drop the role's `RoleTick` entry and
    /// re-check the workspace's pre-flight hold, as an issue dispatch past
    /// token selection does (`clear_token_selection_failures`, #6614), so
    /// one recovered role lifts a hold the step refusals tripped (#11909).
    /// Only this role's entry goes: another role's pool may still be empty.
    pub(crate) fn clear_role_tick_pool_exhausted(&mut self, role: &str) -> bool {
        let source = TokenSelectionFailureSource::RoleTick {
            root: self.config.workspace_root.clone(),
            role: role.to_string(),
        };
        if self.token_selection_failures.remove(&source).is_none() {
            return false;
        }
        log::info!(
            "sweep_registry: a {role} step got past token selection — its pool-exhausted mark \
             is cleared (#11909)"
        );
        self.update_preflight_advisory();
        true
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::fixture_registry;
    use serial_test::serial;
    use tempfile::tempdir;

    /// Three roles' exhausted pools trip the empty-pool hold; one role's
    /// recovery takes its entry out and the hold re-evaluates below the
    /// threshold (#11909).
    #[test]
    #[serial]
    fn one_recovered_role_lifts_the_hold_its_refusals_tripped() {
        let dir = tempdir().unwrap();
        let (mut reg, _) = fixture_registry(dir.path());
        for role in ["curator", "judge", "doctor"] {
            reg.record_role_tick_pool_exhausted(role);
        }
        let now = Utc::now();
        let threshold = resolve_empty_pool_breaker_threshold();
        assert_eq!(reg.token_selection_failure_count(now), 3);
        assert_eq!(reg.empty_pool_breaker_tripped(now), 3 >= threshold);
        assert_eq!(reg.pool_exhausted_roles(now).len(), 3);

        assert!(reg.clear_role_tick_pool_exhausted("judge"));
        assert!(!reg.clear_role_tick_pool_exhausted("judge"), "already cleared");
        assert_eq!(reg.token_selection_failure_count(now), 2);
        assert!(!reg.pool_exhausted_roles(now).contains("judge"));
        if threshold == 3 {
            assert!(!reg.empty_pool_breaker_tripped(now), "the hold lifts");
        }
    }
}
