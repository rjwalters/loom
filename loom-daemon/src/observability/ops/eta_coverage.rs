//! ETA outcome-coverage gauges (Issue #10933, slice 1).
//!
//! Every accuracy figure is computed over the estimates that got an
//! `eta.outcome`. An estimate that never gets one — because its item is
//! still open, or because the tracker dropped it — is invisible to those
//! figures, and the ones that go missing are disproportionately the slow
//! items, so the figures look early (survivorship). These gauges say how
//! much is missing and why, next to the existing `loom.eta.health.*` set:
//!
//! - `loom.eta.health.pending{kind,heuristic,age_bucket}`: pending **series**
//!   (`(repo, issue, kind, heuristic)`), bucketed by the age of the series'
//!   earliest pending estimate;
//! - `loom.eta.health.pending_oldest_age_seconds{kind,heuristic}`;
//! - `loom.eta.health.pending_lost{reason}`: cumulative estimates dropped
//!   without an outcome, by drop path ([`LossReason`]);
//! - `loom.eta.health.outcomes{kind,heuristic,outcome}`: cumulative outcomes
//!   emitted, `outcome` being the outcome kind or `refused` ([`outcome_label`]).
//!
//! The counters are since process start (like `pending_over_cap`), so a
//! restart resets them; SigNoz reads them with a rate or an increase.
//!
//! Authority-only by construction (#10498): a non-authority host holds no
//! pending store and resolves nothing, so it exports no `pending` or
//! `outcomes` points. The one exception is `pending_lost{dropped_authority}`,
//! which is counted where the loss happens: on the host that stopped being
//! the authority (or restarted as a non-authority) and dropped its store.
//!
//! Labels are closed vocabularies; no repo, issue or estimate id.
//! [`points`] is pure over [`Facts`]; the counters live in [`Memory`], held
//! inside the `eta_health` state and written through [`note_lost`],
//! [`note_pass`] and [`note_outcomes`].

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use crate::eta::score::{EstimateSummary, OutcomeKind};
use crate::eta::tracker::{Dropped, Expired, Resolved};
use crate::telemetry::ops::{MetricName, MetricPoint};

/// The `age_bucket` vocabulary, youngest first, with each bucket's upper
/// bound in seconds (exclusive); the last is open-ended.
pub const AGE_BUCKETS: [(&str, i64); 5] = [
    ("lt_4h", 4 * 3600),
    ("4h_24h", 24 * 3600),
    ("1d_3d", 3 * 86_400),
    ("3d_7d", 7 * 86_400),
    ("gt_7d", i64::MAX),
];

/// The bucket of a series whose earliest pending estimate is `age_sec` old.
#[must_use]
pub fn age_bucket(age_sec: i64) -> &'static str {
    AGE_BUCKETS
        .iter()
        .find(|(_, upper)| age_sec < *upper)
        .map_or("gt_7d", |(name, _)| name)
}

/// Why a pending estimate was dropped without an `eta.outcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LossReason {
    /// Evicted by the `MAX_PENDING` cap (thinned refreshes, or a whole
    /// series when distinct series exceed the cap) and not censored.
    EvictedCap,
    /// Dropped because this host is not (or stopped being) the ETA
    /// authority (#10498).
    DroppedAuthority,
    /// Expired at `PENDING_MAX_AGE_DAYS` with no decided late surprise.
    ExpiredUndecided,
    /// Emitted after its own outcome instant (a late resolution's tail).
    OrphanedPostOutcome,
    /// Restored for a heuristic that is no longer registered (#10484).
    RetiredHeuristic,
}

impl LossReason {
    /// Every reason, in label order.
    pub const ALL: [LossReason; 5] = [
        LossReason::EvictedCap,
        LossReason::DroppedAuthority,
        LossReason::ExpiredUndecided,
        LossReason::OrphanedPostOutcome,
        LossReason::RetiredHeuristic,
    ];

    /// The `reason` label.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            LossReason::EvictedCap => "evicted_cap",
            LossReason::DroppedAuthority => "dropped_authority",
            LossReason::ExpiredUndecided => "expired_undecided",
            LossReason::OrphanedPostOutcome => "orphaned_post_outcome",
            LossReason::RetiredHeuristic => "retired_heuristic",
        }
    }
}

