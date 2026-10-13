//! Absolute expiries of the **peer-armed** windows the work finder skips on
//! (Issue #11564).
//!
//! [`SweepRegistry::dispatch_backoff_issues`] and
//! [`SweepRegistry::noop_cooldown_issues`] union this host's own windows with
//! the ones a peer broadcast (#7477), but the expiry maps the work finder
//! publishes as `held_until` (#9311) read the local windows only. A
//! peer-armed row was therefore held with no end on the wire: on 10-11, 7 of
//! 10 `dispatch_backoff` rows on the AWS workers. The peer ad carries the
//! remaining time, so the end is known; these turn it into an instant.

use super::*;

fn instant_after(now: DateTime<Utc>, remaining: Option<Duration>) -> Option<DateTime<Utc>> {
    chrono::Duration::from_std(remaining?).ok().map(|d| now + d)
}

impl SweepRegistry {
    /// When the live dispatch-backoff window a **peer** host armed for
    /// `issue` ends, or `None` when no peer window covers it (or no
    /// peer-claim view is attached).
    #[must_use]
    pub fn peer_dispatch_backoff_until(
        &self,
        issue: u32,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        if !self.dispatch_backoff_config.enabled {
            return None;
        }
        let view = self.peer_claims.as_ref()?;
        let repo = peer_claims::repo_slug(&self.config.workspace_root);
        let remaining = match view.lock() {
            Ok(v) => v.dispatch_backoff_remaining_at(&repo, issue, Instant::now()),
            Err(poisoned) => {
                log::error!("sweep_registry: peer-claim view mutex poisoned ({poisoned:?})");
                None
            }
        };
        instant_after(now, remaining)
    }

    /// [`Self::peer_dispatch_backoff_until`]'s sibling for the no-op
    /// cooldown.
    #[must_use]
    pub fn peer_noop_cooldown_until(
        &self,
        issue: u32,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        if !self.noop_cooldown_config.enabled {
            return None;
        }
        instant_after(now, self.fleet_noop_cooldown_remaining(issue))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::peer_claims::{ClaimAd, PeerClaimView};

    #[test]
    fn a_peer_armed_window_has_an_expiry_and_a_local_only_registry_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut registry =
            crate::sweep_registry::test_support::backoff_registry(dir.path(), 60, 900);
        let now = Utc::now();
        assert_eq!(registry.peer_dispatch_backoff_until(9001, now), None);

        let repo = peer_claims::repo_slug(&registry.config().workspace_root);
        let view =
            Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
        view.lock().unwrap().observe_dispatch_backoff_at(
            &ClaimAd::dispatch_backoff_armed(9001, repo, "peer".into(), 1, "ts".into(), 600),
            Instant::now(),
        );
        registry.set_peer_claims(view);
        let until = registry.peer_dispatch_backoff_until(9001, now).unwrap();
        // About ten minutes out; never in the past.
        assert!(until > now + chrono::Duration::seconds(590), "{until}");
        assert!(until <= now + chrono::Duration::seconds(600), "{until}");
        assert_eq!(registry.peer_dispatch_backoff_until(9002, now), None);
    }
}
