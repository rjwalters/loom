//! The subset breakdown every backtest report carries (#10524): coverage,
//! late surprise and loss on the **starred**, **held** and **sequenced**
//! cases, apart from the whole.
//!
//! # Why
//!
//! #10524's acceptance asks that p25–p75 coverage hold on those three
//! subsets, or that the docs say where it does not. A pooled figure can sit
//! in the band while a subset is far outside it (a held PR waits on a human,
//! a sequenced one on its predecessor), so each subset is reported on its
//! own, with the late-surprise rate (`actual > p90`) beside its coverage.
//!
//! # Membership, point-in-time
//!
//! | key | a case is in it when |
//! |---|---|
//! | [`SUBSET_HELD`] | its replay stage is `merge_hold` (an approved PR under a hold), the state `land-2026-10-06-held-heron` answers as held |
//! | [`SUBSET_STARRED`] | its PR's own labels at `as_of` carry a star at any level ([`FLAG_STARRED`]) |
//! | [`SUBSET_SEQUENCED`] | its PR's own labels at `as_of` carry `loom:sequenced` ([`FLAG_SEQUENCED`]) |
//! | [`SUBSET_LABELS_KNOWN`] | its source reconstructed the PR's labels at all: the population the two label subsets are drawn from |
//!
//! The labels are the ones in force at the case's own `as_of`
//! ([`super::ReplayCase::pr_flags`]), never later ones, so a star added after
//! the replay instant does not move the case into the starred subset. Only a
//! forge label-timeline case knows its labels; a `sweep.outcome` case does
//! not, so it counts in no label subset (and not in
//! [`SUBSET_LABELS_KNOWN`]): compare a label subset with
//! [`SUBSET_LABELS_KNOWN`], not with `overall`. A star that reaches the PR
//! only through its linked issue is not on the PR's own labels and is not
//! seen by [`SUBSET_STARRED`]; [`SUBSET_STARRED_ANY`] (below) sees it.
//!
//! The flags select cases; they are not fed to the estimator, whose replay
//! input carries no labels (`backtest.rs`, `case_input`), so this breakdown
//! never changes a replayed answer.
//!
//! # The linked-issue-aware star subsets (#10508)
//!
//! About half the starred PRs are starred only through their linked issue,
//! which [`SUBSET_STARRED`] cannot see. #10508's acceptance reads late
//! surprise on starred and unstarred items, so three more subsets split the
//! cases by the `eta-fit/v2` input `starred_any`
//! ([`super::ReplayCase::priority`]), built point-in-time by the one
//! priority-input builder (PR or linked-issue star, read strictly before
//! `as_of`):
//!
//! | key | a case is in it when |
//! |---|---|
//! | [`SUBSET_STARRED_ANY`] | `starred_any` is known on |
//! | [`SUBSET_UNSTARRED_ANY`] | `starred_any` is known off |
//! | [`SUBSET_STAR_ANY_UNKNOWN`] | the case carries priority inputs but `starred_any` is unknown (no label shows a star and the linked-issue history does not cover `as_of`) |
//!
//! A case with no priority inputs at all (`priority: None`, any source but
//! [`super::cases_from_pr_records_with_roster`]) is in none of the three. An
//! unknown star is never counted as unstarred. [`SUBSET_STARRED`] keeps its
//! meaning (the PR's own label only).
//!
//! # Paired, per subset
//!
//! `eta backtest --compare` also pairs the two heuristics inside each subset
//! ([`paired_subsets_of`], [`PairedSubset`]): the deciding loss and its
//! issue-bootstrap interval, and both late-surprise rates, each over the
//! cases both sides decided. That is the figure #10508's decision rule
//! reads for starred and unstarred items. It is a report only; the ranking
//! ([`super::Comparison::better`]) is unchanged.

use super::super::labels::{FLAG_SEQUENCED, FLAG_STARRED};
use super::super::offline::evaluate::Estimate;
use super::super::score::Score;
use super::super::Stage;
use super::paired::paired_of;
use super::{bucket_of, Bucket, ReplayCase, Replayed};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// `by_subset` key: the case's stage is `merge_hold`.
pub const SUBSET_HELD: &str = "held";

/// `by_subset` key: the PR's labels at `as_of` carry a star.
pub const SUBSET_STARRED: &str = "starred";

/// `by_subset` key: the PR's labels at `as_of` carry `loom:sequenced`.
pub const SUBSET_SEQUENCED: &str = "sequenced";

/// `by_subset` key: every case whose PR labels at `as_of` are known.
pub const SUBSET_LABELS_KNOWN: &str = "labels_known";

/// `by_subset` key: starred through the PR's own labels or its linked issue
/// at `as_of` (`starred_any` known on, #10508).
pub const SUBSET_STARRED_ANY: &str = "starred_any";

/// `by_subset` key: `starred_any` known off at `as_of` (#10508).
pub const SUBSET_UNSTARRED_ANY: &str = "unstarred_any";

/// `by_subset` key: the case carries priority inputs, but `starred_any` is
/// unknown at `as_of` (#10508). Never counted as unstarred.
pub const SUBSET_STAR_ANY_UNKNOWN: &str = "star_any_unknown";

