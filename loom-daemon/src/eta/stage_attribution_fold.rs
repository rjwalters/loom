//! The nightly stage-error rollup (#10957): the pure half. [`rollup`] folds
//! [`AttributionRow`]s (read by the caller from [`super::attribution_log`])
//! into `eta.stage_attribution` records for one UTC day.
//!
//! - **Window.** `[cutoff − 7d, cutoff)`, `cutoff` being the end of the day,
//!   exclusive like every other nightly fold read. A row enters only when it
//!   was both resolved (`actual_at`) and scored by this daemon
//!   (`observed_at`) before the cutoff, so a later outcome, or a late write
//!   of an earlier one, leaves the day's records bit-identical.
//! - **Exactly once.** A row is one estimate, but the unit counted is the
//!   resolved case, as in the nightly cohort rule: one outcome resolves every
//!   pending estimate of its series (each refresh), so the rows of one
//!   heuristic sharing `(repo, issue, pr_number, actual_at)` are one case and
//!   count once, the earliest-predicted (`as_of`) winning. Distinct PRs, or
//!   laps (a different `actual_at`), on one issue stay distinct cases.
//!   Duplicates in the log (a retried append) collapse the same way.
//!   Each day's records are persisted once (`nightly_folds::day_path`), so a
//!   restart does not re-fold or duplicate them.
//! - **Row budget.** One record per registered heuristic per [`Stage`] plus
//!   one `unattributed`, with `n = 0` and no statistics where nothing
//!   landed: `heuristics × 8`, independent of the number of outcomes.
//! - **Dominant share.** A stage's `dominant_share` is the share of *all*
//!   the heuristic's counted outcomes in the window whose dominant stage it
//!   was, not of only the outcomes that visited the stage (`n`), so a
//!   heuristic's shares across stages sum to at most 1. `None` when `n = 0`
//!   and for `unattributed`.
//! - **Sum property.** Each row's `Σ contribution_sec + unattributed_sec ==
//!   error_sec` (`stage_forecast::attribute`) and the log round-trip keeps
//!   integer seconds, so the per-stage biases and the unattributed bias are
//!   the parts of the mean error.

use super::attribution_log::AttributionRow;
use super::{Kind, Provenance, Stage};
use crate::telemetry::kinds::eta_stage_attribution::{EtaStageAttributionRecord, UNATTRIBUTED};
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use std::collections::BTreeMap;

/// The trailing window, days.
pub const WINDOW_DAYS: u32 = 7;

fn round4(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

/// The end of `day`: the fold's cutoff.
#[must_use]
pub fn cutoff(day: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&day.and_time(NaiveTime::MIN)) + Duration::days(1)
}

/// A resolved case of one heuristic: repo (lowercased), issue, PR and the
/// outcome instant.
type CaseKey<'a> = (String, u32, Option<u32>, &'a str, DateTime<Utc>);

/// The rows that count for `day`: inside the window, known before the
/// cutoff, one per resolved case and heuristic.
fn window(rows: &[AttributionRow], day: NaiveDate) -> Vec<&AttributionRow> {
    let end = cutoff(day);
    let start = end - Duration::days(i64::from(WINDOW_DAYS));
    let mut by_case: BTreeMap<CaseKey, &AttributionRow> = BTreeMap::new();
    for row in rows.iter().filter(|r| {
        r.kind == Kind::Land && r.actual_at >= start && r.actual_at < end && r.observed_at < end
    }) {
        let key = (
            row.repo.to_ascii_lowercase(),
            row.issue,
            row.pr_number,
            row.heuristic.as_str(),
            row.actual_at,
        );
        let slot = by_case.entry(key).or_insert(row);
        let rank = |r: &AttributionRow| (r.as_of, r.observed_at, r.estimate_id.clone());
        if rank(row) < rank(slot) {
            *slot = row;
        }
    }
    by_case.into_values().collect()
}

/// `(n, bias, mean abs)` of `errors`.
fn stats(errors: &[i64]) -> (u64, Option<f64>, Option<f64>) {
    let n = u64::try_from(errors.len()).unwrap_or(u64::MAX);
    if errors.is_empty() {
        return (0, None, None);
    }
    let len = errors.len() as f64;
    let bias = errors.iter().map(|e| *e as f64).sum::<f64>() / len;
    let abs = errors.iter().map(|e| e.unsigned_abs() as f64).sum::<f64>() / len;
    (n, Some(round4(bias)), Some(round4(abs)))
}

/// `day`'s records for `heuristics` (the registered `land` ids): for each, one
/// per [`Stage::EVERY`] stage in path order, then `unattributed`.
#[must_use]
pub fn rollup(
    rows: &[AttributionRow],
    heuristics: &[&str],
    day: NaiveDate,
    loom: &Provenance,
) -> Vec<EtaStageAttributionRecord> {
    let day_s = day.format("%Y-%m-%d").to_string();
    let end = cutoff(day);
    let counted = window(rows, day);
    let mut out = Vec::with_capacity(heuristics.len() * (Stage::EVERY.len() + 1));
    let mut ids: Vec<&str> = heuristics.to_vec();
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        let mine: Vec<&AttributionRow> = counted
            .iter()
            .copied()
            .filter(|r| r.heuristic == id)
            .collect();
        // The dominant-share denominator: every counted outcome of this
        // heuristic in the window, whether or not it visited the stage.
        let total = mine.len();
        let mut push = |stage: &str, errors: &[i64], dominant: Option<usize>| {
            let (n, bias_sec, mean_abs_sec) = stats(errors);
            out.push(EtaStageAttributionRecord {
                row_id: crate::telemetry::trace::derived_hex(
                    &["loom.eta.stage_attribution", id, stage, &day_s],
                    16,
                ),
                day: day_s.clone(),
                window_days: WINDOW_DAYS,
                heuristic: id.to_string(),
                kind: Kind::Land.as_str().to_string(),
                stage: stage.to_string(),
                n,
                bias_sec,
                mean_abs_sec,
                dominant_share: dominant
                    .filter(|_| n > 0 && total > 0)
                    .map(|d| round4(d as f64 / total as f64)),
                cutoff: end,
                loom: loom.clone(),
            });
        };
        for stage in Stage::EVERY {
            let errors: Vec<i64> = mine
                .iter()
                .filter_map(|r| r.attribution.stages.get(&stage))
                .map(|s| s.contribution_sec)
                .collect();
            let dominant = mine
                .iter()
                .filter(|r| r.attribution.dominant_stage == Some(stage))
                .count();
            push(stage.as_str(), &errors, Some(dominant));
        }
        let unattributed: Vec<i64> = mine
            .iter()
            .map(|r| r.attribution.unattributed_sec)
            .collect();
        push(UNATTRIBUTED, &unattributed, None);
    }
    out
}
