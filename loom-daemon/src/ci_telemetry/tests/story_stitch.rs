//! Story stitching of CI runs (Issue #9088, phase 2 of #9037).
//!
//! Every expected story id comes from the D32 v1 cross-language vectors
//! (`tests/fixtures/story_vectors_d32_v1.json`, #9068): the fixture repos are
//! given the vectors' `repo_id`s, so a stitched span's trace and parent are
//! checked against the published values, not against our own derivation.

use super::*;
use crate::ci_telemetry::records::{job_context, run_context, PullRequestRefJson, RunJson};
use crate::ci_telemetry::story::{
    self, closing_refs_query, decide, parse_closing_refs, story_job_span_id, story_run_span_id,
    ClosingRefs, Stitch,
};
use crate::telemetry::repo_identity::RepoIdentity;
use crate::telemetry::trace::{SpanName, SpanRecord};

const D32_VECTORS: &str = include_str!("../../../tests/fixtures/story_vectors_d32_v1.json");

/// `(repo_id, number, trace_id, root_span)` of the vector named `story`.
fn vector(story: &str) -> (u64, u32, String, String) {
    let fixture: Value = serde_json::from_str(D32_VECTORS).unwrap();
    let v = fixture["vectors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["story"] == story)
        .unwrap();
    (
        v["repo_id"].as_u64().unwrap(),
        u32::try_from(v["number"].as_u64().unwrap()).unwrap(),
        v["trace_id"].as_str().unwrap().to_string(),
        v["root_span"].as_str().unwrap().to_string(),
    )
}

const LOOM_REPO_ID: u64 = 1_073_994_527; // rjwalters/loom in the vectors
const HARNESS_OPS_REPO_ID: u64 = 1_377_976_597; // 2AMLogic/harness-ops

/// alpha and beta carry the vectors' repo ids; anything else is unresolvable.
fn fixture_identity(name: &str) -> Option<RepoIdentity> {
    let id = match name {
        "fixture-org/alpha" => LOOM_REPO_ID,
        "fixture-org/beta" => HARNESS_OPS_REPO_ID,
        _ => return None,
    };
    Some(RepoIdentity {
        id,
        full_name: name.to_string(),
    })
}

fn no_identity(_: &str) -> Option<RepoIdentity> {
    None
}

fn stitching(root: &Path) -> CycleContext<'_> {
    CycleContext {
        repo_identity: Some(fixture_identity),
        ..ctx(root)
    }
}

/// The fixture API plus a scripted GraphQL endpoint that records its queries.
struct StoryApi {
    inner: FixtureApi,
    /// `p<N>` → closing references `[(repo databaseId, number)]`.
    closes: BTreeMap<u64, Vec<(u64, u32)>>,
    graphql_calls: Mutex<Vec<String>>,
    graphql_refuses: bool,
}

impl StoryApi {
    fn new() -> Self {
        StoryApi {
            inner: FixtureApi::new(),
            closes: BTreeMap::new(),
            graphql_calls: Mutex::new(Vec::new()),
            graphql_refuses: false,
        }
    }

    fn closes(mut self, pr: u64, refs: &[(u64, u32)]) -> Self {
        self.closes.insert(pr, refs.to_vec());
        self
    }

    /// Edit run `run_id`'s listing row in `repo` (`alpha` / `beta`).
    fn run(self, repo: &str, run_id: u64, edit: impl Fn(&mut Value)) -> Self {
        for key in [
            format!("repos/fixture-org/{repo}/actions/runs?per_page=100"),
            format!("repos/fixture-org/{repo}/actions/runs?per_page=100&page=2"),
        ] {
            if self.inner.responses.lock().unwrap().contains_key(&key) {
                self.inner.edit(&key, |page| {
                    for row in page["body"]["workflow_runs"].as_array_mut().unwrap() {
                        if row["id"] == run_id {
                            edit(row);
                        }
                    }
                });
            }
        }
        self
    }

    fn graphql_calls(&self) -> Vec<String> {
        self.graphql_calls.lock().unwrap().clone()
    }
}

impl GithubApi for StoryApi {
    fn get(&self, path: &str, etag: Option<&str>) -> Result<ApiResponse, ApiError> {
        self.inner.get(path, etag)
    }

    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError> {
        self.inner.get_document(path)
    }

