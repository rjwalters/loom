//! Suite spans (#9089): one `loom.ci.suite` span per shell test suite of a
//! sharded job, parented to that job's span, built from the timings artifact
//! `run-ci-suites.sh` uploads.
//!
//! A `loom.ci.job` span says the `Shell Test Suites (hermetic, 2/2)` leg took
//! 113s and its step spans say ~110s of that was one `run:` step; neither can
//! say which of the leg's ~118 suites spent it, which is what rebalancing the
//! `LOOM_CI_SHARD` split needs. What is easy to regress silently here is not
//! the happy path but the *rejections* — a record the poller cannot trust must
//! produce no spans AND be counted, because a leg with no suite spans and a leg
//! that ran no suites otherwise look identical.

use super::*;
use crate::ci_telemetry::records::{parse_shard, suite_context, ShardInfo, ShardKind};
use crate::ci_telemetry::suites::{self, RejectReason};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus};

/// Job 10012 of alpha run 1001 — the fixture's one `shell-suite-shard` leg.
const SHARD_JOB: u64 = 10012;

// ---------------------------------------------------------------------------
// Fixture seams
//
// `FixtureApi`'s artifact state lives in `tests.rs` (it is shared fixture
// state), but the behaviour built on it lives here, next to the only tests
// that exercise it — a child module can read its parent's private fields, so
// nothing has to be widened for this.
// ---------------------------------------------------------------------------

/// The fixture's downloadable artifact bodies, keyed by artifact name.
pub(super) fn fixture_artifacts(fx: &Value) -> BTreeMap<String, Value> {
    fx["artifacts"]
        .as_object()
        .unwrap()
        .clone()
        .into_iter()
        .collect()
}

/// Unpack a fixture artifact into `dest`, exactly as `gh run download --name X
/// --dir dest` would. Recorded in `requests` under a `download:` pseudo-path so
/// the crash-recovery sweep (`killing_the_process_at_any_request_…`) treats it
/// as one more request it can be killed at, and so request accounting stays
/// comparable with the `gh api` calls.
pub(super) fn serve_artifact(
    api: &FixtureApi,
    repo: &str,
    run_id: u64,
    name: &str,
    dest: &Path,
) -> Result<(), ApiError> {
    let path = format!("download:repos/{repo}/actions/runs/{run_id}/artifacts/{name}");
    {
        let mut requests = api.requests.lock().unwrap();
        requests.push((path.clone(), None));
        let n = requests.len();
        drop(requests);
        assert!(api.panic_at != Some(n), "simulated process kill at request {n}");
    }
    {
        let mut flaky = api.flaky_artifacts.lock().unwrap();
        if let Some(remaining) = flaky.get_mut(name) {
            if *remaining > 0 {
                *remaining -= 1;
                return Err(ApiError::Transport(format!(
                    "synthetic transient failure downloading {name}"
                )));
            }
        }
    }
    let artifacts = api.artifacts.lock().unwrap();
    let Some(entry) = artifacts.get(name) else {
        return Err(ApiError::Http {
            status: 404,
            path,
            detail: "{\"message\":\"Not Found\"}".into(),
        });
    };
    let file = entry["file"].as_str().unwrap();
    let text = entry["text"].as_str().unwrap();
    std::fs::write(dest.join(file), text).unwrap();
    Ok(())
}

/// Fail this artifact's next `n` downloads, then serve it normally.
fn fail_artifact_times(api: &FixtureApi, name: &str, n: usize) {
    api.flaky_artifacts
        .lock()
        .unwrap()
        .insert(name.to_string(), n);
}

/// Replace one artifact's body text — the seam for a record the poller must
/// reject rather than emit spans from.
fn set_artifact_text(api: &FixtureApi, name: &str, text: &str) {
    api.artifacts.lock().unwrap().insert(
        name.to_string(),
        serde_json::json!({ "file": "ci-suite-timings.json", "text": text }),
    );
}

fn suite_spans(root: &Path) -> Vec<SpanRecord> {
    journal(root)
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(s) if s.name == SpanName::CiSuite => Some(s),
            _ => None,
        })
        .collect()
}

fn attr(span: &SpanRecord, key: &str) -> Option<String> {
    span.attributes.get(key).cloned()
}

