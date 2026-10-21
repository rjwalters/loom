//! What the step draw must not pick because someone already works it
//! (#11909, the integrated review's B1 and M5).
//!
//! The classifier's `claimed` pass reads labels only, and a non-Builder step
//! labels its item late (the prompt applies the claim label; a Champion
//! merge never does). Without this every host would redraw an item a peer or
//! this host already runs, every tick, and post lease and yield comments on
//! it. So before the draw the tick removes:
//!
//! - every item this registry runs (a live sweep or step);
//! - every item a peer advertises a live claim on
//!   (`PeerClaimView::claimed_issues_at`);
//! - every other Champion merge in the repo while this host and the peers are
//!   already merging there at the repo's merge concurrency, one by default
//!   (`PeerClaimView::merges_in_flight_at`, the best-effort per-repo merge
//!   serialisation of design §6, `champion.mergeConcurrencyPerRepo` #13026).

use std::collections::HashSet;

use super::*;
use crate::step_role::StepRole;

/// The items the draw skips, and the merges in flight in this repo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrawClaims {
    pub items: HashSet<u32>,
    /// `(PR, host)` of every live Champion merge in this repo, this host's
    /// and the peers', by PR number.
    pub merges_in_flight: Vec<(u32, String)>,
    /// `champion.mergeConcurrencyPerRepo` (#13026): the merges the repo may
    /// run at once. Set by the tick from config, not by the registry; `0`
    /// (the default) reads as `1`, today's serial lane.
    pub merge_concurrency: usize,
}

impl DrawClaims {
    /// The repo's merge concurrency, never below one.
    #[must_use]
    pub fn merge_limit(&self) -> usize {
        self.merge_concurrency.max(1)
    }

    /// How many more merges the repo may start: its limit less the merges in
    /// flight, saturating.
    #[must_use]
    pub fn merge_slots_free(&self) -> usize {
        self.merge_limit()
            .saturating_sub(self.merges_in_flight.len())
    }
}

impl SweepRegistry {
    /// This registry's and its peers' live claims, for the draw.
    #[must_use]
    pub fn draw_claims(&self) -> DrawClaims {
        let mut items: HashSet<u32> = self
            .entries
            .values()
            .filter(|i| matches!(i.state, SweepState::Running | SweepState::Pending))
            .filter_map(|i| match i.kind {
                SweepKind::Issue(n) | SweepKind::Step { item: n, .. } => Some(n),
                SweepKind::PrSet(_) => None,
            })
            .collect();
        items.extend(self.peer_claimed_issues());
        // This host's own live merges count toward the repo's lane too, not
        // only a peer's.
        let mut merges_in_flight: Vec<(u32, String)> = self
            .entries
            .values()
            .filter(|i| matches!(i.state, SweepState::Running | SweepState::Pending))
            .filter_map(|i| match i.kind {
                SweepKind::Step {
                    item,
                    role: StepRole::ChampionMerge,
                } => Some((item, host_identity())),
                _ => None,
            })
            .collect();
        if let Some(view) = self.peer_claims.as_ref() {
            let repo = peer_claims::repo_slug(&self.config.workspace_root);
            let peers = view
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .merges_in_flight_at(&repo, Instant::now());
            for (pr, host) in peers {
                if !merges_in_flight.iter().any(|(own, _)| *own == pr) {
                    merges_in_flight.push((pr, host));
                }
            }
        }
        merges_in_flight.sort();
        DrawClaims {
            items,
            merges_in_flight,
            ..DrawClaims::default()
        }
    }
}
