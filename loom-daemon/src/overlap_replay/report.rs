//! Report writers (#9785 "Publication"): per-issue and per-pair JSONL/CSV
//! plus a summary markdown with the predicted-vs-actual tables, band outcome
//! rates, uncertainty, and the explicit limitations block.
//!
//! No plotting dependency is pulled in for this: the tables are the durable
//! artifact, and any chart can be rendered from the published CSV/JSONL.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

use super::score::EvaluationReport;

/// Write all report artifacts into `out_dir` and return the file paths.
pub fn write_reports(report: &EvaluationReport, out_dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating report dir {}", out_dir.display()))?;
    let mut written = Vec::new();

    let per_issue_jsonl = out_dir.join("per_issue.jsonl");
    write_jsonl(&per_issue_jsonl, &report.per_issue)?;
    written.push(per_issue_jsonl);

    let per_pair_jsonl = out_dir.join("per_pair.jsonl");
    write_jsonl(&per_pair_jsonl, &report.per_pair)?;
    written.push(per_pair_jsonl);

    let per_issue_csv_path = out_dir.join("per_issue.csv");
    std::fs::write(&per_issue_csv_path, per_issue_csv(report))?;
    written.push(per_issue_csv_path);

    let per_pair_csv_path = out_dir.join("per_pair.csv");
    std::fs::write(&per_pair_csv_path, per_pair_csv(report))?;
    written.push(per_pair_csv_path);

    let summary = out_dir.join("summary.md");
    std::fs::write(&summary, summary_markdown(report))?;
    written.push(summary);

    Ok(written)
}

fn write_jsonl<T: Serialize>(path: &Path, rows: &[T]) -> Result<()> {
    let mut buf = String::new();
    for r in rows {
        buf.push_str(&serde_json::to_string(r)?);
        buf.push('\n');
    }
    std::fs::write(path, buf).with_context(|| format!("writing {}", path.display()))
}

fn csv_field(v: &str) -> String {
    if v.contains(',') || v.contains('"') || v.contains('\n') {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

fn opt_f64(v: &Option<f64>) -> String {
    match v {
        Some(x) => format!("{x:.4}"),
        None => String::new(),
    }
}

fn per_issue_csv(report: &EvaluationReport) -> String {
    let mut out = String::from(
        "pair_id,issue,prediction_status,leakage_controlled,actual_changed_files,\
file_precision,file_recall,interval_precision,interval_recall,missed_files,irrelevant_files\n",
    );
    for e in &report.per_issue {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{}\n",
            e.pair_id,
            e.issue,
            csv_field(&e.prediction_status),
            e.leakage_controlled,
            e.actual_changed_files,
            opt_f64(&e.file_precision),
            opt_f64(&e.file_recall),
            opt_f64(&e.interval_precision),
            opt_f64(&e.interval_recall),
            csv_field(&e.missed_files.join(";")),
            csv_field(&e.irrelevant_files.join(";")),
        ));
    }
    out
}

fn per_pair_csv(report: &EvaluationReport) -> String {
    let mut out = String::from(
        "pair_id,cutoff,association,leakage_controlled,hindsight_selected,\
curator_baseline,file_jaccard,hub_weighted,line_overlap,symbol_overlap,edit_edit,blend,\
actual_file_jaccard,actual_shared_files,actual_line_overlap_fraction,actual_coordinate_basis,\
predicted_line_overlap_fraction,conflict_any,conflicted_files,observed_conflict\n",
    );
    for p in &report.per_pair {
        let s = |k: &str| opt_f64(&p.scores.get(k).and_then(|v| *v));
        let actual = p.actual.as_ref();
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            csv_field(&p.pair_id),
            csv_field(&p.cutoff),
            csv_field(&p.association),
            p.leakage_controlled,
            p.hindsight_selected,
            s("curator_baseline"),
            s("file_jaccard"),
            s("hub_weighted"),
            s("line_overlap"),
            s("symbol_overlap"),
            s("edit_edit"),
            s("blend"),
            actual
                .and_then(|a| a.file_jaccard)
                .map(|v| format!("{v:.4}"))
                .unwrap_or_default(),
            actual.map(|a| a.shared_changed_files.len()).unwrap_or(0),
            actual
                .and_then(|a| a.line_overlap_fraction)
                .map(|v| format!("{v:.4}"))
                .unwrap_or_default(),
            actual
                .and_then(|a| a.coordinate_basis.clone())
                .unwrap_or_default(),
            opt_f64(&p.predicted.as_ref().and_then(|o| o.line_overlap_fraction)),
            match p.conflict.any_conflict {
                Some(true) => "conflict",
                Some(false) => "clean",
                None => "unknown",
            },
            csv_field(&p.conflict.conflicted_files.join(";")),
            p.conflict.observed_conflict,
        ));
    }
    out
}