    fn graphql(&self, query: &str) -> Result<ApiResponse, ApiError> {
        self.graphql_calls.lock().unwrap().push(query.to_string());
        if self.graphql_refuses {
            return Err(ApiError::Transport("synthetic GraphQL outage".into()));
        }
        let mut repository = serde_json::Map::new();
        for (pr, refs) in &self.closes {
            if query.contains(&format!("p{pr}:")) {
                let nodes: Vec<Value> = refs
                    .iter()
                    .map(|(repo, n)| {
                        serde_json::json!({"number": n, "repository": {"databaseId": repo}})
                    })
                    .collect();
                repository.insert(
                    format!("p{pr}"),
                    serde_json::json!({"closingIssuesReferences": {
                        "totalCount": nodes.len(), "nodes": nodes}}),
                );
            }
        }
        Ok(ApiResponse {
            status: 200,
            body: serde_json::json!({"data": {"repository": repository}}).to_string(),
            ..ApiResponse::default()
        })
    }
}

fn spans(root: &Path) -> Vec<SpanRecord> {
    journal(root)
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(span) => Some(span),
            _ => None,
        })
        .collect()
}

fn story_spans(root: &Path) -> Vec<SpanRecord> {
    spans(root)
        .into_iter()
        .filter(|span| span.attributes.contains_key("loom.story"))
        .collect()
}

fn set_branch(branch: &'static str) -> impl Fn(&mut Value) {
    move |row| row["head_branch"] = Value::from(branch)
}

fn set_prs(prs: &'static [u64]) -> impl Fn(&mut Value) {
    move |row| {
        row["pull_requests"] = prs
            .iter()
            .map(|n| serde_json::json!({"number": n}))
            .collect();
    }
}

// ---------------------------------------------------------------------------
// AC1: a `feature/issue-N` run (and a PR closing exactly #N) joins N's story
// ---------------------------------------------------------------------------

#[test]
fn a_feature_issue_branch_run_joins_the_story_trace_under_the_root() {
    let (repo_id, _, trace, root) = vector("rjwalters/loom#9027");
    assert_eq!(repo_id, LOOM_REPO_ID);
    let dir = TempDir::new().unwrap();
    let api = StoryApi::new().run("alpha", 1003, set_branch("feature/issue-9027"));
    let report = run_cycle(&stitching(dir.path()), &api).unwrap();
    assert!(api.graphql_calls().is_empty(), "no PR, so no GraphQL request");
    assert_eq!(report.summary.story_runs_stitched, 1, "{}", report.summary());
    assert_eq!(report.summary.story_runs_no_candidate, 5);

    let story = story_spans(dir.path());
    let run_span = story.iter().find(|s| s.name == SpanName::CiRun).unwrap();
    assert_eq!(run_span.context.trace_id.as_str(), trace);
    assert_eq!(run_span.parent_span_id.as_ref().unwrap().as_str(), root);
    assert_eq!(run_span.context.span_id, story_run_span_id(repo_id, 1003, 1));
    assert_eq!(run_span.attributes["loom.story"], "fixture-org/alpha#9027");
    assert_eq!(run_span.attributes["loom.story.key_version"], "v1");
    assert_eq!(run_span.attributes["loom.issue"], "9027");
    assert_eq!(run_span.attributes["loom.ci.run_id"], "1003");
    assert!(
        !run_span.attributes.contains_key("loom.pr_number"),
        "a branch-derived issue number is never carried as a PR number in the story"
    );

    let jobs: Vec<_> = story.iter().filter(|s| s.name == SpanName::CiJob).collect();
    assert_eq!(jobs.len(), 4);
    for job in jobs {
        assert_eq!(job.context.trace_id.as_str(), trace);
        assert_eq!(job.parent_span_id.as_ref(), Some(&run_span.context.span_id));
        let job_id: u64 = job.attributes["loom.ci.job_id"].parse().unwrap();
        assert_eq!(job.context.span_id, story_job_span_id(repo_id, job_id));
    }
    assert_no_duplicates(dir.path());
}

#[test]
fn a_run_whose_pr_closes_exactly_one_issue_joins_that_story_with_its_pr_number() {
    let (repo_id, issue, trace, root) = vector("2AMLogic/harness-ops#307");
    let dir = TempDir::new().unwrap();
    // Branch name says nothing; the PR's closing reference decides.
    let api = StoryApi::new()
        .closes(90_881, &[(repo_id, issue)])
        .run("beta", 2001, set_branch("renamed-branch"))
        .run("beta", 2001, set_prs(&[90_881]));
    let report = run_cycle(&stitching(dir.path()), &api).unwrap();
    assert_eq!(report.summary.story_runs_stitched, 1, "{}", report.summary());
    let run_span = story_spans(dir.path())
        .into_iter()
        .find(|s| s.name == SpanName::CiRun)
        .unwrap();
    assert_eq!(run_span.context.trace_id.as_str(), trace);
    assert_eq!(run_span.parent_span_id.as_ref().unwrap().as_str(), root);
    assert_eq!(run_span.attributes["loom.issue"], "307");
    assert_eq!(run_span.attributes["loom.pr_number"], "90881");
}

