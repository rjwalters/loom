//! Shadow mode and the promotion switch (#9328, Phase 5 of #9289).
//!
//! # Shadow mode
//!
//! The tracker computes an estimate for **every** registered heuristic of a
//! kind ([`super::Registry::for_kind`]), not just `current`. Each one is
//! logged as its own `eta.estimate`; only `current`'s is
//! [`super::tracker::Emission::primary`], so nothing downstream changes
//! behaviour. Because every heuristic of a kind is estimated at the same
//! `as_of` for the same subject, and [`super::tracker::Tracker::resolve`]
//! scores every pending estimate of that `(repo, issue, kind)` series against
//! one outcome, the candidate's score and `current`'s score arrive together —
//! already paired, with no join to invent.
//!
//! [`ShadowLedger`] accumulates those pairs. It is a running-sum ledger, not a
//! sample store: a pair contributes to four counters and is forgotten.
//!
//! # The promotion rule (operator decision 2 on #9289)
//!
//! Two gates, **in this order**, both required:
//!
//! 1. **Backtest.** The candidate must beat `current` on the phase-2 backtest's
//!    mean pinball loss for that kind ([`super::backtest::compare`]), over the
//!    identical replay set. Failing this, the live gate is not even consulted
//!    — a heuristic that cannot win on history it can be re-run against has no
//!    business being judged on a live sample nobody can replay.
//! 2. **Live.** At least [`MIN_LIVE_PAIRS`] paired live observations; the
//!    candidate's paired mean pinball loss no worse than `current`'s; and the
//!    candidate's p25–p75 coverage inside
//!    `[`[`COVERAGE_MIN`]`, `[`COVERAGE_MAX`]`]`.
//!
//! Either gate failing leaves `current` exactly as it was. Every evaluation —
//! promoting or not — produces a [`PromotionDecision`] carrying the numbers
//! that decided it, so an operator can answer "why did this flip?" (or "why
//! has it not?") from the record rather than by re-deriving it.
//!
//! # Where a flip is persisted
//!
//! [`super::config::promote`] writes `autonomous.eta.current.<kind>` into the
//! **host-local** config tier (`.loom-local/local.json`, the highest-precedence
//! tier and a gitignored one). Host-local is the honest scope: the evidence
//! behind the flip is this host's own history and this host's own live pairs
//! (#9343), and a daemon must never dirty a tracked file under a fleet of
//! worktrees. Rolling a promotion fleet-wide is an operator action — copy the
//! key into the committed config — not something a single host may decide for
//! everyone.

use super::backtest::Comparison;
use super::score::Score;
use super::tracker::Resolved;
use super::Kind;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Fewest live paired observations before the live gate may pass.
pub const MIN_LIVE_PAIRS: usize = 50;

/// Lowest candidate p25–p75 coverage the live gate accepts.
pub const COVERAGE_MIN: f64 = 0.40;

/// Highest candidate p25–p75 coverage the live gate accepts.
pub const COVERAGE_MAX: f64 = 0.60;

/// Schema tag of one promotion-decision record.
pub const DECISION_SCHEMA: &str = "eta-promotion-decision/v1";

/// Which two heuristics, for which kind, a run of pairs is between.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PairKey {
    /// What both predict.
    pub kind: Kind,
    /// The `current` heuristic's id at the time the pairs were taken.
    pub current: String,
    /// The candidate's id.
    pub candidate: String,
}

/// Running sums for one [`PairKey`]. Serialised as-is: the ledger persists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PairSums {
    /// Paired observations (both sides scored against the same outcome).
    pub pairs: usize,
    /// Σ of `current`'s pinball loss over those pairs, seconds.
    pub current_loss_sec: f64,
    /// Σ of the candidate's pinball loss over those pairs, seconds.
    pub candidate_loss_sec: f64,
    /// How many pairs `current` covered (`p25 ≤ actual ≤ p75`).
    pub current_covered: usize,
    /// How many pairs the candidate covered.
    pub candidate_covered: usize,
}

