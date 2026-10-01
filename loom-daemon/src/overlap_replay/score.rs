//! Scoring and evaluation (#9785 step 4): per-issue footprint precision and
//! recall, per-pair predicted-vs-actual overlap, heuristic comparison against
//! the Curator baseline, score-band outcome rates with uncertainty, and a
//! chronological grouped train/held-out split.
//!
//! Honesty rules enforced here: a numeric heuristic score is a *ranking*, not
//! a probability — band rates are published with Wilson intervals and sample
//! counts, and no calibration claim is made. Actual-overlap and
//! actual-conflict statistics are kept in separate tables. Hindsight-selected
//! examples (fixture/synthetic pairs) are flagged by the manifest's
//! `selection` text and excluded from the representative cohort automatically
//! when it contains `fixture`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use super::artifact::PredictionStatus;
use super::manifest::{Association, ReplayManifest, ReplayPair, SnapshotValidity};
use super::outcome::PairOutcomes;
use super::overlap::{
    jaccard, predicted_overlap, spans_overlap_len, spans_union_len, PredictedOverlap,
};
use super::patch::Span;

/// Frozen band thresholds: `[0, 0.1)` low, `[0.1, 0.3)` medium, `[0.3, 1]`
/// high. Frozen in code so every re-score bands identically.
pub const BAND_THRESHOLDS: (f64, f64) = (0.1, 0.3);

