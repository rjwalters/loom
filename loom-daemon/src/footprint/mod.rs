//! Issue footprints (#9784) — classified, revision-pinned evidence about
//! *where an issue intends to touch the code*, built from the #9783 cache.
//!
//! # The distinction this module exists for
//!
//! Retrieval relevance is not intended edits: two tasks may read the same
//! helper (context-only overlap), while changes to different files can still
//! conflict through a contract. So every location carries two INDEPENDENT
//! dimensions that must never collapse into one enum:
//!
//! * **intended role** — `edit`, `context_read`, `contract_producer`,
//!   `contract_consumer`, `unknown` (a test may be an intended edit, so
//!   artifact kind is separate);
//! * **artifact kind** — `source`, `test`, `fixture`, `generated`,
//!   `configuration`, `unknown`.
//!
//! plus the proposed operation (`create`/`edit`/`delete`/`rename`/`unknown`)
//! — proposed new paths carry no invented line ranges.
//!
//! Classification is a versioned, deterministic rules pass
//! ([`classify::RulesV1`]) over the cached retrieval results + the Curator
//! affected-file baseline. The classifier's name/version is recorded in the
//! artifact; bumping it recomputes footprints while the underlying retrieval
//! cache entry (keyed on content, #9783) stays valid.
//!
//! **Shadow-only (#9784 acceptance):** footprints expose evidence. Nothing
//! here creates stacking edges, holds, or issue combinations, and unknown is
//! never rendered as zero-risk.

pub mod classify;

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::context_cache::store::ArtifactStore;

/// Schema version of the footprint artifact.
pub const SCHEMA_VERSION: u32 = 1;

/// One classified location.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FootprintLocation {
    /// Repo-relative canonical path.
    pub path: String,
    /// The pinned source revision the classification was validated against.
    pub source_revision: String,
    /// Enclosing qualified symbol when derivable — rules-v1 does not parse
    /// symbols; always `None` (explicit missing, not guessed).
    pub symbol: Option<String>,
    /// 1-based inclusive ranges; empty for proposed new paths (no invented
    /// ranges) and file-level evidence.
    pub ranges: Vec<(u32, u32)>,
    /// Proposed operation.
    pub operation: Operation,
    /// Intended role (independent of artifact kind).
    pub intended_role: IntendedRole,
    /// Artifact kind (independent of intended role).
    pub artifact_kind: ArtifactKind,
    /// Classifier confidence in [0,1]. Low confidence is honest signal.
    pub confidence: f64,
    /// Indices into the source context artifact's `results` (raw evidence
    /// preserved at the reference, not duplicated here).
    pub evidence_refs: Vec<usize>,
    /// Classifier notes (e.g. why contract evidence was not claimed).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Create,
    Edit,
    Delete,
    Rename,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntendedRole {
    Edit,
    ContextRead,
    ContractProducer,
    ContractConsumer,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Source,
    Test,
    Fixture,
    Generated,
    Configuration,
    Unknown,
}

