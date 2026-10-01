//! Replay manifest schema (#9785 step 1): the cohort definition with each
//! issue's as-of-cutoff snapshot, provenance, and the eventual PR
//! associations, all pinned so the evaluation cannot silently drift.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// One historical issue pair plus everything needed to replay it offline.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplayManifest {
    /// Schema version. Only 1 is understood.
    pub manifest_version: u32,
    /// `OWNER/REPO` the issues and PRs live in (recorded; outcomes read git,
    /// not the forge).
    pub repo: String,
    /// RFC3339 timestamp the manifest was assembled.
    pub created_at: String,
    /// Frozen query-policy version the prediction stage used (#9783's cache
    /// key component). Deterministic rescoring keys off this.
    pub query_policy_version: String,
    /// How/why this pair was selected, recorded per pair.
    pub pairs: Vec<ReplayPair>,
}

/// One side of a pair: the issue snapshot reconstructed as of the cutoff.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReplayPair {
    /// Stable pair identifier (used in filenames and grouping).
    pub pair_id: String,
    /// The historical commit both implementations forked from — the declared
    /// common source revision line coordinates are mapped onto.
    pub historical_commit: String,
    /// RFC3339 cutoff: the observation window opens here. Both issues must be
    /// fully known at this instant.
    pub cutoff: String,
    /// The two issues, in a stable order (A then B).
    pub issues: [IssueSnapshot; 2],
    /// How the two issues relate to each other's implementation work.
    pub association: Association,
    /// Free-text association notes (superseded-by, combined-in, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association_notes: Option<String>,
    /// The eventual implementation PRs, with pinned SHAs.
    pub prs: Vec<PrAssociation>,
    /// How/why this pair entered the cohort (selection-population provenance).
    pub selection: String,
    /// Optional frozen hub weights: path → weight (≥ 0; 0 excludes the file
    /// from the weighted metric). Absent = every file weighs 1. Frozen at
    /// manifest-freeze time so ablations replay the same evidence.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub hub_weights: BTreeMap<String, f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IssueSnapshot {
    /// Issue number in `repo`.
    pub issue: u32,
    /// Title as of the cutoff.
    pub title: String,
    /// Body as of the cutoff.
    pub body: String,
    /// Curator `## Affected Files` paths as of the cutoff. Empty when the
    /// section was absent — see `affected_files_known`.
    #[serde(default)]
    pub curator_affected_files: Vec<String>,
    /// False = no parseable affected-files section at the cutoff (the
    /// Curator baseline is *unknown*, not empty).
    pub affected_files_known: bool,
    /// How this snapshot was reconstructed and whether it is trustworthy.
    pub provenance: SnapshotProvenance,
    /// RFC3339 issue creation time (selection-population evidence).
    pub created_at: String,
}

