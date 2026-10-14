//! Steps in the held view (Issue #11898).
//!
//! A live step is held work on this host like a sweep is. Its stage comes
//! from its role, not a checkpoint, and a PR step (Judge, Doctor, Champion
//! merge) is listed under its PR's linked issue, so it shares one row with the
//! review listing's view of that PR instead of adding a second row keyed by
//! the PR number.

use std::collections::BTreeMap;

use super::{FleetInput, FleetStage};
use crate::step_role::StepRole;

/// The `fleet.state` stage a step on its role holds its item in.
///
/// Champion promotion has no held stage of its own (`approval_wait` is an
/// ETA-only stage, never a `fleet.state` row), so it reads as the pre-ready
/// `sweep.curator` stage, the one a sweep holds an issue in before it is ready.
#[must_use]
pub fn step_stage(role: StepRole) -> FleetStage {
    match role {
        StepRole::Curator | StepRole::ChampionPromote => FleetStage::SweepCurator,
        StepRole::Builder => FleetStage::SweepBuilder,
        StepRole::Judge => FleetStage::ReviewWait,
        StepRole::Doctor => FleetStage::Doctor,
        StepRole::ChampionMerge => FleetStage::MergeWait,
    }
}

/// The issue each listed PR links, by `(repo, pr)`.
fn pr_links(input: &FleetInput) -> BTreeMap<(&str, u32), u32> {
    input
        .listings
        .iter()
        .flat_map(|listing| {
            listing
                .prs
                .iter()
                .filter_map(move |pr| Some(((listing.repo.as_str(), pr.number), pr.issue?)))
        })
        .collect()
}

/// The row key of each held entry: its issue, except a PR step's row (the PR
/// named as both `issue` and `pr`), which takes its PR's linked issue when the
/// listings know it. A PR number never equals an issue number on one forge.
#[must_use]
pub(super) fn held_row_keys(input: &FleetInput) -> Vec<u32> {
    let links = pr_links(input);
    input
        .held
        .iter()
        .map(|held| match held.pr {
            Some(pr) if pr == held.issue => links
                .get(&(held.repo.as_str(), pr))
                .copied()
                .unwrap_or(held.issue),
            _ => held.issue,
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::{build_view, HeldSweep, ListedPr, RepoListing};
    use super::*;
    use chrono::Utc;

    fn held(item: u32, role: StepRole) -> HeldSweep {
        HeldSweep {
            repo: "o/r".into(),
            issue: item,
            stage: step_stage(role),
            entered_at: Utc::now(),
            entered_at_lower_bound: false,
            pr: role.acts_on_pr().then_some(item),
            overflow: false,
            start: crate::telemetry::SweepStartFacts::default(),
            sweep_id: Some(format!("step-{role}-{item}-1")),
        }
    }

    /// The held view sees steps: an issue step under its issue, a PR step
    /// under its PR's linked issue, each in its role's stage.
    #[test]
    fn held_view_lists_steps_in_their_role_stage_and_rekeys_pr_steps() {
        let input = FleetInput {
            host_id: "h".into(),
            held: vec![held(10, StepRole::Builder), held(21, StepRole::Judge)],
            listings: vec![RepoListing {
                repo: "o/r".into(),
                prs: vec![ListedPr {
                    number: 21,
                    labels: vec!["loom:review-requested".into()],
                    issue: Some(20),
                }],
            }],
            ..FleetInput::default()
        };
        let view = build_view(&input, None, Utc::now());
        let rows = &view.repos["o/r"].rows;
        assert_eq!(rows[&10].stage, FleetStage::SweepBuilder);
        assert_eq!(rows[&20].stage, FleetStage::ReviewWait);
        assert_eq!(rows[&20].pr, Some(21));
        assert_eq!(rows[&20].host.as_deref(), Some("h"), "the step's row wins as held");
        assert!(!rows.contains_key(&21), "no second row keyed by the PR number");
    }

    #[test]
    fn every_step_role_has_a_held_stage() {
        for role in StepRole::ALL {
            let _ = step_stage(role);
        }
        assert_eq!(step_stage(StepRole::Doctor), FleetStage::Doctor);
        assert_eq!(step_stage(StepRole::ChampionMerge), FleetStage::MergeWait);
    }
}
