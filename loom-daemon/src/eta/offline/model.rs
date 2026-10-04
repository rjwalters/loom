//! A reference fitted model: per-stage, per-age-bucket residual-life
//! quantiles from a weighted Kaplan–Meier estimate over the training rows.
//!
//! It is deliberately simple. Its job here is to give the walk-forward
//! evaluator and the future-invariance test something real to fit; richer
//! candidates (quantile regression, discrete-time hazards) are fitted
//! offline on the exported rows until one clears the promotion gate.
//!
//! Censoring is native: a censored training row contributes its elapsed time
//! to the risk set and no event, so in-flight items pull the quantiles out
//! instead of being dropped (dropping them would reward optimism).
//!
//! **Everything fitted comes from the training rows only** (rule 3): the
//! stage vocabulary, the per-stage age-bucket edges, the per-issue weights
//! (`1 / rows of that issue`, so each issue counts once however often it was
//! re-estimated) and every cell's quantiles.

use super::dataset::{assert_point_in_time, Label, LeakError, Row};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// The model's tunable settings, chosen on the selection folds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct KmSettings {
    /// Age buckets per stage (1 = stage only).
    pub age_buckets: u32,
    /// Fewest distinct issues a cell needs before it answers; below it the
    /// prediction falls back to the stage, then to every stage.
    pub min_cell_issues: u32,
}

impl KmSettings {
    /// The settings grid the selection folds choose from, in a fixed order
    /// (a tie keeps the earliest).
    #[must_use]
    pub fn grid() -> Vec<KmSettings> {
        let mut grid = Vec::new();
        for age_buckets in [1, 2, 4] {
            for min_cell_issues in [10, 30] {
                grid.push(KmSettings {
                    age_buckets,
                    min_cell_issues,
                });
            }
        }
        grid
    }

    /// `km-a<buckets>-m<min>`.
    #[must_use]
    pub fn id(&self) -> String {
        format!("km-a{}-m{}", self.age_buckets, self.min_cell_issues)
    }
}

/// One cell's fitted residual-life quantiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CellFit {
    /// Training rows in the cell.
    pub rows: usize,
    /// Distinct issues in the cell.
    pub issues: usize,
    /// Rows that landed (the rest are censored).
    pub events: usize,
    /// Remaining-time quantiles, seconds; `None` where the Kaplan–Meier
    /// curve never falls that far (too much censoring).
    pub p25: Option<i64>,
    /// Median.
    pub p50: Option<i64>,
    /// 75th percentile.
    pub p75: Option<i64>,
}

impl CellFit {
    fn usable(&self, min_issues: u32) -> Option<(i64, i64, i64)> {
        if self.issues < min_issues as usize {
            return None;
        }
        Some((self.p25?, self.p50?, self.p75?))
    }
}

/// One stage's fit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageFit {
    /// Ascending age edges, seconds: bucket `i` holds ages below
    /// `edges[i]`, the last bucket the rest. Fitted from training ages.
    pub age_edges: Vec<i64>,
    /// The whole stage.
    pub all: CellFit,
    /// One per age bucket (`age_edges.len() + 1`).
    pub buckets: Vec<CellFit>,
}

/// The fitted model and every fitted transform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KmModel {
    /// The settings it was fitted with.
    pub settings: KmSettings,
    /// The fold cutoff: every training datum was knowable before it.
    pub cutoff: DateTime<Utc>,
    /// Training rows used (abandoned rows excluded).
    pub rows: usize,
    /// Distinct training issues.
    pub issues: usize,
    /// Every stage key seen in training (`unknown` for a stageless row).
    pub stage_vocab: Vec<String>,
    /// Per-stage fits.
    pub stages: BTreeMap<String, StageFit>,
    /// Every training row, the last fallback.
    pub global: CellFit,
}

/// `(time, landed, weight)`: one weighted Kaplan–Meier observation.
type Obs = (i64, bool, f64);

fn stage_key(row: &Row) -> String {
    row.snapshot
        .stage
        .map_or_else(|| "unknown".to_string(), |s| s.as_str().to_string())
}

fn observation(row: &Row, weight: f64) -> Option<Obs> {
    match row.label {
        Label::Landed { remaining_sec, .. } => Some((remaining_sec, true, weight)),
        Label::Censored { elapsed_sec, .. } => Some((elapsed_sec.max(0), false, weight)),
        Label::Abandoned { .. } => None,
    }
}

