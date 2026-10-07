//! Shadow fleet lifecycle from the nightly folds (#10525): which candidates
//! are worth promoting, and which are worth retiring.
//!
//! Both read the `eta.backtest.fold` records the nightly walk-forward job
//! (#10492, [`super::nightly_folds`]) already saved under
//! `.loom/state/eta/backtest/`. Neither reads GitHub or recomputes a replay,
//! and neither changes the registry: a retirement is a *proposal* for a human
//! (retiring a heuristic is a code change, #10484).
//!
//! # The promotion short-list
//!
//! Comparing every candidate against `current` and promoting any that clears a
//! gate is a multiple-comparisons problem: the more candidates are tried, the
//! likelier one clears by luck. So `eta promote` only evaluates the
//! [`SHORTLIST_SIZE`] best candidates by paired pinball against `current`
//! over the newest [`SHORTLIST_WINDOW_DAYS`] fold days ([`shortlist`]).
//! Insufficient evidence fails **closed** with the reason recorded:
//!
//! - no fold on this host, or the newest is older than [`MAX_STALE_DAYS`]
//!   behind the newest due day: nobody is short-listed;
//! - a fold compared against a different `current` than today's is not
//!   comparable and is left out;
//! - a candidate with fewer than [`MIN_RANK_DAYS`] paired days is not ranked
//!   and is refused (a day with a non-finite delta is not a decided day);
//! - an id that is unknown, baseline or retired is refused;
//! - ties break by id, so the order never depends on map iteration.
//!
//! # Retirement proposals
//!
//! A candidate is proposed for retirement ([`retirement_proposals`]) only when
//! all of these hold:
//!
//! 1. it is a registered `candidate` (never a baseline, retired id or
//!    `current`);
//! 2. over at least [`RETIREMENT_MIN_DAYS`] decided fold days its per-day paired
//!    pinball delta against `current` is *worse*, with the 95% interval of the
//!    day-level mean excluding 0 (the day, not the case, is the unit of
//!    independence: see the operator's note on #10525);
//! 3. another candidate **dominates** it over at least [`RETIREMENT_MIN_DAYS`]
//!    common days: strictly lower mean pinball, coverage error (distance of
//!    the p25-p75 coverage from 50%) and late-surprise rate no higher. A day
//!    is common only when both folds scored the whole day cohort (see
//!    `metrics`): a fold's aggregates are over its own answered subset, so
//!    anything less does not establish the same items.
//!
//! Any missing or non-finite required value refuses the proposal. The
//! evidence identity ([`RetirementProposal::evidence_id`]) hashes the window
//! and the numbers; the dedup key is the heuristic alone ([`dedup_key`]), so a
//! nightly re-run, or a window that slid one day, never files a second issue
//! for the same heuristic.

use super::nightly_folds::{self, DayRecords};
use crate::telemetry::kinds::eta_backtest::EtaBacktestFoldRecord;
use crate::telemetry::trace::derived_hex;
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Candidates `eta promote` evaluates live.
pub const SHORTLIST_SIZE: usize = 2;
/// Newest fold days a ranking reads.
pub const SHORTLIST_WINDOW_DAYS: usize = 14;
/// Paired days a candidate needs to be ranked at all.
pub const MIN_RANK_DAYS: usize = 3;
/// How many days behind the newest due day the newest fold may be.
pub const MAX_STALE_DAYS: i64 = 2;
/// Decided days a retirement proposal needs (the operator's number, #10525).
pub const RETIREMENT_MIN_DAYS: usize = 14;
/// Newest fold days a retirement scan reads.
pub const RETIREMENT_WINDOW_DAYS: usize = 28;

/// `<root>/.loom/state/eta/retirement-proposals.json`: the keys already filed.
#[must_use]
pub fn filed_path(root: &Path) -> PathBuf {
    nightly_folds::dir(root)
        .parent()
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf)
        .join("retirement-proposals.json")
}