fn summary_markdown(r: &EvaluationReport) -> String {
    let c = &r.cohort;
    let mut m = String::new();
    m.push_str(&format!(
        "# Overlap replay evaluation — {} (query policy `{}`)\n\n",
        r.manifest_repo, r.query_policy_version
    ));
    m.push_str("## Cohort\n\n");
    m.push_str(&format!(
        "- pairs: {} total, {} leakage-controlled\n",
        c.pairs_total, c.pairs_leakage_controlled
    ));
    m.push_str(&format!(
        "- predictions: {} issues present, {} missing, {} content/policy mismatch, \
{} snapshots excluded (unreconstructable)\n",
        c.issues_with_prediction,
        c.issues_missing_prediction,
        c.issues_content_mismatch,
        c.issues_excluded_unreconstructable
    ));
    m.push_str(&format!(
        "- pairs with both predictions: {} (any prediction: {})\n",
        c.pairs_with_both_predictions, c.pairs_with_any_prediction
    ));
    m.push_str(&format!(
        "- associations: {} independent, {} combined, {} superseded, {} ambiguous \
(non-independent pairs are evaluated separately from the primary cohort)\n",
        c.associations_independent,
        c.associations_combined,
        c.associations_superseded,
        c.associations_ambiguous
    ));

    m.push_str("\n## Per-issue footprint (prediction vs actual PR changes)\n\n");
    let present: Vec<_> = r
        .per_issue
        .iter()
        .filter(|e| e.file_recall.is_some())
        .collect();
    if present.is_empty() {
        m.push_str("_No issue has both a scored prediction and actual PR changes._\n");
    } else {
        let mean = |f: &dyn Fn(&super::score::IssueEvaluation) -> Option<f64>| -> String {
            let vs: Vec<f64> = present.iter().filter_map(|e| f(e)).collect();
            if vs.is_empty() {
                "n/a".into()
            } else {
                format!("{:.3}", vs.iter().sum::<f64>() / vs.len() as f64)
            }
        };
        m.push_str(&format!(
            "- {} issues scored: mean file precision {}, mean file recall {}\n",
            present.len(),
            mean(&|e| e.file_precision),
            mean(&|e| e.file_recall),
        ));
        m.push_str(&format!(
            "- mean interval precision {}, mean interval recall {} (where comparable)\n",
            mean(&|e| e.interval_precision),
            mean(&|e| e.interval_recall),
        ));
    }

    m.push_str("\n## Actual overlap distribution (outcome side, all scored pairs)\n\n");
    let overlaps: Vec<f64> = r
        .per_pair
        .iter()
        .filter(|p| p.leakage_controlled && !p.hindsight_selected)
        .filter_map(|p| p.actual.as_ref().and_then(|a| a.file_jaccard))
        .collect();
    if overlaps.is_empty() {
        m.push_str("_No actual overlap measurements available._\n");
    } else {
        let mut sorted = overlaps.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let q = |f: f64| -> f64 {
            let i = ((sorted.len() - 1) as f64 * f).round() as usize;
            sorted[i]
        };
        let shared_any = overlaps.iter().filter(|v| **v > 0.0).count();
        m.push_str(&format!(
            "- n={}, min {:.3}, p25 {:.3}, median {:.3}, p75 {:.3}, max {:.3}; \
pairs sharing ≥1 changed file: {}/{}\n",
            sorted.len(),
            sorted[0],
            q(0.25),
            q(0.5),
            q(0.75),
            sorted[sorted.len() - 1],
            shared_any,
            sorted.len()
        ));
    }

    m.push_str("\n## Conflict labels (counterfactual replay; observed recorded separately)\n\n");
    let conflicts: Vec<_> = r
        .per_pair
        .iter()
        .filter(|p| p.leakage_controlled && !p.hindsight_selected)
        .collect();
    let conf_n = conflicts
        .iter()
        .filter(|p| p.conflict.any_conflict == Some(true))
        .count();
    let clean_n = conflicts
        .iter()
        .filter(|p| p.conflict.any_conflict == Some(false))
        .count();
    let unknown_n = conflicts
        .iter()
        .filter(|p| p.conflict.any_conflict.is_none())
        .count();
    let observed_n = conflicts
        .iter()
        .filter(|p| p.conflict.observed_conflict)
        .count();
    m.push_str(&format!(
        "- textual conflict: {conf_n}, clean: {clean_n}, unknown: {unknown_n}; \
observed production conflicts recorded in the manifest: {observed_n}\n"
    ));

    m.push_str("\n## Heuristics vs outcomes\n\n");
    m.push_str(
        "| heuristic | Spearman vs actual file overlap (n) | AUC for conflict (n_pos/n_neg) |\n",
    );
    m.push_str("|---|---|---|\n");
    for t in &r.heuristic_tables {
        let sp = t
            .spearman_vs_actual_overlap
            .map(|(v, n)| format!("{v:.3} ({n})"))
            .unwrap_or_else(|| "n/a".into());
        let au = t
            .auc_for_conflict
            .map(|(v, p, n)| format!("{v:.3} ({p}/{n})"))
            .unwrap_or_else(|| "n/a".into());
        m.push_str(&format!("| {} | {sp} | {au} |\n", t.heuristic));
    }

    m.push_str("\n### Score-band outcome rates (held-out pairs; overlap event = shares ≥1 changed file)\n\n");
    for t in &r.heuristic_tables {
        let parts: Vec<String> = t
            .overlap_bands
            .iter()
            .map(|b| {
                format!(
                    "{}: n={} rate={:.2} [{:.2},{:.2}]",
                    b.band, b.n, b.rate, b.wilson_lo, b.wilson_hi
                )
            })
            .collect();
        m.push_str(&format!("- {}: {}\n", t.heuristic, parts.join("; ")));
    }
    m.push_str("\n### Score-band conflict rates (held-out; separate event from overlap)\n\n");
    for t in &r.heuristic_tables {
        let parts: Vec<String> = t
            .conflict_bands
            .iter()
            .map(|b| {
                format!(
                    "{}: n={} rate={:.2} [{:.2},{:.2}]",
                    b.band, b.n, b.rate, b.wilson_lo, b.wilson_hi
                )
            })
            .collect();
        m.push_str(&format!("- {}: {}\n", t.heuristic, parts.join("; ")));
    }

    m.push_str("\n## Split\n\n");
    m.push_str(&format!(
        "- train: {} pairs; held-out: {} pairs. {}\n",
        r.split.train_pairs.len(),
        r.split.held_out_pairs.len(),
        r.split.grouping_note
    ));

    m.push_str("\n## Recommendation\n\n");
    m.push_str(&format!(
        "- selected mapping: **{}**\n- {}\n- alternatives: {}\n",
        r.recommendation.selected,
        r.recommendation.rationale,
        r.recommendation
            .alternatives
            .iter()
            .map(|(n, s)| match s {
                Some((v, n_)) => format!("{n} (rho={v:.3}, n={n_})"),
                None => format!("{n} (n/a)"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    ));

    m.push_str("\n## Limitations\n\n");
    for l in &r.limitations {
        m.push_str(&format!("- {l}\n"));
    }
    m
}
