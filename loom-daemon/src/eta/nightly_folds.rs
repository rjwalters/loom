//! The nightly walk-forward backtest fold (#10492): the fleet captain runs it
//! once a day, after 00:30 UTC, for every registered `land` heuristic.
//!
//! The ETA promotion gate (#10233) has a backtest half and a live half. The
//! backtest half used to run only when somebody invoked `eta backtest` /
//! `eta promote`, so a candidate's out-of-sample record built only while
//! someone watched. This module computes it continuously, from the same replay
//! code (`backtest::compare` / `backtest::replay_scored`), as a library call —
//! it never shells out to the CLI.
//!
//! # What a day's run produces
//!
//! For the UTC day `D` (the cutoff is the end of `D`):
//!
//! - one `eta.backtest.fold` per registered `land` heuristic: its own scores on
//!   the cases whose `as_of` fell on `D`, and its paired delta against the
//!   `current` heuristic on those same cases ([`run_day`]);
//! - one `eta.backtest.summary` per non-`current` heuristic: the rolling
//!   per-day win count against `current`, the 95% Wilson lower bound and
//!   `gate_ready`, which is [`shadow::backtest_gate`] — the very function
//!   `eta promote` calls — on a replay built the same way (calibration
//!   evidence included, [`backtest::with_replay_calibration`]). It uses the
//!   same gate function, not the same data: the summary also reads the fleet
//!   snapshots and the offline PR cache, `eta promote` local data only.
//!
//! # Strictly point-in-time
//!
//! [`run_day`] drops, **before** replaying anything, every input observed at
//! or after the cutoff: sweep-outcome envelopes (`emitted_at`), journal rows
//! (`observed_at`), history samples, and every replay case that had not
//! resolved (`actual_at`) by then. The registry's coefficient file is the
//! newest whose own cutoff is strictly before the fold day began
//! (`Registry::load`). So the fold is a function of data knowable at the
//! cutoff only; perturbing anything after it leaves the records bit-identical
//! (the leak test pins this). A case still open at the cutoff is left out of
//! that day's fold rather than guessed at.
//!
//! # No forge call, bounded work
//!
//! Inputs are this host's local telemetry/journal, the cached fleet snapshots
//! (through the configured `historyScope`, exactly as an estimate reads them)
//! and, when present, the offline merged-PR cache an operator saved with
//! `eta backtest --save-pr-history` ([`pr_history_path`]). It adds no GitHub
//! calls. A run folds at most [`MAX_CATCH_UP_DAYS`] missed days, oldest first.
//!
//! # Idempotent
//!
//! One file per day ([`day_path`]) holds that day's records; a day whose file
//! exists is never re-run, so a restart does not duplicate records.

use super::backtest::{self, Filter, ReplayCase};
use super::history::StageSamples;
use super::journal::{self, JournalEntry};
use super::score::Score;
use super::shadow::{self, GateStatus, MIN_FOLDS};
use super::{Kind, Provenance, Registry};
use crate::telemetry::kinds::eta_backtest::{EtaBacktestFoldRecord, EtaBacktestSummaryRecord};
use crate::telemetry::TelemetryEnvelope;
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The singleton job name this task arms under `fleet.captain`.
pub const SINGLETON_JOB_NAME: &str = "eta-nightly-folds";

/// Today's fold for yesterday runs once the UTC clock passes this, so the
/// day's last outcomes have been journalled.
pub const RUN_AFTER: (u32, u32) = (0, 30);

/// Most missed days one run folds, oldest first.
pub const MAX_CATCH_UP_DAYS: i64 = 7;

/// The kind folded.
const KIND: Kind = Kind::Land;

/// `<root>/.loom/state/eta/backtest`.
#[must_use]
pub fn dir(root: &Path) -> PathBuf {
    root.join(".loom")
        .join("state")
        .join("eta")
        .join("backtest")
}

/// One day's persisted records.
#[must_use]
pub fn day_path(root: &Path, day: NaiveDate) -> PathBuf {
    dir(root).join(format!("fold-{}.json", day.format("%Y-%m-%d")))
}

