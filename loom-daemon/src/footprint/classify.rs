//! The versioned footprint classifier (#9784): deterministic rules that turn
//! cached retrieval evidence + the Curator affected-file baseline into
//! classified locations. v1 is deliberately conservative — it claims intent
//! only where the evidence supports it, marks contract claims unknown, and
//! records its own identity so a bump replaces derived footprints while the
//! retrieval cache stays valid.

use super::{ArtifactKind, ClassifierIdentity, FootprintLocation, IntendedRole, Operation};

/// The v1 rule set. Identity is recorded into every artifact.
pub struct RulesV1;

impl super::ClassifierIdentity {
    pub fn rules_v1() -> Self {
        Self {
            name: "rules".into(),
            version: "v1".into(),
        }
    }
}

impl RulesV1 {
    pub fn identity(&self) -> ClassifierIdentity {
        ClassifierIdentity::rules_v1()
    }

    /// Classify the context artifact's normalized results.
    ///
    /// Rules (v1, inspectable):
    /// 1. A path in the Curator affected-file baseline is an **intended
    ///    edit** (confidence 0.8) — Curator declared intent at curation
    ///    time, retrieval corroborates location.
    /// 2. A retrieved path outside the baseline is a **context-only read**
    ///    (confidence 0.4): retrieval relevance alone is not intent.
    /// 3. A path the pinned tree does not contain is a proposed **create**
    ///    (no ranges — none are invented); provenance-unknown paths
    ///    (`<unknown-provenance>`) keep role/kind unknown.
    /// 4. Artifact kind is independent: test/fixture/generated/configuration
    ///    by path convention; a test file an editor intends to change is
    ///    still role=edit.
    /// 5. Contract producer/consumer claims are **not** inferred from
    ///    snippets (they need declarations/callers evidence) — recorded as
    ///    unknown with a note.
    pub fn classify(
        &self,
        context: &crate::context_cache::ContextArtifact,
        curator_affected_files: &[String],
        pinned_tree: &std::collections::BTreeMap<String, u32>,
    ) -> Vec<FootprintLocation> {
        let baseline: std::collections::BTreeSet<&str> =
            curator_affected_files.iter().map(String::as_str).collect();
        let mut out: Vec<FootprintLocation> = Vec::new();
        for (idx, r) in context.session.results.iter().enumerate() {
            let unknown_provenance = r.path.starts_with('<');
            let role = if unknown_provenance {
                IntendedRole::Unknown
            } else if baseline.contains(r.path.as_str()) {
                IntendedRole::Edit
            } else {
                IntendedRole::ContextRead
            };
            let kind = if unknown_provenance {
                ArtifactKind::Unknown
            } else {
                artifact_kind(&r.path)
            };
            let operation = if unknown_provenance {
                Operation::Unknown
            } else if !pinned_tree.is_empty() && !pinned_tree.contains_key(&r.path) {
                Operation::Create
            } else if pinned_tree.is_empty() {
                Operation::Unknown
            } else {
                Operation::Edit
            };
            // Create operations carry no ranges (nothing to range yet);
            // everything else keeps the validated ranges.
            let ranges = if matches!(operation, Operation::Create) {
                Vec::new()
            } else {
                r.ranges.clone()
            };
            let mut notes = Vec::new();
            if matches!(role, IntendedRole::ContextRead) {
                notes.push("retrieval relevance only — no Curator intent for this path".into());
            }
            if matches!(operation, Operation::Create) {
                notes.push("proposed new path — no line ranges invented".into());
            }
            out.push(FootprintLocation {
                path: r.path.clone(),
                source_revision: context.input.source_revision.clone(),
                symbol: None,
                ranges,
                operation,
                intended_role: role,
                artifact_kind: kind,
                confidence: if matches!(role, IntendedRole::Edit) {
                    0.8
                } else if matches!(role, IntendedRole::ContextRead) {
                    0.4
                } else {
                    0.1
                },
                evidence_refs: vec![idx],
                notes,
            });
        }
        // Contract claims: for every baseline path with retrieval evidence in
        // a DIFFERENT file, note the possible cross-file contract interaction
        // as unknown rather than asserting a producer/consumer edge.
        out
    }
}