/// A readable view of one [`PairKey`]'s accumulated evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedStats {
    /// Which comparison.
    pub key: PairKey,
    /// Paired observations.
    pub pairs: usize,
    /// `current`'s paired mean pinball loss, seconds. `None` with no pairs.
    pub current_mean_pinball_loss_sec: Option<f64>,
    /// The candidate's paired mean pinball loss, seconds.
    pub candidate_mean_pinball_loss_sec: Option<f64>,
    /// `current`'s p25–p75 coverage over the pairs.
    pub current_coverage: Option<f64>,
    /// The candidate's p25–p75 coverage over the pairs — the gated one.
    pub candidate_coverage: Option<f64>,
}

impl PairedStats {
    /// The stats of `sums` under `key`.
    #[must_use]
    pub fn of(key: PairKey, sums: PairSums) -> Self {
        let n = sums.pairs;
        let mean = |total: f64| (n > 0).then(|| total / n as f64);
        let share = |count: usize| (n > 0).then(|| count as f64 / n as f64);
        PairedStats {
            key,
            pairs: n,
            current_mean_pinball_loss_sec: mean(sums.current_loss_sec),
            candidate_mean_pinball_loss_sec: mean(sums.candidate_loss_sec),
            current_coverage: share(sums.current_covered),
            candidate_coverage: share(sums.candidate_covered),
        }
    }
}

/// The live paired-scoring accumulator.
///
/// Persisted across restarts as one JSON document (`.loom/state/eta/
/// shadow.json`), like the pending-estimate file beside it: a promotion gate
/// that needs 50 pairs cannot afford to restart its count on every daemon
/// roll.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ShadowLedger {
    /// Running sums per comparison.
    pub pairs: BTreeMap<String, PairSums>,
    /// Keys, alongside their encoded form, so the map stays JSON-object
    /// shaped while [`PairKey`] stays structured.
    pub keys: BTreeMap<String, PairKey>,
}

/// `kind|current|candidate`, the map key for a [`PairKey`].
fn encode(key: &PairKey) -> String {
    format!("{}|{}|{}", key.kind.as_str(), key.current, key.candidate)
}

/// One scored estimate, reduced to what pairing needs.
struct Scored<'a> {
    heuristic: &'a str,
    score: &'a Score,
}

impl ShadowLedger {
    /// Record every pair `resolved` contains.
    ///
    /// Two resolved estimates pair when they share a subject (`repo`,
    /// `issue`), a [`Kind`] **and an `as_of`** — which is exactly the shape
    /// shadow mode produces, since one pass estimates every heuristic of a
    /// kind at one instant. Both sides must have scored (an `abandoned`
    /// outcome and a refusal score nothing, and neither may be counted as a
    /// win for whichever side happened to answer).
    ///
    /// `current` names the heuristic that was primary; every other heuristic
    /// in the group is a candidate paired against it. A group with no
    /// `current` side records nothing: there is no baseline to compare to.
    pub fn record(&mut self, current: &dyn Fn(Kind) -> String, resolved: &[Resolved]) {
        let mut groups: BTreeMap<(String, u32, Kind, DateTime<Utc>), Vec<Scored<'_>>> =
            BTreeMap::new();
        for r in resolved {
            if r.score.pinball_loss_sec.is_none() {
                continue;
            }
            let key = (
                r.estimate.repo.to_ascii_lowercase(),
                r.estimate.issue,
                r.estimate.kind,
                r.estimate.as_of,
            );
            groups.entry(key).or_default().push(Scored {
                heuristic: r.estimate.heuristic.as_str(),
                score: &r.score,
            });
        }

        for ((_, _, kind, _), group) in groups {
            let current_id = current(kind);
            let Some(base) = group.iter().find(|s| s.heuristic == current_id) else {
                continue;
            };
            for other in group.iter().filter(|s| s.heuristic != current_id) {
                let key = PairKey {
                    kind,
                    current: current_id.clone(),
                    candidate: other.heuristic.to_string(),
                };
                let encoded = encode(&key);
                self.keys.entry(encoded.clone()).or_insert(key);
                let sums = self.pairs.entry(encoded).or_default();
                sums.pairs += 1;
                sums.current_loss_sec += base.score.pinball_loss_sec.unwrap_or(0.0);
                sums.candidate_loss_sec += other.score.pinball_loss_sec.unwrap_or(0.0);
                sums.current_covered += usize::from(base.score.covered == Some(true));
                sums.candidate_covered += usize::from(other.score.covered == Some(true));
            }
        }
    }

