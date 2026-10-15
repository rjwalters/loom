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
    /// pool hands out credentials again. Drop the role's `RoleTick` entry,
    /// then re-check the workspace's pre-flight hold whenever an entry went
    /// or the hold stands, as an issue dispatch past token selection does
    /// (`clear_token_selection_failures`, #6614).
    ///
    /// The re-check must not wait on this role having had an entry: the
    /// draw skips exhausted roles, so the hold's recovery probe is always a
    /// role without one, and the hold is a stored flag that only a re-check
    /// clears (aged-out entries included). Only this role's entry goes:
    /// another role's pool may still be empty. Returns whether one went.
    pub(crate) fn clear_role_tick_pool_exhausted(&mut self, role: &str) -> bool {
        let source = TokenSelectionFailureSource::RoleTick {
            root: self.config.workspace_root.clone(),
            role: role.to_string(),
        };
        let removed = self.token_selection_failures.remove(&source).is_some();
        if removed {
            log::info!(
                "sweep_registry: a {role} step got past token selection — its pool-exhausted \
                 mark is cleared (#11909)"
            );
        }
        if removed || self.preflight_advisory_tripped {
            self.update_preflight_advisory();
        }
        removed
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::sweep_registry::test_support::fixture_registry;
    use serial_test::serial;
    use tempfile::tempdir;

    fn exhausted_registry(dir: &Path) -> SweepRegistry {
        std::env::remove_var(EMPTY_POOL_BREAKER_THRESHOLD_ENV);
        std::env::remove_var(EMPTY_POOL_BREAKER_WINDOW_ENV);
        let (mut reg, _) = fixture_registry(dir);
        for role in ["curator", "judge", "doctor"] {
            reg.record_role_tick_pool_exhausted(role);
        }
        assert!(reg.preflight_advisory().0, "three exhausted roles hold the workspace");
        reg
    }

    /// One recovered role lifts the hold its refusals tripped (#11909).
    #[test]
    #[serial]
    fn one_recovered_role_lifts_the_stored_hold() {
        let dir = tempdir().unwrap();
        let mut reg = exhausted_registry(dir.path());
        assert!(reg.clear_role_tick_pool_exhausted("judge"));
        assert!(!reg.preflight_advisory().0, "the stored hold lifts");
        assert!(!reg.pool_exhausted_roles(Utc::now()).contains("judge"));
        assert!(!reg.clear_role_tick_pool_exhausted("judge"), "already cleared");
    }

    /// The draw skips exhausted roles, so the hold's probe is a role with no
    /// entry. Once the entries have aged out of the window, that success
    /// must still clear the stored hold.
    #[test]
    #[serial]
    fn a_success_of_an_unmarked_role_clears_an_aged_out_hold() {
        let dir = tempdir().unwrap();
        let mut reg = exhausted_registry(dir.path());
        let old =
            Utc::now() - chrono::Duration::seconds(resolve_empty_pool_breaker_window_secs() + 60);
        for at in reg.token_selection_failures.values_mut() {
            *at = old;
        }
        assert!(reg.preflight_advisory().0, "a stored flag: aging alone does not clear it");
        assert!(!reg.clear_role_tick_pool_exhausted("builder"), "builder had no entry");
        assert!(!reg.preflight_advisory().0, "the success re-checks and lifts the hold");
    }
}
