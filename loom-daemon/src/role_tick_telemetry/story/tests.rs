//! Role-tick story join (Issue #9168): forge classification, the PR → issue
//! rule, deterministic span identity, and journal idempotence.

use super::*;
use crate::telemetry::trace::{story_context, TraceContext};
use chrono::TimeZone as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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
        tokens_by_model: None,
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
    let (numbers, _) = own_repo_numbers(&targets, &id.full_name, Path::new("/work/loom"));
    assert_eq!(numbers, vec![9201]);
    let forge = Forge::new(&[(9201, pr_node(&[(REPO_ID, 9168)], "feature/issue-9168"))]);
    let resolved = resolve(&forge, &id, &numbers, Instant::now());
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
    let now = Instant::now();
    let first = resolve(&forge, &id, &[1, 2, 3], now);
    assert_eq!(forge.calls.load(Ordering::SeqCst), 1, "one request for all three");
    assert_eq!(first.len(), 2, "#3 is unknown to the forge");
    let second = resolve(&forge, &id, &[1, 2], now);
    assert_eq!(forge.calls.load(Ordering::SeqCst), 1, "cached answers need no request");
    assert_eq!(first.get(&1), second.get(&1));
    // An unreadable number is not cached: asking again asks the forge.
    let _ = resolve(&forge, &id, &[3], now);
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
    let resolved = resolve(&Down, &id, &[1], Instant::now());
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

// ------------------------------------------------------------------------
// #9180: one deadline for the whole step, and a negative cache.
// ------------------------------------------------------------------------

/// A clock that only moves when a test (or a fake forge) advances it.
fn fake_clock() -> (Clock, Arc<AtomicU64>) {
    let base = Instant::now();
    let offset_ms = Arc::new(AtomicU64::new(0));
    let reading = Arc::clone(&offset_ms);
    let clock: Clock =
        Arc::new(move || base + Duration::from_millis(reading.load(Ordering::SeqCst)));
    (clock, offset_ms)
}

/// A forge that answers every number as an issue, each request taking
/// `cost_ms` of the fake clock, counting requests.
struct Slow {
    clock: Arc<AtomicU64>,
    cost_ms: u64,
    calls: AtomicUsize,
}

impl GithubApi for Slow {
    fn get(&self, path: &str, _: Option<&str>) -> Result<ApiResponse, ApiError> {
        Err(ApiError::Transport(format!("unexpected GET {path}")))
    }
    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError> {
        self.get(path, None)
    }
    fn graphql(&self, query: &str) -> Result<ApiResponse, ApiError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.clock.fetch_add(self.cost_ms, Ordering::SeqCst);
        let mut repository = serde_json::Map::new();
        for alias in query
            .split(": issueOrPullRequest")
            .filter_map(|p| p.rsplit(' ').next())
        {
            if alias.starts_with('t') {
                repository.insert(alias.to_owned(), issue_node());
            }
        }
        Ok(ApiResponse {
            status: 200,
            body: serde_json::json!({"data": {"repository": repository}}).to_string(),
            ..ApiResponse::default()
        })
    }
}

#[test]
fn the_deadline_bounds_the_whole_step_not_each_request() {
    let (clock, offset) = fake_clock();
    // Each request costs 12s of the 20s budget — each within it on its own —
    // so two batches are answered and the third is never sent.
    let forge = Arc::new(Slow {
        clock: Arc::clone(&offset),
        cost_ms: 12_000,
        calls: AtomicUsize::new(0),
    });
    let deadline = Deadline::after(RESOLVE_DEADLINE, clock);
    let numbers: Vec<u32> = (1..=2 * u32::try_from(GRAPHQL_BATCH).unwrap() + 1).collect();
    let id = identity("deadline-batches");
    let seeded = id.clone();
    let (got, resolved) = resolve_step(
        "rjwalters/deadline-batches",
        &numbers,
        &deadline,
        move |_| Some(seeded),
        Arc::clone(&forge) as Arc<dyn GithubApi>,
    )
    .unwrap();
    assert_eq!(got, id);
    assert_eq!(forge.calls.load(Ordering::SeqCst), 2, "the third batch is past the deadline");
    assert_eq!(resolved.len(), 2 * GRAPHQL_BATCH);
    assert!(!resolved.contains_key(&numbers[2 * GRAPHQL_BATCH]));
}