impl IssueSnapshot {
    /// Leakage validity of THIS snapshot, derived locally from its own
    /// provenance. Deliberately not looked up from any shared map: the same
    /// issue number in two pairs may have different reconstruction outcomes,
    /// and each pair must be classified independently.
    pub fn validity(&self) -> SnapshotValidity {
        if self.provenance.reconstructed {
            SnapshotValidity::Usable
        } else {
            SnapshotValidity::Excluded(
                self.provenance
                    .exclusion_reason
                    .clone()
                    .unwrap_or_else(|| "snapshot not reconstructed".to_string()),
            )
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SnapshotProvenance {
    /// How the as-of-cutoff content was reconstructed (e.g. "timeline API
    /// edited-event replay", "frozen export").
    pub method: String,
    /// True when the issue's title/body were edited after the cutoff — the
    /// snapshot is then only valid if `method` actually reconstructed the
    /// earlier content, which `reconstructed` asserts.
    pub edited_after_cutoff: bool,
    /// True when the as-of-cutoff content was successfully reconstructed.
    /// False ⇒ the pair is excluded from leakage-controlled evaluation.
    pub reconstructed: bool,
    /// Recorded when `reconstructed` is false (or when reconstruction had a
    /// known gap): why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclusion_reason: Option<String>,
}

/// How a PR relates to the pair's implementation work. Independent pairs are
/// the primary cohort; the other three are evaluated separately (issue
/// #9785 step 3).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Association {
    /// Two independently implemented issues (the primary cohort).
    Independent,
    /// The issues were implemented together in one combined effort.
    Combined,
    /// One implementation superseded/abandoned the other.
    Superseded,
    /// The association could not be established with confidence.
    Ambiguous,
}

/// One implementation PR pinned to its SHAs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrAssociation {
    pub pr: u32,
    /// Which of the pair's issues (`issues[].issue`) this PR implements.
    pub issue: u32,
    /// The pinned base — the fork point at implementation time, *not*
    /// necessarily today's branch tip.
    pub base_sha: String,
    /// Pinned head. Prefer the last head before integration repair or
    /// conflict resolution (issue #9785 step 3): final repaired diffs can
    /// conceal the original conflict.
    pub head_sha: String,
    /// Final landed head, kept as a *separate* endpoint when it differs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_head_sha: Option<String>,
    /// The PR's own commit SHAs when known. When present, the PR's own patch
    /// is derived from exactly these commits, which is what guards against
    /// counting unrelated changes inherited through a base update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub own_commits: Option<Vec<String>>,
    /// How the issue↔PR association was established.
    pub provenance: String,
    /// True when an observed production conflict was recorded for this PR
    /// (as opposed to counterfactual replay only).
    #[serde(default)]
    pub observed_conflict: bool,
}

/// Per-snapshot validity classification from [`ReplayManifest::validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotValidity {
    /// Leakage-controlled: the as-of-cutoff content is trustworthy.
    Usable,
    /// Excluded from leakage-controlled evaluation, with the reason.
    Excluded(String),
}

impl ReplayManifest {
    /// Structural validation: schema version, required fields, referential
    /// integrity. Per-snapshot leakage validity is intentionally NOT built
    /// here — it is derived per pair by [`IssueSnapshot::validity`], because
    /// the same issue number may legitimately appear in two pairs with
    /// different reconstruction outcomes (a global issue-keyed map would let
    /// one pair's unreconstructable snapshot leak into — or wrongly exclude —
    /// the other pair's scoring).
    pub fn validate(&self) -> Result<()> {
        if self.manifest_version != 1 {
            bail!("unsupported manifest_version {} (want 1)", self.manifest_version);
        }
        if self.repo.is_empty() {
            bail!("manifest.repo is empty");
        }
        if self.pairs.is_empty() {
            bail!("manifest has no pairs");
        }
        let mut seen_pairs = BTreeSet::new();
        for pair in &self.pairs {
            if !seen_pairs.insert(pair.pair_id.as_str()) {
                bail!("duplicate pair_id {}", pair.pair_id);
            }
            if pair.historical_commit.is_empty() {
                bail!("pair {} has no historical_commit", pair.pair_id);
            }
            if pair.cutoff.is_empty() {
                bail!("pair {} has no cutoff", pair.pair_id);
            }
            if pair.issues[0].issue == pair.issues[1].issue {
                bail!("pair {} lists issue {} on both sides", pair.pair_id, pair.issues[0].issue);
            }
            for w in pair.hub_weights.values() {
                if !w.is_finite() || *w < 0.0 {
                    bail!("pair {} has a non-finite/negative hub weight", pair.pair_id);
                }
            }
            if pair.prs.is_empty() {
                bail!("pair {} has no PR associations", pair.pair_id);
            }
            let issue_nums: BTreeSet<u32> = pair.issues.iter().map(|i| i.issue).collect();
            for pr in &pair.prs {
                if !issue_nums.contains(&pr.issue) {
                    bail!(
                        "pair {} PR {} references issue {} which is not one of the pair's issues",
                        pair.pair_id,
                        pr.pr,
                        pr.issue
                    );
                }
                if pr.base_sha.is_empty() || pr.head_sha.is_empty() {
                    bail!("pair {} PR {} is missing pinned SHAs", pair.pair_id, pr.pr);
                }
            }
        }
        Ok(())
    }

