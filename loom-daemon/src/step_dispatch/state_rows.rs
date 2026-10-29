//! The rows `eta.dispatch_state` journals (#12918): every classified item of
//! the tick's non-held workspaces, pooled or weight-0.
//!
//! Pure: the step driver hands over what the tick already holds (the weighed
//! and shaped workspaces, and the ladder markers parsed from the bodies the
//! snapshot carries), so building the rows costs no forge call.

use std::collections::{BTreeMap, HashMap};

use super::item_draw::{pool_weights, PoolKnobs, PoolRow, RepoTerms};
use super::reeval_ladder::Reeval;
use super::tick::StepWorkspace;
use crate::observability::dispatch_state::StateRow;
use crate::telemetry::kinds::dispatch_state::Artifact;
use crate::work_finder::step_item::Item;

/// The number and artifact `item` is known by: the step's target, else the
/// issue, else the PR. `None` for an item with neither (never classified).
fn identity(item: &Item) -> Option<(u32, Artifact)> {
    match item.step() {
        Some(step) if step.acts_on_pr() => item.pr.map(|n| (n, Artifact::Pr)),
        Some(_) => item.issue.map(|n| (n, Artifact::Issue)),
        None => item
            .issue
            .map(|n| (n, Artifact::Issue))
            .or_else(|| item.pr.map(|n| (n, Artifact::Pr))),
    }
}

/// One row per classified item of every non-halted workspace. `reevals` are
/// each workspace's ladder markers by item number (keyed by workspace index);
/// `knobs` weigh the host-wide pool the way the draw does.
#[must_use]
pub fn state_rows(
    workspaces: &[StepWorkspace],
    reevals: &HashMap<usize, HashMap<u32, Reeval>>,
    knobs: &PoolKnobs,
) -> Vec<StateRow> {
    let live: Vec<&StepWorkspace> = workspaces.iter().filter(|w| !w.halted).collect();
    let repos: BTreeMap<usize, RepoTerms> = live
        .iter()
        .map(|w| {
            (
                w.idx,
                RepoTerms {
                    weight: w.weight,
                    in_flight: w.in_flight,
                },
            )
        })
        .collect();
    let candidates: Vec<_> = live.iter().flat_map(|w| w.weighing.pool.iter()).collect();
    let pool_rows: Vec<PoolRow> = candidates
        .iter()
        .map(|c| PoolRow {
            repo: c.workspace_idx,
            item_weight: c.terms.weight,
        })
        .collect();
    let weights = pool_weights(&pool_rows, &repos, knobs).weights;

    let reeval_of = |ws_idx: usize, number: u32| {
        reevals
            .get(&ws_idx)
            .and_then(|m| m.get(&number))
            .map(|r| (r.rung, r.next))
    };

    let mut rows = Vec::new();
    for (c, weight) in candidates.iter().zip(weights) {
        let Some((number, artifact)) = identity(&c.item) else {
            continue;
        };
        let reeval = reeval_of(c.workspace_idx, number);
        rows.push(StateRow {
            ws_idx: c.workspace_idx,
            number,
            artifact,
            step: Some(c.step.as_str().to_string()),
            in_pool: true,
            exclusion: None,
            gate_until: None,
            reeval_rung: reeval.map(|(rung, _)| rung),
            reeval_next: reeval.map(|(_, next)| next),
            iw: Some(c.terms.weight),
            weight: Some(weight),
            issue: c.item.issue,
            doctor_rejections: None,
            doctor_escalated: None,
        });
    }
    for w in &live {
        for e in &w.weighing.excluded_items {
            let Some((number, artifact)) = identity(&e.item) else {
                continue;
            };
            let reeval = reeval_of(w.idx, number);
            rows.push(StateRow {
                ws_idx: w.idx,
                number,
                artifact,
                step: e.item.step().map(|s| s.as_str().to_string()),
                in_pool: false,
                exclusion: Some(e.exclusion.as_str().to_string()),
                gate_until: e.gate_until,
                reeval_rung: reeval.map(|(rung, _)| rung),
                reeval_next: reeval.map(|(_, next)| next),
                iw: None,
                weight: None,
                issue: e.item.issue,
                doctor_rejections: None,
                doctor_escalated: None,
            });
        }
    }
    rows
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "state_rows_tests.rs"]
mod tests;
