//! Role-tick story join (Issue #9168): forge classification, the PR → issue
//! rule, deterministic span identity, and journal idempotence.

use super::*;
use crate::telemetry::trace::{story_context, TraceContext};
use chrono::TimeZone as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

const REPO_ID: u64 = 1_073_994_527; // rjwalters/loom in the D32 vectors
const FOREIGN_REPO_ID: u64 = 1_377_976_597;

/// A distinct repository name per test: the classification cache is
/// process-wide, so tests must not share keys.
fn identity(name: &str) -> RepoIdentity {
    RepoIdentity {
        id: REPO_ID,
        full_name: format!("rjwalters/{name}"),
    }
}

fn facts(role: &str) -> TickFacts {
    let started_at = Utc.timestamp_opt(1_800_000_000, 123).unwrap();
    let execution = crate::observability::lifecycle::role_execution_id(role, started_at);
    TickFacts {
        trace: RoleTrace {
            context: TraceContext::derived("execution", &["rjwalters/loom", &execution]),
            execution,
            started_at,
        },
        role: role.to_string(),
        ended_at: started_at + chrono::Duration::seconds(90),
        result: "success".to_string(),
        runtime: Some("claude".to_string()),
        model: Some("claude-sonnet-5".to_string()),
    }
}

fn pr(refs: &[(u64, u32)], head: Option<&str>) -> Resolved {
    Resolved::PullRequest {
        refs: ClosingRefs {
            refs: refs.to_vec(),
            total: refs.len(),
        },
        head_branch: head.map(str::to_owned),
    }
}

/// A GraphQL fake answering `issueOrPullRequest` from a fixed table.
struct Forge {
    answers: HashMap<u32, Value>,
    calls: AtomicUsize,
}

impl Forge {
    fn new(answers: &[(u32, Value)]) -> Self {
        Self {
            answers: answers.iter().cloned().collect(),
            calls: AtomicUsize::new(0),
        }
    }
}

impl GithubApi for Forge {
    fn get(&self, path: &str, _: Option<&str>) -> Result<ApiResponse, ApiError> {
        Err(ApiError::Transport(format!("unexpected GET {path}")))
    }
    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError> {
        self.get(path, None)
    }
    fn graphql(&self, query: &str) -> Result<ApiResponse, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut repository = serde_json::Map::new();
        for (number, answer) in &self.answers {
            if query.contains(&format!("t{number}: issueOrPullRequest(number: {number})")) {
                repository.insert(format!("t{number}"), answer.clone());
            }
        }
        Ok(ApiResponse {
            status: 200,
            body: serde_json::json!({"data": {"repository": repository}}).to_string(),
            ..ApiResponse::default()
        })
    }
}

fn issue_node() -> Value {
    serde_json::json!({"__typename": "Issue"})
}

fn pr_node(refs: &[(u64, u32)], head: &str) -> Value {
    let nodes: Vec<Value> = refs
        .iter()
        .map(|(repo, n)| serde_json::json!({"number": n, "repository": {"databaseId": repo}}))
        .collect();
    serde_json::json!({
        "__typename": "PullRequest",
        "headRefName": head,
        "closingIssuesReferences": {"totalCount": nodes.len(), "nodes": nodes},
    })
}

#[test]
fn classify_query_aliases_every_number() {
    let query = classify_query("rjwalters", "loom", &[7, 9]);
    assert!(query.starts_with("query { repository(owner: \"rjwalters\", name: \"loom\")"));
    assert!(query.contains("t7: issueOrPullRequest(number: 7)"));
    assert!(query.contains("t9: issueOrPullRequest(number: 9)"));
    assert!(query.contains("closingIssuesReferences(first: 10)"));
    assert!(query.contains("headRefName"));
}

#[test]
fn parse_classified_reads_issues_and_prs_and_drops_the_unreadable() {
    let body = serde_json::json!({"data": {"repository": {
        "t1": issue_node(),
        "t2": pr_node(&[(REPO_ID, 1)], "feature/issue-1"),
        "t3": null,
        "t4": {"__typename": "PullRequest", "closingIssuesReferences": {"nodes": []}},
    }}})
    .to_string();
    let parsed = parse_classified(&body, &[1, 2, 3, 4, 5]);
    assert_eq!(parsed.get(&1), Some(&Resolved::Issue));
    assert_eq!(parsed.get(&2), Some(&pr(&[(REPO_ID, 1)], Some("feature/issue-1"))));
    assert_eq!(parsed.len(), 2, "null, malformed and absent numbers are unreadable");
    assert!(parse_classified("not json", &[1]).is_empty());
}

