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
//! - every other Champion merge in the repo while this host or a peer is
//!   merging there (`PeerClaimView::merge_in_flight_at`, the best-effort
//!   per-repo merge serialisation of design §6).

use std::collections::HashSet;

use super::*;
use crate::step_role::StepRole;

/// The items the draw skips, and a peer's merge in flight in this repo.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrawClaims {
    pub items: HashSet<u32>,
    /// `(PR, host)` of a live peer Champion merge in this repo.
    pub merge_in_flight: Option<(u32, String)>,
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
        // This host's own live merge serialises the repo too, not only a
        // peer's.
        let own_merge = self
            .entries
            .values()
            .filter(|i| matches!(i.state, SweepState::Running | SweepState::Pending))
            .find_map(|i| match i.kind {
                SweepKind::Step {
                    item,
                    role: StepRole::ChampionMerge,
                } => Some((item, host_identity())),
                _ => None,
            });
        let merge_in_flight = own_merge.or_else(|| {
            self.peer_claims.as_ref().and_then(|view| {
                let repo = peer_claims::repo_slug(&self.config.workspace_root);
                view.lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .merge_in_flight_at(&repo, Instant::now())
            })
        });
        DrawClaims {
            items,
            merge_in_flight,
        }
    }
}