fn by_suite(spans: &[SpanRecord]) -> BTreeMap<String, SpanRecord> {
    spans
        .iter()
        .map(|s| (attr(s, "loom.ci.suite").unwrap(), s.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// AC3: a sharded job shows per-suite child spans
// ---------------------------------------------------------------------------

/// The whole path end to end over a recorded cycle: the artifacts listing, the
/// download, the parse, the shard→job pairing, and the spans themselves.
#[test]
fn the_sharded_fixture_job_gains_one_suite_span_per_executed_suite() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert!(report.repo_errors.is_empty(), "{:?}", report.repo_errors);
    assert_eq!(report.summary.suite_records_read, 1);
    assert_eq!(report.summary.suite_spans_emitted, FIXTURE_SUITE_SPANS);
    assert_eq!(report.summary.suite_artifact_failures, 0);

    let spans = suite_spans(dir.path());
    assert_eq!(spans.len(), FIXTURE_SUITE_SPANS);
    let named = by_suite(&spans);
    assert_eq!(
        named.keys().cloned().collect::<Vec<_>>(),
        vec![
            "test-alpha-broken.sh".to_string(),
            "test-alpha-fast.sh".to_string(),
            "test-alpha-skewed.sh".to_string(),
            "test-alpha-slow.sh".to_string(),
        ],
        "the skipped suite (no window) and the repeated name emit nothing"
    );
    assert_no_duplicates(dir.path());
}

/// Every suite span is a child of its shard's JOB span (not of the run span,
/// and not of a step span), and repeats that job's identity and shard trio so
/// "which suite of which leg" is one group-by rather than a trace join.
#[test]
fn suite_spans_parent_to_their_shards_job_span_and_carry_its_shard_identity() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();

    let job_span = journal(dir.path())
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(s) if s.name == SpanName::CiJob => Some(s),
            _ => None,
        })
        .find(|s| attr(s, "loom.ci.job_id").as_deref() == Some(&SHARD_JOB.to_string()))
        .expect("the fixture's shard leg has a job span");

    for span in suite_spans(dir.path()) {
        assert_eq!(
            span.parent_span_id
                .as_ref()
                .map(crate::telemetry::trace::SpanId::as_str),
            Some(job_span.context.span_id.as_str()),
            "suite spans must hang off the job span"
        );
        assert_eq!(span.context.trace_id, job_span.context.trace_id);
        assert_eq!(attr(&span, "loom.ci.job_id").as_deref(), Some("10012"));
        assert_eq!(
            attr(&span, "loom.ci.job").as_deref(),
            Some("Shell Test Suites (hermetic, 2/2)")
        );
        assert_eq!(attr(&span, "loom.ci.shard.index").as_deref(), Some("2"));
        assert_eq!(attr(&span, "loom.ci.shard.total").as_deref(), Some("2"));
        assert_eq!(attr(&span, "loom.ci.shard.kind").as_deref(), Some("shell-suite-shard"));
        assert_eq!(attr(&span, "loom.ci.workflow").as_deref(), Some("CI alpha"));
    }
}

/// A suite's status and `outcome` attribute are its OWN, not its job's: the
/// fixture's leg concluded `success` while one of its suites failed, and a
/// query for failing suites has to be able to see that.
#[test]
fn a_suites_status_is_its_own_outcome_not_its_jobs_conclusion() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let named = by_suite(&suite_spans(dir.path()));

    let broken = &named["test-alpha-broken.sh"];
    assert_eq!(attr(broken, "loom.ci.suite.outcome").as_deref(), Some("fail"));
    assert_eq!(broken.status, SpanStatus::Error);
    assert_eq!(attr(broken, "loom.ci.suite.retried").as_deref(), Some("true"));

    let fast = &named["test-alpha-fast.sh"];
    assert_eq!(attr(fast, "loom.ci.suite.outcome").as_deref(), Some("pass"));
    assert_eq!(fast.status, SpanStatus::Ok);
    assert_eq!(attr(fast, "loom.ci.suite.retried").as_deref(), Some("false"));
}