// ---------------------------------------------------------------------------
// AC2: re-polling is idempotent
// ---------------------------------------------------------------------------

#[test]
fn re_deriving_a_run_yields_identical_story_span_ids() {
    let ids = |dir: &Path| -> BTreeSet<String> {
        let api = StoryApi::new().run("alpha", 1002, set_branch("feature/issue-9027"));
        run_cycle(&stitching(dir), &api).unwrap();
        story_spans(dir)
            .iter()
            .map(|s| format!("{}/{}", s.context.trace_id.as_str(), s.context.span_id.as_str()))
            .collect()
    };
    let (first, second) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let a = ids(first.path());
    assert_eq!(a.len(), 5, "one run span + four job spans");
    assert_eq!(a, ids(second.path()), "a second host / replay derives the same ids");

    // And a re-poll over the same ledger emits nothing new at all.
    let before = journal(first.path()).len();
    let api = StoryApi::new().run("alpha", 1002, set_branch("feature/issue-9027"));
    let again = run_cycle(&stitching(first.path()), &api).unwrap();
    assert_eq!(again.summary.story_runs_stitched, 0);
    assert_eq!(journal(first.path()).len(), before);
}

// ---------------------------------------------------------------------------
// AC3: ambiguous or unresolvable runs are not stitched
// ---------------------------------------------------------------------------

#[test]
fn a_run_with_several_candidate_issues_creates_no_story_span() {
    let dir = TempDir::new().unwrap();
    // The branch says #9027 but the PR closes #5: two candidates.
    let api = StoryApi::new()
        .closes(90_882, &[(LOOM_REPO_ID, 5)])
        .run("alpha", 1003, set_branch("feature/issue-9027"))
        .run("alpha", 1003, set_prs(&[90_882]));
    let report = run_cycle(&stitching(dir.path()), &api).unwrap();
    assert_eq!(report.summary.story_runs_ambiguous, 1, "{}", report.summary());
    assert_eq!(report.summary.story_runs_stitched, 0);
    assert!(story_spans(dir.path()).is_empty());
}

#[test]
fn an_unresolvable_repo_id_creates_no_story_span_and_no_graphql_request() {
    let dir = TempDir::new().unwrap();
    let api = StoryApi::new()
        .run("alpha", 1003, set_branch("feature/issue-1"))
        .run("alpha", 1002, set_prs(&[90_885]));
    let ctx = CycleContext {
        repo_identity: Some(no_identity),
        ..ctx(dir.path())
    };
    let report = run_cycle(&ctx, &api).unwrap();
    assert_eq!(report.summary.story_runs_unresolved, 2, "{}", report.summary());
    assert!(story_spans(dir.path()).is_empty(), "no name-derived fallback");
    assert!(api.graphql_calls().is_empty());
}

#[test]
fn unreadable_closing_references_leave_the_run_unstitched() {
    let dir = TempDir::new().unwrap();
    let mut api = StoryApi::new()
        .run("alpha", 1003, set_branch("feature/issue-9027"))
        .run("alpha", 1003, set_prs(&[90_886]));
    api.graphql_refuses = true;
    let report = run_cycle(&stitching(dir.path()), &api).unwrap();
    // The branch alone would say #9027, but the PR's references are unknown:
    // not stitched rather than guessed from the weaker source.
    assert_eq!(report.summary.story_runs_unresolved, 1, "{}", report.summary());
    assert!(story_spans(dir.path()).is_empty());
    assert!(report.repo_errors.is_empty(), "a lookup failure never fails the repo");
}

#[test]
fn stitching_off_emits_exactly_the_pre_9088_records() {
    let (on, off) = (TempDir::new().unwrap(), TempDir::new().unwrap());
    let api = || StoryApi::new().run("alpha", 1003, set_branch("feature/issue-9027"));
    run_cycle(&ctx(off.path()), &api()).unwrap();
    run_cycle(&stitching(on.path()), &api()).unwrap();
    assert!(story_spans(off.path()).is_empty());
    assert!(spans(off.path()).iter().all(|s| s.links.is_empty()));
    assert_eq!(spans(on.path()).len(), spans(off.path()).len() + 5);
}