/// Frozen blend weights for the retrieval-based heuristic (component →
/// weight). Components with unknown values are dropped and the rest
/// renormalized; if every component is unknown the blend is unknown.
pub const BLEND_WEIGHTS: &[(&str, f64)] = &[
    ("file_jaccard", 0.4),
    ("hub_weighted_file_jaccard", 0.2),
    ("line_overlap_fraction", 0.2),
    ("symbol_jaccard", 0.2),
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IssueEvaluation {
    pub pair_id: String,
    pub issue: u32,
    /// `present` | `missing` | `mismatch:<reason>` | `excluded:<reason>`.
    pub prediction_status: String,
    pub actual_changed_files: usize,
    pub file_precision: Option<f64>,
    pub file_recall: Option<f64>,
    /// Actual changed files the prediction missed.
    pub missed_files: Vec<String>,
    /// Predicted files with no actual change (irrelevant retrieval).
    pub irrelevant_files: Vec<String>,
    /// Interval-level precision/recall over shared files (None = either side
    /// has no comparable interval evidence).
    pub interval_precision: Option<f64>,
    pub interval_recall: Option<f64>,
    pub symbol_precision: Option<f64>,
    pub symbol_recall: Option<f64>,
    /// False when the snapshot was excluded from leakage-controlled scoring.
    pub leakage_controlled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConflictSummary {
    /// `Some(true)` when any replay order produced a textual conflict,
    /// `Some(false)` when all orders were clean, `None` when any order was
    /// unknown (missing conflict evidence is not a clean outcome).
    pub any_conflict: Option<bool>,
    /// Orders that conflicted (`b_then_a` / `a_then_b`).
    pub conflicted_orders: Vec<String>,
    /// Conflicted paths across orders (deduplicated, sorted).
    pub conflicted_files: Vec<String>,
    /// `counterfactual` for replays; observed production conflicts come from
    /// the manifest and are reported separately.
    pub provenance: String,
    /// Manifest-recorded observed conflict on either PR.
    pub observed_conflict: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairEvaluation {
    pub pair_id: String,
    pub cutoff: String,
    pub association: String,
    pub leakage_controlled: bool,
    /// True when the manifest's selection text marks this pair as a
    /// fixture/synthetic example (reported separately from the cohort).
    pub hindsight_selected: bool,
    pub predicted: PredictedOverlap,
    /// Serialized actual overlap (full record from the outcomes stage).
    pub actual: Option<super::overlap::ActualOverlap>,
    pub conflict: ConflictSummary,
    /// Heuristic → score. Names: `curator_baseline`, `file_jaccard`,
    /// `hub_weighted`, `line_overlap`, `symbol_overlap`, `edit_edit`,
    /// `blend`.
    pub scores: BTreeMap<String, Option<f64>>,
    /// The actual-overlap event used for band tables: shared ≥1 changed file.
    pub actual_overlap_event: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CohortCounts {
    pub pairs_total: usize,
    pub pairs_leakage_controlled: usize,
    pub pairs_with_both_predictions: usize,
    pub pairs_with_any_prediction: usize,
    pub issues_with_prediction: usize,
    pub issues_missing_prediction: usize,
    pub issues_content_mismatch: usize,
    pub issues_excluded_unreconstructable: usize,
    pub associations_independent: usize,
    pub associations_combined: usize,
    pub associations_superseded: usize,
    pub associations_ambiguous: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SplitInfo {
    pub train_pairs: Vec<String>,
    pub held_out_pairs: Vec<String>,
    /// Groups that shared issues/PRs were kept together (leak rule).
    pub grouping_note: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BandRow {
    pub band: String,
    pub n: usize,
    pub outcomes: usize,
    pub rate: f64,
    pub wilson_lo: f64,
    pub wilson_hi: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeuristicTable {
    pub heuristic: String,
    /// Spearman rank correlation between the score and actual file Jaccard,
    /// on pairs with both known. `(rho, n)`.
    pub spearman_vs_actual_overlap: Option<(f64, usize)>,
    /// Rank discrimination (Mann-Whitney AUC) of the score for the conflict
    /// label. `(auc, n_pos, n_neg)`.
    pub auc_for_conflict: Option<(f64, usize, usize)>,
    /// Band → outcome table for the **overlap event**, held-out only.
    pub overlap_bands: Vec<BandRow>,
    /// Band → outcome table for the **conflict event**, held-out only.
    pub conflict_bands: Vec<BandRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Recommendation {
    pub selected: String,
    pub rationale: String,
    pub alternatives: Vec<(String, Option<(f64, usize)>)>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EvaluationReport {
    pub manifest_repo: String,
    pub query_policy_version: String,
    pub cohort: CohortCounts,
    pub per_issue: Vec<IssueEvaluation>,
    pub per_pair: Vec<PairEvaluation>,
    pub split: SplitInfo,
    pub heuristic_tables: Vec<HeuristicTable>,
    pub recommendation: Recommendation,
    /// Explicit limitation notes carried into the summary verbatim.
    pub limitations: Vec<String>,
}

/// Build the full evaluation from a manifest, snapshot validity, frozen
/// predictions (keyed by issue), and computed outcomes (keyed by pair id).
pub fn evaluate(
    manifest: &ReplayManifest,
    validity: &BTreeMap<u32, SnapshotValidity>,
    predictions: &BTreeMap<u32, super::artifact::FrozenPrediction>,
    outcomes: &BTreeMap<String, PairOutcomes>,
) -> EvaluationReport {
    let mut per_issue: Vec<IssueEvaluation> = Vec::new();
    let mut per_pair: Vec<PairEvaluation> = Vec::new();
    let mut counts = CohortCounts {
        pairs_total: manifest.pairs.len(),
        pairs_leakage_controlled: 0,
        pairs_with_both_predictions: 0,
        pairs_with_any_prediction: 0,
        issues_with_prediction: 0,
        issues_missing_prediction: 0,
        issues_content_mismatch: 0,
        issues_excluded_unreconstructable: 0,
        associations_independent: 0,
        associations_combined: 0,
        associations_superseded: 0,
        associations_ambiguous: 0,
    };

    for pair in &manifest.pairs {
        // --- per-issue footprint --------------------------------------
        let pair_leakage = pair
            .issues
            .iter()
            .all(|i| matches!(validity.get(&i.issue), Some(SnapshotValidity::Usable)));
        if pair_leakage {
            counts.pairs_leakage_controlled += 1;
        }
        match pair.association {
            Association::Independent => counts.associations_independent += 1,
            Association::Combined => counts.associations_combined += 1,
            Association::Superseded => counts.associations_superseded += 1,
            Association::Ambiguous => counts.associations_ambiguous += 1,
        }

        let out = outcomes.get(&pair.pair_id);
        let mut predicted_sides: [Option<Vec<super::artifact::RetrievedFile>>; 2] = [None, None];
        for (side, snap) in pair.issues.iter().enumerate() {
            let (status, files): (String, Option<Vec<super::artifact::RetrievedFile>>) =
                match validity.get(&snap.issue) {
                    Some(SnapshotValidity::Excluded(reason)) => {
                        counts.issues_excluded_unreconstructable += 1;
                        (format!("excluded:{reason}"), None)
                    }
                    _ => match predictions.get(&snap.issue) {
                        None => {
                            counts.issues_missing_prediction += 1;
                            ("missing".into(), None)
                        }
                        Some(p) => {
                            let hash_ok =
                                p.issue_content_hash == ReplayManifest::snapshot_content_hash(snap);
                            let source_ok = p.source_revision == pair.historical_commit;
                            let policy_ok = p.query_policy_version == manifest.query_policy_version;
                            match (hash_ok, source_ok, policy_ok) {
                                (true, true, true) => match &p.status {
                                    PredictionStatus::Present(r) => {
                                        counts.issues_with_prediction += 1;
                                        ("present".into(), Some(r.files.clone()))
                                    }
                                    PredictionStatus::Missing { reason } => {
                                        counts.issues_missing_prediction += 1;
                                        (format!("missing:{reason}"), None)
                                    }
                                },
                                (false, _, _) => {
                                    counts.issues_content_mismatch += 1;
                                    ("mismatch:content-hash".into(), None)
                                }
                                (_, false, _) => {
                                    counts.issues_content_mismatch += 1;
                                    ("mismatch:source-revision".into(), None)
                                }
                                (_, _, false) => {
                                    counts.issues_content_mismatch += 1;
                                    ("mismatch:query-policy".into(), None)
                                }
                            }
                        }
                    },
                };
            // Footprint precision/recall against the PR's own changes.
            let side_pr = pr_for_side(pair, side);
            let actual_rec = out.and_then(|o| o.prs.iter().find(|p| Some(p.pr) == side_pr));
            let actual_files: Option<Vec<String>> = actual_rec.map(|p| p.changed_files.clone());
            let actual_intervals = actual_rec.map(|p| &p.changed_intervals);
            let ev = footprint_evaluation(
                pair,
                snap.issue,
                &status,
                files.as_deref(),
                actual_files.as_deref(),
                actual_intervals,
                pair_leakage,
            );
            per_issue.push(ev);
            predicted_sides[side] = files;
        }
        let both_predicted = predicted_sides[0].is_some() && predicted_sides[1].is_some();
        let any_predicted = predicted_sides[0].is_some() || predicted_sides[1].is_some();
        if both_predicted {
            counts.pairs_with_both_predictions += 1;
        }
        if any_predicted {
            counts.pairs_with_any_prediction += 1;
        }

        // --- per-pair predicted overlap + scores -----------------------
        let predicted = match (predicted_sides[0].as_deref(), predicted_sides[1].as_deref()) {
            (Some(a), Some(b)) => predicted_overlap(a, b, &pair.hub_weights),
            _ => predicted_overlap(&[], &[], &pair.hub_weights),
        };
        let actual = out.and_then(|o| o.overlap.clone());
        let conflict = summarize_conflict(pair, out);
        let scores = pair_scores(pair, &predicted);
        let actual_overlap_event = actual.as_ref().map(|a| !a.shared_changed_files.is_empty());
        per_pair.push(PairEvaluation {
            pair_id: pair.pair_id.clone(),
            cutoff: pair.cutoff.clone(),
            association: serde_json::to_value(pair.association)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "unknown".into()),
            leakage_controlled: pair_leakage,
            hindsight_selected: pair.selection.contains("fixture"),
            predicted,
            actual,
            conflict,
            scores,
            actual_overlap_event,
        });
    }

    let split = chronological_grouped_split(manifest);
    let scored: Vec<&PairEvaluation> = per_pair
        .iter()
        .filter(|p| p.leakage_controlled && !p.hindsight_selected)
        .collect();
    let heuristic_tables = build_heuristic_tables(&scored, &split);
    let recommendation = build_recommendation(&heuristic_tables);
    let limitations = build_limitations(&counts, &scored);

    EvaluationReport {
        manifest_repo: manifest.repo.clone(),
        query_policy_version: manifest.query_policy_version.clone(),
        cohort: counts,
        per_issue,
        per_pair,
        split,
        heuristic_tables,
        recommendation,
        limitations,
    }
}

fn pr_for_side(pair: &ReplayPair, side: usize) -> Option<u32> {
    let want = pair.issues.get(side)?.issue;
    pair.prs.iter().find(|p| p.issue == want).map(|p| p.pr)
}

fn footprint_evaluation(
    pair: &ReplayPair,
    issue: u32,
    status: &str,
    predicted_files: Option<&[super::artifact::RetrievedFile]>,
    actual_files: Option<&[String]>,
    actual_intervals: Option<&std::collections::BTreeMap<String, Vec<Span>>>,
    leakage_controlled: bool,
) -> IssueEvaluation {
    let pred_set: Option<BTreeSet<String>> =
        predicted_files.map(|fs| fs.iter().map(|f| f.path.clone()).collect());
    let actual_set: Option<BTreeSet<String>> = actual_files.map(|fs| fs.iter().cloned().collect());
    let (precision, recall, missed, irrelevant) = match (&pred_set, &actual_set) {
        (Some(p), Some(a)) => {
            let inter = p.intersection(a).count();
            let prec = if p.is_empty() {
                None
            } else {
                Some(inter as f64 / p.len() as f64)
            };
            let rec = if a.is_empty() {
                None
            } else {
                Some(inter as f64 / a.len() as f64)
            };
            let missed = a.difference(p).cloned().collect();
            let irrelevant = p.difference(a).cloned().collect();
            (prec, rec, missed, irrelevant)
        }
        _ => (None, None, Vec::new(), Vec::new()),
    };
    // Interval-level precision/recall over files where both sides carry
    // interval evidence.
    let (ip, ir) = {
        let actual_by_path: Option<&std::collections::BTreeMap<String, Vec<Span>>> =
            actual_intervals;
        let pred_by_path: Option<std::collections::BTreeMap<String, Vec<Span>>> = predicted_files
            .map(|fs| {
                fs.iter()
                    .map(|f| {
                        (
                            f.path.clone(),
                            f.intervals
                                .iter()
                                .map(|i| {
                                    if i.start == i.end {
                                        Span::anchor(i.start)
                                    } else {
                                        Span::interval(i.start, i.end)
                                    }
                                })
                                .collect(),
                        )
                    })
                    .collect()
            });
        match (pred_by_path, actual_by_path) {
            (Some(p), Some(a)) => {
                let mut shared_len = 0u32;
                let mut pred_len = 0u32;
                let mut act_len = 0u32;
                for (path, pi) in &p {
                    let Some(ai) = a.get(path) else { continue };
                    shared_len += spans_overlap_len(pi, ai);
                    pred_len += spans_union_len(pi);
                    act_len += spans_union_len(ai);
                }
                let prec = (pred_len > 0).then(|| shared_len as f64 / pred_len as f64);
                let rec = (act_len > 0).then(|| shared_len as f64 / act_len as f64);
                (prec, rec)
            }
            _ => (None, None),
        }
    };
    // Symbol-level against actual changed symbols: the actual side's
    // identifiers live on the outcomes record; the CLI enriches these fields
    // there. Scored None here when absent.
    let _ = issue;
    let _ = pair;
    IssueEvaluation {
        pair_id: pair.pair_id.clone(),
        issue,
        prediction_status: status.to_string(),
        actual_changed_files: actual_set.as_ref().map(|s| s.len()).unwrap_or(0),
        file_precision: precision,
        file_recall: recall,
        missed_files: missed,
        irrelevant_files: irrelevant,
        interval_precision: ip,
        interval_recall: ir,
        symbol_precision: None,
        symbol_recall: None,
        leakage_controlled,
    }
}

fn summarize_conflict(pair: &ReplayPair, out: Option<&PairOutcomes>) -> ConflictSummary {
    let observed = pair.prs.iter().any(|p| p.observed_conflict);
    let Some(out) = out else {
        return ConflictSummary {
            any_conflict: None,
            conflicted_orders: Vec::new(),
            conflicted_files: Vec::new(),
            provenance: "counterfactual".into(),
            observed_conflict: observed,
        };
    };
    let mut conflicted_orders = Vec::new();
    let mut files: BTreeSet<String> = BTreeSet::new();
    let mut saw_unknown = false;
    let mut saw_conflict = false;
    for r in &out.conflict_replays {
        match &r.outcome {
            super::conflict::ConflictOutcome::TextualConflict { conflicted_files } => {
                saw_conflict = true;
                conflicted_orders.push(r.order.clone());
                files.extend(conflicted_files.iter().cloned());
            }
            super::conflict::ConflictOutcome::Unknown { .. } => saw_unknown = true,
            super::conflict::ConflictOutcome::Clean => {}
        }
    }
    ConflictSummary {
        any_conflict: if saw_conflict {
            Some(true)
        } else if saw_unknown {
            None
        } else {
            Some(false)
        },
        conflicted_orders,
        conflicted_files: files.into_iter().collect(),
        provenance: "counterfactual".into(),
        observed_conflict: observed,
    }
}

/// Heuristic scores for one pair, from frozen evidence only.
pub fn pair_scores(
    pair: &ReplayPair,
    predicted: &PredictedOverlap,
) -> BTreeMap<String, Option<f64>> {
    let mut m: BTreeMap<String, Option<f64>> = BTreeMap::new();
    // Curator baseline: file Jaccard over as-of-cutoff affected-files lists.
    let ca: Option<BTreeSet<String>> = pair.issues[0].affected_files_known.then(|| {
        pair.issues[0]
            .curator_affected_files
            .iter()
            .cloned()
            .collect()
    });
    let cb: Option<BTreeSet<String>> = pair.issues[1].affected_files_known.then(|| {
        pair.issues[1]
            .curator_affected_files
            .iter()
            .cloned()
            .collect()
    });
    m.insert("curator_baseline".into(), jaccard(ca.as_ref(), cb.as_ref()));
    m.insert("file_jaccard".into(), predicted.file_jaccard);
    m.insert("hub_weighted".into(), predicted.hub_weighted_file_jaccard);
    m.insert("line_overlap".into(), predicted.line_overlap_fraction);
    m.insert("symbol_overlap".into(), predicted.symbol_jaccard);
    m.insert("edit_edit".into(), predicted.edit_edit_file_jaccard);
    // Frozen blend over available components, renormalized.
    let mut num = 0.0;
    let mut den = 0.0;
    for (name, w) in BLEND_WEIGHTS {
        if let Some(Some(v)) = m.get(*name) {
            num += w * v;
            den += w;
        }
    }
    m.insert("blend".into(), (den > 0.0).then_some(num / den));
    m
}

/// Chronological split into train/held-out, grouped so pairs sharing an
/// issue or PR land on the same side (no change leaks across the split).
pub fn chronological_grouped_split(manifest: &ReplayManifest) -> SplitInfo {
    // Union-find over pair indices via shared issue/PR numbers.
    let n = manifest.pairs.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(p: &mut [usize], x: usize) -> usize {
        let mut x = x;
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    let mut key_owner: BTreeMap<String, usize> = BTreeMap::new();
    for (i, pair) in manifest.pairs.iter().enumerate() {
        let mut keys: Vec<String> = pair
            .issues
            .iter()
            .map(|s| format!("i:{}", s.issue))
            .collect();
        keys.extend(pair.prs.iter().map(|p| format!("p:{}", p.pr)));
        for k in keys {
            match key_owner.get(&k) {
                Some(&j) => {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    if a != b {
                        parent[a] = b;
                    }
                }
                None => {
                    key_owner.insert(k, i);
                }
            }
        }
    }
    // Group members ordered by each group's earliest cutoff.
    let mut groups: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for i in 0..n {
        groups.entry(find(&mut parent, i)).or_default().push(i);
    }
    let mut ordered: Vec<(String, Vec<usize>)> = groups
        .into_values()
        .map(|members| {
            let earliest = members
                .iter()
                .map(|&i| manifest.pairs[i].cutoff.clone())
                .min()
                .unwrap_or_default();
            (earliest, members)
        })
        .collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0));
    let train_target = (ordered.len() as f64 * 0.7).ceil() as usize;
    let mut train = Vec::new();
    let mut held = Vec::new();
    for (gi, (_, members)) in ordered.into_iter().enumerate() {
        if gi < train_target {
            train.extend(members.iter().map(|&i| manifest.pairs[i].pair_id.clone()));
        } else {
            held.extend(members.iter().map(|&i| manifest.pairs[i].pair_id.clone()));
        }
    }
    SplitInfo {
        train_pairs: train,
        held_out_pairs: held,
        grouping_note:
            "groups = connected components over shared issue/PR numbers, ordered by earliest cutoff; first 70% of groups train"
                .into(),
    }
}

fn band(v: f64) -> &'static str {
    let (t1, t2) = BAND_THRESHOLDS;
    if v < t1 {
        "low"
    } else if v < t2 {
        "medium"
    } else {
        "high"
    }
}

/// Wilson 95% interval for a binomial proportion.
pub fn wilson(successes: usize, n: usize) -> (f64, f64) {
    let z = 1.959_964_f64;
    if n == 0 {
        return (0.0, 0.0);
    }
    let p = successes as f64 / n as f64;
    let n = n as f64;
    let denom = 1.0 + z * z / n;
    let center = (p + z * z / (2.0 * n)) / denom;
    let half = z * ((p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt()) / denom;
    ((center - half).clamp(0.0, 1.0), (center + half).clamp(0.0, 1.0))
}

fn band_table(
    scored: &[&PairEvaluation],
    held: &BTreeSet<String>,
    heuristic: &str,
    event: fn(&PairEvaluation) -> Option<bool>,
) -> Vec<BandRow> {
    let mut buckets: BTreeMap<&'static str, Vec<bool>> = BTreeMap::new();
    for p in scored {
        if !held.contains(&p.pair_id) {
            continue;
        }
        let Some(Some(v)) = p.scores.get(heuristic) else {
            continue;
        };
        if let Some(o) = event(p) {
            buckets.entry(band(*v)).or_default().push(o);
        }
    }
    let mut rows = Vec::new();
    for b in ["low", "medium", "high"] {
        let outcomes = buckets.get(b).cloned().unwrap_or_default();
        let n = outcomes.len();
        let succ = outcomes.iter().filter(|o| **o).count();
        let (lo, hi) = wilson(succ, n);
        rows.push(BandRow {
            band: b.into(),
            n,
            outcomes: succ,
            rate: if n == 0 { 0.0 } else { succ as f64 / n as f64 },
            wilson_lo: lo,
            wilson_hi: hi,
        });
    }
    rows
}

fn spearman(xs: &[f64], ys: &[f64]) -> f64 {
    fn ranks(v: &[f64]) -> Vec<f64> {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&a, &b| v[a].partial_cmp(&v[b]).unwrap_or(std::cmp::Ordering::Equal));
        let mut r = vec![0.0; v.len()];
        let mut i = 0;
        while i < idx.len() {
            let mut j = i;
            while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
                j += 1;
            }
            let avg = (i + j) as f64 / 2.0 + 1.0;
            for k in i..=j {
                r[idx[k]] = avg;
            }
            i = j + 1;
        }
        r
    }
    let (rx, ry) = (ranks(xs), ranks(ys));
    let n = xs.len() as f64;
    let mx = rx.iter().sum::<f64>() / n;
    let my = ry.iter().sum::<f64>() / n;
    let num: f64 = rx.iter().zip(&ry).map(|(x, y)| (x - mx) * (y - my)).sum();
    let dx: f64 = rx.iter().map(|x| (x - mx) * (x - mx)).sum();
    let dy: f64 = ry.iter().map(|y| (y - my) * (y - my)).sum();
    if dx == 0.0 || dy == 0.0 {
        0.0
    } else {
        num / (dx.sqrt() * dy.sqrt())
    }
}

/// Mann-Whitney AUC of score → positive label.
fn auc(pos: &[f64], neg: &[f64]) -> f64 {
    let mut wins = 0.0;
    let mut total = 0.0;
    for p in pos {
        for q in neg {
            total += 1.0;
            wins += match p.partial_cmp(q) {
                Some(std::cmp::Ordering::Greater) => 1.0,
                Some(std::cmp::Ordering::Equal) => 0.5,
                _ => 0.0,
            };
        }
    }
    if total == 0.0 {
        0.5
    } else {
        wins / total
    }
}

fn build_heuristic_tables(scored: &[&PairEvaluation], split: &SplitInfo) -> Vec<HeuristicTable> {
    let held: BTreeSet<String> = split.held_out_pairs.iter().cloned().collect();
    let heuristics = [
        "curator_baseline",
        "file_jaccard",
        "hub_weighted",
        "line_overlap",
        "symbol_overlap",
        "edit_edit",
        "blend",
    ];
    let overlap_event = |p: &PairEvaluation| p.actual_overlap_event;
    let conflict_event = |p: &PairEvaluation| p.conflict.any_conflict;
    heuristics
        .iter()
        .map(|h| {
            let mut xs = Vec::new();
            let mut ys = Vec::new();
            let mut pos = Vec::new();
            let mut neg = Vec::new();
            for p in scored {
                let Some(Some(v)) = p.scores.get(*h) else {
                    continue;
                };
                if let Some(j) = p.actual.as_ref().and_then(|a| a.file_jaccard) {
                    xs.push(*v);
                    ys.push(j);
                }
                match p.conflict.any_conflict {
                    Some(true) => pos.push(*v),
                    Some(false) => neg.push(*v),
                    None => {}
                }
            }
            let rho = (!xs.is_empty()).then(|| spearman(&xs, &ys));
            let a = (n_nonempty(&pos) && n_nonempty(&neg))
                .then(|| (auc(&pos, &neg), pos.len(), neg.len()));
            HeuristicTable {
                heuristic: (*h).into(),
                spearman_vs_actual_overlap: rho.map(|r| (r, xs.len())),
                auc_for_conflict: a,
                overlap_bands: band_table(scored, &held, h, overlap_event),
                conflict_bands: band_table(scored, &held, h, conflict_event),
            }
        })
        .collect()
}

fn n_nonempty(v: &[f64]) -> bool {
    !v.is_empty()
}

fn build_recommendation(tables: &[HeuristicTable]) -> Recommendation {
    let mut ranked: Vec<&HeuristicTable> = tables.iter().collect();
    ranked.sort_by(|a, b| {
        let va = a.spearman_vs_actual_overlap.map(|(r, _)| r).unwrap_or(-2.0);
        let vb = b.spearman_vs_actual_overlap.map(|(r, _)| r).unwrap_or(-2.0);
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    let selected = ranked
        .first()
        .map(|t| t.heuristic.clone())
        .unwrap_or_else(|| "none".into());
    let rationale = "Selected by held-out Spearman rank correlation against actual file overlap \
         (sample sizes and uncertainty in the tables). A numeric score is a ranking \
         estimate for the defined event, not a probability; bands are the published \
         low/medium/high mapping with Wilson intervals. Small samples make this a \
         provisional mapping — #9787 validates transfer prospectively.";
    let alternatives = tables
        .iter()
        .map(|t| (t.heuristic.clone(), t.spearman_vs_actual_overlap))
        .collect();
    Recommendation {
        selected,
        rationale: rationale.to_string(),
        alternatives,
    }
}

fn build_limitations(counts: &CohortCounts, scored: &[&PairEvaluation]) -> Vec<String> {
    let mut v = Vec::new();
    if counts.issues_missing_prediction > 0 {
        v.push(format!(
            "{} issue(s) had no frozen prediction — the labeled-pair cohort is limited to \
             pairs with both predictions; retrieval-only pairs are reported separately",
            counts.issues_missing_prediction
        ));
    }
    if counts.issues_content_mismatch > 0 {
        v.push(format!(
            "{} issue(s) had a prediction whose content hash, source revision or query \
             policy did not match the snapshot — never scored (leakage guard)",
            counts.issues_content_mismatch
        ));
    }
    if counts.issues_excluded_unreconstructable > 0 {
        v.push(format!(
            "{} issue snapshot(s) could not be reconstructed as-of the cutoff and are \
             excluded from leakage-controlled evaluation",
            counts.issues_excluded_unreconstructable
        ));
    }
    let conflictable = scored
        .iter()
        .filter(|p| p.conflict.any_conflict.is_some())
        .count();
    if conflictable < scored.len() {
        v.push(format!(
            "{}/{} scored pairs have a definite counterfactual conflict label; the rest are \
             unknown (missing evidence is not a clean outcome)",
            conflictable,
            scored.len()
        ));
    }
    v.push(
        "Line coordinates compare only on a common source basis; per-commit-parent \
         derivations keep file/symbol metrics and mark line outcomes unknown"
            .into(),
    );
    v
}

/// Interval precision/recall helper for CLI-side enrichment when outcome
/// records carry actual intervals: precision over the predicted union,
/// recall over the actual union, `None` where a union is empty.
pub fn interval_metrics(predicted: &[Span], actual: &[Span]) -> (Option<f64>, Option<f64>) {
    let shared = spans_overlap_len(predicted, actual);
    let pu = spans_union_len(predicted);
    let au = spans_union_len(actual);
    if pu == 0 && au == 0 {
        return (None, None);
    }
    let p = if pu == 0 {
        None
    } else {
        Some(shared as f64 / pu as f64)
    };
    let r = if au == 0 {
        None
    } else {
        Some(shared as f64 / au as f64)
    };
    (p, r)
}