#[test]
fn an_issue_target_is_its_own_story() {
    let id = identity("issue-story");
    let Stitch::Stitched(story) = story_of(&id, 42, Some(&Resolved::Issue)) else {
        panic!("an issue always resolves");
    };
    assert_eq!((story.issue, story.pr_number), (42, None));
    assert_eq!(story.root, story_context(REPO_ID, 42).unwrap());
    assert_eq!(story.story, "rjwalters/issue-story#42");
}

#[test]
fn a_pr_target_resolves_to_its_closing_issue() {
    let id = identity("pr-story");
    let Stitch::Stitched(story) = story_of(&id, 77, Some(&pr(&[(REPO_ID, 42)], Some("fix-x"))))
    else {
        panic!("exactly one same-repo closing reference stitches");
    };
    assert_eq!((story.issue, story.pr_number), (42, Some(77)));
    assert_eq!(story.root, story_context(REPO_ID, 42).unwrap());
    // The head-branch fallback alone is enough, and agrees with the refs.
    for resolved in [
        pr(&[], Some("feature/issue-42")),
        pr(&[(REPO_ID, 42)], Some("feature/issue-42")),
    ] {
        assert!(matches!(story_of(&id, 77, Some(&resolved)), Stitch::Stitched(s) if s.issue == 42));
    }
}

#[test]
fn an_ambiguous_or_unresolvable_pr_joins_no_story() {
    let id = identity("pr-ambiguous");
    for (resolved, why) in [
        (Some(pr(&[(REPO_ID, 1), (REPO_ID, 2)], None)), "two closing issues"),
        (Some(pr(&[(REPO_ID, 1)], Some("feature/issue-2"))), "refs and branch disagree"),
        (Some(pr(&[(FOREIGN_REPO_ID, 1)], None)), "closes a foreign issue only"),
        (
            Some(pr(&[(REPO_ID, 1), (FOREIGN_REPO_ID, 5)], None)),
            "also closes a foreign one",
        ),
        (Some(pr(&[], Some("main"))), "no candidate at all"),
        (None, "the forge could not classify it"),
    ] {
        assert!(!matches!(story_of(&id, 77, resolved.as_ref()), Stitch::Stitched(_)), "{why}");
    }
    let truncated = Resolved::PullRequest {
        refs: ClosingRefs {
            refs: vec![(REPO_ID, 1)],
            total: 11,
        },
        head_branch: None,
    };
    assert!(!matches!(story_of(&id, 77, Some(&truncated)), Stitch::Stitched(_)));
}

/// The acceptance case: a Judge tick labelling PR #M (closing #N) yields a
/// `loom.role_attempt` with `loom.role=judge` inside `story_context(repo_id, N)`
/// — driven from the recorded transcript through the real scanner.
#[test]
fn a_judge_tick_labelling_a_pr_joins_the_closing_issues_story() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/role_tick_targets/judge_single.jsonl");
    let (_, targets) =
        crate::role_tick_telemetry::scan_transcripts_with_targets(&[fixture]).unwrap();
    let id = identity("judge-accept");
    let (numbers, _) = own_repo_numbers(&targets, &id.full_name);
    assert_eq!(numbers, vec![9201]);
    let forge = Forge::new(&[(9201, pr_node(&[(REPO_ID, 9168)], "feature/issue-9168"))]);
    let resolved = resolve(&forge, &id, &numbers);
    let facts = facts("judge");
    let spans = plan(&facts, &id, &id.full_name, &numbers, &resolved);
    assert_eq!(spans.len(), 1);
    let span = &spans[0];
    let story = story_context(REPO_ID, 9168).unwrap();
    assert_eq!(span.name, SpanName::RoleAttempt);
    assert_eq!(span.context.trace_id, story.trace_id);
    assert_eq!(span.parent_span_id.as_ref(), Some(&story.span_id));
    let attr = |k: &str| span.attributes.get(k).map(String::as_str);
    assert_eq!(attr("loom.role"), Some("judge"));
    assert_eq!(attr("loom.issue"), Some("9168"));
    assert_eq!(attr("loom.pr_number"), Some("9201"));
    assert_eq!(attr("loom.story"), Some("rjwalters/judge-accept#9168"));
    assert_eq!(attr("loom.story.key_version"), Some("v1"));
    assert_eq!(attr("loom.timing_source"), Some("tick"));
    assert_eq!(attr("loom.result"), Some("success"));
    assert_eq!(attr("loom.runtime"), Some("claude"));
    assert_eq!(attr("loom.model"), Some("claude-sonnet-5"));
    assert_eq!(attr("loom.sweep_id"), Some(facts.trace.execution.as_str()));
    assert!(attr(crate::telemetry::trace::provenance::DAEMON_VERSION).is_some());
    assert_eq!(span.status, SpanStatus::Ok);
    assert_eq!((span.started_at, span.ended_at), (facts.trace.started_at, facts.ended_at));
    assert_eq!(span.links.len(), 1);
    assert_eq!(span.links[0].context, facts.trace.context, "links to the tick's own root");
}