/// The windows are the suite's own, and a record whose clock disagrees with
/// GitHub's is clamped into the job's window rather than rendered as a bar
/// detached from its parent.
#[test]
fn suite_windows_are_the_suites_own_and_never_escape_the_jobs_window() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let named = by_suite(&suite_spans(dir.path()));

    let job_started: DateTime<Utc> = "2026-09-20T09:00:10Z".parse().unwrap();
    let job_ended: DateTime<Utc> = "2026-09-20T09:00:30Z".parse().unwrap();

    let slow = &named["test-alpha-slow.sh"];
    assert_eq!(slow.started_at, "2026-09-20T09:00:12Z".parse::<DateTime<Utc>>().unwrap());
    assert_eq!(slow.ended_at, job_ended);

    // Recorded 08:59:00 -> 09:05:00, i.e. entirely outside the job.
    let skewed = &named["test-alpha-skewed.sh"];
    assert_eq!(skewed.started_at, job_started);
    assert_eq!(skewed.ended_at, job_ended);

    for span in suite_spans(dir.path()) {
        assert!(span.started_at >= job_started && span.ended_at <= job_ended);
        assert!(span.ended_at >= span.started_at);
    }
}

// ---------------------------------------------------------------------------
// Cost: nothing is requested for a repo that does not shard shell suites
// ---------------------------------------------------------------------------

/// The artifacts listing is requested for exactly the one run that has a
/// not-yet-emitted sharded job — never for the other five runs, and never
/// again once that run is emitted. The suite-timings download is the only one
/// this module is about; the sibling `ci-test-timings` download that the same
/// single listing also serves (#9456) is asserted in `super::test_spans`.
#[test]
fn only_a_run_with_an_unemitted_shard_leg_costs_an_artifacts_request() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let listings: Vec<String> = api
        .requests()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.contains("/artifacts") && !p.contains("ci-test-timings"))
        .collect();
    assert_eq!(
        listings,
        vec![
            "repos/fixture-org/alpha/actions/runs/1001/artifacts?per_page=100".to_string(),
            "download:repos/fixture-org/alpha/actions/runs/1001/artifacts/ci-suite-timings-2"
                .to_string(),
        ],
        "one listing + one download, and only for the sharded run"
    );

    // A second cycle over the same fixture re-lists the runs (the trailing
    // rescan window) but must not re-list or re-download any artifact.
    let again = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &again).unwrap();
    assert_eq!(report.summary.suite_spans_emitted, 0);
    assert!(
        !again
            .requests()
            .iter()
            .any(|(p, _)| p.contains("/artifacts")),
        "{:?}",
        again.requests()
    );
}

/// The per-cycle request counter includes the artifact work, so `status`
/// accounting stays honest — an uncounted forge call is an invisible one.
#[test]
fn artifact_requests_are_counted_in_the_cycles_request_total() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.requests, api.requests().len());
}

/// An artifact whose name does not start with one of the documented prefixes
/// is never downloaded (the fixture's `build-daemon` artifact would be a whole
/// compiled binary), and neither is an EXPIRED timings artifact — GitHub
/// answers 410 for those, which would be re-recorded as a failure on every
/// cycle. The fixture's listing holds five artifacts: the two prefix matches
/// below, the unrelated build output, the expired `ci-suite-timings-9`, and
/// #9456's `ci-test-timings-…` record.
#[test]
fn only_unexpired_prefix_matching_artifacts_are_downloaded() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let downloads: Vec<String> = api
        .requests()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.starts_with("download:"))
        .collect();
    assert_eq!(downloads.len(), 2, "{downloads:?}");
    assert!(downloads[0].ends_with("ci-suite-timings-2"), "{downloads:?}");
    assert!(downloads[1].ends_with("ci-test-timings-rust-unit-tests-2-3"), "{downloads:?}");
}

// ---------------------------------------------------------------------------
// Rejections: named, counted, and never guessed
// ---------------------------------------------------------------------------

/// A failed download costs the suite spans of that leg and nothing else: the
/// run's own `ci.run`/`ci.job` records still land. Holding the primary signal
/// hostage to a side artifact would be the worse failure.
#[test]
fn a_failed_download_still_emits_every_run_and_job_record() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    fail_artifact_times(&api, "ci-suite-timings-2", 1);
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (6, 24));
    assert_eq!(report.summary.suite_spans_emitted, 0);
    assert_eq!(report.summary.suite_artifact_failures, 1);
    assert!(suite_spans(dir.path()).is_empty());
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, FIXTURE_SPANS - FIXTURE_SUITE_SPANS));
}