/// The newest run's summaries, the `eta doctor` scoreboard's source.
#[must_use]
pub fn state_path(root: &Path) -> PathBuf {
    dir(root).join("summary.json")
}

/// The optional offline merged-PR cache (`eta backtest --save-pr-history`
/// output) whose `land` cases are replayed too. Absent: only locally
/// journalled cases are.
#[must_use]
pub fn pr_history_path(root: &Path) -> PathBuf {
    dir(root).join("pr-history.json")
}

/// One day's records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DayRecords {
    /// `YYYY-MM-DD`.
    pub day: String,
    /// One per registered `land` heuristic.
    pub folds: Vec<EtaBacktestFoldRecord>,
    /// One per non-`current` `land` heuristic.
    pub summaries: Vec<EtaBacktestSummaryRecord>,
}

/// The newest run, as `eta doctor` reads it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// When it was written.
    pub written_at: DateTime<Utc>,
    /// The newest fold day (`YYYY-MM-DD`).
    pub day: String,
    /// That day's summaries.
    pub summaries: Vec<EtaBacktestSummaryRecord>,
}

/// The raw material of a fold, before any cutoff is applied.
#[derive(Debug, Default, Clone)]
pub struct Inputs {
    /// `sweep.outcome` envelopes.
    pub envelopes: Vec<TelemetryEnvelope>,
    /// The stage journal.
    pub journal: Vec<JournalEntry>,
    /// Extra `land` cases (the offline merged-PR cache).
    pub pr_cases: Vec<ReplayCase>,
}

/// Read this host's inputs. No forge call.
#[must_use]
pub fn load_inputs(root: &Path) -> Inputs {
    let path = root
        .join(".loom")
        .join("logs")
        .join("sweep-outcome-telemetry.jsonl");
    let rotated = path.with_file_name("sweep-outcome-telemetry.jsonl.1");
    let mut envelopes = crate::sweep_outcomes::read_all_outcome_telemetry(&rotated);
    envelopes.extend(crate::sweep_outcomes::read_all_outcome_telemetry(&path));
    let pr_cases = std::fs::read_to_string(pr_history_path(root))
        .ok()
        .and_then(|text| backtest::parse_pr_records(&text).ok())
        .map(|records| backtest::cases_from_pr_records(&records).0)
        .unwrap_or_default();
    Inputs {
        envelopes,
        journal: journal::read(&journal::journal_path(root)),
        pr_cases,
    }
}

fn day_start(day: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&day.and_time(NaiveTime::MIN))
}

/// The newest day whose fold is due at `now`: yesterday once the clock passes
/// [`RUN_AFTER`], the day before until then.
#[must_use]
pub fn newest_due_day(now: DateTime<Utc>) -> NaiveDate {
    let today = now.date_naive();
    let gate = day_start(today) + Duration::minutes(i64::from(RUN_AFTER.0 * 60 + RUN_AFTER.1));
    if now >= gate {
        today - Duration::days(1)
    } else {
        today - Duration::days(2)
    }
}

/// The days to fold at `now`, oldest first, given the days already done.
///
/// A host that has never folded does only the newest due day (it has no
/// stream to catch up on); otherwise every missing day after the newest done
/// one, at most [`MAX_CATCH_UP_DAYS`] of them (the newest ones).
#[must_use]
pub fn due_days(now: DateTime<Utc>, done: &BTreeSet<NaiveDate>) -> Vec<NaiveDate> {
    let newest = newest_due_day(now);
    let Some(last) = done.iter().next_back().copied() else {
        return vec![newest];
    };
    let mut days: Vec<NaiveDate> = (1..)
        .map(|n| last + Duration::days(n))
        .take_while(|d| *d <= newest)
        .filter(|d| !done.contains(d))
        .collect();
    let keep = usize::try_from(MAX_CATCH_UP_DAYS).unwrap_or(usize::MAX);
    if days.len() > keep {
        days.drain(..days.len() - keep);
    }
    days
}

