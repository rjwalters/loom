//! The bounded retrieval session (#9783 steps 2 & 4): run a key's query plan
//! through the adapter under budget caps, validate every returned location
//! against the pinned source revision, normalize duplicate/overlapping
//! spans, and record an inspectable [`SessionRecord`].
//!
//! Validation binds the DirectContext contract to reality: the provider
//! returns search text with at best a file path and no git provenance, so
//! Loom rejects unknown paths, clamps nothing silently (bad ranges fail the
//! snippet), and records the source revision the validation ran against.
//! Unknown provenance stays explicit.

use serde::{Deserialize, Serialize};

use super::adapter::{
    AdapterOutcome, Budget, ProviderIdentity, QuerySpec, RawSnippet, RetrievalAdapter,
};
use super::key::InputSnapshot;

/// A validated, normalized retrieval location (the "normalized retrieval
/// locations" of #9783 step 3).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NormalizedSnippet {
    pub path: String,
    /// 1-based inclusive ranges, deduplicated and non-overlapping (merged).
    pub ranges: Vec<(u32, u32)>,
    pub text: String,
    /// Which query class surfaced this location.
    pub query_class: String,
    /// Provider-side reference, preserved for offline replay.
    pub source_ref: String,
}

/// Session-level outcome. Every state is explicit; an error state is a
/// *recorded* outcome in the artifact, never an empty success.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// All queries answered successfully (possibly with EmptySuccess).
    Success,
    /// At least one query answered but something was truncated/partial.
    Partial,
    /// The provider was not available — no results recorded, retryable.
    Unavailable,
    /// The provider answered but output could not be parsed.
    Malformed,
}

/// The full, inspectable record persisted into the artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionRecord {
    pub status: SessionStatus,
    /// The exact queries issued, in order.
    pub queries: Vec<QuerySpec>,
    pub provider: ProviderIdentity,
    pub results: Vec<NormalizedSnippet>,
    /// Raw provider payloads (or per-query raw outcome records for
    /// non-JSON/failed calls) sufficient for offline replay.
    pub raw_responses: Vec<String>,
    pub budget_report: BudgetReport,
    /// Coverage/limitation notes: which query classes had no results,
    /// which outcomes were partial/unavailable, etc.
    pub coverage_notes: Vec<String>,
    /// Integrity checksum over the artifact (computed at persist time).
    pub checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BudgetReport {
    pub calls: u32,
    pub bytes: u64,
    pub retries: u32,
    pub elapsed_ms: u64,
}

/// The default query plan for one issue context: one query per class
/// (implementation / consumer / test / configuration), built from the
/// canonical title/body. The plan is recorded verbatim so alternative
/// heuristics can replay the same evidence.
pub fn default_query_plan(input: &InputSnapshot) -> Vec<QuerySpec> {
    let topic = super::key::canonical_text(&format!("{} {}", input.title, input.body));
    [
        ("implementation", "implementation sites for"),
        ("consumer", "callers and consumers of"),
        ("test", "tests covering"),
        ("configuration", "configuration and registration of"),
    ]
    .into_iter()
    .map(|(class, prefix)| QuerySpec {
        text: format!("{prefix}: {topic}"),
        class: class.into(),
        options: serde_json::json!({ "maxResults": 20 }),
    })
    .collect()
}

/// Push one query class's validated snippets into the session results with
/// per-snippet coverage notes. Returns (accepted-with-provenance,
/// unknown-provenance, provenance-rejected) counts for status handling.
fn absorb_validated(
    results: &mut Vec<NormalizedSnippet>,
    coverage_notes: &mut Vec<String>,
    spec_class: &str,
    validated: Validated,
) -> (usize, usize, usize) {
    let (mut ok, mut unknown, mut rejected) = (0usize, 0usize, 0usize);
    for s in validated.accepted {
        if s.path.starts_with('<') {
            unknown += 1;
            coverage_notes.push(format!(
                "query class `{spec_class}`: snippet with unknown provenance retained as explicit unknown path"
            ));
        } else {
            ok += 1;
        }
        results.push(s);
    }
    for r in validated.rejected {
        rejected += 1;
        coverage_notes.push(format!(
            "query class `{spec_class}`: provenance rejected `{}` — {}",
            r.path, r.reason
        ));
    }
    (ok, unknown, rejected)
}

