//! `loom-daemon overlap-replay` (#9785) — historical replay of scheduling
//! predictions against the PRs that eventually implemented them.
//!
//! # What this is
//!
//! An ad-hoc experiment harness, not live scheduling machinery: it takes a
//! **replay manifest** of historical issue pairs, their as-of-cutoff snapshots
//! and eventual implementation PRs (pinned SHAs), plus optional **frozen
//! prediction artifacts** in the #9783/#9784 cache contract shape, and
//! produces the paired prediction/outcome dataset the issue's evaluation
//! stage consumes. Nothing here dispatches, serializes, or reorders anything;
//! the experiment changes no live scheduling behavior.
//!
//! # The three stages (issue #9785 § "Historical replay protocol")
//!
//! 1. **Historical inputs** — the manifest pins each issue's snapshot
//!    (title/body/Curator affected files *as of the cutoff*) with provenance.
//!    Snapshots whose earlier content could not be reconstructed are excluded
//!    from leakage-controlled evaluation and counted in the report.
//! 2. **Frozen predictions** — cached retrieval results keyed to the
//!    snapshot's content hash and source revision. A prediction whose content
//!    hash does not match its snapshot, or whose source revision differs from
//!    the pair's historical commit, is *not* scored — it is recorded as a
//!    mismatch. Searching today's code for an old issue is exactly the leak
//!    this guard exists to catch.
//! 3. **Outcomes** — each PR's own changes derived from git at pinned
//!    SHAs (base-update contamination flagged, not silently absorbed), the
//!    pair's actual overlap measured at file/line level, and a counterfactual
//!    textual-conflict replay via `git merge-tree --write-tree` in both
//!    orders on the declared common source.
//!
//! Scoring compares heuristics (Curator baseline, retrieval features, a
//! frozen blend) against actual overlap and conflict outcomes, with a
//! chronological grouped train/held-out split. Overlap and conflict are
//! reported as **separate** labels; a heuristic score is published as a
//! ranking/band estimate with uncertainty — never as a probability.
//!
//! # CLI contract
//!
//! | verb | inputs | outputs | exit |
//! |---|---|---|---|
//! | `validate` | manifest (+ `--predictions-dir`) | findings on stdout (JSON with `--json`) | 0 valid (warnings allowed), 1 invalid |
//! | `outcomes` | manifest + `--repo` (git checkout holding pinned SHAs) | one outcomes JSON per pair under `--out-dir` | 0 ok, 2 execution error |
//! | `score` | manifest, optional `--predictions-dir`, outcomes from `--repo` and/or `--outcomes-dir` | `per_issue.jsonl`/`.csv`, `per_pair.jsonl`/`.csv`, `summary.md` under `--out-dir` | 0 ok, 2 execution error |
//!
//! All verbs are offline. `score` never needs provider credentials: missing
//! predictions are a recorded stratum, and deterministic rescoring of frozen
//! artifacts is stable by construction.

pub mod artifact;
pub mod conflict;
pub mod manifest;
pub mod outcome;
pub mod overlap;
pub mod patch;
pub mod report;
pub mod score;

pub use artifact::FrozenPrediction;
pub use manifest::{ReplayManifest, SnapshotValidity};

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Load and validate a manifest, returning it plus the per-snapshot validity
/// classification (leakage-controlled vs excluded) the rest of the pipeline
/// threads through.
pub fn load_manifest(path: &Path) -> Result<(ReplayManifest, BTreeMap<u32, SnapshotValidity>)> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading manifest {}", path.display()))?;
    let manifest: ReplayManifest = serde_json::from_str(&raw)
        .with_context(|| format!("parsing manifest {}", path.display()))?;
    let validity = manifest.validate()?;
    Ok((manifest, validity))
}
