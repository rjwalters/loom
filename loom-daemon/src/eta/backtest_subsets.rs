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
//! seen here.
//!
//! The flags select cases; they are not fed to the estimator, whose replay
//! input carries no labels (`backtest.rs`, `case_input`), so this breakdown
//! never changes a replayed answer.

use super::super::labels::{FLAG_SEQUENCED, FLAG_STARRED};
use super::super::score::Score;
use super::super::Stage;
use super::{bucket_of, Bucket, Replayed};
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

/// The subsets `replayed` has members in, each with its aggregate. Empty
/// when no case is held and no case knows its labels, so a report over
/// sources that carry no labels (and no hold) is unchanged.
pub(super) fn subsets_of(replayed: &[Replayed]) -> BTreeMap<String, SubsetBucket> {
    let mut members: BTreeMap<&'static str, Vec<&Score>> = BTreeMap::new();
    for r in replayed {
        if r.case.stage == Stage::MergeHold {
            members.entry(SUBSET_HELD).or_default().push(&r.score);
        }
        if let Some(flags) = r.case.pr_flags {
            members
                .entry(SUBSET_LABELS_KNOWN)
                .or_default()
                .push(&r.score);
            if flags & FLAG_STARRED != 0 {
                members.entry(SUBSET_STARRED).or_default().push(&r.score);
            }
            if flags & FLAG_SEQUENCED != 0 {
                members.entry(SUBSET_SEQUENCED).or_default().push(&r.score);
            }
        }
    }
    members
        .into_iter()
        .map(|(key, scores)| (key.to_string(), subset_of(&scores)))
        .collect()
}
