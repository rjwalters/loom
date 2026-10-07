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
//!   `D`'s cohort — the cases first known (resolved) during `D`, whichever day
//!   they were predicted on ([`cohort`]) — and its paired delta against the
//!   `current` heuristic on those same cases ([`run_day`]);
//! - one `eta.backtest.summary` per non-`current` heuristic: the rolling
//!   per-day win count against `current`, the 95% Wilson lower bound and
//!   `gate_ready`, which is [`shadow::backtest_gate`] — the very function
//!   `eta promote` calls — on a replay built the same way (calibration
//!   evidence included, [`backtest::with_replay_calibration`]). It uses the
//!   same gate function, not the same data: the summary also reads the fleet
//!   snapshots and the offline PR cache, `eta promote` local data only.
//!
//! # Cohorts: every case is folded exactly once
//!
//! A case is predicted at its `as_of` and known only once it resolves
//! (`actual_at`), possibly days later. Folding by prediction day would have to
//! drop a case that crosses midnight (unresolved at its prediction day's
//! cutoff, and never revisited once that day's file is written), so the
//! longest-running cases would systematically leave the scoreboard (#10532
//! review, finding 1). `D`'s cohort is therefore the cases known at `D`'s
//! cutoff whose identity was not yet known at `D`'s start ([`cohort`]): each
//! resolved case lands in exactly one daily fold. The identity names the PR,
//! so a second PR on the same issue resolving out of prediction order cannot
//! take over the first one's identity (#10626).
//!
//! # Every case is scored by its prediction day's registry
//!
//! The prediction itself stays at prediction time: the case's own `as_of`
//! input, history strictly before it, and the registry the fold for its
//! **prediction** day serves — the coefficient file newest strictly before
//! that day began ([`ByDay`]). The daily fold and every later summary score a
//! case with that same registry, so the summary accumulates the walk-forward
//! daily fits rather than re-scoring old days with one fit (#10532 review,
//! finding 2). A prediction day older than every retained coefficient file
//! (the fit writer keeps 14) has no historical fit to score with; the summary
//! leaves such cases out and says so (`fitted_from`, `cases_before_fit`)
//! instead of charging fitted heuristics a `no_model` refusal live serving
//! never gave.
//!
//! # Strictly point-in-time
//!
//! [`run_day`] drops, **before** replaying anything, every input observed at
//! or after the cutoff: sweep-outcome envelopes (`emitted_at`), journal rows
//! (`observed_at`), history samples, and every replay case that had not
//! resolved (`actual_at`) by then. Each case's coefficient file is the newest
//! whose own cutoff is strictly before its prediction day began
//! (`Registry::load`'s rule). So the fold is a function of data knowable at the
//! cutoff only; perturbing anything after it leaves the records bit-identical
//! (the leak test pins this). A case still open at the cutoff is not guessed
//! at: it joins the cohort of the day it resolves.
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
use super::{EstimateInput, Explanation, Heuristic, Kind, Provenance, Registry, Stage, Tier};
use crate::telemetry::kinds::eta_backtest::{EtaBacktestFoldRecord, EtaBacktestSummaryRecord};
use crate::telemetry::TelemetryEnvelope;
use chrono::{DateTime, Duration, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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

/// The marker that a day's records were durably queued for delivery. Computed
/// state (`fold-<day>.json`) and delivered state are separate files so a
/// restart between the two can recover and queue what was folded but never
/// delivered.
#[must_use]
pub fn delivered_path(root: &Path, day: NaiveDate) -> PathBuf {
    dir(root).join(format!("delivered-{}", day.format("%Y-%m-%d")))
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
        .map(|records| {
            // v2 priority inputs (#10508) from the cached roster history.
            let history = super::roster_history::load_for(root, Utc::now()).0;
            backtest::cases_from_pr_records_with_roster(&records, history.as_deref()).0
        })
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

/// Record that `day`'s records were durably queued.
///
/// # Errors
/// The marker could not be written; the day stays pending.
pub fn mark_delivered(root: &Path, day: NaiveDate) -> std::io::Result<()> {
    super::health::write_atomic(&delivered_path(root, day), "delivered\n")
}

/// Every folded day whose records were not yet durably queued, oldest first,
/// read back from the saved fold files. A file that no longer parses is
/// skipped (and logged): it can never be delivered.
#[must_use]
pub fn pending_delivery(root: &Path) -> Vec<(NaiveDate, DayRecords)> {
    done_days(root)
        .into_iter()
        .filter(|day| !delivered_path(root, *day).exists())
        .filter_map(|day| {
            let path = day_path(root, day);
            let parsed = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<DayRecords>(&text).ok());
            if parsed.is_none() {
                log::warn!("eta nightly folds: {} is unreadable; not delivered", path.display());
            }
            parsed.map(|records| (day, records))
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

    (history, known_cases(inputs, cutoff))
}

/// The `land` cases known resolved at `cutoff`: built only from evidence
/// observed before it, predicted and resolved before it.
///
/// The offline PR cache is cut at `cutoff` **before** it is merged with the
/// local cases (#10626): [`backtest::merge_case_sets`] numbers each side's
/// cases per issue and stage, so a PR still open at the cutoff would shift
/// the numbering of the cache side and could let a known case be dropped
/// for, or counted beside, another PR's — the fold changing with data from
/// after its cutoff.
fn known_cases(inputs: &Inputs, cutoff: DateTime<Utc>) -> Vec<ReplayCase> {
    let resolved = |c: &ReplayCase| c.kind == KIND && c.as_of < cutoff && c.actual_at < cutoff;
    let envelopes: Vec<&TelemetryEnvelope> = inputs
        .envelopes
        .iter()
        .filter(|e| e.emitted_at < cutoff)
        .collect();
    let journal: Vec<JournalEntry> = inputs
        .journal
        .iter()
        .filter(|e| e.observed_at < cutoff)
        .cloned()
        .collect();
    let mut cases = backtest::cases_from_envelopes(envelopes);
    cases.extend(backtest::cases_from_journal(&journal));
    cases.retain(resolved);
    let pr_cases: Vec<ReplayCase> = inputs
        .pr_cases
        .iter()
        .filter(|c| resolved(c))
        .cloned()
        .collect();
    let (cases, _dropped) = backtest::merge_case_sets(cases, pr_cases);
    cases
}

/// A case's identity across the daily snapshots (#10626): subject (repo,
/// case-insensitive, issue **and PR**), kind, stage, and its lap — the
/// *n*-th, by `as_of`, of that PR's cases of that kind and stage. Every
/// `land` case of one PR resolves at the same instant (its merge), so another
/// PR on the same issue resolving earlier or later never shifts a lap; and
/// both sources carry the PR number (a `sweep.outcome` record that ends in
/// `merge` is `landed`, which implies `pr_number`, #9441), so the same case
/// read from a second source is not new.
type CohortKey = (CohortGroup, usize);

/// Repo (lowercased), issue, PR, kind and stage: the cases a lap counts among.
type CohortGroup = (String, u32, Option<u32>, Kind, Stage);

fn cohort_keys(cases: &[ReplayCase]) -> Vec<CohortKey> {
    let mut groups: BTreeMap<CohortGroup, Vec<usize>> = BTreeMap::new();
    for (i, c) in cases.iter().enumerate() {
        let s = &c.subject;
        groups
            .entry((s.repo.to_ascii_lowercase(), s.issue, s.pr_number, c.kind, c.stage))
            .or_default()
            .push(i);
    }
    let mut keys = vec![None; cases.len()];
    for (group, mut members) in groups {
        members.sort_by_key(|&i| (cases[i].as_of, i));
        for (lap, i) in members.into_iter().enumerate() {
            keys[i] = Some((group.clone(), lap));
        }
    }
    keys.into_iter().flatten().collect()
}

/// `day`'s cohort out of `known` (the cases known at its cutoff): those whose
/// `CohortKey` was not yet known when `day` began ([`known_cases`] at its
/// start). Each resolved case is in exactly one day's cohort: the day it
/// became known, whichever day it was predicted on, and whatever order the
/// other cases of its issue resolve in.
#[must_use]
pub fn cohort(inputs: &Inputs, day: NaiveDate, known: &[ReplayCase]) -> Vec<ReplayCase> {
    let before: BTreeSet<CohortKey> = cohort_keys(&known_cases(inputs, day_start(day)))
        .into_iter()
        .collect();
    known
        .iter()
        .zip(cohort_keys(known))
        .filter(|(_, key)| !before.contains(key))
        .map(|(c, _)| c.clone())
        .collect()
}

/// One registry per prediction day: the one the fold for that day serves,
/// fitted with the coefficient file newest strictly before the day began
/// (`registry_for(day start)`). A case is scored with its own `as_of` day's
/// registry by every run that scores it — its day's fold and every later
/// summary — so no later fit re-scores an earlier day (#10532 review).
pub struct ByDay {
    days: BTreeMap<NaiveDate, Registry>,
    /// Ids, kinds and tiers only (every registry registers the same ones),
    /// and the registry for a day not in `days` (never asked: `days` covers
    /// every case it was built from).
    unfitted: Registry,
}

impl ByDay {
    /// The registries for `day` and every prediction day in `cases`.
    #[must_use]
    pub fn new(
        cases: &[ReplayCase],
        day: NaiveDate,
        registry_for: &dyn Fn(DateTime<Utc>) -> Registry,
    ) -> Self {
        let mut days = BTreeMap::new();
        for d in cases.iter().map(|c| c.as_of.date_naive()).chain([day]) {
            days.entry(d).or_insert_with(|| registry_for(day_start(d)));
        }
        ByDay {
            days,
            unfitted: Registry::builtin(),
        }
    }

    /// The registry predictions made on `day` are scored with.
    #[must_use]
    pub fn on(&self, day: NaiveDate) -> &Registry {
        self.days.get(&day).unwrap_or(&self.unfitted)
    }

    /// `id`, each estimate served by its own `as_of` day's registry.
    #[must_use]
    pub fn heuristic(&self, id: &str) -> Option<PerDay<'_>> {
        let h = self.unfitted.get(id)?;
        Some(PerDay {
            by_day: self,
            id: h.id(),
            kind: h.kind(),
            tier: h.tier(),
            models_hold: h.models_hold(),
        })
    }

    /// The first prediction day whose registry carries every coefficient
    /// schema any day's does (once a day has a file every later one does:
    /// the newest file is never pruned). `None` when no day has any file.
    #[must_use]
    pub fn fitted_from(&self) -> Option<NaiveDate> {
        let v1 = self.days.values().any(|r| r.fit().is_some());
        let v2 = self.days.values().any(|r| r.fit_v2().is_some());
        if !v1 && !v2 {
            return None;
        }
        self.days
            .iter()
            .find(|(_, r)| (!v1 || r.fit().is_some()) && (!v2 || r.fit_v2().is_some()))
            .map(|(d, _)| *d)
    }
}

/// One registered heuristic replayed through [`ByDay`].
pub struct PerDay<'a> {
    by_day: &'a ByDay,
    id: &'static str,
    kind: Kind,
    tier: Tier,
    models_hold: bool,
}

impl Heuristic for PerDay<'_> {
    fn id(&self) -> &'static str {
        self.id
    }

    fn kind(&self) -> Kind {
        self.kind
    }

    fn tier(&self) -> Tier {
        self.tier
    }

    fn models_hold(&self) -> bool {
        self.models_hold
    }

    fn estimate(&self, input: &EstimateInput, history: &StageSamples) -> Explanation {
        let registry = self.by_day.on(input.as_of.date_naive());
        registry
            .get(self.id)
            .or_else(|| self.by_day.unfitted.get(self.id))
            .expect("every registry registers the same ids")
            .estimate(input, history)
    }
}

