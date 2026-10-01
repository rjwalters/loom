//! Per-PR outcome assembly (#9785 step 3): turn pinned SHAs into a PR's own
//! changes ([`OwnPatch`] wrapped with extraction provenance) and a pair's
//! combined outcome record ([`PairOutcomes`]) that the score stage consumes.
//!
//! Everything here is derived from git at pinned SHAs in a caller-supplied
//! checkout. Nothing is fetched from the forge, and no field is inferred
//! from data the protocol forbids (PR descriptions, reviews, resolution
//! comments).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;

use super::conflict::{self, ConflictReplay};
use super::manifest::{Association, ReplayManifest, ReplayPair};
use super::overlap::{actual_overlap, extract_identifiers, ActualOverlap};
use super::patch::{
    own_chain, own_patch, transplant, CoordinateBasis, OwnChain, OwnPatch, Transplant,
    TransplantStatus,
};

/// A PR's derived changes plus the extraction evidence the score stage needs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileChanges {
    pub pr: u32,
    pub base_sha: String,
    pub head_sha: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_base: Option<String>,
    pub coordinate_basis: CoordinateBasis,
    pub base_update_contamination: bool,
    pub changed_files: Vec<String>,
    /// Changed intervals (new coordinates) by file, for line-level
    /// footprint evaluation.
    pub changed_intervals: std::collections::BTreeMap<String, Vec<super::patch::Span>>,
    pub added_lines: u32,
    pub deleted_lines: u32,
    /// Light heuristic identifiers extracted from the PR's own diff lines.
    pub changed_identifiers: Option<BTreeSet<String>>,
    /// Errors/unknowns recorded per PR, never silently dropped (#9785's
    /// "errors/unknowns" publication requirement).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The computed outcomes for one replay pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairOutcomes {
    pub pair_id: String,
    pub historical_commit: String,
    pub association: Association,
    /// Per-PR own changes keyed by PR number.
    pub prs: Vec<FileChanges>,
    /// The pair-level actual overlap (side A = the PR implementing
    /// `issues[0]`, side B = the PR implementing `issues[1]`; combined /
    /// superseded associations choose their sides the same way and are
    /// evaluated separately).
    pub overlap: Option<ActualOverlap>,
    /// Counterfactual conflict replays (both orders) on the declared common
    /// source. **Raw-head instrument**: merges the pinned head commits as-is
    /// and folds in any upstream commits a head contains — reported for
    /// comparability with earlier runs, never as a pair-caused label.
    pub conflict_replays: Vec<ConflictReplay>,
    /// Patch-preserving reconstruction records per PR (#9785 repair §A):
    /// chain qualification, exact preimage/postimage checks, and the
    /// synthetic common-base commits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reconstructions: Vec<super::patch::Transplant>,
    /// Chain-validation records per PR (the precondition for
    /// `conflict_replays_transplanted`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub chains: Vec<super::patch::OwnChain>,
    /// **Pair-caused conflict instrument** (both orders): merges the two
    /// transplanted net patches — each exactly rebuilt on the unique common
    /// base of the pair's patch bases, with preimage/postimage verified —
    /// so inherited upstream history cannot enter either side. Empty unless
    /// both sides reconstructed; a missing entry is `unknown`, not clean.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflict_replays_transplanted: Vec<ConflictReplay>,
    /// Side which PRs were used for A/B (association disambiguation).
    pub side_a_pr: Option<u32>,
    pub side_b_pr: Option<u32>,
}

/// Pick, per side, the PR implementing that side's issue. Ambiguous
/// associations keep every PR in `prs` but still pick a primary per side
/// (first in manifest order) so overlap has a defined measurement; the
/// association label tells the consumer how much to trust it.
fn pick_sides(
    pair: &ReplayPair,
) -> (Option<&super::manifest::PrAssociation>, Option<&super::manifest::PrAssociation>) {
    let a = pair.prs.iter().find(|p| p.issue == pair.issues[0].issue);
    let b = pair.prs.iter().find(|p| p.issue == pair.issues[1].issue);
    (a, b)
}