    /// The accumulated evidence for one comparison (all-zero when there is
    /// none — "no pairs yet" is an answer, never an absence).
    #[must_use]
    pub fn stats(&self, kind: Kind, current: &str, candidate: &str) -> PairedStats {
        let key = PairKey {
            kind,
            current: current.to_string(),
            candidate: candidate.to_string(),
        };
        let sums = self.pairs.get(&encode(&key)).copied().unwrap_or_default();
        PairedStats::of(key, sums)
    }

    /// Every comparison the ledger holds evidence for.
    #[must_use]
    pub fn all(&self) -> Vec<PairedStats> {
        self.keys
            .iter()
            .map(|(encoded, key)| {
                PairedStats::of(key.clone(), self.pairs.get(encoded).copied().unwrap_or_default())
            })
            .collect()
    }

    /// Forget every pair recorded against `key` — what a completed promotion
    /// does, so the new `current`'s own comparisons start from zero rather
    /// than inheriting the pairs that justified the flip.
    pub fn clear(&mut self, key: &PairKey) {
        let encoded = encode(key);
        self.pairs.remove(&encoded);
        self.keys.remove(&encoded);
    }
}

/// How one gate came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateStatus {
    /// The gate passed.
    Passed,
    /// The gate failed; `current` stands.
    Failed,
    /// The gate was never reached — an earlier gate in the order failed, so
    /// this one was deliberately not consulted.
    NotReached,
}

impl GateStatus {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GateStatus::Passed => "passed",
            GateStatus::Failed => "failed",
            GateStatus::NotReached => "not_reached",
        }
    }
}

/// The backtest gate's verdict and the numbers behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BacktestGate {
    /// Outcome.
    pub status: GateStatus,
    /// Why, in one line.
    pub detail: String,
    /// Cases the backtest actually scored for `current`.
    pub current_scored: usize,
    /// Cases it scored for the candidate (the same replay set).
    pub candidate_scored: usize,
    /// `current`'s mean pinball loss over them, seconds.
    pub current_mean_pinball_loss_sec: Option<f64>,
    /// The candidate's.
    pub candidate_mean_pinball_loss_sec: Option<f64>,
}

/// The live gate's verdict and the numbers behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveGate {
    /// Outcome.
    pub status: GateStatus,
    /// Why, in one line.
    pub detail: String,
    /// The paired evidence, as accumulated.
    pub stats: PairedStats,
    /// Pairs the gate required.
    pub min_pairs: usize,
    /// Coverage band the gate required.
    pub coverage_band: (f64, f64),
}

/// One promotion evaluation, whatever it decided.
///
/// This is the auditable record the operator reads: which candidate, against
/// which `current`, what each gate saw, and whether the config was flipped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromotionDecision {
    /// Always [`DECISION_SCHEMA`].
    pub schema: String,
    /// When the evaluation ran.
    pub at: DateTime<Utc>,
    /// What is being promoted for.
    pub kind: Kind,
    /// The incumbent.
    pub current: String,
    /// The challenger.
    pub candidate: String,
    /// Gate 1.
    pub backtest: BacktestGate,
    /// Gate 2 — `NotReached` when gate 1 failed.
    pub live: LiveGate,
    /// Whether both gates passed, i.e. whether `current` may be flipped.
    pub promote: bool,
    /// One-line summary of the deciding reason.
    pub reason: String,
    /// The config file the flip was written to, when one was.
    pub config_path: Option<String>,
}