/// `history` with the calibrating heuristics' evidence replayed in from
/// `cases`, each base estimate served by its prediction day's registry
/// ([`backtest::with_replay_calibration`], the helper `eta backtest` and
/// `eta promote` call). Leak-free because `history` and `cases` were already
/// cut at the cutoff ([`point_in_time`]); without it
/// `land-2026-10-06-even-lark` would degrade to plain `land-v2` in every
/// fold.
fn calibrated(
    by_day: &ByDay,
    history: &StageSamples,
    cases: &[ReplayCase],
    loom: &Provenance,
) -> StageSamples {
    let mut history = history.clone();
    backtest::with_replay_calibration(|id| by_day.heuristic(id), &mut history, cases, loom);
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
/// of `day` is read (module docs). The fold scores `day`'s [`cohort`], each
/// case with its prediction day's registry ([`ByDay`]).
#[must_use]
pub fn run_day(
    inputs: &Inputs,
    day: NaiveDate,
    current_land: Option<&str>,
    scope: &dyn Fn(StageSamples) -> StageSamples,
    registry_for: &dyn Fn(DateTime<Utc>) -> Registry,
    loom: &Provenance,
) -> DayRecords {
    let cutoff = day_start(day) + Duration::days(1);
    let day_s = day.format("%Y-%m-%d").to_string();
    let (uncalibrated, cases) = point_in_time(inputs, cutoff, scope);
    let day_cases = cohort(inputs, day, &cases);
    let no_filter = Filter::default();

    let by_day = ByDay::new(&cases, day, registry_for);
    let registry = by_day.on(day);
    let day_history = calibrated(&by_day, &uncalibrated, &cases, loom);
    let history = &day_history;
    let Some(current) = by_day.heuristic(registry.current(KIND, current_land).id()) else {
        return DayRecords {
            day: day_s,
            folds: Vec::new(),
            summaries: Vec::new(),
        };
    };
    let current_id = current.id();
    let current_scores = backtest::replay_scored(&current, history, &day_cases, no_filter, loom);
    let current_stats = own_stats(&current_scores);

    let mut folds = Vec::new();
    for h in registry
        .for_kind(KIND)
        .filter_map(|h| by_day.heuristic(h.id()))
    {
        let is_current = h.id() == current_id;
        let scores = backtest::replay_scored(&h, history, &day_cases, no_filter, loom);
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
            if let Ok(cmp) = backtest::compare(&current, &h, history, &day_cases, no_filter, loom) {
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

    let summaries = summaries_for(&cases, history, &current, &by_day, day, loom);
    DayRecords {
        day: day_s,
        folds,
        summaries,
    }
}

/// The rolling standing of every non-`current` heuristic on every case known at
/// the cutoff, judged by the gate function `eta promote` uses
/// ([`shadow::backtest_gate`]). Every case is scored exactly as its own day's
/// fold scored it: with its prediction day's registry ([`ByDay`]) and the
/// same calibrated `history`. Cases predicted before the first day with a
/// retained coefficient file ([`ByDay::fitted_from`]) are left out and
/// counted (`cases_before_fit`): no historical fit survives to score them.
fn summaries_for(
    cases: &[ReplayCase],
    history: &StageSamples,
    current: &PerDay<'_>,
    by_day: &ByDay,
    day: NaiveDate,
    loom: &Provenance,
) -> Vec<EtaBacktestSummaryRecord> {
    let cutoff = day_start(day) + Duration::days(1);
    let day_s = day.format("%Y-%m-%d").to_string();
    let fitted_from = by_day.fitted_from();
    let (window, before_fit): (Vec<ReplayCase>, Vec<ReplayCase>) = cases
        .iter()
        .cloned()
        .partition(|c| fitted_from.is_none_or(|first| c.as_of.date_naive() >= first));
    let registry = by_day.on(day);
    let mut out = Vec::new();
    for h in registry
        .for_kind(KIND)
        .filter(|h| h.id() != current.id())
        .filter_map(|h| by_day.heuristic(h.id()))
    {
        let comparison =
            backtest::compare(current, &h, history, &window, Filter::default(), loom).ok();
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
            fitted_from: fitted_from.map(|d| d.format("%Y-%m-%d").to_string()),
            cases_before_fit: count(before_fit.len()),
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

/// Every retained coefficient file under a workspace (`eta-fit/v1` and
/// `eta-fit/v2`), read once per run: the production `registry_for`. One run
/// builds a registry per prediction day ([`ByDay`]), so re-reading the
/// directory per day ([`Registry::load`]) would parse every file once per day.
pub struct FitArchive {
    v1: Vec<Arc<super::fit::CoefficientFile>>,
    v2: Vec<Arc<super::fit::CoefficientFile>>,
}

impl FitArchive {
    /// Read `root`'s fit directories (an unreadable or foreign file is
    /// skipped, as [`Registry::load`] skips it).
    #[must_use]
    pub fn load(root: &Path) -> Self {
        let read = |dir: PathBuf, schema: &str| -> Vec<Arc<super::fit::CoefficientFile>> {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Vec::new();
            };
            let mut paths: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "json"))
                .collect();
            paths.sort();
            paths
                .iter()
                .filter_map(|p| super::fit::coeffs::read_schema(p, schema))
                .map(Arc::new)
                .collect()
        };
        FitArchive {
            v1: read(super::fit::coeffs::fit_dir(root), super::fit::SCHEMA),
            v2: read(super::fit::v2::fit_dir_v2(root), super::fit::features_v2::SCHEMA_V2),
        }
    }

    /// The registry [`Registry::load`] builds for `before`: each schema's
    /// newest file cut off strictly before it, a later name winning a tie.
    #[must_use]
    pub fn registry_before(&self, before: DateTime<Utc>) -> Registry {
        let newest = |files: &[Arc<super::fit::CoefficientFile>]| {
            files
                .iter()
                .filter(|f| f.as_of < before)
                .max_by(|a, b| a.as_of.cmp(&b.as_of))
                .cloned()
        };
        Registry::with_fits(newest(&self.v1), newest(&self.v2))
    }
}

#[cfg(test)]
#[path = "nightly_folds_tests.rs"]
mod tests;