/// The newest `n` saved fold days, oldest first. An unreadable file is skipped.
#[must_use]
pub fn load_days(root: &Path, n: usize) -> Vec<DayRecords> {
    let days: Vec<NaiveDate> = nightly_folds::done_days(root).into_iter().collect();
    let skip = days.len().saturating_sub(n);
    days.into_iter()
        .skip(skip)
        .filter_map(|day| {
            std::fs::read_to_string(nightly_folds::day_path(root, day))
                .ok()
                .and_then(|text| serde_json::from_str::<DayRecords>(&text).ok())
        })
        .collect()
}

/// One ranked candidate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ranked {
    /// The heuristic.
    pub id: String,
    /// Paired days read.
    pub days: usize,
    /// Mean paired pinball delta against `current`, seconds, weighted by each
    /// day's paired pairs; negative is better.
    pub mean_delta_pinball4_sec: f64,
}

/// A candidate left out of the short-list, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    /// The heuristic.
    pub id: String,
    /// The reason, for the decision record.
    pub reason: String,
}

/// The short-list and the evidence it was computed from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Shortlist {
    /// The `current` heuristic the deltas are against.
    pub current: String,
    /// Newest fold day read (`YYYY-MM-DD`), when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_fold_day: Option<String>,
    /// Fold days read.
    pub window_days: usize,
    /// The ranked candidates, best first (at most [`SHORTLIST_SIZE`] selected).
    pub ranked: Vec<Ranked>,
    /// The ids that may be evaluated live.
    pub selected: Vec<String>,
    /// Everything else and why.
    pub refused: Vec<Refused>,
    /// A reason nobody was short-listed, when that is so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_evidence: Option<String>,
}

impl Shortlist {
    /// Whether `candidate` may go on to the live gate; otherwise the reason.
    ///
    /// # Errors
    /// `candidate` is not short-listed.
    pub fn admits(&self, candidate: &str) -> Result<(), String> {
        if self.selected.iter().any(|id| id == candidate) {
            return Ok(());
        }
        if let Some(reason) = &self.no_evidence {
            return Err(format!("{candidate} is not short-listed: {reason}"));
        }
        let why = self.refused.iter().find(|r| r.id == candidate).map_or_else(
            || {
                format!(
                    "outside the top {SHORTLIST_SIZE} by paired pinball (short-list: {})",
                    self.selected.join(", ")
                )
            },
            |r| r.reason.clone(),
        );
        Err(format!("{candidate} is not short-listed: {why}"))
    }
}

/// [`shortlist`] for `eta promote`: the eligible ids are the registered
/// `candidate`-tier ones other than `current`, the folds are this host's saved
/// ones.
#[must_use]
pub fn shortlist_for(
    root: &Path,
    registry: &super::Registry,
    current: &str,
    requested: &str,
    now: DateTime<Utc>,
) -> Shortlist {
    let eligible: Vec<&str> = registry
        .ids()
        .into_iter()
        .filter(|id| *id != current && registry.tier_of(id) == Some(super::Tier::Candidate))
        .collect();
    shortlist(&load_days(root, SHORTLIST_WINDOW_DAYS), current, &eligible, &[requested], now)
}

fn fold_rows<'a>(
    days: &'a [DayRecords],
    id: &str,
    current: &str,
) -> Vec<&'a EtaBacktestFoldRecord> {
    days.iter()
        .filter_map(|d| {
            d.folds
                .iter()
                .find(|f| f.heuristic == id && !f.is_current && f.compared_to == current)
        })
        .collect()
}

/// `(pairs, delta)` for each day with a finite paired delta.
fn paired(rows: &[&EtaBacktestFoldRecord]) -> Vec<(u64, f64)> {
    rows.iter()
        .filter(|f| f.paired_pairs > 0)
        .filter_map(|f| {
            f.delta_pinball4_loss_sec
                .filter(|v| v.is_finite())
                .map(|v| (f.paired_pairs, v))
        })
        .collect()
}