// ---------------------------------------------------------------------------
// AC4: per-run CI trace ids unchanged; links both ways
// ---------------------------------------------------------------------------

#[test]
fn per_run_spans_keep_their_ids_and_link_both_ways_with_the_story_copy() {
    let dir = TempDir::new().unwrap();
    let api = StoryApi::new().run("alpha", 1003, set_branch("feature/issue-9027"));
    run_cycle(&stitching(dir.path()), &api).unwrap();
    let all = spans(dir.path());
    let per_run = all
        .iter()
        .find(|s| {
            s.name == SpanName::CiRun && s.context == run_context("fixture-org/alpha", 1003, 1)
        })
        .expect("the per-run root keeps its derived (repo, run_id, attempt) ids");
    assert!(per_run.parent_span_id.is_none());
    let story_run = all
        .iter()
        .find(|s| s.name == SpanName::CiRun && s.attributes.contains_key("loom.story"))
        .unwrap();
    assert_eq!(per_run.links.len(), 1);
    assert_eq!(per_run.links[0].context, story_run.context);
    assert_eq!(story_run.links.len(), 1);
    assert_eq!(story_run.links[0].context, per_run.context);

    for job_id in [10031_u64, 10032, 10033, 10034] {
        let per_job_ctx = job_context("fixture-org/alpha", 1003, 1, job_id);
        let per_job = all.iter().find(|s| s.context == per_job_ctx).unwrap();
        assert_eq!(per_job.parent_span_id.as_ref(), Some(&per_run.context.span_id));
        let story_job = all
            .iter()
            .find(|s| s.context.span_id == story_job_span_id(LOOM_REPO_ID, job_id))
            .unwrap();
        assert_eq!(per_job.links[0].context, story_job.context);
        assert_eq!(story_job.links[0].context, per_job.context);
    }
}

// ---------------------------------------------------------------------------
// API budget and attribute allowlist
// ---------------------------------------------------------------------------

#[test]
fn closing_references_are_batched_into_one_request_per_repo_and_cached() {
    let (repo_id, issue, ..) = vector("2AMLogic/harness-ops#1");
    let dir = TempDir::new().unwrap();
    let api = StoryApi::new()
        .closes(90_883, &[(repo_id, issue)])
        .closes(90_884, &[(repo_id, issue)])
        .run("beta", 2001, set_prs(&[90_883]))
        .run("beta", 2002, set_prs(&[90_884]))
        .run("beta", 2003, set_prs(&[90_883]));
    let report = run_cycle(&stitching(dir.path()), &api).unwrap();
    assert_eq!(report.summary.story_runs_stitched, 3, "{}", report.summary());
    let calls = api.graphql_calls();
    assert_eq!(calls.len(), 1, "one batched request: {calls:?}");
    assert!(calls[0].contains("p90883:") && calls[0].contains("p90884:"));

    // The same PRs in a later cycle are served from the per-PR cache.
    let second =
        story::closing_refs(&api, "fixture-org/beta", &[90_883, 90_884], &mut 0, &mut true);
    assert_eq!(second.len(), 2);
    assert_eq!(api.graphql_calls().len(), 1);
}

#[test]
fn story_span_attributes_are_all_inside_the_telemetry_allowlist() {
    let dir = TempDir::new().unwrap();
    let api = StoryApi::new().closes(90_887, &[(LOOM_REPO_ID, 9027)]).run(
        "alpha",
        1003,
        set_prs(&[90_887]),
    );
    run_cycle(&stitching(dir.path()), &api).unwrap();
    let span_keep = keep_keys("span");
    let story = story_spans(dir.path());
    assert_eq!(story.len(), 5);
    for span in story {
        assert_eq!(span.clone().bounded().attributes, span.attributes, "daemon allowlist");
        for key in span.attributes.keys() {
            assert!(span_keep.contains(key), "collector span keep_keys lacks {key}");
        }
    }
}

// ---------------------------------------------------------------------------
// The pure decision and the GraphQL shapes
// ---------------------------------------------------------------------------