#[test]
fn a_slow_identity_lookup_consumes_the_budget_and_nothing_is_requested() {
    // The identity lookup "takes" 21s of the fake clock: no GraphQL request.
    let (clock, offset) = fake_clock();
    let forge = Arc::new(Forge::new(&[(1, issue_node())]));
    let deadline = Deadline::after(RESOLVE_DEADLINE, clock);
    let id = identity("deadline-identity");
    let seeded = id.clone();
    let slow_identity = move |_: String| {
        offset.fetch_add(21_000, Ordering::SeqCst);
        Some(seeded)
    };
    let (_, resolved) = resolve_step(
        "rjwalters/deadline-identity",
        &[1],
        &deadline,
        slow_identity,
        Arc::clone(&forge) as Arc<dyn GithubApi>,
    )
    .unwrap();
    assert!(resolved.is_empty());
    assert_eq!(forge.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn a_hung_identity_lookup_is_cut_off_at_the_deadline() {
    // Real time with wide slack: a 50ms budget against a 500ms lookup (the
    // abandoned worker finishes on its own; the test does not wait for it).
    let deadline = Deadline::after(Duration::from_millis(50), Arc::new(Instant::now));
    let forge = Arc::new(Forge::new(&[]));
    let started = Instant::now();
    let step = resolve_step(
        "rjwalters/deadline-hung",
        &[1],
        &deadline,
        |_| {
            std::thread::sleep(Duration::from_millis(500));
            Some(identity("deadline-hung"))
        },
        Arc::clone(&forge) as Arc<dyn GithubApi>,
    );
    assert!(step.is_none());
    assert!(started.elapsed() < Duration::from_millis(400), "{:?}", started.elapsed());
    assert_eq!(forge.calls.load(Ordering::SeqCst), 0);
    // An already-expired deadline starts nothing at all.
    let (clock, offset) = fake_clock();
    let expired = Deadline::after(Duration::from_secs(1), clock);
    offset.store(1_000, Ordering::SeqCst);
    assert_eq!(expired.run(|| 1), None);
}

#[test]
fn a_failed_lookup_is_negatively_cached_for_the_backoff() {
    struct Down(AtomicUsize);
    impl GithubApi for Down {
        fn get(&self, p: &str, _: Option<&str>) -> Result<ApiResponse, ApiError> {
            Err(ApiError::Transport(p.into()))
        }
        fn get_document(&self, p: &str) -> Result<ApiResponse, ApiError> {
            Err(ApiError::Transport(p.into()))
        }
        fn graphql(&self, _: &str) -> Result<ApiResponse, ApiError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(ApiResponse {
                status: 200,
                body: r#"{"errors":[{"type":"RATE_LIMITED"}]}"#.into(),
                ..ApiResponse::default()
            })
        }
    }
    let down = Down(AtomicUsize::new(0));
    let id = identity("negative-cache");
    let now = Instant::now();
    assert!(resolve(&down, &id, &[1], now).is_empty());
    assert_eq!(down.0.load(Ordering::SeqCst), 1);
    // Every tick inside the backoff asks nothing.
    for later in [1, 30, 59] {
        assert!(resolve(&down, &id, &[1, 2], now + Duration::from_secs(later)).is_empty());
    }
    assert_eq!(down.0.load(Ordering::SeqCst), 1, "no request while backed off");
    // Past it, the forge is asked again; a success clears the backoff.
    let _ = resolve(&down, &id, &[1], now + FAILURE_BACKOFF + Duration::from_secs(1));
    assert_eq!(down.0.load(Ordering::SeqCst), 2);
    let forge = Forge::new(&[(1, issue_node())]);
    let after = now + 2 * FAILURE_BACKOFF + Duration::from_secs(2);
    assert_eq!(resolve(&forge, &id, &[1], after).len(), 1);
    assert_eq!(resolve(&forge, &id, &[2], after).len(), 0);
    assert_eq!(forge.calls.load(Ordering::SeqCst), 2, "a success does not back off");
    // Cached answers still answer during a backoff; the rest waits.
    let _ = resolve(&down, &id, &[3], after);
    assert_eq!(down.0.load(Ordering::SeqCst), 3);
    let during = resolve(&down, &id, &[1, 3], after + Duration::from_secs(1));
    assert_eq!(during.keys().copied().collect::<Vec<_>>(), vec![1]);
    assert_eq!(down.0.load(Ordering::SeqCst), 3);
}

#[test]
fn a_failure_backs_off_from_when_it_was_observed() {
    // The only request fails 20s into the step: the backoff runs 60s from
    // there, not from the step's start.
    struct LateFailure(Arc<AtomicU64>, AtomicUsize);
    impl GithubApi for LateFailure {
        fn get(&self, p: &str, _: Option<&str>) -> Result<ApiResponse, ApiError> {
            Err(ApiError::Transport(p.into()))
        }
        fn get_document(&self, p: &str) -> Result<ApiResponse, ApiError> {
            Err(ApiError::Transport(p.into()))
        }
        fn graphql(&self, _: &str) -> Result<ApiResponse, ApiError> {
            self.1.fetch_add(1, Ordering::SeqCst);
            self.0.fetch_add(20_000, Ordering::SeqCst);
            Err(ApiError::Transport("gateway timeout".into()))
        }
    }
    let (clock, offset) = fake_clock();
    let start = clock();
    let api = Arc::new(LateFailure(Arc::clone(&offset), AtomicUsize::new(0)));
    let deadline = Deadline::after(Duration::from_secs(30), clock);
    let id = identity("late-failure");
    let seeded = id.clone();
    let (_, resolved) = resolve_step(
        "rjwalters/late-failure",
        &[1],
        &deadline,
        move |_| Some(seeded),
        Arc::clone(&api) as Arc<dyn GithubApi>,
    )
    .unwrap();
    assert!(resolved.is_empty());
    assert_eq!(api.1.load(Ordering::SeqCst), 1);
    // 79s after the start is 59s after the failure: still backed off.
    let _ = resolve(api.as_ref(), &id, &[1], start + Duration::from_secs(79));
    assert_eq!(api.1.load(Ordering::SeqCst), 1);
    let _ = resolve(api.as_ref(), &id, &[1], start + Duration::from_secs(81));
    assert_eq!(api.1.load(Ordering::SeqCst), 2);
}