/// Rank `eligible` candidates (registered `candidate`-tier ids other than
/// `current`) by the nightly folds in `days` (oldest first), as of `now`.
/// Pure: the clock and the days are arguments.
#[must_use]
pub fn shortlist(
    days: &[DayRecords],
    current: &str,
    eligible: &[&str],
    requested: &[&str],
    now: DateTime<Utc>,
) -> Shortlist {
    let start = days.len().saturating_sub(SHORTLIST_WINDOW_DAYS);
    let window = &days[start..];
    let newest = window.last().map(|d| d.day.clone());
    let mut out = Shortlist {
        current: current.to_string(),
        newest_fold_day: newest.clone(),
        window_days: window.len(),
        ranked: Vec::new(),
        selected: Vec::new(),
        refused: Vec::new(),
        no_evidence: None,
    };
    let refuse_all = |out: &mut Shortlist, why: String| {
        out.no_evidence = Some(why);
    };
    let Some(newest_day) = newest
        .as_deref()
        .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
    else {
        refuse_all(
            &mut out,
            "no nightly fold is saved on this host (autonomous.eta.nightlyFolds); \
             evidence is insufficient, so nobody is short-listed"
                .to_string(),
        );
        return out;
    };
    let due = nightly_folds::newest_due_day(now);
    if newest_day < due - Duration::days(MAX_STALE_DAYS) {
        refuse_all(
            &mut out,
            format!(
                "the newest nightly fold ({newest_day}) is more than {MAX_STALE_DAYS} days \
                 behind the newest due day ({due}); stale evidence"
            ),
        );
        return out;
    }
    let mut ranked: Vec<Ranked> = Vec::new();
    for id in eligible.iter().filter(|id| **id != current) {
        let rows = fold_rows(window, id, current);
        let pairs = paired(&rows);
        let total: u64 = pairs.iter().map(|(n, _)| *n).sum();
        if pairs.len() < MIN_RANK_DAYS || total == 0 {
            out.refused.push(Refused {
                id: (*id).to_string(),
                reason: format!(
                    "{} comparable paired fold day(s) against {current}; {MIN_RANK_DAYS} needed",
                    pairs.len()
                ),
            });
            continue;
        }
        let mean = pairs.iter().map(|(n, d)| *n as f64 * d).sum::<f64>() / total as f64;
        ranked.push(Ranked {
            id: (*id).to_string(),
            days: pairs.len(),
            mean_delta_pinball4_sec: mean,
        });
    }
    ranked.sort_by(|a, b| {
        a.mean_delta_pinball4_sec
            .total_cmp(&b.mean_delta_pinball4_sec)
            .then_with(|| a.id.cmp(&b.id))
    });
    out.selected = ranked
        .iter()
        .take(SHORTLIST_SIZE)
        .map(|r| r.id.clone())
        .collect();
    out.ranked = ranked;
    for id in requested {
        let known = eligible.iter().any(|e| e == id);
        if !known && !out.refused.iter().any(|r| r.id == *id) {
            out.refused.push(Refused {
                id: (*id).to_string(),
                reason: "not an eligible candidate (unknown, baseline, retired or current)"
                    .to_string(),
            });
        }
    }
    out.refused.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// A heuristic worth retiring, with the evidence for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetirementProposal {
    /// The heuristic proposed for retirement.
    pub heuristic: String,
    /// The candidate that dominates it.
    pub dominated_by: String,
    /// The `current` heuristic the deltas are against.
    pub current: String,
    /// First and last fold day of the window read.
    pub window: (String, String),
    /// Decided days of paired delta.
    pub decided_days: usize,
    /// Mean per-day paired pinball delta, seconds (positive is worse).
    pub mean_delta_pinball4_sec: f64,
    /// 95% interval of that mean (lower, upper); the lower bound is above 0.
    pub delta_ci95: (f64, f64),
    /// Common days the domination was measured on.
    pub common_days: usize,
    /// Mean pinball of the proposed heuristic and of the dominator.
    pub pinball4_sec: (f64, f64),
    /// Coverage error `|cov_25_75 - 0.5|` of each.
    pub coverage_error: (f64, f64),
    /// Late-surprise rate of each.
    pub late_surprise: (f64, f64),
    /// Hash of the window and these numbers: what was looked at.
    pub evidence_id: String,
    /// Fold ids of the proposed heuristic over the window.
    pub fold_ids: Vec<String>,
}