/// The days that already have a record file.
#[must_use]
pub fn done_days(root: &Path) -> BTreeSet<NaiveDate> {
    let Ok(entries) = std::fs::read_dir(dir(root)) else {
        return BTreeSet::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let stem = name.strip_prefix("fold-")?.strip_suffix(".json")?;
            NaiveDate::parse_from_str(stem, "%Y-%m-%d").ok()
        })
        .collect()
}

fn round4(x: f64) -> f64 {
    (x * 1e4).round() / 1e4
}

fn share(count: usize, n: usize) -> Option<f64> {
    (n > 0).then(|| round4(count as f64 / n as f64))
}

fn mean(values: impl Iterator<Item = f64>) -> Option<f64> {
    let (sum, n) = values.fold((0.0, 0_usize), |(s, n), v| (s + v, n + 1));
    (n > 0).then(|| round4(sum / n as f64))
}

fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// Drop every history sample observed at or after `cutoff`.
fn prune_history(history: &mut StageSamples, cutoff: DateTime<Utc>) {
    history.stages.retain(|s| s.observed_at < cutoff);
    history.censored.retain(|s| s.observed_at < cutoff);
    history.verdicts.retain(|s| s.observed_at < cutoff);
    history.paths.retain(|s| s.observed_at < cutoff);
}

/// The `land` cases known resolved at `cutoff`, and the history they replay
/// against — both with everything observed at or after `cutoff` removed.
fn point_in_time(
    inputs: &Inputs,
    cutoff: DateTime<Utc>,
    scope: &dyn Fn(StageSamples) -> StageSamples,
) -> (StageSamples, Vec<ReplayCase>) {
    let envelopes: Vec<TelemetryEnvelope> = inputs
        .envelopes
        .iter()
        .filter(|e| e.emitted_at < cutoff)
        .cloned()
        .collect();
    let journal: Vec<JournalEntry> = inputs
        .journal
        .iter()
        .filter(|e| e.observed_at < cutoff)
        .cloned()
        .collect();
    let mut local = StageSamples::default();
    local.push_envelopes(&envelopes);
    local.push_journal(&journal, "local");
    let mut history = scope(local);
    prune_history(&mut history, cutoff);
    // Calibration evidence comes only from the cut replay below
    // ([`calibrated`]), exactly as `eta backtest` / `eta promote` build it —
    // never from whatever the scope hook merged in, which could carry a
    // landing resolved after the cutoff.
    history.calibration.clear();

    let mut cases = backtest::cases_from_envelopes(&envelopes);
    cases.extend(backtest::cases_from_journal(&journal));
    let (mut cases, _dropped) = backtest::merge_case_sets(cases, inputs.pr_cases.clone());
    cases.retain(|c| c.kind == KIND && c.as_of < cutoff && c.actual_at < cutoff);
    (history, cases)
}

/// `history` with the calibrating heuristic's evidence replayed in from
/// `cases` under `registry`'s base ([`backtest::with_replay_calibration`],
/// the helper `eta backtest` and `eta promote` call). Leak-free because
/// `history` and `cases` were already cut at the cutoff ([`point_in_time`]);
/// without it `land-2026-10-06-calm-plover` would degrade to plain `land-v2`
/// in every fold.
fn calibrated(
    registry: &Registry,
    history: &StageSamples,
    cases: &[ReplayCase],
    loom: &Provenance,
) -> StageSamples {
    let mut history = history.clone();
    backtest::with_replay_calibration(|id| registry.get(id), &mut history, cases, loom);
    history
}

/// One heuristic's own scores on a day's cases.
struct OwnStats {
    n: usize,
    answered: usize,
    answer_rate: Option<f64>,
    pinball4: Option<f64>,
    coverage: Option<f64>,
    late: Option<f64>,
}

fn own_stats(scores: &[Score]) -> OwnStats {
    let answered: Vec<&Score> = scores
        .iter()
        .filter(|s| s.pinball_loss_sec.is_some())
        .collect();
    let decided_late: Vec<bool> = scores.iter().filter_map(|s| s.above_p90).collect();
    OwnStats {
        n: scores.len(),
        answered: answered.len(),
        answer_rate: share(answered.len(), scores.len()),
        pinball4: mean(scores.iter().filter_map(|s| s.pinball4_loss_sec)),
        coverage: share(
            answered.iter().filter(|s| s.covered == Some(true)).count(),
            answered.len(),
        ),
        late: share(decided_late.iter().filter(|l| **l).count(), decided_late.len()),
    }
}

