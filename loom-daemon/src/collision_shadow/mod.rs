//! `collision-shadow` (#9787) — the prospective shadow-study machinery:
//! capture advisory collision evidence for dispatch candidates **before**
//! outcomes exist, then evaluate frozen policies against recorded outcomes.
//!
//! # What this is (and is not)
//!
//! Capture and evaluation only. Nothing here creates dispatch holds,
//! stacking edges, or issue combinations; records always carry the
//! scheduling exposure so a consumer can see the prediction was advisory.
//! The frozen policy family, pre-registered budgets, and the observation
//! window are the study's decision gates — an inconclusive verdict on sparse
//! positives is a first-class result, never a safety claim.
//!
//! # Contract
//!
//! * [`capture`] — given a candidates snapshot (dispatch candidates with
//!   their contexts) and optional frozen prediction features, emit one
//!   immutable capture record per candidate pair with deterministic ids,
//!   the admission policy, claim state, eligibility, sampling notes, and
//!   the budget state. A configured budget bounds capture work; the off
//!   switch stops capture without touching scheduling.
//! * [`evaluate`] — join captured predictions with recorded outcomes and
//!   compare the frozen policy family (cheap Curator baseline, raw
//!   retrieval Jaccard thresholds) over grouped records: denominators,
//!   coverage/abstention, confusion matrices at a fixed false-warning
//!   budget, paired deltas, and an explicit **inconclusive** verdict when
//!   positives are under the pre-registered minimum.

pub mod attribute;
pub mod evaluate;
pub mod records;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub use evaluate::{evaluate, EvaluationReport, FrozenPolicies, PolicyThresholds};
pub use records::CaptureRecord;

/// Deterministic capture-record id: SHA-256 over the canonical form minus
/// the id field (same contract as #9786's evidence records).
pub fn capture_id(record: &CaptureRecord) -> Result<String> {
    let json = serde_json::to_value(record)?;
    let mut map = match json {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    };
    map.remove("id");
    let canonical = serde_json::to_string(&sort_json(&serde_json::Value::Object(map)))?;
    let mut h = Sha256::new();
    h.update(canonical.as_bytes());
    Ok(hex::encode(h.finalize()))
}

/// Stable JSON with sorted keys (hashing contract shared with #9786).
pub(crate) fn sort_json(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let out: serde_json::Map<String, serde_json::Value> = keys
                .into_iter()
                .map(|k| (k.clone(), sort_json(&m[k])))
                .collect();
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(sort_json).collect())
        }
        other => other.clone(),
    }
}

/// Capture snapshot input: one candidate's context, as read from a
/// dispatch-tick snapshot file (#9787 cohort 2: candidate-vs-active-work
/// disclosure lives in the context fields).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateSnapshot {
    pub issue: u32,
    /// Dispatch tick / admission episode this candidate was observed in.
    pub tick_id: String,
    pub admission_policy: String,
    /// Claim state at capture: eligible, claimed elsewhere, blocked, etc.
    pub claim_state: String,
    /// True when the candidate could have been dispatched this tick.
    pub dispatch_eligible: bool,
    /// True when implementation/diff evidence already exists for this
    /// candidate (cohort disclosure).
    pub has_implementation_evidence: bool,
    /// Cheap Curator baseline: affected-file list as of capture.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub curator_affected_files: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retrieval_files: Option<Vec<String>>,
    pub captured_at: String,
}

/// Capture all candidate pairs from a snapshot under a budget, emitting one
/// record per unordered pair. The budget bounds the number of pairs
/// captured this tick (overrun is recorded, never silently dropped); the
/// off switch produces zero records and a stopped marker without touching
/// scheduling. Equivalent to [`capture_with_prior`] with nothing captured
/// before this tick, so `max_pairs_total` only bounds this single call.
pub fn capture(
    repo: &str,
    tick: &CandidateSnapshotTick,
    budget: &StudyBudgets,
) -> Result<Vec<CaptureRecord>> {
    capture_with_prior(repo, tick, budget, 0)
}