/// The dedup key: one proposal per heuristic, ever, until a human acts.
#[must_use]
pub fn dedup_key(heuristic: &str) -> String {
    derived_hex(&["loom.eta.retirement-proposal", heuristic], 16)
}

fn t_crit(df: usize) -> f64 {
    // Slightly conservative closed form of the 97.5% Student-t quantile.
    1.96 + 3.0 / df.max(1) as f64
}

fn mean_ci(values: &[f64]) -> (f64, f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let half = t_crit(values.len() - 1) * (var / n).sqrt();
    (mean, mean - half, mean + half)
}

/// The best dominator found so far: its mean pinball on the common days, id,
/// common days, and the `(proposed, dominator)` means of each metric.
struct Dominator<'a> {
    pinball: f64,
    id: &'a str,
    common_days: usize,
    pin: (f64, f64),
    cov: (f64, f64),
    late: (f64, f64),
}

struct DayMetrics {
    pinball: f64,
    cov_err: f64,
    late: f64,
}

/// A fold's metrics for the candidate-to-candidate domination test, or `None`
/// when the day cannot be compared.
///
/// A fold's `pinball4`, coverage and late-surprise are each the heuristic's
/// *own* aggregate over the cases it answered (and carried a p90 for), so two
/// folds sharing a day do not thereby share items: a model answering only the
/// easy cases would look better than one answering all of them. Only a fold
/// that answered every case of the day's cohort **and** was paired on every
/// case (`paired_pairs`: p90 present on both sides) scored the whole cohort,
/// so only two such folds scored the same items. Any other day refuses.
fn metrics(f: &EtaBacktestFoldRecord) -> Option<DayMetrics> {
    if f.n_cases == 0 || f.n_answered != f.n_cases || f.paired_pairs != f.n_cases {
        return None;
    }
    let finite = |v: Option<f64>| v.filter(|x| x.is_finite());
    Some(DayMetrics {
        pinball: finite(f.pinball4_loss_sec)?,
        cov_err: (finite(f.cov_25_75)? - 0.5).abs(),
        late: finite(f.late_surprise)?,
    })
}

/// Per-day metrics keyed by fold day, for one heuristic against `current`.
fn day_metrics(days: &[DayRecords], id: &str, current: &str) -> BTreeMap<String, DayMetrics> {
    days.iter()
        .filter_map(|d| {
            let f = d
                .folds
                .iter()
                .find(|f| f.heuristic == id && !f.is_current && f.compared_to == current)?;
            metrics(f).map(|m| (d.day.clone(), m))
        })
        .collect()
}