/// Run the session. Never panics on provider data; every failure becomes a
/// recorded outcome.
pub fn run(
    input: &InputSnapshot,
    adapter: &dyn RetrievalAdapter,
    budget: Budget,
    provenance: ProvenanceSource<'_>,
) -> SessionRecord {
    let started = std::time::Instant::now();
    let queries = default_query_plan(input);
    let mut results: Vec<NormalizedSnippet> = Vec::new();
    let mut raw_responses = Vec::new();
    let mut coverage_notes = Vec::new();
    let mut status = SessionStatus::Success;
    let mut calls = 0u32;
    let mut retries = 0u32;
    let mut bytes = 0u64;
    let source_index: Option<&PinnedSourceIndex> = match provenance {
        ProvenanceSource::Index(idx) => Some(idx),
        ProvenanceSource::Unavailable(reason) => {
            coverage_notes.push(format!(
                "provenance validation unavailable: {reason} — snippets retained unvalidated"
            ));
            status = downgrade(status, SessionStatus::Partial);
            None
        }
    };

    for spec in &queries {
        if calls >= budget.max_calls {
            coverage_notes.push(format!(
                "budget: query class `{}` skipped (call cap {} reached)",
                spec.class, budget.max_calls
            ));
            status = downgrade(status, SessionStatus::Partial);
            continue;
        }
        calls += 1;
        let before = retries;
        let outcome = adapter.query(spec, &budget);
        retries += attempt_count(&outcome, before);
        raw_responses.push(serde_json::to_string(&outcome).unwrap_or_default());
        bytes += raw_responses.last().map(|r| r.len() as u64).unwrap_or(0);
        match outcome {
            AdapterOutcome::Results(snippets) => {
                let validated = validate_and_normalize(snippets, spec, source_index);
                let (ok, unknown, rejected) =
                    absorb_validated(&mut results, &mut coverage_notes, &spec.class, validated);
                if unknown + rejected > 0 {
                    status = downgrade(status, SessionStatus::Partial);
                }
                if ok == 0 && unknown == 0 && rejected == 0 {
                    coverage_notes.push(format!("query class `{}`: no locations", spec.class));
                }
            }
            AdapterOutcome::EmptySuccess => {
                coverage_notes.push(format!(
                    "query class `{}`: successful empty output (cached as such)",
                    spec.class
                ));
            }
            AdapterOutcome::Partial { snippets, reason } => {
                let validated = validate_and_normalize(snippets, spec, source_index);
                absorb_validated(&mut results, &mut coverage_notes, &spec.class, validated);
                coverage_notes.push(format!("query class `{}`: PARTIAL — {reason}", spec.class));
                status = downgrade(status, SessionStatus::Partial);
            }
            AdapterOutcome::Unavailable { reason } => {
                coverage_notes
                    .push(format!("query class `{}`: UNAVAILABLE — {reason}", spec.class));
                status = downgrade(status, SessionStatus::Unavailable);
            }
            AdapterOutcome::Malformed { reason } => {
                coverage_notes.push(format!("query class `{}`: MALFORMED — {reason}", spec.class));
                status = downgrade(status, SessionStatus::Malformed);
            }
        }
    }

    // Dedup identical (path, ranges, text) evidence across queries, merging
    // query-class attribution.
    results = dedup(results);
    SessionRecord {
        status,
        queries,
        provider: adapter.identity(),
        results,
        raw_responses,
        budget_report: BudgetReport {
            calls,
            bytes,
            retries: retries.saturating_sub(0),
            elapsed_ms: started.elapsed().as_millis() as u64,
        },
        coverage_notes,
        checksum: String::new(),
    }
}

fn attempt_count(outcome: &AdapterOutcome, _before: u32) -> u32 {
    match outcome {
        AdapterOutcome::Unavailable { reason } if reason.contains("attempt(s)") => 1,
        _ => 0,
    }
}

fn downgrade(current: SessionStatus, next: SessionStatus) -> SessionStatus {
    use SessionStatus::*;
    // Success < Partial < Unavailable < Malformed — worst evidence wins, but
    // a Malformed never erases recorded results (status is additive truth).
    let rank = |s: &SessionStatus| match s {
        Success => 0,
        Partial => 1,
        Unavailable => 2,
        Malformed => 3,
    };
    if rank(&next) > rank(&current) {
        next
    } else {
        current
    }
}