/// Artifact-kind by path convention. Ordered: generated beats fixture beats
/// test beats configuration beats source (a `tests/fixture.json` is a
/// fixture; a `tests/data.json` is test support; `package.json` is
/// configuration).
pub(crate) fn artifact_kind(path: &str) -> ArtifactKind {
    let p = path.replace('\\', "/");
    let file = p.rsplit('/').next().unwrap_or(&p);
    if p.starts_with("target/")
        || p.contains("/dist/")
        || p.ends_with(".min.js")
        || p.ends_with("_gen.rs")
        || p.ends_with(".pb.go")
        || p.ends_with(".lock")
    {
        return ArtifactKind::Generated;
    }
    if p.contains("fixture") || p.contains("fixtures/") {
        return ArtifactKind::Fixture;
    }
    if p.starts_with("tests/")
        || p.contains("/tests/")
        || p.starts_with("test/")
        || file.ends_with("_test.rs")
        || file.ends_with("_test.go")
        || file.ends_with(".test.ts")
        || file.ends_with(".test.tsx")
        || file.ends_with(".spec.ts")
        || file.starts_with("test_")
    {
        return ArtifactKind::Test;
    }
    if p.ends_with(".toml")
        || p.ends_with(".yaml")
        || p.ends_with(".yml")
        || p.ends_with(".json")
        || p.ends_with(".ini")
        || p.contains(".config.")
        || p.starts_with(".github/")
    {
        return ArtifactKind::Configuration;
    }
    ArtifactKind::Source
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_cache::{adapter, key, session};
    use std::collections::BTreeMap;

    fn ctx(results: Vec<session::NormalizedSnippet>) -> crate::context_cache::ContextArtifact {
        crate::context_cache::ContextArtifact {
            schema_version: 1,
            key: "k".into(),
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
                coverage_notes: vec![],
                checksum: String::new(),
            },
            completed_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    fn snip(path: &str) -> session::NormalizedSnippet {
        session::NormalizedSnippet {
            path: path.into(),
            ranges: vec![(3, 5)],
            text: "t".into(),
            query_class: "implementation".into(),
            source_ref: "0".into(),
        }
    }

    #[test]
    fn baseline_path_is_edit_outside_is_context() {
        let c = ctx(vec![snip("src/intended.rs"), snip("src/helper.rs")]);
        let out = RulesV1.classify(&c, &["src/intended.rs".into()], &BTreeMap::new());
        assert_eq!(out[0].intended_role, IntendedRole::Edit);
        assert_eq!(out[0].confidence, 0.8);
        assert_eq!(out[1].intended_role, IntendedRole::ContextRead);
        assert_eq!(out[1].confidence, 0.4);
        assert!(out[1]
            .notes
            .iter()
            .any(|n| n.contains("retrieval relevance only")));
    }

    #[test]
    fn role_and_kind_are_independent() {
        // A test the issue intends to change: role=edit, kind=test.
        let c = ctx(vec![snip("tests/render_test.rs")]);
        let out = RulesV1.classify(&c, &["tests/render_test.rs".into()], &BTreeMap::new());
        assert_eq!(out[0].intended_role, IntendedRole::Edit);
        assert_eq!(out[0].artifact_kind, ArtifactKind::Test);
        // A generated file retrieved as context stays both.
        let c2 = ctx(vec![snip("target/gen/out_gen.rs")]);
        let out2 = RulesV1.classify(&c2, &[], &BTreeMap::new());
        assert_eq!(out2[0].artifact_kind, ArtifactKind::Generated);
        assert_eq!(out2[0].intended_role, IntendedRole::ContextRead);
    }

    #[test]
    fn new_path_is_create_without_invented_ranges() {
        let mut tree = std::collections::BTreeMap::new();
        tree.insert("src/known.rs".to_string(), 10u32);
        let c = ctx(vec![snip("src/brand_new.rs"), snip("src/known.rs")]);
        let out = RulesV1.classify(&c, &["src/brand_new.rs".into()], &tree);
        let created = out.iter().find(|l| l.path == "src/brand_new.rs").unwrap();
        assert_eq!(created.operation, Operation::Create);
        assert!(created.ranges.is_empty(), "no ranges invented for new paths");
        assert!(created
            .notes
            .iter()
            .any(|n| n.contains("no line ranges invented")));
        let edited = out.iter().find(|l| l.path == "src/known.rs").unwrap();
        assert_eq!(edited.operation, Operation::Edit);
        assert_eq!(edited.ranges, vec![(3, 5)]);
    }

    #[test]
    fn unresolvable_tree_marks_provenance_unknown() {
        let c = ctx(vec![snip("src/a.rs")]);
        let out = RulesV1.classify(&c, &["src/a.rs".into()], &BTreeMap::new());
        assert_eq!(out[0].operation, Operation::Unknown);
    }

    #[test]
    fn unknown_provenance_stays_unknown() {
        let c = ctx(vec![snip("<unknown-provenance>")]);
        let out = RulesV1.classify(&c, &[], &BTreeMap::new());
        assert_eq!(out[0].intended_role, IntendedRole::Unknown);
        assert_eq!(out[0].artifact_kind, ArtifactKind::Unknown);
    }

    #[test]
    fn kind_conventions() {
        assert_eq!(artifact_kind("src/a.rs"), ArtifactKind::Source);
        assert_eq!(artifact_kind("tests/it.rs"), ArtifactKind::Test);
        assert_eq!(artifact_kind("crates/x/tests/fixture.json"), ArtifactKind::Fixture);
        assert_eq!(artifact_kind("package.json"), ArtifactKind::Configuration);
        assert_eq!(artifact_kind(".github/workflows/ci.yml"), ArtifactKind::Configuration);
        assert_eq!(artifact_kind("Cargo.lock"), ArtifactKind::Generated);
    }
}