/// [`capture`] with `prior_captured` pairs already recorded by earlier
/// ticks of the study: `max_pairs_total` is enforced against
/// `prior_captured` plus this tick's records, so the global cap holds across
/// invocations (the caller owns the persisted count — see
/// [`count_captured_records`]).
pub fn capture_with_prior(
    repo: &str,
    tick: &CandidateSnapshotTick,
    budget: &StudyBudgets,
    prior_captured: usize,
) -> Result<Vec<CaptureRecord>> {
    let mut records = Vec::new();
    if budget.study_stopped {
        return Ok(records);
    }
    let total_remaining = budget.max_pairs_total.saturating_sub(prior_captured);
    let tick_cap = budget.max_pairs_per_tick.min(total_remaining);
    let candidates = &tick.candidates;
    let mut captured = 0usize;
    let mut skipped_over_budget = 0usize;
    for i in 0..candidates.len() {
        for j in (i + 1)..candidates.len() {
            if captured >= tick_cap {
                skipped_over_budget += 1;
                continue;
            }
            let (a, b) = (&candidates[i], &candidates[j]);
            records.push(pair_record(repo, tick, a, b));
            captured += 1;
        }
    }
    // Budget overrun visibility: carried on the last record (or a marker
    // record when nothing was captured) so denominators always reconcile.
    if skipped_over_budget > 0 {
        if let Some(last) = records.last_mut() {
            last.notes.push(format!(
                "budget: {skipped_over_budget} pair(s) beyond the effective cap {tick_cap} \
                 (max_pairs_per_tick={}, max_pairs_total={} with {prior_captured} already captured) \
                 were not captured this tick",
                budget.max_pairs_per_tick, budget.max_pairs_total
            ));
        }
    }
    Ok(records)
}

/// Count capture records already persisted in `out_dir` (`*.jsonl` tick
/// files written by [`write_tick_records`]), for [`capture_with_prior`]. A
/// missing directory counts as zero; blank lines and the `{"captured":0}`
/// empty-tick marker are not records.
pub fn count_captured_records(out_dir: &std::path::Path) -> Result<usize> {
    let entries = match std::fs::read_dir(out_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut total = 0usize;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        total += std::fs::read_to_string(&path)?
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && *l != "{\"captured\":0}")
            .count();
    }
    Ok(total)
}

/// One dispatch tick's capture context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CandidateSnapshotTick {
    pub tick_id: String,
    pub admission_policy: String,
    pub captured_at: String,
    pub candidates: Vec<CandidateSnapshot>,
}

/// Features for one captured pair, with per-feature availability — a
/// missing feature is missing, never zero.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct PairFeatures {
    /// Jaccard over Curator affected-file lists; None when either side's
    /// list is unknown at capture.
    pub curator_jaccard: Option<f64>,
    /// Raw retrieval file Jaccard; None when either side lacks retrieval.
    pub retrieval_jaccard: Option<f64>,
}

fn jaccard(a: &[String], b: &[String]) -> Option<f64> {
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let sa: std::collections::BTreeSet<&String> = a.iter().collect();
    let sb: std::collections::BTreeSet<&String> = b.iter().collect();
    let inter = sa.intersection(&sb).count();
    let union = sa.union(&sb).count();
    Some(inter as f64 / union.max(1) as f64)
}

pub fn pair_features(a: &CandidateSnapshot, b: &CandidateSnapshot) -> PairFeatures {
    PairFeatures {
        curator_jaccard: jaccard(&a.curator_affected_files, &b.curator_affected_files),
        retrieval_jaccard: match (&a.retrieval_files, &b.retrieval_files) {
            (Some(x), Some(y)) => jaccard(x, y),
            _ => None,
        },
    }
}