/// One subset's aggregate: the [`Bucket`] figures plus its late surprise.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubsetBucket {
    /// Cases, scored, refused, mean pinball loss, p25–p75 coverage, bias.
    #[serde(flatten)]
    pub bucket: Bucket,
    /// Scored cases whose late surprise is decided (the estimate had a p90).
    pub late_decided: usize,
    /// Fraction of those whose actual fell after p90. `None` when none was
    /// decided.
    pub late_rate: Option<f64>,
}

fn subset_of(scores: &[&Score]) -> SubsetBucket {
    let decided: Vec<bool> = scores.iter().filter_map(|s| s.above_p90).collect();
    let late_decided = decided.len();
    let late_rate = (late_decided > 0)
        .then(|| decided.iter().filter(|late| **late).count() as f64 / late_decided as f64);
    SubsetBucket {
        bucket: bucket_of(scores),
        late_decided,
        late_rate,
    }
}

/// Every subset `case` belongs to, in a fixed order. The one membership
/// rule both [`subsets_of`] and [`paired_subsets_of`] read.
pub(super) fn memberships(case: &ReplayCase) -> Vec<&'static str> {
    let mut keys = Vec::new();
    if case.stage == Stage::MergeHold {
        keys.push(SUBSET_HELD);
    }
    if let Some(flags) = case.pr_flags {
        keys.push(SUBSET_LABELS_KNOWN);
        if flags & FLAG_STARRED != 0 {
            keys.push(SUBSET_STARRED);
        }
        if flags & FLAG_SEQUENCED != 0 {
            keys.push(SUBSET_SEQUENCED);
        }
    }
    if let Some(priority) = &case.priority {
        keys.push(match priority.starred_any {
            Some(true) => SUBSET_STARRED_ANY,
            Some(false) => SUBSET_UNSTARRED_ANY,
            None => SUBSET_STAR_ANY_UNKNOWN,
        });
    }
    keys
}

/// The subsets `replayed` has members in, each with its aggregate. Empty
/// when no case is held, no case knows its labels and none carries priority
/// inputs, so a report over sources that carry none of them is unchanged.
pub(super) fn subsets_of(replayed: &[Replayed]) -> BTreeMap<String, SubsetBucket> {
    let mut members: BTreeMap<&'static str, Vec<&Score>> = BTreeMap::new();
    for r in replayed {
        for key in memberships(&r.case) {
            members.entry(key).or_default().push(&r.score);
        }
    }
    members
        .into_iter()
        .map(|(key, scores)| (key.to_string(), subset_of(&scores)))
        .collect()
}

/// Two heuristics paired inside one subset: the figures of
/// [`super::Paired`] that a subset decision reads, each over the cases of
/// the subset both sides decided. `a` is the incumbent, `b` the challenger,
/// as in [`super::Paired`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PairedSubset {
    /// Cases of the subset in the union.
    pub cases: usize,
    /// `a`'s answer rate over them.
    pub a_answer_rate: Option<f64>,
    /// `b`'s.
    pub b_answer_rate: Option<f64>,
    /// Cases both scored with a p90 (the deciding loss's population).
    pub loss4_pairs: usize,
    /// `a`'s mean `pinball4_loss_sec` over them.
    pub a_mean_pinball4_loss_sec: Option<f64>,
    /// `b`'s.
    pub b_mean_pinball4_loss_sec: Option<f64>,
    /// `b − a` mean `pinball4_loss_sec`, with its 95% issue-bootstrap
    /// interval. Negative favours `b`.
    pub delta_pinball4_loss_sec: Option<Estimate>,
    /// Distinct issues behind it: the subset's independence count.
    pub delta4_items: usize,
    /// Cases whose late surprise is decided on both sides.
    pub late_pairs: usize,
    /// `a`'s late-surprise rate (`actual > p90`) over them.
    pub a_late_rate: Option<f64>,
    /// `b`'s.
    pub b_late_rate: Option<f64>,
}

/// [`PairedSubset`] for every subset with a member, from `a` and `b`
/// replayed over the same cases in the same order. Empty when no case is in
/// any subset.
pub(super) fn paired_subsets_of(a: &[Replayed], b: &[Replayed]) -> BTreeMap<String, PairedSubset> {
    debug_assert_eq!(a.len(), b.len());
    let mut members: BTreeMap<&'static str, (Vec<Replayed>, Vec<Replayed>)> = BTreeMap::new();
    for (ra, rb) in a.iter().zip(b) {
        for key in memberships(&ra.case) {
            let entry = members.entry(key).or_default();
            entry.0.push(ra.clone());
            entry.1.push(rb.clone());
        }
    }
    members
        .into_iter()
        .map(|(key, (sa, sb))| {
            let p = paired_of(&sa, &sb);
            let subset = PairedSubset {
                cases: p.cases,
                a_answer_rate: p.a_answer_rate,
                b_answer_rate: p.b_answer_rate,
                loss4_pairs: p.loss4_pairs,
                a_mean_pinball4_loss_sec: p.a_mean_pinball4_loss_sec,
                b_mean_pinball4_loss_sec: p.b_mean_pinball4_loss_sec,
                delta_pinball4_loss_sec: p.delta_pinball4_loss_sec,
                delta4_items: p.delta4_items,
                late_pairs: p.late_pairs,
                a_late_rate: p.a_late_rate,
                b_late_rate: p.b_late_rate,
            };
            (key.to_string(), subset)
        })
        .collect()
}
