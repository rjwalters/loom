//! `loom-daemon collision-evidence publish` (#9786) — wire the #9785
//! overlap-replay pilot/study outputs into the versioned evidence record
//! contract and publish them as checksummed JSONL bundles + OTLP payload
//! bodies. All record logic lives in
//! [`loom_daemon::collision_evidence`]; this file is argument parsing and
//! the report→record conversion.
//!
//! The converter reads only the frozen inputs (manifest + score report) and
//! derives one `PredictionRecord` + one `OutcomeRecord` per directed pair
//! evaluation. Records carry the manifest/report artifact hashes as
//! resolvable references; deterministic ids make re-publication idempotent.

use anyhow::{bail, Context as _, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use loom_daemon::collision_evidence::{
    self, otlp, records, EvidenceBundle, OutcomeKind, OutcomeRecord, PredictionRecord,
    SchedulingExposure,
};

#[derive(clap::Subcommand)]
pub(crate) enum CollisionEvidenceCommand {
    /// Convert an overlap-replay report (manifest + score output) into
    /// versioned evidence records and publish them.
    Publish {
        /// The replay manifest used for the scored run.
        #[arg(long, value_name = "PATH")]
        manifest: PathBuf,

        /// The overlap-replay `score` output directory (per_pair.jsonl etc.).
        #[arg(long, value_name = "DIR")]
        report_dir: PathBuf,

        /// Publication output directory (JSONL bundle + manifest).
        #[arg(long, value_name = "DIR", default_value = "collision-evidence")]
        out_dir: PathBuf,

        /// Scorer/policy version recorded into every prediction record.
        #[arg(long, value_name = "V", default_value = "overlap-replay-v1")]
        scorer_version: String,

        /// Also write the OTLP log-record payload bodies (for the SigNoz
        /// surface) into the out dir.
        #[arg(long, default_value_t = true)]
        otlp_bodies: bool,
    },
}

impl CollisionEvidenceCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Publish {
                manifest,
                report_dir,
                out_dir,
                scorer_version,
                otlp_bodies,
            } => run_publish(&manifest, &report_dir, &out_dir, &scorer_version, otlp_bodies),
        }
    }
}