/// The persisted, versioned footprint artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FootprintArtifact {
    pub schema_version: u32,
    /// The #9783 content key whose cached evidence this footprint classifies.
    pub context_key: String,
    /// The pinned source revision (mirrored from the context artifact).
    pub source_revision: String,
    /// Classifier identity — bumping it recomputes footprints over unchanged
    /// cached retrieval.
    pub classifier: ClassifierIdentity,
    /// Curator affected-file baseline consumed by the classifier (kept so
    /// the classification is replayable without re-reading the issue).
    pub curator_affected_files: Vec<String>,
    pub locations: Vec<FootprintLocation>,
    /// Coverage + unresolved provenance, preserved honestly ("unknown is not
    /// a zero-risk footprint").
    pub coverage: Coverage,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClassifierIdentity {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Coverage {
    /// Locations classified as intended edits.
    pub intended_edits: usize,
    /// Locations that are context-only reads (overlap here is NOT collision).
    pub context_reads: usize,
    /// Locations with unknown provenance (path the pinned tree rejects).
    pub unknown_provenance: usize,
    /// Query classes with no locations in the source artifact.
    pub empty_classes: Vec<String>,
    /// Free notes (missing evidence, unresolved contract claims).
    pub notes: Vec<String>,
}

/// Build a footprint from a cached context artifact. `pinned_tree` maps
/// repo-relative paths to line counts at the pinned revision (missing map =
/// tree unresolvable → provenance unknown for all locations).
pub fn build(
    context: &crate::context_cache::ContextArtifact,
    curator_affected_files: &[String],
    classifier: ClassifierIdentity,
    pinned_tree: &BTreeMap<String, u32>,
) -> FootprintArtifact {
    let locations = classify::RulesV1.classify(context, curator_affected_files, pinned_tree);
    let coverage = summarize(&locations, context, pinned_tree.is_empty());
    FootprintArtifact {
        schema_version: SCHEMA_VERSION,
        context_key: context.key.clone(),
        source_revision: context.input.source_revision.clone(),
        classifier,
        curator_affected_files: curator_affected_files.to_vec(),
        locations,
        coverage,
    }
}

fn summarize(
    locations: &[FootprintLocation],
    context: &crate::context_cache::ContextArtifact,
    tree_unresolved: bool,
) -> Coverage {
    let mut c = Coverage::default();
    for l in locations {
        match l.intended_role {
            IntendedRole::Edit => c.intended_edits += 1,
            IntendedRole::ContextRead => c.context_reads += 1,
            _ => {}
        }
        if l.path.starts_with('<') {
            c.unknown_provenance += 1;
        }
    }
    for note in &context.session.coverage_notes {
        if note.contains("successful empty output") {
            if let Some(class) = note.split('`').nth(1) {
                c.empty_classes.push(class.to_string());
            }
        }
    }
    if tree_unresolved {
        c.notes.push(
            "pinned source tree unresolvable — create-vs-edit provenance is unknown for all locations"
                .into(),
        );
    }
    c.notes.push(
        "contract producer/consumer claims require declarations/callers evidence — \
         rules-v1 records them as unknown rather than guessing"
            .into(),
    );
    c
}

/// Load a context artifact by key from the store.
pub fn load_context(
    store: &ArtifactStore,
    key: &str,
) -> Result<crate::context_cache::ContextArtifact> {
    store
        .load(key)?
        .ok_or_else(|| anyhow::anyhow!("no context artifact for key {key}"))
}

/// Persist a footprint beside the context cache (separate namespace — the
/// footprint is DERIVED state; a classifier bump replaces it while the
/// retrieval entry survives untouched).
pub fn persist(store: &ArtifactStore, footprint: &FootprintArtifact) -> Result<PathBuf> {
    let dir = store.root().join("footprints");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating footprint dir {}", dir.display()))?;
    let path = dir.join(format!("{}-{}.json", footprint.context_key, footprint.classifier.version));
    std::fs::write(&path, serde_json::to_vec_pretty(footprint)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Read a persisted footprint.
pub fn load(
    store: &ArtifactStore,
    context_key: &str,
    classifier_version: &str,
) -> Result<FootprintArtifact> {
    let path = store
        .root()
        .join("footprints")
        .join(format!("{context_key}-{classifier_version}.json"));
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))
}

/// Pair-overlap summary across two footprints (#9784's shared read-only
/// overlap helper): intended-edit overlap is the collision signal;
/// context-only overlap is recorded but explicitly separated.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairOverlap {
    pub context_key_a: String,
    pub context_key_b: String,
    /// Paths BOTH sides classify as intended edits — the collision signal.
    pub intended_edit_overlap: Vec<String>,
    /// Paths both sides touch where at least one side is context-only.
    pub context_only_overlap: Vec<String>,
    /// Paths where either side is unknown-role.
    pub unknown_overlap: Vec<String>,
}

pub fn pair_overlap(a: &FootprintArtifact, b: &FootprintArtifact) -> PairOverlap {
    fn by_path(f: &FootprintArtifact) -> BTreeMap<&str, &FootprintLocation> {
        f.locations.iter().map(|l| (l.path.as_str(), l)).collect()
    }
    let (ma, mb) = (by_path(a), by_path(b));
    let mut intended = Vec::new();
    let mut context_only = Vec::new();
    let mut unknown = Vec::new();
    for (path, la) in &ma {
        let Some(lb) = mb.get(path) else { continue };
        match (la.intended_role, lb.intended_role) {
            (IntendedRole::Edit, IntendedRole::Edit) => intended.push((*path).to_string()),
            (IntendedRole::Unknown, _) | (_, IntendedRole::Unknown) => {
                unknown.push((*path).to_string())
            }
            _ => context_only.push((*path).to_string()),
        }
    }
    intended.sort();
    context_only.sort();
    unknown.sort();
    PairOverlap {
        context_key_a: a.context_key.clone(),
        context_key_b: b.context_key.clone(),
        intended_edit_overlap: intended,
        context_only_overlap: context_only,
        unknown_overlap: unknown,
    }
}

