//! `loom-daemon overlap-replay validate | outcomes | score` (#9785) — the
//! historical Augment replay harness. All logic lives in
//! [`loom_daemon::overlap_replay`]; this file is argument parsing, output
//! rendering, and the orchestration glue between the manifest, the frozen
//! prediction artifacts, the outcomes stage and the report writers.
//!
//! Exit contract: 0 on success (including "valid with warnings"), 1 on any
//! failure (invalid manifest, unresolvable pins, missing outcomes coverage).
//! `score` never needs provider credentials — absent predictions are a
//! recorded stratum, and rescoring frozen artifacts is deterministic.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use loom_daemon::overlap_replay::artifact::{self, FrozenPrediction, PredictionStatus};
use loom_daemon::overlap_replay::manifest::SnapshotValidity;
use loom_daemon::overlap_replay::{self, outcome, report, score};

#[derive(clap::Subcommand)]
pub(crate) enum OverlapReplayCommand {
    /// Validate a replay manifest (and optionally its frozen predictions)
    /// without computing anything: schema, provenance and leakage guards.
    Validate {
        /// Path to the replay manifest JSON.
        #[arg(long, value_name = "PATH")]
        manifest: PathBuf,

        /// Directory of frozen prediction artifacts (#9783 cache shape) to
        /// cross-check against the snapshots.
        #[arg(long, value_name = "DIR")]
        predictions_dir: Option<PathBuf>,

        /// Emit findings as JSON instead of text.
        #[arg(long)]
        json: bool,
    },

    /// Derive each PR's own changes and the pair conflict replays from a git
    /// checkout at the manifest's pinned SHAs, writing one outcomes JSON per
    /// pair. Fully offline.
    Outcomes {
        /// Path to the replay manifest JSON.
        #[arg(long, value_name = "PATH")]
        manifest: PathBuf,

        /// Git checkout containing every pinned SHA (base/head/historical
        /// commit). Pins that do not resolve are an error, never a skip.
        #[arg(long, value_name = "DIR")]
        repo: PathBuf,

        /// Directory to write `<pair_id>.outcomes.json` files into.
        #[arg(long, value_name = "DIR", default_value = "overlap-replay-outcomes")]
        out_dir: PathBuf,
    },

    /// Run the full evaluation: frozen predictions + PR outcomes → per-issue
    /// and per-pair JSONL/CSV plus `summary.md`. Deterministic on frozen
    /// inputs (re-scoring replays to identical numbers).
    Score {
        /// Path to the replay manifest JSON.
        #[arg(long, value_name = "PATH")]
        manifest: PathBuf,

        /// Git checkout for outcome derivation. Omit when `--outcomes-dir`
        /// already covers every pair (the offline replay path).
        #[arg(long, value_name = "DIR")]
        repo: Option<PathBuf>,

        /// Precomputed outcomes directory (from the `outcomes` verb).
        #[arg(long, value_name = "DIR")]
        outcomes_dir: Option<PathBuf>,

        /// Directory of frozen prediction artifacts; omit for the
        /// baseline-only stratum (every prediction recorded missing).
        #[arg(long, value_name = "DIR")]
        predictions_dir: Option<PathBuf>,

        /// Directory for the published reports (and freshly computed
        /// outcomes).
        #[arg(long, value_name = "DIR", default_value = "overlap-replay-report")]
        out_dir: PathBuf,
    },
}

impl OverlapReplayCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Validate {
                manifest,
                predictions_dir,
                json,
            } => run_validate(&manifest, predictions_dir.as_deref(), json),
            Self::Outcomes {
                manifest,
                repo,
                out_dir,
            } => {
                let m = overlap_replay::load_manifest(&manifest)?;
                let all = outcome::compute_and_write(&repo, &m, &out_dir)?;
                println!(
                    "outcomes: wrote {} pair outcome file(s) to {}",
                    all.len(),
                    out_dir.display()
                );
                Ok(())
            }
            Self::Score {
                manifest,
                repo,
                outcomes_dir,
                predictions_dir,
                out_dir,
            } => run_score(
                &manifest,
                repo.as_deref(),
                outcomes_dir.as_deref(),
                predictions_dir.as_deref(),
                &out_dir,
            ),
        }
    }
}