fn run_publish(
    manifest_path: &Path,
    report_dir: &Path,
    out_dir: &Path,
    scorer_version: &str,
    otlp_bodies: bool,
) -> Result<()> {
    let manifest_raw = std::fs::read_to_string(manifest_path)?;
    let manifest: serde_json::Value = serde_json::from_str(&manifest_raw)?;
    let repo = manifest
        .get("repo")
        .and_then(|v| v.as_str())
        .context("manifest.repo missing")?
        .to_string();
    let policy_version = manifest
        .get("query_policy_version")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let manifest_sha = collision_evidence::sha256_file(manifest_path)?;

    let per_pair_path = report_dir.join("per_pair.jsonl");
    let raw = std::fs::read_to_string(&per_pair_path)
        .with_context(|| format!("reading {}", per_pair_path.display()))?;
    let mut predictions = Vec::new();
    let mut outcomes = Vec::new();
    let mut issues_seen: BTreeMap<u32, String> = BTreeMap::new();
    for (row_no, line) in raw.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let row: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("parsing per_pair.jsonl row {}", row_no + 1))?;
        let pair_id = row
            .get("pair_id")
            .and_then(|v| v.as_str())
            .context("per_pair row missing pair_id")?
            .to_string();
        // Issue numbers come from the pair id convention `<prefix>-<a>-<b>`.
        let nums: Vec<u32> = pair_id
            .split('-')
            .filter_map(|s| s.parse::<u32>().ok())
            .collect();
        let (a, b) = match nums.as_slice() {
            [a, b] => (*a, *b),
            _ => bail!("cannot derive issue numbers from pair_id {pair_id}"),
        };
        let pair = manifest
            .get("pairs")
            .and_then(|v| v.as_array())
            .and_then(|pairs| {
                pairs.iter().find(|p| {
                    p.get("pair_id").and_then(|v| v.as_str()) == Some(pair_id.as_str())
                })
            })
            .with_context(|| format!("pair {pair_id} missing from manifest"))?;
        let source_sha = pair
            .get("historical_commit")
            .and_then(|v| v.as_str())
            .context("pair missing historical_commit")?
            .to_string();
        let issues = pair
            .get("issues")
            .and_then(|v| v.as_array())
            .context("pair missing issues")?;
        for issue in issues {
            let n = issue.get("issue").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let title = issue.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let body = issue.get("body").and_then(|v| v.as_str()).unwrap_or("");
            issues_seen.insert(n, collision_evidence::sha256_str(&format!("{title}\n{body}")));
        }
        let eval_id = records::directed_eval_id(a, b, &source_sha);
        let unordered = records::unordered_pair_id(a, b);

        // Features: keep only finite values, named verbatim from the report.
        let mut features = BTreeMap::new();
        for key in [
            "curator_baseline",
            "file_jaccard",
            "hub_weighted",
            "line_overlap",
            "symbol_overlap",
            "edit_edit",
            "blend",
        ] {
            if let Some(v) = row
                .get("scores")
                .and_then(|s| s.get(key))
                .and_then(|v| v.as_f64())
            {
                if v.is_finite() {
                    features.insert(key.to_string(), v);
                }
            }
        }

        let predicted_at = chrono::Utc::now().to_rfc3339();
        predictions.push(PredictionRecord {
            id: String::new(),
            schema_version: records::PREDICTION_SCHEMA_VERSION,
            repo: repo.clone(),
            forge: "github".into(),
            unordered_pair_id: unordered.clone(),
            directed_eval_id: eval_id.clone(),
            issue_content_hashes: issues_seen
                .iter()
                .filter(|(n, _)| *n == a || *n == b)
                .map(|(n, h)| (*n, h.clone()))
                .collect(),
            retrieval_artifact: None,
            footprint_artifact: Some(collision_evidence::records::ArtifactRef {
                kind: "replay-manifest".into(),
                sha256: manifest_sha.clone(),
                uri: manifest_path.display().to_string(),
                availability: Some("local-report; export bundle governs retention".into()),
            }),
            source_sha: source_sha.clone(),
            index_identity: "overlap-replay-pinned-tree".into(),
            base_sha: source_sha.clone(),
            actual_pr_heads: None,
            predicted_at: predicted_at.clone(),
            scorer_version: scorer_version.to_string(),
            policy_version,
            intended_order: vec![a, b],
            exposure: SchedulingExposure::AdvisoryOnly,
            features,
        });

        // Outcome from the conflict summary + instrument provenance.
        let kind = match row.get("conflict").and_then(|c| c.get("any_conflict")) {
            Some(serde_json::Value::Bool(true)) => {
                let files = row
                    .get("conflict")
                    .and_then(|c| c.get("conflicted_files"))
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                OutcomeKind::TextualConflictPinned {
                    conflicted_files: files,
                    replay_provenance: row
                        .get("conflict")
                        .and_then(|c| c.get("provenance"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("counterfactual")
                        .to_string(),
                }
            }
            Some(serde_json::Value::Bool(false)) => OutcomeKind::Missing {
                window: "replay-replay-clean; clean merge does not prove semantic compatibility"
                    .into(),
            },
            _ => OutcomeKind::Unknown {
                reason: "conflict instrument unavailable (reconstruction or replay failed)"
                    .into(),
            },
        };
        outcomes.push(OutcomeRecord {
            id: String::new(),
            schema_version: records::OUTCOME_SCHEMA_VERSION,
            directed_eval_id: eval_id,
            unordered_pair_id: unordered,
            repo: repo.clone(),
            kind,
            evidence_refs: vec![collision_evidence::records::ArtifactRef {
                kind: "score-report".into(),
                sha256: collision_evidence::sha256_file(&per_pair_path)?,
                uri: per_pair_path.display().to_string(),
                availability: None,
            }],
            observation_window: (predicted_at.clone(), predicted_at),
            attribution: collision_evidence::records::Attribution {
                event_id: format!("replay:{pair_id}"),
                share: 1.0,
                note: Some(
                    "counterfactual replay observation; production repair events carry their own ids"
                        .into(),
                ),
            },
            recorded_at: chrono::Utc::now().to_rfc3339(),
        });
    }
    records::validate_attribution(&outcomes)
        .map_err(|e| anyhow::anyhow!("attribution invariant violated: {e}"))?;

    let bundle = collision_evidence::publish(out_dir, &repo, predictions, outcomes, None)?;
    println!(
        "published {} record(s) to {} (bundle {})",
        bundle.entries.len(),
        out_dir.display(),
        &bundle.bundle_sha256[..16]
    );
    if otlp_bodies {
        let mut lines: BTreeMap<String, String> = BTreeMap::new();
        for f in ["predictions.jsonl", "outcomes.jsonl"] {
            let raw = std::fs::read_to_string(out_dir.join(f))?;
            for l in raw.lines() {
                if l.trim().is_empty() {
                    continue;
                }
                let v: serde_json::Value = serde_json::from_str(l)?;
                if let Some(id) = v.get("id").and_then(|i| i.as_str()) {
                    lines.insert(id.to_string(), l.to_string());
                }
            }
        }
        let (log_records, skipped) = otlp::build_log_records(
            &lines.into_iter().collect::<Vec<_>>(),
            &repo,
            env!("CARGO_PKG_VERSION"),
            "not-run-inside-daemon",
            &chrono::Utc::now().to_rfc3339(),
        );
        let payload = otlp::build_otlp_payload(&log_records);
        std::fs::write(
            out_dir.join("otlp-payload.json"),
            serde_json::to_vec_pretty(&payload)?,
        )?;
        println!(
            "otlp: {} record body/bodies ({} skipped, counted) → otlp-payload.json",
            log_records.len(),
            skipped
        );
    }
    Ok(())
}