/// Retirement proposals from the nightly folds in `days` (oldest first).
///
/// `eligible` is the registered `candidate`-tier ids other than `current`.
/// Pure and deterministic; baselines, retired ids, `current`, too few decided
/// days, missing evidence and non-dominated candidates produce none. Output is
/// ordered by heuristic id.
#[must_use]
pub fn retirement_proposals(
    days: &[DayRecords],
    current: &str,
    eligible: &[&str],
) -> Vec<RetirementProposal> {
    let start = days.len().saturating_sub(RETIREMENT_WINDOW_DAYS);
    let window = &days[start..];
    let (Some(first), Some(last)) = (window.first(), window.last()) else {
        return Vec::new();
    };
    let mut ids: Vec<&str> = eligible.iter().copied().filter(|i| *i != current).collect();
    ids.sort_unstable();
    ids.dedup();
    let metrics_of: BTreeMap<&str, BTreeMap<String, DayMetrics>> = ids
        .iter()
        .map(|id| (*id, day_metrics(window, id, current)))
        .collect();
    let mut out = Vec::new();
    for id in &ids {
        let rows = fold_rows(window, id, current);
        let deltas: Vec<f64> = paired(&rows).into_iter().map(|(_, d)| d).collect();
        if deltas.len() < RETIREMENT_MIN_DAYS {
            continue;
        }
        let (mean, lo, hi) = mean_ci(&deltas);
        if !(lo.is_finite() && hi.is_finite() && lo > 0.0) {
            continue;
        }
        let mine = &metrics_of[id];
        // The dominator: best (lowest) mean pinball on the common days among
        // the others that dominate; ties by id.
        let mut best: Option<Dominator<'_>> = None;
        for other in ids.iter().filter(|o| *o != id) {
            let theirs = &metrics_of[other];
            let common: Vec<(&DayMetrics, &DayMetrics)> = mine
                .iter()
                .filter_map(|(day, m)| theirs.get(day).map(|t| (m, t)))
                .collect();
            if common.len() < RETIREMENT_MIN_DAYS {
                continue;
            }
            let avg = |f: &dyn Fn(&DayMetrics) -> f64, side: usize| {
                common
                    .iter()
                    .map(|p| f(if side == 0 { p.0 } else { p.1 }))
                    .sum::<f64>()
                    / common.len() as f64
            };
            let pin = (avg(&|m| m.pinball, 0), avg(&|m| m.pinball, 1));
            let cov = (avg(&|m| m.cov_err, 0), avg(&|m| m.cov_err, 1));
            let late = (avg(&|m| m.late, 0), avg(&|m| m.late, 1));
            let dominates = pin.1 < pin.0 && cov.1 <= cov.0 && late.1 <= late.0;
            if dominates && best.as_ref().is_none_or(|b| pin.1 < b.pinball) {
                best = Some(Dominator {
                    pinball: pin.1,
                    id: other,
                    common_days: common.len(),
                    pin,
                    cov,
                    late,
                });
            }
        }
        let Some(Dominator {
            id: dominator,
            common_days,
            pin,
            cov,
            late,
            ..
        }) = best
        else {
            continue;
        };
        let fold_ids: Vec<String> = rows.iter().map(|f| f.fold_id.clone()).collect();
        let numbers = format!("{mean:.4}|{lo:.4}|{hi:.4}|{:.4}|{:.4}", pin.0, pin.1);
        let evidence_id = derived_hex(
            &[
                "loom.eta.retirement-evidence",
                id,
                dominator,
                &first.day,
                &last.day,
                &numbers,
                &fold_ids.join(","),
            ],
            16,
        );
        out.push(RetirementProposal {
            heuristic: (*id).to_string(),
            dominated_by: dominator.to_string(),
            current: current.to_string(),
            window: (first.day.clone(), last.day.clone()),
            decided_days: deltas.len(),
            mean_delta_pinball4_sec: mean,
            delta_ci95: (lo, hi),
            common_days,
            pinball4_sec: pin,
            coverage_error: cov,
            late_surprise: late,
            evidence_id,
            fold_ids,
        });
    }
    out
}

impl RetirementProposal {
    /// The issue title.
    #[must_use]
    pub fn title(&self) -> String {
        format!(
            "ETA: retire shadow heuristic {} (dominated by {})",
            self.heuristic, self.dominated_by
        )
    }

    /// The issue body: the evidence, and the dedup marker.
    #[must_use]
    pub fn body(&self) -> String {
        format!(
            "<!-- loom:eta-retirement key={key} evidence={ev} -->\n\
             The nightly folds (#10492) propose retiring `{h}`. This is a proposal only; \
             nothing was removed. Retiring is a code change (#10484, #10525).\n\n\
             - window: {w0} .. {w1} ({days} decided days against `{cur}`)\n\
             - paired pinball delta vs `{cur}`: mean {mean:+.1}s (95% CI {lo:+.1}s .. {hi:+.1}s); \
               positive is worse\n\
             - dominated by `{d}` over {common} common days: pinball {p0:.1}s vs {p1:.1}s, \
               coverage error {c0:.3} vs {c1:.3}, late surprise {l0:.3} vs {l1:.3}\n\
             - evidence id: `{ev}`\n\
             - fold ids: {folds}\n\n\
             Not assessed: adaptation time (`t_p50`, `t_cov`, #10528) is not in the fold \
             records yet.\n",
            key = dedup_key(&self.heuristic),
            ev = self.evidence_id,
            h = self.heuristic,
            w0 = self.window.0,
            w1 = self.window.1,
            days = self.decided_days,
            cur = self.current,
            mean = self.mean_delta_pinball4_sec,
            lo = self.delta_ci95.0,
            hi = self.delta_ci95.1,
            d = self.dominated_by,
            common = self.common_days,
            p0 = self.pinball4_sec.0,
            p1 = self.pinball4_sec.1,
            c0 = self.coverage_error.0,
            c1 = self.coverage_error.1,
            l0 = self.late_surprise.0,
            l1 = self.late_surprise.1,
            folds = self.fold_ids.join(", "),
        )
    }
}