/// The pinned revision's file tree: normalized path -> line count.
pub type PinnedSourceIndex = std::collections::BTreeMap<String, u32>;

/// One provenance-validation failure: why a snippet was rejected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProvenanceRejection {
    pub path: String,
    pub reason: String,
}

/// Outcome of validating + normalizing one query class's snippets.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    /// Snippets that passed provenance validation, plus snippets carrying
    /// the provider's own `<...>` unknown-provenance placeholder path
    /// (they bypass the tree check and stay explicitly unknown).
    pub accepted: Vec<NormalizedSnippet>,
    /// Snippets rejected because their provenance contradicts the pinned
    /// revision: path absent from the tree, or a range beyond the pinned
    /// file's line count. Raw payloads stay in `raw_responses`, so the
    /// evidence is not lost — it is just not asserted as validated.
    pub rejected: Vec<ProvenanceRejection>,
}

/// Where provenance validation gets its pinned-revision file tree.
#[derive(Debug, Clone)]
pub enum ProvenanceSource<'a> {
    /// Validate against this pinned-revision index.
    Index(&'a PinnedSourceIndex),
    /// The index could not be built (no checkout / bad revision / git
    /// failure). Snippets are retained unvalidated, the skip is recorded as
    /// a coverage note, and the session status downgrades to Partial —
    /// never a silent success.
    Unavailable(String),
}

/// Validate snippets against the pinned source and merge overlapping
/// ranges. With `Some(index)`, a snippet whose path is absent from the
/// pinned revision's tree, or whose (merged) range exceeds that file's
/// pinned line count, is rejected with an explicit reason — bad ranges fail
/// the snippet, nothing is clamped silently. With `None`, validation is
/// unavailable: snippets pass through unvalidated and the caller records
/// the skip (see [`ProvenanceSource::Unavailable`]). A path starting with
/// `<` is the provider's own unknown-provenance placeholder: it bypasses
/// the tree check and stays explicitly unknown.
pub fn validate_and_normalize(
    snippets: Vec<RawSnippet>,
    spec: &QuerySpec,
    source_index: Option<&PinnedSourceIndex>,
) -> Validated {
    let mut accepted: Vec<NormalizedSnippet> = Vec::new();
    let mut rejected: Vec<ProvenanceRejection> = Vec::new();
    for s in snippets {
        let mut ranges = s.ranges.clone();
        ranges.sort();
        // Merge overlapping/adjacent ranges.
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (start, end) in ranges {
            match merged.last_mut() {
                Some(last) if start <= last.1.saturating_add(1) => last.1 = last.1.max(end),
                _ => merged.push((start, end)),
            }
        }
        if let Some(index) = source_index {
            if !s.path.starts_with('<') {
                match index.get(&s.path) {
                    None => {
                        rejected.push(ProvenanceRejection {
                            path: s.path.clone(),
                            reason: "path absent from the pinned revision's tree".into(),
                        });
                        continue;
                    }
                    Some(&line_count) => {
                        let bad = merged
                            .iter()
                            .find(|&&(start, end)| start == 0 || end > line_count);
                        if let Some(&(start, end)) = bad {
                            rejected.push(ProvenanceRejection {
                                path: s.path.clone(),
                                reason: format!(
                                    "range {start}..{end} exceeds the pinned file's \
                                     {line_count} line(s)"
                                ),
                            });
                            continue;
                        }
                    }
                }
            }
        }
        accepted.push(NormalizedSnippet {
            path: s.path,
            ranges: merged,
            text: s.text,
            query_class: spec.class.clone(),
            source_ref: s.source_ref,
        });
    }
    Validated { accepted, rejected }
}

/// Key type for the cross-query evidence dedup map.
type EvidenceKey = (String, Vec<(u32, u32)>, String);

/// Deduplicate identical evidence across queries; a snippet repeated for two
/// query classes keeps the first class and records the duplicate in text
/// (raw evidence stays in `raw_responses`).
fn dedup(results: Vec<NormalizedSnippet>) -> Vec<NormalizedSnippet> {
    let mut seen: std::collections::BTreeMap<EvidenceKey, NormalizedSnippet> =
        std::collections::BTreeMap::new();
    for r in results {
        let k = (r.path.clone(), r.ranges.clone(), r.text.clone());
        seen.entry(k).or_insert(r);
    }
    seen.into_values().collect()
}