/// Resolve the pinned revision's path→line-count tree via a git checkout
/// (same helper family as the cache's source validation).
pub fn pinned_tree(repo: &Path, source_revision: &str) -> BTreeMap<String, u32> {
    crate::context_cache::session::pinned_source_index(repo, source_revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_cache::{adapter, key, session};

    pub(crate) fn context_with_results(
        key_str: &str,
        results: Vec<session::NormalizedSnippet>,
    ) -> crate::context_cache::ContextArtifact {
        crate::context_cache::ContextArtifact {
            schema_version: 1,
            key: key_str.into(),
            input: key::InputSnapshot {
                schema_version: 1,
                repo: "o/r".into(),
                issue: 1,
                title: "t".into(),
                body: "b".into(),
                requirement_comments: vec![],
                source_revision: "0123456789abcdef0123456789abcdef01234567".into(),
                index_identity: "i".into(),
                query_policy_version: "q".into(),
                adapter_version: "a".into(),
            },
            session: session::SessionRecord {
                status: session::SessionStatus::Success,
                queries: vec![],
                provider: adapter::ProviderIdentity {
                    name: "fake".into(),
                    version: "1".into(),
                },
                results,
                raw_responses: vec![],
                budget_report: session::BudgetReport {
                    calls: 1,
                    bytes: 1,
                    retries: 0,
                    elapsed_ms: 1,
                },
                coverage_notes: vec![
                    "query class `configuration`: successful empty output (cached as such)".into(),
                ],
                checksum: String::new(),
            },
            completed_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    pub(crate) fn classifier() -> ClassifierIdentity {
        ClassifierIdentity {
            name: "rules".into(),
            version: "v1".into(),
        }
    }

    #[test]
    fn coverage_counts_roles_and_preserves_unknowns() {
        let ctx = context_with_results(
            "k",
            vec![
                session::NormalizedSnippet {
                    path: "src/a.rs".into(),
                    ranges: vec![(1, 2)],
                    text: "t".into(),
                    query_class: "implementation".into(),
                    source_ref: "0".into(),
                },
                session::NormalizedSnippet {
                    path: "<unknown-provenance>".into(),
                    ranges: vec![],
                    text: "mystery".into(),
                    query_class: "consumer".into(),
                    source_ref: "1".into(),
                },
            ],
        );
        let f = build(&ctx, &["src/a.rs".to_string()], classifier(), &BTreeMap::new());
        assert_eq!(f.coverage.intended_edits, 1);
        assert_eq!(f.coverage.unknown_provenance, 1);
        assert_eq!(f.coverage.empty_classes, vec!["configuration"]);
        assert!(f.coverage.notes.iter().any(|n| n.contains("unresolvable")));
    }

    #[test]
    fn pair_overlap_separates_intended_from_context() {
        let ctx = |k: &str| context_with_results(k, vec![]);
        let mk = |k: &str, edits: &[&str], ctxs: &[&str]| {
            let mut f = build(&ctx(k), &[], classifier(), &BTreeMap::new());
            for e in edits {
                f.locations.push(FootprintLocation {
                    path: (*e).into(),
                    source_revision: f.source_revision.clone(),
                    symbol: None,
                    ranges: vec![],
                    operation: Operation::Edit,
                    intended_role: IntendedRole::Edit,
                    artifact_kind: ArtifactKind::Source,
                    confidence: 0.8,
                    evidence_refs: vec![],
                    notes: vec![],
                });
            }
            for c in ctxs {
                f.locations.push(FootprintLocation {
                    path: (*c).into(),
                    source_revision: f.source_revision.clone(),
                    symbol: None,
                    ranges: vec![],
                    operation: Operation::Edit,
                    intended_role: IntendedRole::ContextRead,
                    artifact_kind: ArtifactKind::Source,
                    confidence: 0.4,
                    evidence_refs: vec![],
                    notes: vec![],
                });
            }
            f
        };
        let a = mk("A", &["src/shared.rs", "src/only_a.rs"], &["src/helper.rs"]);
        let b = mk("B", &["src/shared.rs"], &["src/helper.rs", "src/only_a.rs"]);
        let ov = pair_overlap(&a, &b);
        // Both intend to rewrite shared.rs → collision signal.
        assert_eq!(ov.intended_edit_overlap, vec!["src/shared.rs"]);
        // only_a is an intended edit on A but context-only on B → NOT a
        // collision; helper is context-only on both.
        assert!(ov
            .context_only_overlap
            .contains(&"src/only_a.rs".to_string()));
        assert!(ov
            .context_only_overlap
            .contains(&"src/helper.rs".to_string()));
        assert!(!ov
            .intended_edit_overlap
            .contains(&"src/only_a.rs".to_string()));
    }
}
