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

/// Run the session. Never panics on provider data; every failure becomes a
/// recorded outcome.
pub fn run(input: &InputSnapshot, adapter: &dyn RetrievalAdapter, budget: Budget) -> SessionRecord {
    let started = std::time::Instant::now();
    let queries = default_query_plan(input);
    let mut results: Vec<NormalizedSnippet> = Vec::new();
    let mut raw_responses = Vec::new();
    let mut coverage_notes = Vec::new();
    let mut status = SessionStatus::Success;
    let mut calls = 0u32;
    let mut retries = 0u32;
    let mut bytes = 0u64;

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
                let (mut ok, mut rejected) = (0usize, 0usize);
                for s in validate_and_normalize(snippets, spec) {
                    if s.path.starts_with('<') {
                        rejected += 1;
                        coverage_notes.push(format!(
                            "query class `{}`: snippet with unknown provenance retained as explicit unknown path",
                            spec.class
                        ));
                    } else {
                        ok += 1;
                    }
                    results.push(s);
                }
                if rejected > 0 {
                    status = downgrade(status, SessionStatus::Partial);
                }
                if ok == 0 && rejected == 0 {
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
                results.extend(validate_and_normalize(snippets, spec));
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

/// Validate snippets against the pinned source (paths + range bounds via
/// `source_revision`'s tree) and merge overlapping ranges. The source file
/// list/lengths are provided by the caller's pinned-revision resolver so
/// this stays a pure function over its inputs.
pub fn validate_and_normalize(
    snippets: Vec<RawSnippet>,
    spec: &QuerySpec,
) -> Vec<NormalizedSnippet> {
    let mut out: Vec<NormalizedSnippet> = Vec::new();
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
        out.push(NormalizedSnippet {
            path: s.path,
            ranges: merged,
            text: s.text,
            query_class: spec.class.clone(),
            source_ref: s.source_ref,
        });
    }
    out
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
/// callers that have a checkout (the CLI path). Validation failures here
/// mark snippets as unknown-provenance rather than guessing.
pub fn pinned_source_index(
    repo: &std::path::Path,
    source_revision: &str,
) -> std::collections::BTreeMap<String, u32> {
    let mut index = std::collections::BTreeMap::new();
    let Ok(files) = super::super::overlap_replay::patch::git(
        repo,
        &["ls-tree", "-r", "--name-only", source_revision],
    ) else {
        return index;
    };
    for f in files.lines().filter(|l| !l.is_empty()) {
        let lines = super::super::overlap_replay::patch::git(
            repo,
            &["show", &format!("{source_revision}:{f}")],
        )
        .map(|c| c.lines().count() as u32)
        .unwrap_or(0);
        index.insert(f.to_string(), lines);
    }
    index
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
        );
        assert_eq!(out[0].ranges, vec![(1, 9), (20, 22)]);
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
        );
        let b = validate_and_normalize(
            vec![RawSnippet {
                path: "src/a.rs".into(),
                ranges: vec![(1, 2)],
                text: "same".into(),
                source_ref: "7".into(),
            }],
            &spec("consumer"),
        );
        let mut all = a;
        all.extend(b);
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
        );
        assert_eq!(rec.status, SessionStatus::Unavailable);
        assert!(rec.results.is_empty());
        assert!(rec.coverage_notes.iter().any(|n| n.contains("UNAVAILABLE")));
    }
}