fn run_with(branch: &str, prs: &[u64]) -> RunJson {
    let mut run: RunJson = serde_json::from_value(serde_json::json!({
        "id": 42, "head_branch": branch, "status": "completed",
        "created_at": "2026-09-20T09:00:00Z", "updated_at": "2026-09-20T09:01:00Z",
    }))
    .unwrap();
    run.pull_requests = prs
        .iter()
        .map(|n| PullRequestRefJson { number: *n })
        .collect();
    run
}

#[test]
fn decide_stitches_only_exactly_one_same_repo_issue() {
    let identity = fixture_identity("fixture-org/alpha");
    let refs = |list: &[(u64, u32)], total: usize| {
        Some(ClosingRefs {
            refs: list.to_vec(),
            total,
        })
    };
    let closing: HashMap<u64, Option<ClosingRefs>> = [
        (1, refs(&[(LOOM_REPO_ID, 9027)], 1)),
        (2, refs(&[(LOOM_REPO_ID, 9027), (LOOM_REPO_ID, 9028)], 2)),
        (3, refs(&[(HARNESS_OPS_REPO_ID, 307)], 1)),
        (4, refs(&[], 0)),
        (5, refs(&[(LOOM_REPO_ID, 9027)], 11)),
        (6, None),
        (7, refs(&[(HARNESS_OPS_REPO_ID, 1), (HARNESS_OPS_REPO_ID, 2)], 2)),
    ]
    .into_iter()
    .collect();
    let d = |branch: &str, prs: &[u64]| decide(&run_with(branch, prs), identity.as_ref(), &closing);

    let Stitch::Stitched(story) = d("main", &[1]) else {
        panic!("one closing reference stitches");
    };
    let (_, _, trace, root) = vector("rjwalters/loom#9027");
    assert_eq!((story.root.trace_id.as_str(), story.root.span_id.as_str()), (&*trace, &*root));
    assert_eq!(story.pr_number, Some(1));
    // The branch agreeing with the PR is still one candidate.
    assert!(matches!(d("feature/issue-9027", &[1]), Stitch::Stitched(_)));
    // A PR closing nothing falls back to the branch.
    assert!(
        matches!(d("feature/issue-9027", &[4]), Stitch::Stitched(ref s) if s.pr_number == Some(4))
    );
    // A truncated reference list is ambiguous whatever it holds.
    assert!(matches!(d("main", &[1, 5]), Stitch::Ambiguous(_)), "truncated");

    assert_eq!(d("main", &[]), Stitch::NoCandidate);
    assert_eq!(d("main", &[4]), Stitch::NoCandidate);
    assert_eq!(d("main", &[2]), Stitch::Ambiguous(vec![9027, 9028]));
    assert_eq!(d("feature/issue-1", &[1]), Stitch::Ambiguous(vec![1, 9027]));
    assert!(matches!(d("main", &[3]), Stitch::Unresolved(_)), "cross-repo reference");
    assert!(
        matches!(d("main", &[7]), Stitch::Unresolved(_)),
        "several cross-repo references are unresolved, not ambiguous: there is no candidate \
         in this repository to choose between"
    );
    assert!(matches!(d("feature/issue-9027", &[6]), Stitch::Unresolved(_)));
    assert!(matches!(
        decide(&run_with("feature/issue-9027", &[]), None, &closing),
        Stitch::Unresolved(_)
    ));
    // Deterministic: the same inputs, the same answer.
    assert_eq!(d("main", &[1]), d("main", &[1]));
}

#[test]
fn closing_refs_query_and_parse_round_trip() {
    let query = closing_refs_query("rjwalters", "loom", &[7, 8]);
    assert!(query.starts_with("query { repository(owner: \"rjwalters\", name: \"loom\")"));
    assert!(query.contains("p7: pullRequest(number: 7)") && query.contains("p8: pullRequest"));
    let body = r#"{"data":{"repository":{
        "p7":{"closingIssuesReferences":{"totalCount":1,"nodes":[{"number":9027,"repository":{"databaseId":1073994527}}]}},
        "p8":null}}}"#;
    let parsed = parse_closing_refs(body, &[7, 8, 9]);
    assert_eq!(
        parsed.get(&7),
        Some(&ClosingRefs {
            refs: vec![(LOOM_REPO_ID, 9027)],
            total: 1
        })
    );
    assert!(!parsed.contains_key(&8), "a null PR is unreadable, not 'closes nothing'");
    assert!(!parsed.contains_key(&9));
    assert!(parse_closing_refs(r#"{"errors":[{"type":"RATE_LIMITED"}]}"#, &[7]).is_empty());
}