/// Evaluate the promotion rule for `candidate` against `current`.
///
/// `comparison` is the phase-2 backtest of the two on the identical replay set
/// (`None` when no backtest could be run at all, which is a gate **failure**,
/// never a pass: an unrun gate has not been cleared). `stats` is the live
/// paired evidence from a [`ShadowLedger`].
///
/// Pure: it decides, it does not write. [`promote_if_ready`] is what acts on
/// the decision.
#[must_use]
pub fn evaluate(
    kind: Kind,
    current: &str,
    candidate: &str,
    comparison: Option<&Comparison>,
    stats: &PairedStats,
    at: DateTime<Utc>,
) -> PromotionDecision {
    let backtest = backtest_gate(current, candidate, comparison);
    let live = if backtest.status == GateStatus::Passed {
        live_gate(stats)
    } else {
        LiveGate {
            status: GateStatus::NotReached,
            detail: "backtest gate failed; the live gate is not consulted".to_string(),
            stats: stats.clone(),
            min_pairs: MIN_LIVE_PAIRS,
            coverage_band: (COVERAGE_MIN, COVERAGE_MAX),
        }
    };
    let promote = backtest.status == GateStatus::Passed && live.status == GateStatus::Passed;
    let reason = if promote {
        format!("both gates passed: {} then {}", backtest.detail, live.detail)
    } else if backtest.status == GateStatus::Passed {
        format!("live gate failed: {}", live.detail)
    } else {
        format!("backtest gate failed: {}", backtest.detail)
    };
    PromotionDecision {
        schema: DECISION_SCHEMA.to_string(),
        at,
        kind,
        current: current.to_string(),
        candidate: candidate.to_string(),
        backtest,
        live,
        promote,
        reason,
        config_path: None,
    }
}

/// Which report in `comparison` belongs to `id`.
fn report_for<'a>(
    comparison: &'a Comparison,
    id: &str,
) -> Option<&'a super::backtest::BacktestReport> {
    if comparison.a.heuristic == id {
        Some(&comparison.a)
    } else if comparison.b.heuristic == id {
        Some(&comparison.b)
    } else {
        None
    }
}

fn backtest_gate(current: &str, candidate: &str, comparison: Option<&Comparison>) -> BacktestGate {
    let fail = |detail: String| BacktestGate {
        status: GateStatus::Failed,
        detail,
        current_scored: 0,
        candidate_scored: 0,
        current_mean_pinball_loss_sec: None,
        candidate_mean_pinball_loss_sec: None,
    };
    let Some(comparison) = comparison else {
        return fail("no backtest was run".to_string());
    };
    let (Some(a), Some(b)) = (report_for(comparison, current), report_for(comparison, candidate))
    else {
        return fail(format!("the comparison is not between {current} and {candidate}"));
    };
    let gate = |status: GateStatus, detail: String| BacktestGate {
        status,
        detail,
        current_scored: a.overall.scored,
        candidate_scored: b.overall.scored,
        current_mean_pinball_loss_sec: a.overall.mean_pinball_loss_sec,
        candidate_mean_pinball_loss_sec: b.overall.mean_pinball_loss_sec,
    };
    if a.overall.scored == 0 || b.overall.scored == 0 {
        // #9579: the `land` kind derives zero replay cases on this fleet
        // today. Nothing to rule on is a refusal to promote, not a pass.
        return gate(
            GateStatus::Failed,
            format!(
                "the replay set scored {} case(s) for {current} and {} for {candidate}: \
                 nothing to compare",
                a.overall.scored, b.overall.scored
            ),
        );
    }
    if comparison.better.as_deref() == Some(candidate) {
        gate(
            GateStatus::Passed,
            format!(
                "{candidate} mean pinball {:.1}s beats {current}'s {:.1}s over {} case(s)",
                b.overall.mean_pinball_loss_sec.unwrap_or(f64::NAN),
                a.overall.mean_pinball_loss_sec.unwrap_or(f64::NAN),
                b.overall.scored
            ),
        )
    } else {
        gate(
            GateStatus::Failed,
            format!(
                "{candidate} mean pinball {:.1}s does not beat {current}'s {:.1}s",
                b.overall.mean_pinball_loss_sec.unwrap_or(f64::NAN),
                a.overall.mean_pinball_loss_sec.unwrap_or(f64::NAN),
            ),
        )
    }
}