    /// SHA-256 over the snapshot's title+body — the content hash frozen
    /// prediction artifacts must pin (#9783's cache key content component).
    pub fn snapshot_content_hash(snap: &IssueSnapshot) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(snap.title.as_bytes());
        h.update(b"\n");
        h.update(snap.body.as_bytes());
        hex::encode(h.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(issue: u32, reconstructed: bool) -> IssueSnapshot {
        IssueSnapshot {
            issue,
            title: format!("issue {issue}"),
            body: "body".into(),
            curator_affected_files: vec!["src/a.rs".into()],
            affected_files_known: true,
            provenance: SnapshotProvenance {
                method: "test".into(),
                edited_after_cutoff: false,
                reconstructed,
                exclusion_reason: (!reconstructed).then(|| "no earlier content".to_string()),
            },
            created_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn pair(id: &str, a: u32, b: u32) -> ReplayPair {
        ReplayPair {
            pair_id: id.into(),
            historical_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            cutoff: "2026-01-02T00:00:00Z".into(),
            issues: [snap(a, true), snap(b, true)],
            association: Association::Independent,
            association_notes: None,
            prs: vec![PrAssociation {
                pr: 100 + a,
                issue: a,
                base_sha: "aaaa".into(),
                head_sha: "bbbb".into(),
                final_head_sha: None,
                own_commits: None,
                provenance: "closing link".into(),
                observed_conflict: false,
            }],
            selection: "same-week same-label".into(),
            hub_weights: BTreeMap::new(),
        }
    }

    #[test]
    fn valid_manifest_passes() {
        let m = ReplayManifest {
            manifest_version: 1,
            repo: "o/r".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            query_policy_version: "qp-v1".into(),
            pairs: vec![pair("p1", 1, 2)],
        };
        m.validate().unwrap();
        assert_eq!(m.pairs[0].issues[0].validity(), SnapshotValidity::Usable);
        assert_eq!(m.pairs[0].issues[1].validity(), SnapshotValidity::Usable);
    }

    #[test]
    fn unreconstructed_snapshot_excludes() {
        let mut m = ReplayManifest {
            manifest_version: 1,
            repo: "o/r".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            query_policy_version: "qp-v1".into(),
            pairs: vec![pair("p1", 1, 2)],
        };
        m.pairs[0].issues[1].provenance.reconstructed = false;
        m.pairs[0].issues[1].provenance.exclusion_reason = Some("no earlier content".into());
        m.validate().unwrap();
        assert_eq!(
            m.pairs[0].issues[1].validity(),
            SnapshotValidity::Excluded("no earlier content".into())
        );
    }

    #[test]
    fn duplicate_pair_and_cross_issue_prs_fail() {
        let m = ReplayManifest {
            manifest_version: 1,
            repo: "o/r".into(),
            created_at: "x".into(),
            query_policy_version: "qp".into(),
            pairs: vec![pair("p1", 1, 2), pair("p1", 3, 4)],
        };
        assert!(m.validate().is_err());
        let mut m2 = ReplayManifest {
            manifest_version: 1,
            repo: "o/r".into(),
            created_at: "x".into(),
            query_policy_version: "qp".into(),
            pairs: vec![pair("p1", 1, 2)],
        };
        m2.pairs[0].prs[0].issue = 99;
        assert!(m2.validate().is_err());
    }

    #[test]
    fn content_hash_is_stable() {
        let s = snap(7, true);
        let h1 = ReplayManifest::snapshot_content_hash(&s);
        let h2 = ReplayManifest::snapshot_content_hash(&s);
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64);
        let mut t = s.clone();
        t.body.push('!');
        assert_ne!(h1, ReplayManifest::snapshot_content_hash(&t));
    }
}