/// The `outcome` label of a resolved estimate: `refused` for a refusal (no
/// p50, so never scored, whatever happened to the item), else the outcome
/// kind (`started`, `landed`, `finished`, `abandoned`, `censored`).
#[must_use]
pub fn outcome_label(resolved: &Resolved) -> &'static str {
    if resolved.estimate.p50_sec.is_none() {
        return "refused";
    }
    match resolved.score.outcome {
        OutcomeKind::Started => "started",
        OutcomeKind::Landed => "landed",
        OutcomeKind::Finished => "finished",
        OutcomeKind::Abandoned => "abandoned",
        OutcomeKind::Censored => "censored",
    }
}

/// `(kind, heuristic)`.
type SeriesKey = (String, String);
/// `(kind, heuristic, age_bucket)` or `(kind, heuristic, outcome)`.
type Key3 = (String, String, String);

/// The pending store, as the coverage gauges read it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PendingAges {
    /// Pending series per `(kind, heuristic, age_bucket)`.
    pub series: BTreeMap<Key3, u64>,
    /// Age in seconds of the oldest pending series per `(kind, heuristic)`.
    pub oldest: BTreeMap<SeriesKey, i64>,
}

/// Bucket `pending` by series age at `now`. A series is
/// `(repo, issue, kind, heuristic)` and its age is that of its **earliest**
/// pending estimate, so refreshes never make a series look young. Pure.
#[must_use]
pub fn pending_ages(pending: &[EstimateSummary], now: DateTime<Utc>) -> PendingAges {
    let mut earliest: BTreeMap<(String, u32, &str, &str), DateTime<Utc>> = BTreeMap::new();
    for p in pending {
        let key = (p.repo.to_ascii_lowercase(), p.issue, p.kind.as_str(), p.heuristic.as_str());
        let at = earliest.entry(key).or_insert(p.as_of);
        if p.as_of < *at {
            *at = p.as_of;
        }
    }
    let mut out = PendingAges::default();
    for ((_, _, kind, heuristic), at) in earliest {
        let age = (now - at).num_seconds().max(0);
        let bucket = age_bucket(age);
        *out.series
            .entry((kind.to_string(), heuristic.to_string(), bucket.to_string()))
            .or_default() += 1;
        let oldest = out
            .oldest
            .entry((kind.to_string(), heuristic.to_string()))
            .or_insert(age);
        *oldest = (*oldest).max(age);
    }
    out
}

/// The cumulative counters and what was last exported. Lives in the
/// `eta_health` state; `Default` is "nothing measured yet".
#[derive(Debug, Default, Clone)]
pub struct Memory {
    /// Estimates lost per reason; `None` before the first note (unknown is
    /// not zero).
    lost: Option<BTreeMap<LossReason, u64>>,
    /// Outcomes emitted per `(kind, heuristic, outcome)`.
    outcomes: BTreeMap<Key3, u64>,
    /// `pending` bucket keys exported on earlier passes, so a bucket that
    /// empties is zeroed rather than left at its last value.
    emitted_pending: BTreeSet<Key3>,
}

impl Memory {
    /// Add `n` losses for `reason`. `n == 0` still marks the counters as
    /// measured.
    pub fn lost(&mut self, reason: LossReason, n: usize) {
        let lost = self.lost.get_or_insert_with(BTreeMap::new);
        let total = lost.entry(reason).or_default();
        *total = total.saturating_add(n as u64);
    }

    /// Count one tracker pass: its [`Expired`] (undecided expiries, and the
    /// cap evictions that were censored rather than lost) and its
    /// [`Dropped`] (cap evictions, post-outcome orphans).
    pub fn pass(&mut self, expired: &Expired, dropped: &Dropped) {
        let evicted = dropped.over_cap.saturating_sub(expired.cap_censored);
        self.lost(LossReason::EvictedCap, evicted);
        self.lost(LossReason::ExpiredUndecided, expired.undecided);
        self.lost(LossReason::OrphanedPostOutcome, dropped.orphaned);
    }

    /// Count emitted outcomes by kind, heuristic and [`outcome_label`].
    pub fn outcomes(&mut self, outcomes: &[Resolved]) {
        for r in outcomes {
            let key = (
                r.estimate.kind.as_str().to_string(),
                r.estimate.heuristic.clone(),
                outcome_label(r).to_string(),
            );
            *self.outcomes.entry(key).or_default() += 1;
        }
    }