/// Compute all outcomes for one pair from a git checkout.
pub fn compute_pair_outcomes(repo: &Path, pair: &ReplayPair) -> Result<PairOutcomes> {
    let (a_pr, b_pr) = pick_sides(pair);
    let mut prs: Vec<FileChanges> = Vec::new();
    let mut a_patch: Option<super::patch::OwnPatch> = None;
    let mut b_patch: Option<super::patch::OwnPatch> = None;
    for pr in &pair.prs {
        let own = own_patch(repo, pr.pr, &pr.base_sha, &pr.head_sha, pr.own_commits.as_deref())
            .with_context(|| {
                format!("pair {}: deriving own patch for PR {}", pair.pair_id, pr.pr)
            })?;
        let notes: Vec<String> = own
            .base_update_contamination
            .then(|| {
                "base-update contamination detected: base..head contains upstream commits; \
                 with own_commits present the patch counts only those commits"
                    .to_string()
            })
            .into_iter()
            .collect();
        let ids = (!own.files.is_empty()).then(|| extract_identifiers(&own.raw_diff));
        let fc = FileChanges {
            pr: own.pr,
            base_sha: own.base_sha.clone(),
            head_sha: own.head_sha.clone(),
            merge_base: own.merge_base.clone(),
            coordinate_basis: own.coordinate_basis,
            base_update_contamination: own.base_update_contamination,
            changed_files: own.files.iter().map(|f| f.path.clone()).collect(),
            changed_intervals: own
                .files
                .iter()
                .map(|f| (f.path.clone(), f.intervals_new.clone()))
                .collect(),
            added_lines: own.files.iter().map(|f| f.added_lines).sum(),
            deleted_lines: own.files.iter().map(|f| f.deleted_lines).sum(),
            changed_identifiers: ids,
            notes,
        };
        if a_pr.is_some_and(|p| p.pr == pr.pr) {
            a_patch = Some(own.clone());
        }
        if b_pr.is_some_and(|p| p.pr == pr.pr) {
            b_patch = Some(own);
        }
        prs.push(fc);
    }

    let overlap = match (&a_patch, &b_patch) {
        (Some(a), Some(b)) => {
            let basis = |o: &OwnPatch| FileChanges {
                pr: o.pr,
                base_sha: o.base_sha.clone(),
                head_sha: o.head_sha.clone(),
                merge_base: o.merge_base.clone(),
                coordinate_basis: o.coordinate_basis,
                base_update_contamination: o.base_update_contamination,
                changed_files: o.files.iter().map(|f| f.path.clone()).collect(),
                changed_intervals: o
                    .files
                    .iter()
                    .map(|f| (f.path.clone(), f.intervals_new.clone()))
                    .collect(),
                added_lines: 0,
                deleted_lines: 0,
                changed_identifiers: None,
                notes: Vec::new(),
            };
            Some(actual_overlap(&a.files, &b.files, &basis(a), &basis(b)))
        }
        _ => None,
    };

    // Conflict replay needs both sides present; per the issue, replay on the
    // declared common source. With only one side, skip (unknown, not clean).
    let conflict_replays = match (&a_patch, &b_patch) {
        (Some(a), Some(b)) => {
            conflict::replay_pair(repo, &pair.historical_commit, &a.head_sha, &b.head_sha)
        }
        _ => Vec::new(),
    };

    // Patch-preserving instruments (#9785 repair §A): validate each side's
    // own-commit chain, transplant each net patch exactly onto the unique
    // common base of the two patch bases, and replay both orders on the
    // synthetic commits. Every failure mode is an explicit record — a
    // missing entry is unknown, never clean.
    let mut chains: Vec<OwnChain> = Vec::new();
    let mut reconstructions: Vec<Transplant> = Vec::new();
    let mut conflict_replays_transplanted: Vec<ConflictReplay> = Vec::new();
    if let (Some(a), Some(b)) = (&a_pr, &b_pr) {
        let chain_of = |pr: &super::manifest::PrAssociation| -> OwnChain {
            match &pr.own_commits {
                Some(commits) if !commits.is_empty() => {
                    own_chain(repo, pr.pr, &pr.head_sha, commits).unwrap_or_else(|_| OwnChain {
                        pr: pr.pr,
                        status: super::patch::ChainStatus::Unqualified,
                        patch_base: None,
                        head: pr.head_sha.clone(),
                        commit_dates: Vec::new(),
                        net_changed_paths: Vec::new(),
                        reason: Some("own_chain_validation_git_failure".into()),
                    })
                }
                _ => OwnChain {
                    pr: pr.pr,
                    status: super::patch::ChainStatus::Unqualified,
                    patch_base: None,
                    head: pr.head_sha.clone(),
                    commit_dates: Vec::new(),
                    net_changed_paths: Vec::new(),
                    reason: Some("no_own_commits_recorded".into()),
                },
            }
        };
        let (ca, cb) = (chain_of(a), chain_of(b));
        chains.push(ca.clone());
        chains.push(cb.clone());
        if ca.status == super::patch::ChainStatus::Qualified
            && cb.status == super::patch::ChainStatus::Qualified
        {
            let bases = super::patch::git(
                repo,
                &[
                    "merge-base",
                    "--all",
                    ca.patch_base.as_deref().expect("qualified has patch_base"),
                    cb.patch_base.as_deref().expect("qualified has patch_base"),
                ],
            )
            .ok()
            .map(|s| {
                s.lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
            if bases.len() == 1 {
                let common = &bases[0];
                let ta = transplant(repo, &ca, common, &format!("{} side-a", pair.pair_id))?;
                let tb = transplant(repo, &cb, common, &format!("{} side-b", pair.pair_id))?;
                if ta.status == TransplantStatus::Reconstructed
                    && tb.status == TransplantStatus::Reconstructed
                {
                    conflict_replays_transplanted = conflict::replay_pair(
                        repo,
                        common,
                        ta.commit.as_deref().expect("reconstructed has commit"),
                        tb.commit.as_deref().expect("reconstructed has commit"),
                    );
                }
                reconstructions.push(ta);
                reconstructions.push(tb);
            } else {
                for c in [&ca, &cb] {
                    reconstructions.push(Transplant {
                        pr: c.pr,
                        status: TransplantStatus::PreimageDiffers,
                        commit: None,
                        tree: None,
                        patch_sha256: None,
                        paths: Vec::new(),
                        reason: Some("no_unique_common_base_of_patch_bases".into()),
                    });
                }
            }
        }
    }

    Ok(PairOutcomes {
        pair_id: pair.pair_id.clone(),
        historical_commit: pair.historical_commit.clone(),
        association: pair.association,
        prs,
        overlap,
        conflict_replays,
        reconstructions,
        chains,
        conflict_replays_transplanted,
        side_a_pr: a_pr.map(|p| p.pr),
        side_b_pr: b_pr.map(|p| p.pr),
    })
}

/// Compute outcomes for every pair, writing one JSON file per pair into
/// `out_dir`.
pub fn compute_and_write(
    repo: &Path,
    manifest: &ReplayManifest,
    out_dir: &Path,
) -> Result<Vec<PairOutcomes>> {
    std::fs::create_dir_all(out_dir)
        .with_context(|| format!("creating out dir {}", out_dir.display()))?;
    let mut all = Vec::new();
    for pair in &manifest.pairs {
        let outcomes = compute_pair_outcomes(repo, pair)?;
        let path = out_dir.join(format!("{}.outcomes.json", pair.pair_id));
        std::fs::write(&path, serde_json::to_vec_pretty(&outcomes)?)
            .with_context(|| format!("writing {}", path.display()))?;
        all.push(outcomes);
    }
    Ok(all)
}

/// Load a previously computed outcomes set (offline replay input for
/// `score`).
pub fn load_outcomes_dir(dir: &Path) -> Result<Vec<PairOutcomes>> {
    let mut out = Vec::new();
    let mut paths: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().ends_with(".outcomes.json"))
        })
        .collect();
    paths.sort();
    for p in paths {
        let raw = std::fs::read_to_string(&p)?;
        out.push(serde_json::from_str(&raw).with_context(|| format!("parsing {}", p.display()))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_sides_matches_by_issue() {
        let pair = ReplayPair {
            pair_id: "p".into(),
            historical_commit: "c".into(),
            cutoff: "t".into(),
            issues: [
                crate::overlap_replay::manifest::IssueSnapshot {
                    issue: 1,
                    title: "a".into(),
                    body: String::new(),
                    curator_affected_files: vec![],
                    affected_files_known: false,
                    provenance: crate::overlap_replay::manifest::SnapshotProvenance {
                        method: "m".into(),
                        edited_after_cutoff: false,
                        reconstructed: true,
                        exclusion_reason: None,
                    },
                    created_at: "t".into(),
                },
                crate::overlap_replay::manifest::IssueSnapshot {
                    issue: 2,
                    title: "b".into(),
                    body: String::new(),
                    curator_affected_files: vec![],
                    affected_files_known: false,
                    provenance: crate::overlap_replay::manifest::SnapshotProvenance {
                        method: "m".into(),
                        edited_after_cutoff: false,
                        reconstructed: true,
                        exclusion_reason: None,
                    },
                    created_at: "t".into(),
                },
            ],
            association: Association::Independent,
            association_notes: None,
            prs: vec![
                super::super::manifest::PrAssociation {
                    pr: 11,
                    issue: 2,
                    base_sha: "b".into(),
                    head_sha: "h".into(),
                    final_head_sha: None,
                    own_commits: None,
                    provenance: "p".into(),
                    observed_conflict: false,
                },
                super::super::manifest::PrAssociation {
                    pr: 10,
                    issue: 1,
                    base_sha: "b".into(),
                    head_sha: "h".into(),
                    final_head_sha: None,
                    own_commits: None,
                    provenance: "p".into(),
                    observed_conflict: false,
                },
            ],
            selection: "s".into(),
            hub_weights: Default::default(),
        };
        let (a, b) = pick_sides(&pair);
        assert_eq!(a.map(|p| p.pr), Some(10));
        assert_eq!(b.map(|p| p.pr), Some(11));
    }
}