/// Each rejection is a NAMED reason rather than an empty result, so the cycle
/// log says which record was dropped and why.
#[test]
fn a_record_the_poller_cannot_trust_is_rejected_by_name() {
    let jobs = [(SHARD_JOB, parse_shard("Shell Test Suites (hermetic, 2/2)"))];
    let good = |shard: &str, run_id: &str| {
        format!(
            r#"{{"schema":"{}","run_id":"{run_id}","shard":"{shard}","suites":[]}}"#,
            suites::SCHEMA
        )
    };

    assert!(matches!(
        suites::parse("not json at all", 1001),
        Err(RejectReason::Unparseable(_))
    ));
    assert!(matches!(
        suites::parse(r#"{"schema":"loom.ci.suite-timings/2","run_id":"1001","shard":"1/2"}"#, 1001),
        Err(RejectReason::UnknownSchema(s)) if s == "loom.ci.suite-timings/2"
    ));
    assert!(
        matches!(suites::parse(&good("1/2", "999"), 1001), Err(RejectReason::ForeignRun { .. })),
        "a record naming another run must not be attributed to this one"
    );
    assert!(
        matches!(suites::parse(&good("", "1001"), 1001), Err(RejectReason::NoShard(_))),
        "an unsharded (local) run's record pairs with no leg"
    );
    assert!(matches!(
        suites::parse(&good("3/2", "1001"), 1001),
        Err(RejectReason::NoShard(_))
    ));

    // A well-formed record for a shard this run has no leg for.
    let timings = suites::parse(&good("1/2", "1001"), 1001).unwrap();
    assert!(matches!(
        suites::match_job(&timings, &jobs),
        Err(RejectReason::NoUniqueJob { matches: 0, .. })
    ));
    // …and one whose shard matches TWO legs: ambiguity is never guessed.
    let ambiguous = [
        (10012, parse_shard("Shell Test Suites (hermetic, 2/2)")),
        (10013, parse_shard("Shell Test Suites (hermetic, 2/2)")),
    ];
    let two = suites::parse(&good("2/2", "1001"), 1001).unwrap();
    assert!(matches!(
        suites::match_job(&two, &ambiguous),
        Err(RejectReason::NoUniqueJob { matches: 2, .. })
    ));
    // A nextest partition leg with the same (k/N) is NOT a shell-suite shard.
    let nextest = [(10099, parse_shard("Rust Unit Tests (2/2)"))];
    assert!(matches!(
        suites::match_job(&two, &nextest),
        Err(RejectReason::NoUniqueJob { matches: 0, .. })
    ));
    assert_eq!(suites::match_job(&two, &jobs).unwrap(), SHARD_JOB);
}

/// An unparseable record reaching the poller (not just `suites::parse`) is
/// counted as a failure and emits nothing — the same accounting a failed
/// download gets, because both mean "this leg's suite data is missing".
#[test]
fn an_unparseable_artifact_is_counted_as_a_failure() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    set_artifact_text(&api, "ci-suite-timings-2", "}{ not json");
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.suite_spans_emitted, 0);
    assert_eq!(report.summary.suite_records_read, 0);
    assert_eq!(report.summary.suite_artifact_failures, 1);
    assert!(report.repo_errors.is_empty(), "a bad artifact is not a repo failure");
}

// ---------------------------------------------------------------------------
// Untrusted input: a fork's script writes this file
// ---------------------------------------------------------------------------