/// A proposal already filed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filed {
    /// [`dedup_key`].
    pub key: String,
    /// The heuristic.
    pub heuristic: String,
    /// What the filer returned (an issue URL).
    pub reference: String,
    /// When.
    pub at: DateTime<Utc>,
}

/// The filed ledger; a missing or unreadable file is empty.
#[must_use]
pub fn read_filed(root: &Path) -> Vec<Filed> {
    std::fs::read_to_string(filed_path(root))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// What filing a proposal needs from the forge.
pub trait ProposalForge {
    /// An issue, open **or closed**, whose body carries the dedup `key`, if
    /// any. A closed one counts: a human who declined a retirement has
    /// answered it.
    ///
    /// # Errors
    /// The forge could not be asked; the proposal is then not filed.
    fn find(&mut self, key: &str) -> Result<Option<String>, String>;

    /// File an issue; returns a reference to it (its URL).
    ///
    /// # Errors
    /// The issue was not filed.
    fn file(&mut self, title: &str, body: &str) -> Result<String, String>;
}

/// What [`file_proposals`] did with each proposal, by heuristic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FilingReport {
    /// Newly filed.
    pub filed: Vec<String>,
    /// Already recorded on this host, or found on the forge (filed by
    /// another host, or by an earlier run whose record was lost).
    pub already: Vec<String>,
}

/// File every proposal not yet filed through `forge` (the supported creator
/// is `.loom/scripts/create-issue.sh`). Dedup is two-layered: this host's
/// ledger ([`filed_path`]), then the forge itself, searched for the
/// [`dedup_key`] marker every body carries (so a declined proposal is not
/// re-filed). The search-then-create is **not atomic**: the caller must be
/// the only filer fleet-wide (`loom-daemon eta retire --file` is gated on
/// `fleet.captain`). A forge that cannot be searched refuses the filing
/// (fails closed); it is retried next run.
///
/// # Errors
/// The ledger could not be written after a filing (the proposal is then
/// filed but unrecorded; the forge search still dedups it), or the first
/// forge error, after the remaining proposals were attempted.
pub fn file_proposals(
    root: &Path,
    proposals: &[RetirementProposal],
    now: DateTime<Utc>,
    forge: &mut dyn ProposalForge,
) -> Result<FilingReport, String> {
    let mut ledger = read_filed(root);
    let mut report = FilingReport::default();
    let mut first_error: Option<String> = None;
    for p in proposals {
        let key = dedup_key(&p.heuristic);
        if ledger.iter().any(|f| f.key == key) {
            report.already.push(p.heuristic.clone());
            continue;
        }
        let reference = match forge.find(&key) {
            Ok(Some(found)) => {
                report.already.push(p.heuristic.clone());
                found
            }
            Ok(None) => match forge.file(&p.title(), &p.body()) {
                Ok(filed) => {
                    report.filed.push(p.heuristic.clone());
                    filed
                }
                Err(e) => {
                    first_error.get_or_insert(format!("{}: {e}", p.heuristic));
                    continue;
                }
            },
            Err(e) => {
                first_error.get_or_insert(format!(
                    "{}: could not search for an existing proposal, not filing: {e}",
                    p.heuristic
                ));
                continue;
            }
        };
        ledger.push(Filed {
            key,
            heuristic: p.heuristic.clone(),
            reference,
            at: now,
        });
        let text = serde_json::to_string_pretty(&ledger).map_err(|e| e.to_string())?;
        super::health::write_atomic(&filed_path(root), &text)
            .map_err(|e| format!("could not record the filing: {e}"))?;
    }
    first_error.map_or(Ok(report), Err)
}