fn delta(challenger: Option<f64>, current: Option<f64>) -> Option<f64> {
    Some(round4(challenger? - current?))
}

/// Fold `day` (UTC) for every registered `land` heuristic.
///
/// Pure given its inputs: `scope` applies the configured `historyScope` to
/// the local history, `registry_for(before)` builds the registry whose fit is
/// the newest strictly before `before`. Nothing observed at or after the end
/// of `day` is read (module docs).
#[must_use]
pub fn run_day(
    inputs: &Inputs,
    day: NaiveDate,
    current_land: Option<&str>,
    scope: &dyn Fn(StageSamples) -> StageSamples,
    registry_for: &dyn Fn(DateTime<Utc>) -> Registry,
    loom: &Provenance,
) -> DayRecords {
    let start = day_start(day);
    let cutoff = start + Duration::days(1);
    let day_s = day.format("%Y-%m-%d").to_string();
    let (uncalibrated, cases) = point_in_time(inputs, cutoff, scope);
    let day_cases: Vec<ReplayCase> = cases.iter().filter(|c| c.as_of >= start).cloned().collect();
    let no_filter = Filter::default();

    let registry = registry_for(start);
    let day_history = calibrated(&registry, &uncalibrated, &cases, loom);
    let history = &day_history;
    let current = registry.current(KIND, current_land);
    let current_id = current.id();
    let current_scores = backtest::replay_scored(current, history, &day_cases, no_filter, loom);
    let current_stats = own_stats(&current_scores);

    let mut folds = Vec::new();
    for h in registry.for_kind(KIND) {
        let is_current = h.id() == current_id;
        let scores = backtest::replay_scored(h, history, &day_cases, no_filter, loom);
        let own = own_stats(&scores);
        let mut fold = EtaBacktestFoldRecord {
            fold_id: crate::telemetry::trace::derived_hex(
                &["loom.eta.backtest.fold", h.id(), &day_s],
                16,
            ),
            heuristic: h.id().to_string(),
            kind: KIND.as_str().to_string(),
            day: day_s.clone(),
            cutoff,
            compared_to: current_id.to_string(),
            is_current,
            n_cases: count(own.n),
            n_answered: count(own.answered),
            answer_rate: own.answer_rate,
            pinball4_loss_sec: own.pinball4,
            cov_25_75: own.coverage,
            late_surprise: own.late,
            paired_pairs: 0,
            delta_pinball4_loss_sec: None,
            delta_answer_rate: None,
            delta_late_surprise: None,
            win: None,
            fit_id: registry.fit_id().map(str::to_string),
            loom: loom.clone(),
        };
        if !is_current {
            if let Ok(cmp) = backtest::compare(current, h, history, &day_cases, no_filter, loom) {
                let p = &cmp.paired;
                fold.paired_pairs = count(p.loss4_pairs);
                fold.delta_pinball4_loss_sec =
                    delta(p.b_mean_pinball4_loss_sec, p.a_mean_pinball4_loss_sec);
                fold.win = match (p.a_mean_pinball4_loss_sec, p.b_mean_pinball4_loss_sec) {
                    (Some(a), Some(b)) if b < a => Some(true),
                    (Some(a), Some(b)) if a < b => Some(false),
                    _ => None,
                };
            }
            fold.delta_answer_rate = delta(own.answer_rate, current_stats.answer_rate);
            fold.delta_late_surprise = delta(own.late, current_stats.late);
        }
        folds.push(fold);
    }

    let summaries = summaries_for(&cases, &uncalibrated, current_land, registry_for, day, loom);
    DayRecords {
        day: day_s,
        folds,
        summaries,
    }
}