    /// Remember the `pending` keys a pass exported. An unknown pass keeps
    /// the memory, so the series is still zeroed once it is measurable.
    pub fn remember(&mut self, facts: &Facts) {
        if let Some(pending) = &facts.pending {
            self.emitted_pending = pending.series.keys().cloned().collect();
        }
    }

    /// Take `other`'s export memory (the collector pass's copy), keeping
    /// this one's counters, which a writer may have moved mid-pass.
    pub fn keep_emitted(&mut self, other: Memory) {
        self.emitted_pending = other.emitted_pending;
    }
}

/// Everything one coverage export reads.
#[derive(Debug, Default)]
pub struct Facts {
    /// `None` when ETA is disabled (no tracker).
    pub pending: Option<PendingAges>,
    /// Cumulative losses; `None` before anything was counted.
    pub lost: Option<BTreeMap<LossReason, u64>>,
    /// Cumulative outcomes per `(kind, heuristic, outcome)`.
    pub outcomes: BTreeMap<Key3, u64>,
    /// `pending` keys exported on earlier passes; zeroed when absent now.
    pub prev_pending: BTreeSet<Key3>,
}

/// The facts for one export: `memory`'s counters plus the live `pending`
/// reading.
#[must_use]
pub fn gather(memory: &Memory, pending: Option<PendingAges>) -> Facts {
    Facts {
        pending,
        lost: memory.lost.clone(),
        outcomes: memory.outcomes.clone(),
        prev_pending: memory.emitted_pending.clone(),
    }
}

fn count(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// The coverage gauges for `facts`. Pure.
#[must_use]
pub fn points(facts: &Facts) -> Vec<MetricPoint> {
    let mut out = Vec::new();
    if let Some(pending) = &facts.pending {
        let zeroed = facts
            .prev_pending
            .iter()
            .filter(|k| !pending.series.contains_key(*k))
            .map(|k| (k, 0));
        for ((kind, heuristic, bucket), n) in
            pending.series.iter().map(|(k, n)| (k, *n)).chain(zeroed)
        {
            out.push(
                MetricPoint::int(MetricName::EtaHealthPending, count(n))
                    .label("kind", kind)
                    .label("heuristic", heuristic)
                    .label("age_bucket", bucket),
            );
        }
        for ((kind, heuristic), age) in &pending.oldest {
            out.push(
                MetricPoint::int(MetricName::EtaHealthPendingOldestAgeSeconds, *age)
                    .label("kind", kind)
                    .label("heuristic", heuristic),
            );
        }
    }
    if let Some(lost) = &facts.lost {
        for reason in LossReason::ALL {
            let n = lost.get(&reason).copied().unwrap_or(0);
            out.push(
                MetricPoint::int(MetricName::EtaHealthPendingLost, count(n))
                    .label("reason", reason.as_str()),
            );
        }
    }
    for ((kind, heuristic, outcome), n) in &facts.outcomes {
        out.push(
            MetricPoint::int(MetricName::EtaHealthOutcomes, count(*n))
                .label("kind", kind)
                .label("heuristic", heuristic)
                .label("outcome", outcome),
        );
    }
    out
}

/// Add `n` losses for `reason` to the global counters. A zero is not
/// noted: the per-pass drop on a non-authority host (`dropped_authority`,
/// almost always 0) must not make that host export the loss gauges.
pub fn note_lost(reason: LossReason, n: usize) {
    if n == 0 {
        return;
    }
    super::eta_health::note_coverage(|m| m.lost(reason, n));
}

/// Count one tracker pass's losses in the global counters ([`Memory::pass`]).
pub fn note_pass(expired: &Expired, dropped: &Dropped) {
    super::eta_health::note_coverage(|m| m.pass(expired, dropped));
}

/// Count emitted outcomes in the global counters.
pub fn note_outcomes(outcomes: &[Resolved]) {
    if outcomes.is_empty() {
        return;
    }
    super::eta_health::note_coverage(|m| m.outcomes(outcomes));
}

#[cfg(test)]
#[path = "eta_coverage_tests.rs"]
mod tests;