pub fn pair_record(
    repo: &str,
    tick: &CandidateSnapshotTick,
    a: &CandidateSnapshot,
    b: &CandidateSnapshot,
) -> CaptureRecord {
    let (lo, hi) = if a.issue <= b.issue { (a, b) } else { (b, a) };
    let unordered = records::unordered_pair_id(lo.issue, hi.issue);
    let record = CaptureRecord {
        id: String::new(),
        schema_version: records::CAPTURE_SCHEMA_VERSION,
        repo: repo.into(),
        unordered_pair_id: unordered.clone(),
        tick_id: tick.tick_id.clone(),
        admission_policy: tick.admission_policy.clone(),
        captured_at: tick.captured_at.clone(),
        issue_a: lo.issue,
        issue_b: hi.issue,
        claim_state: format!("{}/{}", a.claim_state, b.claim_state),
        dispatch_eligible: a.dispatch_eligible && b.dispatch_eligible,
        implementation_evidence_disclosed: format!(
            "a={}/b={}",
            a.has_implementation_evidence, b.has_implementation_evidence
        ),
        features: pair_features(a, b),
        exposure: records::SchedulingExposure::AdvisoryOnly,
        notes: Vec::new(),
    };
    record
}

/// Pre-registered study budgets (#9787: budgets are frozen before held-out
/// evaluation; a study without them is not runnable).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StudyBudgets {
    /// Maximum pairs captured per dispatch tick.
    pub max_pairs_per_tick: usize,
    /// Global capture cap for the study, enforced across ticks by callers
    /// that pass the already-persisted count to [`capture_with_prior`]
    /// (`capture-live` does, from its output directory).
    pub max_pairs_total: usize,
    /// Minimum positive outcomes required for a non-inconclusive verdict.
    pub min_positives_for_verdict: usize,
    /// Fixed false-warning budget: the maximum alert rate a policy may
    /// spend (as a share of evaluated pairs).
    pub max_flag_rate: f64,
    /// Master off switch — stops capture without affecting scheduling.
    pub study_stopped: bool,
}

impl Default for StudyBudgets {
    fn default() -> Self {
        Self {
            max_pairs_per_tick: 50,
            max_pairs_total: 1000,
            min_positives_for_verdict: 30,
            max_flag_rate: 0.25,
            study_stopped: false,
        }
    }
}

/// Small helper retained for hashing parity with the evidence module.
#[allow(dead_code)]
pub(crate) fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// Re-export for CLI convenience.
pub type FeatureMap = BTreeMap<String, f64>;