#[test]
fn span_ids_are_deterministic_and_recomputable_from_the_span() {
    let id = identity("determinism");
    let resolved = HashMap::from([(7, pr(&[(REPO_ID, 3)], None)), (3, Resolved::Issue)]);
    let facts = facts("champion");
    let first = plan(&facts, &id, &id.full_name, &[3, 7], &resolved);
    let again = plan(&facts, &id, &id.full_name, &[3, 7], &resolved);
    assert_eq!(first, again, "a re-run over the same tick emits identical spans");
    assert_eq!(first.len(), 2);
    // Two targets in one story are distinct siblings.
    assert_ne!(first[0].context.span_id, first[1].context.span_id);
    // Every derivation input is on the span itself.
    for span in &first {
        let root = TraceContext {
            trace_id: span.context.trace_id.clone(),
            span_id: span.parent_span_id.clone().unwrap(),
            flags: 1,
        };
        let target = match span.attributes.get("loom.pr_number") {
            Some(pr) => format!("pr:{pr}"),
            None => format!("issue:{}", span.attributes["loom.issue"]),
        };
        let expected =
            root.derived_child(&["loom.role_tick", &span.attributes["loom.sweep_id"], &target]);
        assert_eq!(span.context, expected);
    }
    // A later tick of the same role is a different span in the same story.
    let mut later = facts.clone();
    later.trace.execution = "role-champion-2027-01-15T08:00:00.000000000Z".into();
    let other = plan(&later, &id, &id.full_name, &[3], &resolved);
    assert_eq!(other[0].context.trace_id, first[0].context.trace_id);
    assert_ne!(other[0].context.span_id, first[0].context.span_id);
}

#[test]
fn resolve_batches_once_and_caches_answers() {
    let id = identity("cache");
    let forge = Forge::new(&[(1, issue_node()), (2, pr_node(&[(REPO_ID, 1)], "x"))]);
    let first = resolve(&forge, &id, &[1, 2, 3]);
    assert_eq!(forge.calls.load(Ordering::SeqCst), 1, "one request for all three");
    assert_eq!(first.len(), 2, "#3 is unknown to the forge");
    let second = resolve(&forge, &id, &[1, 2]);
    assert_eq!(forge.calls.load(Ordering::SeqCst), 1, "cached answers need no request");
    assert_eq!(first.get(&1), second.get(&1));
    // An unreadable number is not cached: asking again asks the forge.
    let _ = resolve(&forge, &id, &[3]);
    assert_eq!(forge.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_forge_failure_emits_nothing() {
    struct Down;
    impl GithubApi for Down {
        fn get(&self, p: &str, _: Option<&str>) -> Result<ApiResponse, ApiError> {
            Err(ApiError::Transport(p.into()))
        }
        fn get_document(&self, p: &str) -> Result<ApiResponse, ApiError> {
            Err(ApiError::Transport(p.into()))
        }
    }
    let id = identity("down");
    let resolved = resolve(&Down, &id, &[1]);
    assert!(resolved.is_empty());
    assert!(plan(&facts("judge"), &id, &id.full_name, &[1], &resolved).is_empty());
}

#[test]
fn journalling_is_idempotent_per_span_id() {
    let dir = tempfile::tempdir().unwrap();
    let id = identity("journal");
    let facts = facts("judge");
    let resolved = HashMap::from([(5, Resolved::Issue)]);
    let spans = plan(&facts, &id, &id.full_name, &[5], &resolved);
    assert_eq!(journal(dir.path(), &facts.trace.execution, spans.clone()).unwrap(), 1);
    assert_eq!(journal(dir.path(), &facts.trace.execution, spans.clone()).unwrap(), 0);
    let store = crate::telemetry::trace::store::TraceStore::new(dir.path());
    let journalled = crate::telemetry::trace::journal::Journal::for_context(
        &store.path(dir.path(), &facts.trace.execution),
    )
    .completed()
    .unwrap();
    assert_eq!(journalled, spans);
    assert_eq!(journal(dir.path(), &facts.trace.execution, Vec::new()).unwrap(), 0);
}

#[test]
fn every_story_attribute_survives_the_daemon_allowlist() {
    let span = story_span(
        &facts("doctor"),
        "rjwalters/loom",
        &StoryRef {
            repo_id: REPO_ID,
            story: "rjwalters/loom#1".into(),
            issue: 1,
            pr_number: Some(2),
            root: story_context(REPO_ID, 1).unwrap(),
        },
    );
    for key in STORY_SPAN_ATTRIBUTE_KEYS {
        assert!(span.attributes.contains_key(*key), "allowlist drops {key}");
    }
}