/// Weighted Kaplan–Meier quantiles `(p25, p50, p75)`. At a tied time,
/// events are applied before censorings (the usual convention).
fn km_quantiles(obs: &mut [Obs]) -> [Option<i64>; 3] {
    obs.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut at_risk: f64 = obs.iter().map(|o| o.2).sum();
    let mut survival = 1.0_f64;
    let targets = [0.75_f64, 0.5, 0.25];
    let mut out = [None; 3];
    let mut i = 0;
    while i < obs.len() {
        let t = obs[i].0;
        let mut died = 0.0;
        let mut left = 0.0;
        while i < obs.len() && obs[i].0 == t {
            if obs[i].1 {
                died += obs[i].2;
            }
            left += obs[i].2;
            i += 1;
        }
        if at_risk > 0.0 && died > 0.0 {
            survival *= 1.0 - died / at_risk;
        }
        at_risk -= left;
        for (slot, target) in out.iter_mut().zip(targets) {
            if slot.is_none() && survival <= target + 1e-12 {
                *slot = Some(t);
            }
        }
    }
    out
}

fn fit_cell(rows: &[(&Row, f64)]) -> CellFit {
    let mut obs: Vec<Obs> = rows
        .iter()
        .filter_map(|(r, w)| observation(r, *w))
        .collect();
    let issues: BTreeSet<&str> = rows
        .iter()
        .map(|(r, _)| r.snapshot.story.as_str())
        .collect();
    let events = obs.iter().filter(|o| o.1).count();
    let [p25, p50, p75] = km_quantiles(&mut obs);
    CellFit {
        rows: rows.len(),
        issues: issues.len(),
        events,
        p25,
        p50,
        p75,
    }
}

/// Nearest-rank age edges splitting `ages` into `buckets` groups.
fn age_edges(ages: &mut [i64], buckets: u32) -> Vec<i64> {
    ages.sort_unstable();
    let mut edges: Vec<i64> = Vec::new();
    if ages.is_empty() {
        return edges;
    }
    for k in 1..buckets {
        let rank = (ages.len() * k as usize) / buckets as usize;
        let edge = ages[rank.min(ages.len() - 1)];
        if edges.last().is_none_or(|last| edge > *last) {
            edges.push(edge);
        }
    }
    edges
}

fn bucket_of(edges: &[i64], age: Option<i64>) -> usize {
    let age = age.unwrap_or(0);
    edges.iter().take_while(|edge| age >= **edge).count()
}

impl KmModel {
    /// Fit on `training`, every row of which must be knowable before
    /// `cutoff` (checked first: a leaky training set is refused, not fitted).
    pub fn fit(
        training: &[Row],
        cutoff: DateTime<Utc>,
        settings: KmSettings,
    ) -> Result<Self, LeakError> {
        assert_point_in_time(training, cutoff)?;
        let usable: Vec<&Row> = training
            .iter()
            .filter(|r| !matches!(r.label, Label::Abandoned { .. }))
            .collect();
        let mut per_issue: BTreeMap<&str, usize> = BTreeMap::new();
        for row in &usable {
            *per_issue.entry(row.snapshot.story.as_str()).or_default() += 1;
        }
        let weighted: Vec<(&Row, f64)> = usable
            .iter()
            .map(|r| (*r, 1.0 / per_issue[r.snapshot.story.as_str()] as f64))
            .collect();

        let mut by_stage: BTreeMap<String, Vec<(&Row, f64)>> = BTreeMap::new();
        for (row, w) in &weighted {
            by_stage.entry(stage_key(row)).or_default().push((*row, *w));
        }
        let mut stages = BTreeMap::new();
        for (key, rows) in &by_stage {
            let mut ages: Vec<i64> = rows
                .iter()
                .filter_map(|(r, _)| r.snapshot.age_sec)
                .collect();
            let edges = age_edges(&mut ages, settings.age_buckets.max(1));
            let mut grouped: Vec<Vec<(&Row, f64)>> = vec![Vec::new(); edges.len() + 1];
            for (row, w) in rows {
                grouped[bucket_of(&edges, row.snapshot.age_sec)].push((*row, *w));
            }
            stages.insert(
                key.clone(),
                StageFit {
                    all: fit_cell(rows),
                    buckets: grouped.iter().map(|g| fit_cell(g)).collect(),
                    age_edges: edges,
                },
            );
        }
        Ok(KmModel {
            settings,
            cutoff,
            rows: usable.len(),
            issues: per_issue.len(),
            stage_vocab: by_stage.keys().cloned().collect(),
            stages,
            global: fit_cell(&weighted),
        })
    }

    /// Remaining-time quantiles for `row`'s snapshot, or `None` when no cell
    /// on its fallback chain (age bucket → stage → every stage) can answer.
    #[must_use]
    pub fn predict(&self, row: &Row) -> Option<(i64, i64, i64)> {
        let min = self.settings.min_cell_issues;
        if let Some(stage) = self.stages.get(&stage_key(row)) {
            let bucket = bucket_of(&stage.age_edges, row.snapshot.age_sec);
            if let Some(q) = stage.buckets.get(bucket).and_then(|c| c.usable(min)) {
                return Some(q);
            }
            if let Some(q) = stage.all.usable(min) {
                return Some(q);
            }
        }
        self.global.usable(min)
    }
}