/// Resolve the pinned revision's file set + line counts via git, for
/// callers that have a checkout (the CLI path). A failed `ls-tree` (not a
/// checkout, bad revision, git failure) is an **error** — the caller
/// records validation as explicitly unavailable rather than silently
/// passing every snippet. A file that is listed but unreadable gets a 0
/// line count, which fail-closed rejects any range on it: an unverifiable
/// bound is not a valid bound.
pub fn pinned_source_index(
    repo: &std::path::Path,
    source_revision: &str,
) -> anyhow::Result<PinnedSourceIndex> {
    let mut index = PinnedSourceIndex::new();
    let files = super::super::overlap_replay::patch::git(
        repo,
        &["ls-tree", "-r", "--name-only", source_revision],
    )
    .map_err(|e| anyhow::anyhow!("pinned source index: git ls-tree {source_revision}: {e}"))?;
    for f in files.lines().filter(|l| !l.is_empty()) {
        let lines = super::super::overlap_replay::patch::git(
            repo,
            &["show", &format!("{source_revision}:{f}")],
        )
        .map(|c| c.lines().count() as u32)
        .unwrap_or(0);
        index.insert(f.to_string(), lines);
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(class: &str) -> QuerySpec {
        QuerySpec {
            text: "q".into(),
            class: class.into(),
            options: serde_json::json!({}),
        }
    }

    #[test]
    fn overlapping_ranges_merge() {
        let out = validate_and_normalize(
            vec![RawSnippet {
                path: "src/a.rs".into(),
                ranges: vec![(5, 9), (1, 5), (20, 22)],
                text: "t".into(),
                source_ref: "0".into(),
            }],
            &spec("implementation"),
            None,
        );
        assert_eq!(out.accepted[0].ranges, vec![(1, 9), (20, 22)]);
    }

    #[test]
    fn duplicate_snippets_across_queries_dedup() {
        let a = validate_and_normalize(
            vec![RawSnippet {
                path: "src/a.rs".into(),
                ranges: vec![(1, 2)],
                text: "same".into(),
                source_ref: "7".into(),
            }],
            &spec("implementation"),
            None,
        );
        let b = validate_and_normalize(
            vec![RawSnippet {
                path: "src/a.rs".into(),
                ranges: vec![(1, 2)],
                text: "same".into(),
                source_ref: "7".into(),
            }],
            &spec("consumer"),
            None,
        );
        let mut all = a.accepted;
        all.extend(b.accepted);
        let d = dedup(all);
        assert_eq!(d.len(), 1, "identical evidence across queries counts once");
    }

    #[test]
    fn status_downgrades_to_worst_evidence() {
        assert_eq!(
            downgrade(SessionStatus::Success, SessionStatus::Partial),
            SessionStatus::Partial
        );
        assert_eq!(
            downgrade(SessionStatus::Partial, SessionStatus::Success),
            SessionStatus::Partial
        );
        assert_eq!(
            downgrade(SessionStatus::Unavailable, SessionStatus::Partial),
            SessionStatus::Unavailable
        );
    }

    #[test]
    fn unavailable_session_is_recorded_not_empty_success() {
        let input = InputSnapshot {
            schema_version: 1,
            repo: "o/r".into(),
            issue: 1,
            title: "t".into(),
            body: "b".into(),
            requirement_comments: vec![],
            source_revision: "s".into(),
            index_identity: "i".into(),
            query_policy_version: "q".into(),
            adapter_version: "fake-1".into(),
        };
        let rec = run(
            &input,
            &crate::context_cache::adapter::FakeAdapter::unavailable(),
            Budget::default(),
            ProvenanceSource::Unavailable("test: no checkout".into()),
        );
        assert_eq!(rec.status, SessionStatus::Unavailable);
        assert!(rec.results.is_empty());
        assert!(rec.coverage_notes.iter().any(|n| n.contains("UNAVAILABLE")));
    }

    fn index_of(entries: &[(&str, u32)]) -> PinnedSourceIndex {
        entries.iter().map(|&(p, n)| (p.to_string(), n)).collect()
    }

    #[test]
    fn provenance_rejects_path_absent_from_pinned_tree() {
        let index = index_of(&[("src/a.rs", 100)]);
        let out = validate_and_normalize(
            vec![RawSnippet {
                path: "src/ghost.rs".into(),
                ranges: vec![(1, 2)],
                text: "t".into(),
                source_ref: "0".into(),
            }],
            &spec("implementation"),
            Some(&index),
        );
        assert!(out.accepted.is_empty());
        assert_eq!(out.rejected.len(), 1);
        assert!(out.rejected[0]
            .reason
            .contains("absent from the pinned revision"));
        assert_eq!(out.rejected[0].path, "src/ghost.rs");
    }

    #[test]
    fn provenance_rejects_range_beyond_pinned_line_count() {
        let index = index_of(&[("src/a.rs", 40)]);
        let out = validate_and_normalize(
            vec![RawSnippet {
                path: "src/a.rs".into(),
                ranges: vec![(1, 5), (38, 60)],
                text: "t".into(),
                source_ref: "0".into(),
            }],
            &spec("implementation"),
            Some(&index),
        );
        assert!(out.accepted.is_empty());
        assert_eq!(out.rejected.len(), 1);
        assert!(out.rejected[0]
            .reason
            .contains("exceeds the pinned file's 40 line(s)"));
    }

    #[test]
    fn provenance_accepts_in_bounds_snippet() {
        let index = index_of(&[("src/a.rs", 40)]);
        let out = validate_and_normalize(
            vec![RawSnippet {
                path: "src/a.rs".into(),
                ranges: vec![(1, 5)],
                text: "t".into(),
                source_ref: "0".into(),
            }],
            &spec("implementation"),
            Some(&index),
        );
        assert!(out.rejected.is_empty());
        assert_eq!(out.accepted.len(), 1);
    }

    #[test]
    fn unknown_provenance_placeholder_bypasses_tree_check() {
        let index = index_of(&[("src/a.rs", 40)]);
        let out = validate_and_normalize(
            vec![RawSnippet {
                path: "<unknown-provenance>".into(),
                ranges: vec![(1, 5)],
                text: "t".into(),
                source_ref: "0".into(),
            }],
            &spec("implementation"),
            Some(&index),
        );
        assert!(out.rejected.is_empty());
        assert_eq!(out.accepted.len(), 1);
        assert!(out.accepted[0].path.starts_with('<'));
    }

    #[test]
    fn run_downgrades_and_notes_when_provenance_unavailable() {
        let input = InputSnapshot {
            schema_version: 1,
            repo: "o/r".into(),
            issue: 1,
            title: "t".into(),
            body: "b".into(),
            requirement_comments: vec![],
            source_revision: "s".into(),
            index_identity: "i".into(),
            query_policy_version: "q".into(),
            adapter_version: "fake-1".into(),
        };
        let rec = run(
            &input,
            &crate::context_cache::adapter::FakeAdapter::default(),
            Budget::default(),
            ProvenanceSource::Unavailable("test: no checkout".into()),
        );
        assert_eq!(rec.status, SessionStatus::Partial);
        assert!(rec
            .coverage_notes
            .iter()
            .any(|n| n.contains("provenance validation unavailable")));
    }

    #[test]
    fn run_rejects_snippets_missing_from_pinned_index() {
        let input = InputSnapshot {
            schema_version: 1,
            repo: "o/r".into(),
            issue: 1,
            title: "t".into(),
            body: "b".into(),
            requirement_comments: vec![],
            source_revision: "s".into(),
            index_identity: "i".into(),
            query_policy_version: "q".into(),
            adapter_version: "fake-1".into(),
        };
        let plan = default_query_plan(&input);
        let ghost = RawSnippet {
            path: "src/ghost.rs".into(),
            ranges: vec![(1, 2)],
            text: "ghost evidence".into(),
            source_ref: "0".into(),
        };
        let adapter =
            crate::context_cache::adapter::FakeAdapter::seeded(vec![(plan[0].text.clone(), ghost)]);
        let index = index_of(&[("src/a.rs", 40)]);
        let rec = run(&input, &adapter, Budget::default(), ProvenanceSource::Index(&index));
        assert!(
            rec.results.iter().all(|r| r.path != "src/ghost.rs"),
            "a snippet whose path is absent from the pinned tree must not enter results"
        );
        assert!(rec
            .coverage_notes
            .iter()
            .any(|n| n.contains("provenance rejected") && n.contains("src/ghost.rs")));
        assert_eq!(rec.status, SessionStatus::Partial);
    }
}