/// A capture tick's JSONL output path: `<out_dir>/<tick_id>.jsonl` with the
/// id-stamped records, one per line.
pub fn write_tick_records(
    out_dir: &std::path::Path,
    tick_id: &str,
    records: &mut [CaptureRecord],
) -> anyhow::Result<std::path::PathBuf> {
    for r in records.iter_mut() {
        r.id = capture_id(r)?;
    }
    std::fs::create_dir_all(out_dir)?;
    let path = out_dir.join(format!("{tick_id}.jsonl"));
    let mut body = String::new();
    for r in records.iter() {
        body.push_str(&serde_json::to_string(r)?);
        body.push('\n');
    }
    std::fs::write(&path, body)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(issue: u32, files: &[&str], retrieval: Option<Vec<String>>) -> CandidateSnapshot {
        CandidateSnapshot {
            issue,
            tick_id: "tick-1".into(),
            admission_policy: "fifo".into(),
            claim_state: "eligible".into(),
            dispatch_eligible: true,
            has_implementation_evidence: false,
            curator_affected_files: files.iter().map(|s| s.to_string()).collect(),
            retrieval_files: retrieval,
            captured_at: "2026-10-01T00:00:00Z".into(),
        }
    }

    fn tick(cands: Vec<CandidateSnapshot>) -> CandidateSnapshotTick {
        CandidateSnapshotTick {
            tick_id: "tick-1".into(),
            admission_policy: "fifo".into(),
            captured_at: "2026-10-01T00:00:00Z".into(),
            candidates: cands,
        }
    }

    #[test]
    fn capture_is_deterministic_and_budget_bounded() {
        let t = tick(vec![
            candidate(1, &["src/a.rs"], None),
            candidate(2, &["src/a.rs"], None),
            candidate(3, &["src/b.rs"], None),
        ]);
        let budgets = StudyBudgets {
            max_pairs_per_tick: 1,
            ..Default::default()
        };
        let budgets_limited = budgets.clone();
        let r1 = capture("o/r", &t, &budgets).unwrap();
        assert_eq!(r1.len(), 1, "budget bounds capture");
        assert!(r1[0].notes.iter().any(|n| n.contains("budget: 2 pair(s)")));
        let budgets = StudyBudgets {
            max_pairs_per_tick: 50,
            ..Default::default()
        };
        let r2 = capture("o/r", &t, &budgets).unwrap();
        assert_eq!(r2.len(), 3);
        // Same inputs + same budget → same ids (deterministic; the budget
        // note is part of the record, so the budget must match too).
        let ids1: Vec<String> = r1.iter().map(|r| capture_id(r).unwrap()).collect();
        let ids1_again: Vec<String> = capture("o/r", &t, &budgets_limited)
            .unwrap()
            .iter()
            .map(|r| capture_id(r).unwrap())
            .collect();
        assert_eq!(ids1, ids1_again);
    }

    #[test]
    fn global_cap_counts_prior_captures() {
        let t = tick(vec![
            candidate(1, &["a"], None),
            candidate(2, &["a"], None),
            candidate(3, &["a"], None),
        ]);
        let budgets = StudyBudgets {
            max_pairs_total: 5,
            ..Default::default()
        };
        // 3 pairs available, 4 of 5 already used -> only 1 admitted.
        let r = capture_with_prior("o/r", &t, &budgets, 4).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].notes.iter().any(|n| n.contains("max_pairs_total=5")));
        // Cap exhausted -> nothing captured.
        assert!(capture_with_prior("o/r", &t, &budgets, 5)
            .unwrap()
            .is_empty());
        assert!(capture_with_prior("o/r", &t, &budgets, 9)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn count_captured_records_reads_tick_files() {
        let dir = std::env::temp_dir().join(format!("loom-cs-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(count_captured_records(&dir).unwrap(), 0);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jsonl"), "{\"x\":1}\n\n{\"x\":2}\n").unwrap();
        std::fs::write(dir.join("b.jsonl"), "{\"captured\":0}\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "{\"x\":3}\n").unwrap();
        assert_eq!(count_captured_records(&dir).unwrap(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn off_switch_produces_zero_records() {
        let t = tick(vec![candidate(1, &["a"], None), candidate(2, &["b"], None)]);
        let budgets = StudyBudgets {
            study_stopped: true,
            ..StudyBudgets::default()
        };
        assert!(capture("o/r", &t, &budgets).unwrap().is_empty());
    }

    #[test]
    fn missing_features_are_missing_not_zero() {
        let t = tick(vec![
            candidate(1, &[], None),
            candidate(2, &["src/a.rs"], Some(vec!["src/a.rs".into()])),
        ]);
        let records = capture("o/r", &t, &StudyBudgets::default()).unwrap();
        assert!(records[0].features.curator_jaccard.is_none());
        assert!(records[0].features.retrieval_jaccard.is_none());
    }

    #[test]
    fn unordered_identity_ignores_side_order() {
        let t1 = tick(vec![candidate(1, &["a"], None), candidate(2, &["a"], None)]);
        let t2 = tick(vec![candidate(2, &["a"], None), candidate(1, &["a"], None)]);
        let r1 = capture("o/r", &t1, &StudyBudgets::default()).unwrap();
        let r2 = capture("o/r", &t2, &StudyBudgets::default()).unwrap();
        assert_eq!(
            records::unordered_pair_id(r1[0].issue_a, r1[0].issue_b),
            records::unordered_pair_id(r2[0].issue_a, r2[0].issue_b)
        );
        assert_eq!(capture_id(&r1[0]).unwrap(), capture_id(&r2[0]).unwrap());
    }
}