/// The rolling standing of every non-`current` heuristic on every case known at
/// the cutoff, judged by the gate function `eta promote` uses
/// ([`shadow::backtest_gate`]). `history` is uncalibrated; it is calibrated
/// here under the window's own registry.
fn summaries_for(
    cases: &[ReplayCase],
    history: &StageSamples,
    current_land: Option<&str>,
    registry_for: &dyn Fn(DateTime<Utc>) -> Registry,
    day: NaiveDate,
    loom: &Provenance,
) -> Vec<EtaBacktestSummaryRecord> {
    let cutoff = day_start(day) + Duration::days(1);
    let day_s = day.format("%Y-%m-%d").to_string();
    // One registry for the whole window, fitted before its first day: a fit
    // dated later would have seen the early days' outcomes.
    let first = cases
        .iter()
        .map(|c| c.as_of.date_naive())
        .min()
        .unwrap_or(day);
    let registry = registry_for(day_start(first));
    // Calibrated under this window's registry, as `eta promote` would.
    let history = &calibrated(&registry, history, cases, loom);
    let current = registry.current(KIND, current_land);
    let mut out = Vec::new();
    for h in registry.for_kind(KIND).filter(|h| h.id() != current.id()) {
        let comparison =
            backtest::compare(current, h, history, cases, Filter::default(), loom).ok();
        let gate = shadow::backtest_gate(current.id(), h.id(), comparison.as_ref());
        let w = gate.day_wins;
        out.push(EtaBacktestSummaryRecord {
            summary_id: crate::telemetry::trace::derived_hex(
                &["loom.eta.backtest.summary", h.id(), &day_s],
                16,
            ),
            heuristic: h.id().to_string(),
            kind: KIND.as_str().to_string(),
            compared_to: current.id().to_string(),
            as_of_day: day_s.clone(),
            cutoff,
            cases: count(gate.cases),
            days: count(w.days),
            wins: count(w.wins),
            ties: count(w.ties),
            win_rate: w.win_rate,
            ci_low: w.ci_low,
            ci_high: w.ci_high,
            min_folds: count(MIN_FOLDS),
            gate_ready: gate.status == GateStatus::Passed,
            gate_detail: gate.detail,
            fit_id: registry.fit_id().map(str::to_string),
            loom: loom.clone(),
        });
    }
    out
}

/// Fold every due day for `root`, persist each (one file per day, then the
/// newest summary), and return what was folded, oldest first. Empty when
/// nothing is due. A day that could not be written is not returned, so the
/// next run retries it.
pub fn run_due(
    root: &Path,
    now: DateTime<Utc>,
    current_land: Option<&str>,
    scope: &dyn Fn(StageSamples) -> StageSamples,
    registry_for: &dyn Fn(DateTime<Utc>) -> Registry,
    loom: &Provenance,
) -> Vec<DayRecords> {
    let due = due_days(now, &done_days(root));
    if due.is_empty() {
        return Vec::new();
    }
    let inputs = load_inputs(root);
    let mut out = Vec::new();
    for day in due {
        let records = run_day(&inputs, day, current_land, scope, registry_for, loom);
        let Ok(text) = serde_json::to_string_pretty(&records) else {
            continue;
        };
        if let Err(e) = super::health::write_atomic(&day_path(root, day), &text) {
            log::warn!("eta nightly folds: writing {} failed: {e}", day_path(root, day).display());
            break;
        }
        out.push(records);
    }
    if let Some(newest) = out.last() {
        let state = State {
            written_at: now,
            day: newest.day.clone(),
            summaries: newest.summaries.clone(),
        };
        if let Ok(text) = serde_json::to_string_pretty(&state) {
            if let Err(e) = super::health::write_atomic(&state_path(root), &text) {
                log::warn!("eta nightly folds: writing summary.json failed: {e}");
            }
        }
    }
    out
}

/// The newest run's state, when one was written and parses.
#[must_use]
pub fn read_state(root: &Path) -> Option<State> {
    serde_json::from_str(&std::fs::read_to_string(state_path(root)).ok()?).ok()
}

/// The built-in registry fitted before `before` — the production
/// `registry_for`.
#[must_use]
pub fn registry_before(root: &Path, before: DateTime<Utc>) -> Registry {
    Registry::load(root, before)
}

#[cfg(test)]
#[path = "nightly_folds_tests.rs"]
mod tests;
