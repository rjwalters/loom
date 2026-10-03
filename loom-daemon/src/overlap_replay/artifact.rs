//! Frozen prediction artifact schema (#9785 step 2) — the shape a #9783
//! cache entry must expose for the replay to score it, plus the intent
//! classes #9784 adds. This module defines the **contract** the cache and
//! footprint stages land against; it does not call any provider.
//!
//! A prediction is only scoreable when its `issue_content_hash` matches the
//! manifest snapshot's content hash and its `source_revision` equals the
//! pair's historical commit — otherwise it is recorded as a mismatch and
//! never scored (the leakage guard that stops "today's index, old issue").

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FrozenPrediction {
    /// Schema version. Only 1 is understood.
    pub artifact_version: u32,
    /// Issue number this prediction was generated for.
    pub issue: u32,
    /// SHA-256 over the snapshot title+body the query used. Must equal
    /// [`crate::overlap_replay::manifest::ReplayManifest::
    /// snapshot_content_hash`] for the same issue.
    pub issue_content_hash: String,
    /// The source revision that was indexed and searched. Must equal the
    /// pair's `historical_commit`.
    pub source_revision: String,
    /// Query-policy version — must match the manifest's for deterministic
    /// rescoring.
    pub query_policy_version: String,
    /// Provider/model/index provenance (recorded verbatim; retrospective
    /// evaluation of the current method is legitimate when labeled).
    pub provenance: PredictionProvenance,
    /// RFC3339 time the prediction was generated (before any PR data was
    /// read — the freeze point).
    pub retrieved_at: String,
    /// The retrieval result, or an explicit missing status.
    pub status: PredictionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PredictionProvenance {
    pub provider: String,
    pub model: String,
    /// Index build identifier/version the retrieval ran against.
    pub index_version: String,
}

/// Present with results, or explicitly missing. "Missing" is data, not an
/// error — the score verb reports missing strata rather than dropping them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum PredictionStatus {
    Present(RetrievalResult),
    Missing { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct RetrievalResult {
    /// Retrieved files in stable order (deduplicated by path).
    pub files: Vec<RetrievedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievedFile {
    pub path: String,
    /// Line intervals (1-based, inclusive) the retrieval surfaced on this
    /// file. Empty = file-level evidence only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub intervals: Vec<LineInterval>,
    /// Qualified symbols the retrieval surfaced for this file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<String>,
    /// Coarse intent classification of this evidence (#9784).
    #[serde(default)]
    pub intent: Intent,
    /// Evidence pointer (cache key, snippet id) — provenance, not content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// 1-based inclusive line interval. `start == end` is a single-line interval;
/// a zero-width *anchor* (a pure insertion point or deletion point) is
/// represented by [`crate::overlap_replay::overlap::LineSpan`] at measurement
/// time, not here.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineInterval {
    pub start: u32,
    pub end: u32,
}

impl LineInterval {
    pub fn new(start: u32, end: u32) -> Self {
        Self { start, end }
    }
    pub fn contains(&self, line: u32) -> bool {
        line >= self.start && line <= self.end
    }
}

/// Coarse intent of retrieved evidence: an intended edit location versus
/// context the retrieval quoted (#9784). `Unknown` preserves files whose
/// provenance does not say.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    Edit,
    Context,
    #[default]
    Unknown,
}

/// Load every prediction JSON under `dir`, keyed by issue, rejecting
/// unknown artifact versions and duplicate issues (a duplicate means the
/// cache key collided — rescored determinism would be ambiguous).
pub fn load_strict(dir: &std::path::Path) -> anyhow::Result<BTreeMap<u32, FrozenPrediction>> {
    let mut out = BTreeMap::new();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("reading predictions dir {}: {e}", dir.display()))?;
    let mut paths: Vec<_> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    paths.sort();
    for p in paths {
        let raw = std::fs::read_to_string(&p)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
        let pred: FrozenPrediction = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", p.display()))?;
        if pred.artifact_version != 1 {
            anyhow::bail!(
                "{}: unsupported artifact_version {}",
                p.display(),
                pred.artifact_version
            );
        }
        let issue = pred.issue;
        if out.insert(issue, pred).is_some() {
            anyhow::bail!("{}: duplicate prediction for issue {}", p.display(), issue);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pred(issue: u32, files: &[&str]) -> FrozenPrediction {
        FrozenPrediction {
            artifact_version: 1,
            issue,
            issue_content_hash: "cafe".into(),
            source_revision: "0123".into(),
            query_policy_version: "qp-v1".into(),
            provenance: PredictionProvenance {
                provider: "augment".into(),
                model: "m".into(),
                index_version: "idx-1".into(),
            },
            retrieved_at: "2026-01-01T00:00:00Z".into(),
            status: PredictionStatus::Present(RetrievalResult {
                files: files
                    .iter()
                    .map(|f| RetrievedFile {
                        path: (*f).into(),
                        intervals: vec![LineInterval::new(1, 5)],
                        symbols: vec![],
                        intent: Intent::Edit,
                        evidence: None,
                    })
                    .collect(),
            }),
        }
    }

    #[test]
    fn round_trips_through_json() {
        let p = pred(9, &["src/a.rs", "src/b.rs"]);
        let s = serde_json::to_string(&p).unwrap();
        let q: FrozenPrediction = serde_json::from_str(&s).unwrap();
        assert_eq!(q, p);
    }

    #[test]
    fn missing_status_round_trips() {
        let p = pred(9, &[]);
        let p = FrozenPrediction {
            status: PredictionStatus::Missing {
                reason: "no cache entry".into(),
            },
            ..p
        };
        let s = serde_json::to_string(&p).unwrap();
        assert!(s.contains("\"kind\":\"missing\""));
    }
}