fn run_validate(manifest: &Path, predictions_dir: Option<&Path>, json: bool) -> Result<()> {
    let m = overlap_replay::load_manifest(manifest)?;
    let mut findings: Vec<String> = Vec::new();
    // Per-occurrence: an issue shared by two pairs counts once per pair.
    let excluded = m
        .pairs
        .iter()
        .flat_map(|p| p.issues.iter())
        .filter(|s| !matches!(s.validity(), SnapshotValidity::Usable))
        .count();
    findings.push(format!(
        "manifest: {} pair(s), {excluded} snapshot occurrence(s) excluded from leakage-controlled evaluation",
        m.pairs.len()
    ));
    let preds: BTreeMap<u32, FrozenPrediction> = match predictions_dir {
        Some(dir) => {
            let p = artifact::load_strict(dir)?;
            findings.push(format!("predictions: {} artifact(s) loaded", p.len()));
            // Cross-check against every snapshot: unknown issues are a
            // warning (stray cache entry), not an error.
            let known: std::collections::BTreeSet<u32> = m
                .pairs
                .iter()
                .flat_map(|p| p.issues.iter())
                .map(|s| s.issue)
                .collect();
            for issue in p.keys() {
                if !known.contains(issue) {
                    findings.push(format!("warning: prediction for issue {issue} matches no snapshot in this manifest"));
                }
            }
            p
        }
        None => {
            findings.push("predictions: none supplied (validation of the manifest only)".into());
            BTreeMap::new()
        }
    };
    // Per-snapshot hash/source/policy cross-check, same rules as scoring.
    for pair in &m.pairs {
        for snap in &pair.issues {
            let Some(p) = preds.get(&snap.issue) else {
                continue;
            };
            let hash_ok = p.issue_content_hash
                == overlap_replay::manifest::ReplayManifest::snapshot_content_hash(snap);
            let source_ok = p.source_revision == pair.historical_commit;
            let policy_ok = p.query_policy_version == m.query_policy_version;
            if !hash_ok {
                findings.push(format!(
                    "warning: issue {} prediction content-hash mismatch (not scoreable)",
                    snap.issue
                ));
            }
            if !source_ok {
                findings.push(format!(
                    "warning: issue {} prediction source revision {} != pair {} historical commit {}",
                    snap.issue, p.source_revision, pair.pair_id, pair.historical_commit
                ));
            }
            if !policy_ok {
                findings.push(format!(
                    "warning: issue {} prediction query policy {} != manifest {}",
                    snap.issue, p.query_policy_version, m.query_policy_version
                ));
            }
            if matches!(p.status, PredictionStatus::Missing { .. }) {
                findings.push(format!(
                    "warning: issue {} prediction is explicitly missing ({} reason recorded)",
                    snap.issue,
                    match &p.status {
                        PredictionStatus::Missing { reason } => reason.as_str(),
                        _ => "",
                    }
                ));
            }
        }
    }
    if json {
        let out = serde_json::json!({
            "valid": true,
            "pairs": m.pairs.len(),
            "excluded_snapshots": excluded,
            "findings": findings,
        });
        println!("{out}");
    } else {
        for f in &findings {
            println!("{f}");
        }
    }
    Ok(())
}

fn run_score(
    manifest: &Path,
    repo: Option<&Path>,
    outcomes_dir: Option<&Path>,
    predictions_dir: Option<&Path>,
    out_dir: &Path,
) -> Result<()> {
    let m = overlap_replay::load_manifest(manifest)?;
    let predictions: BTreeMap<u32, FrozenPrediction> = match predictions_dir {
        Some(dir) => artifact::load_strict(dir)?,
        None => BTreeMap::new(),
    };

    // Outcomes: reuse a precomputed set when it covers every pair; otherwise
    // compute from the git checkout (and persist for future offline rescore).
    let outcomes: Vec<outcome::PairOutcomes> = match outcomes_dir {
        Some(dir) => {
            let loaded = outcome::load_outcomes_dir(dir)?;
            let have: std::collections::BTreeSet<&str> =
                loaded.iter().map(|o| o.pair_id.as_str()).collect();
            let missing: Vec<&str> = m
                .pairs
                .iter()
                .map(|p| p.pair_id.as_str())
                .filter(|id| !have.contains(id))
                .collect();
            if !missing.is_empty() {
                bail!(
                    "outcomes dir {} is missing pair(s): {} — recompute with the outcomes verb",
                    dir.display(),
                    missing.join(", ")
                );
            }
            loaded
        }
        None => {
            let Some(repo) = repo else {
                bail!("score needs --repo (to derive outcomes) or --outcomes-dir (precomputed)");
            };
            let o = outcome::compute_and_write(repo, &m, &out_dir.join("outcomes"))?;
            println!(
                "outcomes: derived and wrote {} pair outcome file(s) to {}/outcomes",
                o.len(),
                out_dir.display()
            );
            o
        }
    };
    let by_pair: BTreeMap<String, outcome::PairOutcomes> = outcomes
        .into_iter()
        .map(|o| (o.pair_id.clone(), o))
        .collect();

    let eval = score::evaluate(&m, &predictions, &by_pair);
    let written = report::write_reports(&eval, out_dir)?;
    println!(
        "score: {} pair(s) evaluated ({} leakage-controlled) — reports:",
        eval.cohort.pairs_total, eval.cohort.pairs_leakage_controlled
    );
    for p in written {
        println!("  {}", p.display());
    }
    Ok(())
}