/// A hostile or buggy record cannot blow up span volume, smuggle a control
/// character past `bounded_attributes` (which would DROP the whole value and
/// lose the one attribute naming the suite), or introduce unbounded outcome
/// cardinality.
#[test]
fn a_hostile_record_is_capped_sanitized_and_vocabulary_bounded() {
    let mut entries = Vec::new();
    for n in 0..(suites::MAX_SUITE_SPANS_PER_JOB + 40) {
        entries.push(serde_json::json!({
            "suite": format!("test-flood-{n}.sh"), "outcome": "pass",
            "started_at_epoch": 1789894810, "ended_at_epoch": 1789894811,
            "retried": false,
        }));
    }
    entries.push(serde_json::json!({
        "suite": format!("test-{}\u{7}.sh", "x".repeat(400)),
        "outcome": "definitely-not-an-outcome",
        "started_at_epoch": 1789894810, "ended_at_epoch": 1789894811, "retried": false,
    }));
    let text = serde_json::json!({
        "schema": suites::SCHEMA, "run_id": "1001", "shard": "2/2", "suites": entries,
    })
    .to_string();
    let timings = suites::parse(&text, 1001).unwrap();

    let target = suites::SuiteSpanTarget {
        repo: "fixture-org/alpha",
        visibility: crate::telemetry::RepoVisibility::Public,
        run_id: 1001,
        attempt: 1,
        job_id: SHARD_JOB,
        job: "Shell Test Suites (hermetic, 2/2)",
        workflow: "CI alpha",
        shard: ShardInfo {
            kind: ShardKind::ShellSuiteShard,
            index: Some(2),
            total: Some(2),
        },
        job_context: crate::ci_telemetry::records::job_context(
            "fixture-org/alpha",
            1001,
            1,
            SHARD_JOB,
        ),
        job_started: "2026-09-20T09:00:10Z".parse().unwrap(),
        job_ended: "2026-09-20T09:00:30Z".parse().unwrap(),
    };
    let envelopes = suites::suite_envelopes(&target, &timings, "host-1");
    assert_eq!(
        envelopes.len(),
        suites::MAX_SUITE_SPANS_PER_JOB,
        "the per-job cap binds, and the overflow is dropped rather than truncated mid-record"
    );

    // Same record, but only the hostile entry, so the sanitizer is observable.
    let single = suites::parse(
        &serde_json::json!({
            "schema": suites::SCHEMA, "run_id": "1001", "shard": "2/2",
            "suites": [entries.last().unwrap()],
        })
        .to_string(),
        1001,
    )
    .unwrap();
    let spans: Vec<SpanRecord> = suites::suite_envelopes(&target, &single, "host-1")
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(spans.len(), 1);
    let name = attr(&spans[0], "loom.ci.suite").unwrap();
    assert!(
        name.chars().count() <= 201,
        "truncated to the attribute budget: {}",
        name.chars().count()
    );
    assert!(!name.chars().any(char::is_control), "control characters are stripped");
    assert_eq!(
        attr(&spans[0], "loom.ci.suite.outcome").as_deref(),
        Some("unknown"),
        "an outcome outside the closed vocabulary must not be forwarded verbatim"
    );
    assert_eq!(spans[0].status, SpanStatus::Unset);
    // The span is inside the declared vocabulary the gateway forwards.
    let vocabulary: BTreeSet<&str> = CI_SPAN_ATTRIBUTE_KEYS.iter().copied().collect();
    for key in spans[0].attributes.keys() {
        if key.starts_with("loom.ci.") {
            assert!(vocabulary.contains(key.as_str()), "{key} is not in CI_SPAN_ATTRIBUTE_KEYS");
        }
    }
}

/// Identity is derived from `(repo, job_id, sanitized suite name)` — stable
/// across replays and hosts, and independent of the suite's position in the
/// manifest, which shifts whenever a suite is added to `ci-wired.txt`.
#[test]
fn a_suite_span_id_is_derived_from_its_name_not_its_position() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let named = by_suite(&suite_spans(dir.path()));
    for (suite, span) in &named {
        let expected = suite_context("fixture-org/alpha", 1001, 1, SHARD_JOB, suite);
        assert_eq!(span.context.span_id, expected.span_id, "{suite}");
        assert_eq!(span.context.trace_id, expected.trace_id, "{suite}");
    }
    // Re-deriving from a different starting point gives the same ids, which is
    // what makes journal dedup on `span|<span_id>` correct.
    assert_eq!(
        suite_context("fixture-org/alpha", 1001, 1, SHARD_JOB, "test-alpha-fast.sh").span_id,
        suite_context("fixture-org/alpha", 1001, 1, SHARD_JOB, "test-alpha-fast.sh").span_id
    );
    assert_ne!(
        suite_context("fixture-org/alpha", 1001, 1, SHARD_JOB, "test-alpha-fast.sh").span_id,
        suite_context("fixture-org/alpha", 1001, 1, SHARD_JOB, "test-alpha-slow.sh").span_id
    );
}

/// A suite that never ran has a `0/0` window and must produce nothing, not a
/// zero-length span at the job's start — "ran instantly" is the opposite of
/// "was skipped", and the skip is already visible in the job log.
#[test]
fn a_suite_that_never_ran_has_no_span_at_all() {
    let entry: suites::SuiteEntryJson = serde_json::from_value(serde_json::json!({
        "suite": "test-guarded.sh", "outcome": "skip",
        "started_at_epoch": 0, "ended_at_epoch": 0, "retried": false,
    }))
    .unwrap();
    let started: DateTime<Utc> = "2026-09-20T09:00:10Z".parse().unwrap();
    let ended: DateTime<Utc> = "2026-09-20T09:00:30Z".parse().unwrap();
    assert!(suites::window(&entry, started, ended).is_none());
}