fn live_gate(stats: &PairedStats) -> LiveGate {
    let gate = |status: GateStatus, detail: String| LiveGate {
        status,
        detail,
        stats: stats.clone(),
        min_pairs: MIN_LIVE_PAIRS,
        coverage_band: (COVERAGE_MIN, COVERAGE_MAX),
    };
    if stats.pairs < MIN_LIVE_PAIRS {
        return gate(
            GateStatus::Failed,
            format!("{} live pair(s), {MIN_LIVE_PAIRS} required", stats.pairs),
        );
    }
    let (Some(current_loss), Some(candidate_loss)) =
        (stats.current_mean_pinball_loss_sec, stats.candidate_mean_pinball_loss_sec)
    else {
        return gate(GateStatus::Failed, "no paired losses to compare".to_string());
    };
    if candidate_loss > current_loss {
        return gate(
            GateStatus::Failed,
            format!("paired mean pinball {candidate_loss:.1}s is worse than {current_loss:.1}s"),
        );
    }
    let Some(coverage) = stats.candidate_coverage else {
        return gate(GateStatus::Failed, "no coverage to check".to_string());
    };
    if !(COVERAGE_MIN..=COVERAGE_MAX).contains(&coverage) {
        return gate(
            GateStatus::Failed,
            format!(
                "coverage {:.1}% is outside [{:.0}%, {:.0}%]",
                coverage * 100.0,
                COVERAGE_MIN * 100.0,
                COVERAGE_MAX * 100.0
            ),
        );
    }
    gate(
        GateStatus::Passed,
        format!(
            "{} pair(s), paired mean pinball {candidate_loss:.1}s vs {current_loss:.1}s, \
             coverage {:.1}%",
            stats.pairs,
            coverage * 100.0
        ),
    )
}

/// Evaluate, and **on a pass only**, flip `autonomous.eta.current.<kind>` in
/// `config_path` and clear the comparison's accumulated pairs.
///
/// The decision is returned either way, with `config_path` filled in exactly
/// when a flip was written. A failed gate touches nothing.
///
/// # Errors
///
/// The config file could not be read or rewritten. The decision is lost with
/// it — deliberately: a flip that was not persisted must not be recorded as
/// one.
pub fn promote_if_ready(
    ledger: &mut ShadowLedger,
    kind: Kind,
    current: &str,
    candidate: &str,
    comparison: Option<&Comparison>,
    config_path: &std::path::Path,
    at: DateTime<Utc>,
) -> std::io::Result<PromotionDecision> {
    let stats = ledger.stats(kind, current, candidate);
    let mut decision = evaluate(kind, current, candidate, comparison, &stats, at);
    if decision.promote {
        super::config::promote(config_path, kind, candidate)?;
        decision.config_path = Some(config_path.display().to_string());
        ledger.clear(&stats.key);
    }
    Ok(decision)
}

/// File name of the promotion-decision log under `<workspace>/.loom/logs/`.
pub const DECISION_LOG_FILENAME: &str = "eta-promotions.jsonl";

/// Where the shadow ledger persists, and where decisions are logged.
#[must_use]
pub fn ledger_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root
        .join(".loom")
        .join("state")
        .join("eta")
        .join("shadow.json")
}

/// The promotion-decision log for `workspace_root`.
#[must_use]
pub fn decision_log_path(workspace_root: &std::path::Path) -> std::path::PathBuf {
    workspace_root
        .join(".loom")
        .join("logs")
        .join(DECISION_LOG_FILENAME)
}

/// Append `decision` to the log at `path`, one JSON line.
///
/// # Errors
///
/// The directory could not be created, or the append failed.
pub fn append_decision(
    path: &std::path::Path,
    decision: &PromotionDecision,
) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(decision).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")
}

/// Read the ledger at `path`; an absent or malformed file is an empty ledger,
/// never a failure — a lost count costs a longer wait, not correctness.
#[must_use]
pub fn read_ledger(path: &std::path::Path) -> ShadowLedger {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Persist `ledger` to `path` atomically (write-then-rename).
///
/// # Errors
///
/// The directory could not be created, or the write/rename failed.
pub fn write_ledger(path: &std::path::Path, ledger: &ShadowLedger) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(ledger).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}
