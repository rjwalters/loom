//! The dependency graph an estimate pass reads (#10510).
//!
//! The edges come from [`Tracker::dependencies`], filled by the caller's
//! forge reads (`observability::eta_dependency`); the nodes are the
//! tracker's own items, each as the `land` input it is estimated from this
//! pass, plus the parents the caller observed closed. A parent that is
//! neither is unknown to the graph. The graph is built only when some edge
//! was observed, so a pass with none hands every input `dependencies: None`.

use super::{EstimateContext, ItemKey, Tracker};
use crate::eta::dependency::{DependencyGraph, Node, NodeKey};
use crate::eta::labels::{pr_flags, FLAG_SEQUENCED};
use crate::eta::{Kind, NoEstimateReason};
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::sync::Arc;

/// What the caller needs to know of a tracked item to read its edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DependencyCandidate {
    /// The item.
    pub key: ItemKey,
    /// Its PR, once one exists.
    pub pr_number: Option<u32>,
    /// Not started, and refused `blocked` or `no_dispatch_plan`: a park
    /// record or a native "blocked by" may say what it waits on.
    pub parked: bool,
    /// Its PR carries `loom:sequenced`: a sequence marker names its
    /// predecessor.
    pub sequenced: bool,
}

impl Tracker {
    /// Every tracked item, as the dependency reads see it.
    #[must_use]
    pub fn dependency_candidates(&self) -> Vec<DependencyCandidate> {
        self.items
            .iter()
            .filter(|(_, item)| !item.landed)
            .map(|(key, item)| DependencyCandidate {
                key: key.clone(),
                pr_number: item.pr_number,
                parked: item.pr_number.is_none()
                    && matches!(
                        item.refused,
                        Some(NoEstimateReason::Blocked | NoEstimateReason::NoDispatchPlan)
                    ),
                sequenced: item.pr_number.is_some() && pr_flags(&item.labels) & FLAG_SEQUENCED != 0,
            })
            .collect()
    }

    /// This pass's graph, or `None` when no edge was observed.
    pub(super) fn dependency_graph(
        &self,
        ctx: &EstimateContext<'_>,
        now: DateTime<Utc>,
    ) -> Option<Arc<DependencyGraph>> {
        if self.dependencies.edges.is_empty() {
            return None;
        }
        let mut nodes = BTreeMap::new();
        for (key, item) in &self.items {
            let Some(input) = self.input_for(key, item, Kind::Land, ctx, now) else {
                continue;
            };
            let modeled = self
                .modeled_input(key, item, Kind::Land, &input, ctx)
                .map(|(view, _)| Box::new(view));
            nodes.insert(
                NodeKey::new(&key.repo, key.issue),
                Node::Open {
                    input: Box::new(input),
                    modeled,
                },
            );
        }
        for (parent, at) in &self.dependencies.landed {
            nodes
                .entry(parent.clone())
                .or_insert(Node::Landed { at: *at });
        }
        Some(Arc::new(DependencyGraph {
            edges: self.dependencies.edges.clone(),
            nodes,
        }))
    }
}
