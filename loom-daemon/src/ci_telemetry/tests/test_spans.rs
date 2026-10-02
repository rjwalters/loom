//! Per-test spans (#9456): `loom.ci.test` spans for the slow tail of a
//! `nextest-partition` leg's tests, parented to that leg's job span, built
//! from the JUnit XML `.config/nextest.toml`'s `ci` profile writes and
//! `ci.yml` uploads.
//!
//! A `loom.ci.job` span says the `Rust Unit Tests (2/3)` leg took 40s and its
//! step spans say most of that was one `run:` step; neither can say which of
//! the leg's 4,242 tests spent it, which is what rebalancing
//! `--partition count:k/N` needs. Two things here are easy to regress
//! silently, so each gets its own test: the *rejections* (a leg with no test
//! spans and a leg whose slow tail is genuinely empty otherwise look
//! identical) and the *volume bound* (one span per test would be two orders of
//! magnitude more trace than every other CI span combined).
//!
//! The fixture document is real nextest JUnit shape, captured from
//! `cargo nextest run --profile ci` rather than hand-invented: self-closing
//! `<testcase/>` for a pass, `<failure>`/`<rerunFailure>` with the panic text
//! inline for a failure, `<flakyFailure>` for a retry that ultimately passed,
//! and `timestamp`/`time` on the start tag.

use super::*;
use crate::ci_telemetry::nextest::{self, RejectReason, TestCase, TestOutcome};
use crate::ci_telemetry::records::{parse_shard, test_context, ShardInfo, ShardKind};
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus};

/// Job 10014 of alpha run 1001 — the fixture's one `nextest-partition` leg.
const NEXTEST_JOB: u64 = 10014;
const ARTIFACT: &str = "ci-test-timings-rust-unit-tests-2-3";

/// The fixture leg's own GitHub-reported window — the authority every test
/// span's window is clamped into.
fn job_window() -> (DateTime<Utc>, DateTime<Utc>) {
    ("2026-09-20T09:00:10Z".parse().unwrap(), "2026-09-20T09:00:50Z".parse().unwrap())
}

/// Replace the fixture artifact's body — the seam for a document the poller
/// must reject rather than emit spans from.
fn set_artifact_text(api: &FixtureApi, name: &str, text: &str) {
    api.artifacts
        .lock()
        .unwrap()
        .insert(name.to_string(), serde_json::json!({ "file": "junit.xml", "text": text }));
}

/// Re-key the fixture artifact under a different NAME, listing and body both —
/// the seam for the pairing key, which lives entirely in the name.
fn rename_artifact(api: &FixtureApi, from: &str, to: &str) {
    let body = api.artifacts.lock().unwrap().remove(from).unwrap();
    api.artifacts.lock().unwrap().insert(to.to_string(), body);
    api.edit("repos/fixture-org/alpha/actions/runs/1001/artifacts?per_page=100", |listing| {
        for artifact in listing["body"]["artifacts"].as_array_mut().unwrap() {
            if artifact["name"] == from {
                artifact["name"] = serde_json::json!(to);
            }
        }
    });
}

fn test_spans(root: &Path) -> Vec<SpanRecord> {
    journal(root)
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(s) if s.name == SpanName::CiTest => Some(s),
            _ => None,
        })
        .collect()
}

fn attr(span: &SpanRecord, key: &str) -> Option<String> {
    span.attributes.get(key).cloned()
}

/// Keyed by `"<binary> <test>"` — the identity pair, since a test path is only
/// unique within its binary.
fn by_test(spans: &[SpanRecord]) -> BTreeMap<String, SpanRecord> {
    spans
        .iter()
        .map(|s| {
            (
                format!(
                    "{} {}",
                    attr(s, "loom.ci.test.binary").unwrap(),
                    attr(s, "loom.ci.test").unwrap()
                ),
                s.clone(),
            )
        })
        .collect()
}

fn case(binary: &str, name: &str, start: Option<&str>, ms: i64, outcome: TestOutcome) -> TestCase {
    TestCase {
        binary: binary.to_string(),
        name: name.to_string(),
        started_at: start.map(|at| at.parse().unwrap()),
        duration_ms: ms,
        outcome,
    }
}

fn target() -> nextest::TestSpanTarget<'static> {
    let (job_started, job_ended) = job_window();
    nextest::TestSpanTarget {
        repo: "fixture-org/alpha",
        visibility: crate::telemetry::RepoVisibility::Public,
        run_id: 1001,
        attempt: 1,
        job_id: NEXTEST_JOB,
        job: "Rust Unit Tests (2/3)",
        workflow: "CI alpha",
        shard: ShardInfo {
            kind: ShardKind::NextestPartition,
            index: Some(2),
            total: Some(3),
        },
        job_context: crate::ci_telemetry::records::job_context(
            "fixture-org/alpha",
            1001,
            1,
            NEXTEST_JOB,
        ),
        job_started,
        job_ended,
    }
}

// ---------------------------------------------------------------------------
// AC2: a nextest-partition leg shows per-test child spans over a full cycle
// ---------------------------------------------------------------------------

/// The whole path end to end over a recorded cycle: the artifacts listing, the
/// download, the name → job pairing, the XML parse, the tail selection, and
/// the spans themselves.
#[test]
fn the_nextest_fixture_leg_gains_one_test_span_per_selected_slow_test() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert!(report.repo_errors.is_empty(), "{:?}", report.repo_errors);
    assert_eq!(report.summary.test_records_read, 1);
    assert_eq!(report.summary.test_spans_emitted, FIXTURE_TEST_SPANS);
    assert_eq!(report.summary.test_artifact_failures, 0);

    let spans = test_spans(dir.path());
    assert_eq!(spans.len(), FIXTURE_TEST_SPANS);
    assert_eq!(
        by_test(&spans).keys().cloned().collect::<Vec<_>>(),
        vec![
            "loom-daemon::integration_basic ci_telemetry::poll::slow_path".to_string(),
            "loom-daemon::lib ci_telemetry::ledger::medium".to_string(),
            "loom-daemon::lib ci_telemetry::poll::slow_path".to_string(),
            "loom-daemon::lib ci_telemetry::story::flaky_one".to_string(),
            "loom-daemon::lib tests::skewed_clock".to_string(),
        ],
        "the sub-floor test, the <skipped/> one, the one with no timestamp and the repeated \
         (classname, name) emit nothing — and the same test name under a second binary is its own \
         span"
    );
    assert_no_duplicates(dir.path());
}

/// Every test span is a child of its leg's JOB span (not of the run span, and
/// not of a step span), and repeats that job's identity and shard trio so
/// "which test of which partition" is one group-by rather than a trace join.
#[test]
fn test_spans_parent_to_their_legs_job_span_and_carry_its_shard_identity() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();

    let job_span = journal(dir.path())
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(s) if s.name == SpanName::CiJob => Some(s),
            _ => None,
        })
        .find(|s| attr(s, "loom.ci.job_id").as_deref() == Some(&NEXTEST_JOB.to_string()))
        .expect("the fixture's nextest leg has a job span");

    for span in test_spans(dir.path()) {
        assert_eq!(
            span.parent_span_id
                .as_ref()
                .map(crate::telemetry::trace::SpanId::as_str),
            Some(job_span.context.span_id.as_str()),
            "test spans must hang off the job span"
        );
        assert_eq!(span.context.trace_id, job_span.context.trace_id);
        assert_eq!(attr(&span, "loom.ci.job_id").as_deref(), Some("10014"));
        assert_eq!(attr(&span, "loom.ci.job").as_deref(), Some("Rust Unit Tests (2/3)"));
        assert_eq!(attr(&span, "loom.ci.shard.index").as_deref(), Some("2"));
        assert_eq!(attr(&span, "loom.ci.shard.total").as_deref(), Some("3"));
        assert_eq!(attr(&span, "loom.ci.shard.kind").as_deref(), Some("nextest-partition"));
        assert_eq!(attr(&span, "loom.ci.workflow").as_deref(), Some("CI alpha"));
        assert_eq!(attr(&span, "loom.ci.run_id").as_deref(), Some("1001"));
    }
}

/// A test's status and `outcome` attribute are its OWN, not its job's: the
/// fixture's leg concluded `success` while one of its tests failed and another
/// was flaky, and both #7789's flake query and a "which test is red" query
/// have to be able to see that.
#[test]
fn a_tests_status_is_its_own_outcome_not_its_jobs_conclusion() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let named = by_test(&test_spans(dir.path()));

    let failed = &named["loom-daemon::lib ci_telemetry::ledger::medium"];
    assert_eq!(attr(failed, "loom.ci.test.outcome").as_deref(), Some("fail"));
    assert_eq!(failed.status, SpanStatus::Error);

    // Flaky: it failed once and ultimately PASSED, so the span is Ok and only
    // the outcome attribute distinguishes it from a clean pass.
    let flaky = &named["loom-daemon::lib ci_telemetry::story::flaky_one"];
    assert_eq!(attr(flaky, "loom.ci.test.outcome").as_deref(), Some("flaky"));
    assert_eq!(flaky.status, SpanStatus::Ok);

    let passed = &named["loom-daemon::lib ci_telemetry::poll::slow_path"];
    assert_eq!(attr(passed, "loom.ci.test.outcome").as_deref(), Some("pass"));
    assert_eq!(passed.status, SpanStatus::Ok);
}

/// The windows are the test's own `timestamp` + `time`, and a document whose
/// clock disagrees with GitHub's is clamped into the job's window rather than
/// rendered as a bar detached from its parent.
#[test]
fn test_windows_are_the_tests_own_and_never_escape_the_jobs_window() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let named = by_test(&test_spans(dir.path()));
    let (job_started, job_ended) = job_window();

    let slow = &named["loom-daemon::lib ci_telemetry::poll::slow_path"];
    assert_eq!(slow.started_at, "2026-09-20T09:00:12Z".parse::<DateTime<Utc>>().unwrap());
    assert_eq!(
        slow.ended_at,
        "2026-09-20T09:00:24.500Z".parse::<DateTime<Utc>>().unwrap(),
        "12.500s after its own start, to the millisecond"
    );

    // Recorded 08:59:00 + 400s, i.e. starting before and ending after the job.
    let skewed = &named["loom-daemon::lib tests::skewed_clock"];
    assert_eq!(skewed.started_at, job_started);
    assert_eq!(skewed.ended_at, job_ended);

    for span in test_spans(dir.path()) {
        assert!(span.started_at >= job_started && span.ended_at <= job_ended);
        assert!(span.ended_at >= span.started_at);
    }
}

// ---------------------------------------------------------------------------
// Cost: one listing for both artifact families, and only when it can pay off
// ---------------------------------------------------------------------------

/// The artifacts listing is requested ONCE for the run that has an unemitted
/// sharded leg — not once per artifact family — and both the suite-timings and
/// the test-timings artifact are downloaded from that one listing. Never again
/// once the run is emitted.
#[test]
fn one_artifacts_listing_serves_both_the_suite_and_the_test_artifact() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let listings: Vec<String> = api
        .requests()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.contains("/artifacts"))
        .collect();
    assert_eq!(
        listings,
        vec![
            "repos/fixture-org/alpha/actions/runs/1001/artifacts?per_page=100".to_string(),
            "download:repos/fixture-org/alpha/actions/runs/1001/artifacts/ci-suite-timings-2"
                .to_string(),
            format!("download:repos/fixture-org/alpha/actions/runs/1001/artifacts/{ARTIFACT}"),
        ],
        "one listing + one download per matching artifact, and only for the sharded run"
    );

    // A second cycle over the same fixture re-lists the runs (the trailing
    // rescan window) but must not re-list or re-download any artifact.
    let again = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &again).unwrap();
    assert_eq!(report.summary.test_spans_emitted, 0);
    assert!(
        !again
            .requests()
            .iter()
            .any(|(p, _)| p.contains("/artifacts")),
        "{:?}",
        again.requests()
    );
}

/// The cost gate is per family: a run whose only sharded legs are nextest
/// partitions never downloads a `ci-suite-timings` artifact, and a run with no
/// sharded leg at all makes no artifacts request whatsoever.
#[test]
fn the_cost_gate_is_per_shard_family() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    // Demote the shell-suite leg to an unsharded job: the nextest leg is then
    // the only reason to list artifacts, and the suite record must be left
    // alone even though it is sitting in the same listing.
    api.edit(
        "repos/fixture-org/alpha/actions/runs/1001/jobs?filter=all&per_page=100",
        |page| {
            for job in page["body"]["jobs"].as_array_mut().unwrap() {
                if job["id"] == 10012 {
                    job["name"] = serde_json::json!("Shell Test Suites (hermetic)");
                }
            }
        },
    );
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.test_spans_emitted, FIXTURE_TEST_SPANS);
    assert_eq!(report.summary.suite_spans_emitted, 0);
    assert_eq!(report.summary.suite_artifact_failures, 0, "never even attempted");
    let downloads: Vec<String> = api
        .requests()
        .into_iter()
        .map(|(p, _)| p)
        .filter(|p| p.starts_with("download:"))
        .collect();
    assert_eq!(downloads.len(), 1, "{downloads:?}");
    assert!(downloads[0].ends_with(ARTIFACT), "{downloads:?}");
}

/// The per-cycle request counter includes the JUnit download, so `status`
/// accounting stays honest — an uncounted forge call is an invisible one.
#[test]
fn test_artifact_requests_are_counted_in_the_cycles_request_total() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.requests, api.requests().len());
}

// ---------------------------------------------------------------------------
// Rejections: named, counted, and never guessed
// ---------------------------------------------------------------------------

/// A failed download costs the test spans of that leg and nothing else: the
/// run's own `ci.run`/`ci.job` records still land, and so do the suite spans
/// of the sibling leg whose artifact was fine.
#[test]
fn a_failed_download_still_emits_every_run_and_job_record() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.flaky_artifacts
        .lock()
        .unwrap()
        .insert(ARTIFACT.to_string(), 1);
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (6, 24));
    assert_eq!(report.summary.test_spans_emitted, 0);
    assert_eq!(report.summary.test_artifact_failures, 1);
    assert_eq!(report.summary.suite_spans_emitted, FIXTURE_SUITE_SPANS);
    assert!(test_spans(dir.path()).is_empty());
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, FIXTURE_SPANS - FIXTURE_TEST_SPANS));
}

/// Each rejection is a NAMED reason rather than an empty result, so the cycle
/// log says which artifact was dropped and why. The pairing key is the
/// artifact NAME: JUnit XML carries no run id, no shard and no job name.
#[test]
fn an_artifact_the_poller_cannot_trust_is_rejected_by_name() {
    let jobs = [
        (NEXTEST_JOB, "Rust Unit Tests (2/3)", parse_shard("Rust Unit Tests (2/3)")),
        (
            10012,
            "Shell Test Suites (hermetic, 2/2)",
            parse_shard("Shell Test Suites (hermetic, 2/2)"),
        ),
    ];
    let identity = nextest::parse_artifact_name(ARTIFACT).unwrap();
    assert_eq!(nextest::match_job(&identity, &jobs).unwrap(), NEXTEST_JOB);

    // The name has to carry a family AND a k/N with 1 <= k <= N.
    for bad in [
        "ci-test-timings",
        "ci-test-timings-",
        "ci-test-timings-2-3",
        "ci-test-timings-rust-unit-tests",
        "ci-test-timings-rust-unit-tests-2",
        "ci-test-timings-rust-unit-tests-4-3",
        "ci-test-timings-rust-unit-tests-0-3",
        "ci-test-timings-rust-unit-tests-x-3",
        "ci-suite-timings-2",
    ] {
        assert!(nextest::parse_artifact_name(bad).is_none(), "{bad}");
    }

    // A family this run has no leg for...
    let unknown = nextest::parse_artifact_name("ci-test-timings-rust-doc-tests-2-3").unwrap();
    assert!(matches!(
        nextest::match_job(&unknown, &jobs),
        Err(RejectReason::NoUniqueJob { matches: 0, .. })
    ));
    // ...a shard this run's family does not have...
    let wrong_shard = nextest::parse_artifact_name("ci-test-timings-rust-unit-tests-1-3").unwrap();
    assert!(matches!(
        nextest::match_job(&wrong_shard, &jobs),
        Err(RejectReason::NoUniqueJob { matches: 0, .. })
    ));
    // ...and two legs of the SAME family and shard: ambiguity is never
    // guessed, the same rule `suites::match_job` applies.
    let ambiguous = [
        (NEXTEST_JOB, "Rust Unit Tests (2/3)", parse_shard("Rust Unit Tests (2/3)")),
        (10099, "Rust Unit Tests (2/3)", parse_shard("Rust Unit Tests (2/3)")),
    ];
    assert!(matches!(
        nextest::match_job(&identity, &ambiguous),
        Err(RejectReason::NoUniqueJob { matches: 2, .. })
    ));

    // The family is load-bearing, not cosmetic: `ci.yml` has TWO
    // nextest-partition families both sharding 1..3, so (kind, k, N) alone
    // would match both and reject every record.
    let both_families = [
        (NEXTEST_JOB, "Rust Unit Tests (2/3)", parse_shard("Rust Unit Tests (2/3)")),
        (
            10098,
            "Rust OTLP Feature Tests (2/3)",
            parse_shard("Rust OTLP Feature Tests (2/3)"),
        ),
    ];
    assert_eq!(nextest::match_job(&identity, &both_families).unwrap(), NEXTEST_JOB);
    let otlp = nextest::parse_artifact_name("ci-test-timings-rust-otlp-feature-tests-2-3").unwrap();
    assert_eq!(nextest::match_job(&otlp, &both_families).unwrap(), 10098);

    // A shell-suite leg with the same (k/N) is not a nextest partition.
    let shell = [(
        10012,
        "Shell Test Suites (hermetic, 2/3)",
        parse_shard("Shell Test Suites (hermetic, 2/3)"),
    )];
    assert!(matches!(
        nextest::match_job(&identity, &shell),
        Err(RejectReason::NoUniqueJob { matches: 0, .. })
    ));

    // And the document itself must be JUnit with at least one <testcase>.
    assert_eq!(nextest::parse("not xml at all"), Err(RejectReason::NotJunit));
    assert_eq!(
        nextest::parse("{\"schema\":\"loom.ci.suite-timings/1\"}"),
        Err(RejectReason::NotJunit),
        "a suite-timings JSON record mis-uploaded under this prefix is not JUnit"
    );
    assert_eq!(
        nextest::parse("<testsuites name=\"nextest-run\" tests=\"0\"/>"),
        Err(RejectReason::NoTestCases)
    );
}

/// **The artifact-name contract, asserted against `ci.yml` itself.**
///
/// The name is the whole pairing key (JUnit XML carries no run id, shard or
/// job name), so a drift between the workflow's `name:` and its upload's
/// `name:` does not fail anything in CI — it just silently stops test spans,
/// which looks exactly like "this leg has no slow tests". A comment in both
/// places is not enforcement; this is. For every `ci-test-timings-…` upload in
/// `ci.yml`, the slug the poller will derive from the owning job's display
/// name must be the slug the artifact name carries.
#[test]
fn every_ci_yml_test_timings_upload_pairs_with_its_own_jobs_display_name() {
    let ci_yml = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(".github/workflows/ci.yml");
    let text = std::fs::read_to_string(&ci_yml).unwrap();

    // Walk the file tracking the most recent `name: <display name>` at job
    // level (4-space indent), which is the name GitHub reports for the job and
    // therefore the one `parse_shard` / `job_family_slug` see.
    let mut job_name = String::new();
    let mut uploads: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("    name: ") {
            job_name = rest.trim().to_string();
        } else if let Some(rest) = line.trim().strip_prefix("name: ci-test-timings-") {
            uploads.push((job_name.clone(), rest.trim().to_string()));
        }
    }
    assert_eq!(
        uploads.len(),
        2,
        "ci.yml has two nextest-partition families, each uploading its JUnit XML: {uploads:?}"
    );

    for (display_name, artifact_suffix) in &uploads {
        // Both names resolved for one concrete leg of the matrix, exactly as
        // GitHub would substitute them.
        let concrete_job = display_name
            .replace("${{ matrix.partition }}", "2")
            .replace("${{ strategy.job-total }}", "3");
        // The leg really is a nextest partition — the only shard kind these
        // spans hang off — and its display name yields a k/N.
        assert_eq!(parse_shard(&concrete_job).kind, ShardKind::NextestPartition, "{concrete_job}");
        let concrete_artifact = format!("ci-test-timings-{artifact_suffix}")
            .replace("${{ matrix.partition }}", "2")
            .replace("${{ strategy.job-total }}", "3");
        let identity = nextest::parse_artifact_name(&concrete_artifact)
            .unwrap_or_else(|| panic!("{concrete_artifact} must be a parseable artifact name"));
        assert_eq!(
            identity.family,
            nextest::job_family_slug(&concrete_job),
            "{concrete_artifact} does not pair with job {concrete_job:?} — the upload's name and \
             the job's display name have drifted apart, which silently stops test spans"
        );
        assert_eq!((identity.shard_index, identity.shard_total), (2, 3));
        let jobs = [(NEXTEST_JOB, concrete_job.as_str(), parse_shard(&concrete_job))];
        assert_eq!(nextest::match_job(&identity, &jobs).unwrap(), NEXTEST_JOB);
    }

    // And the upload really is the JUnit file nextest's `ci` profile writes,
    // guarded `if: always()` so a FAILING leg — the one whose test data matters
    // most — still uploads it.
    assert!(
        text.contains("path: target/nextest/ci/junit.xml"),
        "the upload path must be the [profile.ci.junit] output"
    );
    let nextest_toml = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(".config/nextest.toml");
    let toml = std::fs::read_to_string(nextest_toml).unwrap();
    assert!(
        toml.contains("[profile.ci.junit]") && toml.contains("path = \"junit.xml\""),
        "the ci profile must emit JUnit XML, or there is nothing to upload"
    );
    assert!(
        !toml.contains("[profile.default.junit]"),
        "JUnit output is the ci profile's alone: a local `cargo nextest run` has no artifact \
         upload, no poller and no reason to pay for the file"
    );
}

/// The family slug is derived from the job's display name exactly as `ci.yml`
/// writes it into the artifact name, so a drift between the two is a named
/// rejection rather than a wrong attribution.
#[test]
fn the_family_slug_strips_the_shard_suffix_and_slugifies_the_rest() {
    assert_eq!(nextest::job_family_slug("Rust Unit Tests (2/3)"), "rust-unit-tests");
    assert_eq!(
        nextest::job_family_slug("Rust OTLP Feature Tests (1/3)"),
        "rust-otlp-feature-tests"
    );
    assert_eq!(
        nextest::job_family_slug("Shell Test Suites (hermetic, 1/2)"),
        "shell-test-suites"
    );
    // No shard suffix to strip, and a trailing parenthesised group that is
    // NOT a k/N fraction is part of the name.
    assert_eq!(nextest::job_family_slug("Rust Unit Tests"), "rust-unit-tests");
    assert_eq!(nextest::job_family_slug("Build (daemon)"), "build-daemon");
    assert_eq!(nextest::job_family_slug("Lint"), "lint");
}

/// A document the poller cannot parse reaching the poller (not just
/// `nextest::parse`) is counted as a failure and emits nothing — the same
/// accounting a failed download gets, because both mean "this leg's test data
/// is missing".
#[test]
fn an_unparseable_artifact_is_counted_as_a_failure() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    set_artifact_text(&api, ARTIFACT, "<html><body>503 Service Unavailable</body></html>");
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.test_spans_emitted, 0);
    assert_eq!(report.summary.test_records_read, 0);
    assert_eq!(report.summary.test_artifact_failures, 1);
    assert!(report.repo_errors.is_empty(), "a bad artifact is not a repo failure");
}

/// An artifact whose NAME does not pair with a leg is rejected over a full
/// cycle too — the pairing key is checked before the document is parsed, so a
/// perfectly good JUnit file under a drifted name emits nothing and is counted.
#[test]
fn an_unpairable_artifact_name_is_counted_as_a_failure() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    rename_artifact(&api, ARTIFACT, "ci-test-timings-rust-unit-tests-renamed-2-3");
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.test_spans_emitted, 0);
    assert_eq!(report.summary.test_records_read, 0);
    assert_eq!(report.summary.test_artifact_failures, 1);
    assert_eq!(report.summary.suite_spans_emitted, FIXTURE_SUITE_SPANS, "unaffected");
}

// ---------------------------------------------------------------------------
// Volume: the bound is the whole design, so it is asserted, not asserted-about
// ---------------------------------------------------------------------------

/// Tests a real `--partition count:1/3` leg runs: `cargo nextest list
/// --workspace --profile ci --partition count:1/3` on `main`, 2026-10-02
/// (12,659 workspace tests over three partitions).
const MEASURED_LEG_TESTS: usize = 4_242;

/// How many of those were at or above [`nextest::MIN_TEST_DURATION_MS`] on the
/// recorded run of that same leg — 7.2% of them, carrying 89% of the leg's 873s
/// of summed test time. The number `MAX_TEST_SPANS_PER_JOB` is sized against.
const MEASURED_ABOVE_FLOOR_TESTS: usize = 306;

/// The per-leg bound, against the **measured** shape of a real leg rather than
/// a round number. `cargo nextest run --workspace --profile ci --partition
/// count:1/3` on `main` (2026-10-02) ran 4,242 tests, of which 306 (7.2%) were
/// at or above the 250 ms floor and carried 89% of the leg's 873s of test time.
/// So the floor is the mechanism — it discards 93% of the records — and
/// `MAX_TEST_SPANS_PER_JOB` (512, ~1.7x the measured 306) is a backstop. Both
/// halves are asserted here rather than only described, because the whole
/// feature's affordability rests on them.
#[test]
fn span_volume_per_leg_is_bounded_by_the_floor_then_by_the_cap() {
    // The measured leg's shape: 4,242 tests, the overwhelming majority of them
    // milliseconds long.
    let mut cases: Vec<TestCase> = (0..MEASURED_LEG_TESTS)
        .map(|n| {
            case(
                "loom-daemon",
                &format!("tests::fast_{n}"),
                Some("2026-09-20T09:00:11Z"),
                3,
                TestOutcome::Pass,
            )
        })
        .collect();
    assert!(
        nextest::select(&cases).is_empty(),
        "a whole leg of millisecond tests must produce ZERO spans: the floor, not the cap, is what \
         keeps the steady state cheap"
    );

    // The measured above-floor population, spread over the real 250 ms…144s
    // range the recorded leg showed: ALL of it must survive selection. That is
    // the property that makes the cap a backstop rather than the mechanism —
    // lowering `MAX_TEST_SPANS_PER_JOB` to or below the measured 306 would
    // silently truncate the ranking on every ordinary run, and fails here.
    let above_floor: Vec<TestCase> = (0..MEASURED_ABOVE_FLOOR_TESTS)
        .map(|n| {
            case(
                "loom-daemon",
                &format!("tests::above_floor_{n}"),
                Some("2026-09-20T09:00:11Z"),
                nextest::MIN_TEST_DURATION_MS + i64::try_from(n).unwrap(),
                TestOutcome::Pass,
            )
        })
        .collect();
    assert_eq!(
        nextest::select(&above_floor).len(),
        MEASURED_ABOVE_FLOOR_TESTS,
        "the per-leg cap must leave the measured above-floor population intact; at or below it, \
         the cap stops being a backstop and starts truncating every ordinary run"
    );

    // A test exactly ON the floor is in; one millisecond below it is out.
    cases.push(case(
        "loom-daemon",
        "tests::on_the_floor",
        Some("2026-09-20T09:00:11Z"),
        nextest::MIN_TEST_DURATION_MS,
        TestOutcome::Pass,
    ));
    cases.push(case(
        "loom-daemon",
        "tests::under_the_floor",
        Some("2026-09-20T09:00:11Z"),
        nextest::MIN_TEST_DURATION_MS - 1,
        TestOutcome::Pass,
    ));
    let selected = nextest::select(&cases);
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].name, "tests::on_the_floor");

    // Now a slow tail far larger than the cap: the cap binds, and it keeps
    // the SLOWEST ones, which are the only ones the question is about.
    let mut flood = cases.clone();
    for n in 0..(nextest::MAX_TEST_SPANS_PER_JOB + 500) {
        flood.push(case(
            "loom-daemon",
            &format!("tests::slow_{n}"),
            Some("2026-09-20T09:00:11Z"),
            1_000 + i64::try_from(n).unwrap(),
            TestOutcome::Pass,
        ));
    }
    let selected = nextest::select(&flood);
    assert_eq!(selected.len(), nextest::MAX_TEST_SPANS_PER_JOB);
    assert_eq!(
        selected[0].duration_ms,
        1_000 + i64::try_from(nextest::MAX_TEST_SPANS_PER_JOB + 499).unwrap(),
        "slowest first"
    );
    assert!(
        selected.iter().all(|c| c.duration_ms >= 1_000),
        "the overflow dropped is the FAST end of the tail, never the slow end"
    );
    // Deterministic across calls: the same input selects the same tests, which
    // is what keeps the derived span ids stable across replays.
    assert_eq!(nextest::select(&flood), selected);

    // And the cap holds all the way through to the envelopes.
    assert_eq!(
        nextest::test_envelopes(&target(), &flood, "host-1").len(),
        nextest::MAX_TEST_SPANS_PER_JOB
    );
}

// ---------------------------------------------------------------------------
// Untrusted input: a fork's ci.yml and nextest.toml write this file
// ---------------------------------------------------------------------------

/// A hostile or buggy document cannot smuggle a control character past
/// `bounded_attributes` (which would DROP the whole value and lose the one
/// attribute naming the test), overrun the attribute budget, or introduce
/// unbounded outcome cardinality.
#[test]
fn a_hostile_document_is_sanitized_and_vocabulary_bounded() {
    let hostile = format!(
        "<testsuites name=\"nextest-run\">\n<testsuite name=\"x\">\n\
         <testcase name=\"tests::{}\u{7}x\" classname=\"loom-daemon::\u{1}lib\" \
         timestamp=\"2026-09-20T09:00:12.000+00:00\" time=\"9.0\">\n\
         <somethingElse type=\"definitely-not-an-outcome\"/>\n\
         </testcase>\n</testsuite>\n</testsuites>",
        "y".repeat(400)
    );
    let cases = nextest::parse(&hostile).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(
        cases[0].outcome,
        TestOutcome::Pass,
        "an unrecognised child element is not an outcome: the vocabulary is a Rust enum, so there \
         is no path for a fork's text to reach the attribute at all"
    );

    let spans: Vec<SpanRecord> = nextest::test_envelopes(&target(), &cases, "host-1")
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::Span(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(spans.len(), 1);
    for key in ["loom.ci.test", "loom.ci.test.binary"] {
        let value = attr(&spans[0], key).unwrap();
        assert!(
            value.chars().count() <= 201,
            "{key} truncated to the attribute budget: {}",
            value.chars().count()
        );
        assert!(
            !value.chars().any(char::is_control),
            "{key} must have its control characters stripped"
        );
    }
    assert_eq!(attr(&spans[0], "loom.ci.test.outcome").as_deref(), Some("pass"));

    // Every `loom.ci.*` key is inside the declared vocabulary the gateway
    // forwards; anything else would be silently stripped in flight.
    let vocabulary: BTreeSet<&str> = CI_SPAN_ATTRIBUTE_KEYS.iter().copied().collect();
    for key in spans[0].attributes.keys() {
        if key.starts_with("loom.ci.") {
            assert!(vocabulary.contains(key.as_str()), "{key} is not in CI_SPAN_ATTRIBUTE_KEYS");
        }
    }
}

/// The parser reads element names and four start-tag attributes — never a
/// `<failure>`'s `message` or text, which is the test's raw captured output
/// and is therefore log text the gateway's BODY scrub would not see on a span
/// attribute. The fixture document embeds a complete decoy `<testcase>` inside
/// its failure text for exactly this: an escaped `&lt;testcase …/&gt;` must
/// not become a ninth test, and its `time="999.0"` must not become a span.
#[test]
fn a_decoy_testcase_inside_failure_text_is_never_read_as_a_test() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    for span in test_spans(dir.path()) {
        assert_ne!(attr(&span, "loom.ci.test").as_deref(), Some("smuggled"));
        assert_ne!(attr(&span, "loom.ci.test.binary").as_deref(), Some("smuggled"));
        // 999s would be the longest span in the trace if it had been read.
        assert!(span.ended_at - span.started_at <= chrono::Duration::seconds(40));
    }
    // No span attribute anywhere carries the panic text.
    for span in test_spans(dir.path()) {
        for value in span.attributes.values() {
            assert!(!value.contains("RUST_BACKTRACE"), "{value}");
            assert!(!value.contains("panicked at"), "{value}");
        }
    }
}

/// An `<testcase>` element count beyond the parse cap stops the scan rather
/// than allocating for a hostile flood, and the document is still usable.
#[test]
fn the_parse_cap_bounds_a_flood_of_testcase_elements() {
    let mut xml = String::from("<testsuites name=\"nextest-run\"><testsuite name=\"x\">");
    for n in 0..(nextest::MAX_TESTCASES_PARSED + 10) {
        xml.push_str(&format!(
            "<testcase name=\"t{n}\" classname=\"b\" timestamp=\"2026-09-20T09:00:12.000+00:00\" \
             time=\"1.0\"/>"
        ));
    }
    xml.push_str("</testsuite></testsuites>");
    assert_eq!(nextest::parse(&xml).unwrap().len(), nextest::MAX_TESTCASES_PARSED);
}

/// A truncated document stops the scan at the incomplete element and keeps the
/// prefix it already read — a cut-off upload is still real data about the tests
/// that did complete.
#[test]
fn a_truncated_document_keeps_the_prefix_it_already_parsed() {
    let xml = "<testsuites name=\"nextest-run\"><testsuite name=\"x\">\
         <testcase name=\"tests::complete\" classname=\"b\" \
         timestamp=\"2026-09-20T09:00:12.000+00:00\" time=\"1.0\"/>\
         <testcase name=\"tests::cut_off\" classn";
    let cases = nextest::parse(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].name, "tests::complete");
}

/// `<testcases…` is not `<testcase`, and `&lt;` in text is not markup: neither
/// can inject an element the document does not have.
#[test]
fn the_scanner_only_matches_a_real_testcase_start_tag() {
    let xml = "<testsuites name=\"nextest-run\"><testsuite name=\"x\">\
         <testcases name=\"not-an-element\" time=\"5.0\"/>\
         <testcase name=\"tests::real\" classname=\"b\" \
         timestamp=\"2026-09-20T09:00:12.000+00:00\" time=\"1.0\"/>\
         </testsuite></testsuites>";
    let cases = nextest::parse(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].name, "tests::real");
}

/// The five predefined XML entities are decoded in an attribute value, and a
/// raw `>` inside a quoted value does not end the start tag early.
#[test]
fn attribute_values_are_unescaped_and_quote_aware() {
    let xml = "<testsuites name=\"nextest-run\"><testsuite name=\"x\">\
         <testcase name=\"tests::a &gt; b &amp;&amp; c &lt;d&gt; &apos;e&apos; &quot;f&quot;\" \
         classname=\"b\" timestamp=\"2026-09-20T09:00:12.000+00:00\" time=\"1.0\"/>\
         </testsuite></testsuites>";
    let cases = nextest::parse(xml).unwrap();
    assert_eq!(cases.len(), 1);
    assert_eq!(cases[0].name, "tests::a > b && c <d> 'e' \"f\"");
}

/// Identity is derived from `(repo, job_id, binary, test name)` — stable
/// across replays and hosts, and independent of the test's ordinal, which
/// `--partition count:k/N` reshuffles on every suite edit.
#[test]
fn a_test_span_id_is_derived_from_its_name_not_its_position() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    for (key, span) in &by_test(&test_spans(dir.path())) {
        let (binary, name) = key.split_once(' ').unwrap();
        let expected = test_context("fixture-org/alpha", 1001, 1, NEXTEST_JOB, binary, name);
        assert_eq!(span.context.span_id, expected.span_id, "{key}");
        assert_eq!(span.context.trace_id, expected.trace_id, "{key}");
    }
    // Re-deriving from a different starting point gives the same ids, which is
    // what makes journal dedup on `span|<span_id>` correct…
    let id = |binary: &str, name: &str| {
        test_context("fixture-org/alpha", 1001, 1, NEXTEST_JOB, binary, name).span_id
    };
    assert_eq!(id("loom-daemon::lib", "tests::a"), id("loom-daemon::lib", "tests::a"));
    assert_ne!(id("loom-daemon::lib", "tests::a"), id("loom-daemon::lib", "tests::b"));
    // …and the BINARY is part of the key, because a test path is only unique
    // within its binary: `tests::smoke` exists in several.
    assert_ne!(id("loom-daemon::lib", "tests::a"), id("loom-daemon::other", "tests::a"));
}

/// A test that did not run emits nothing, not a zero-length span at the job's
/// start — "ran instantly" is the opposite of "was skipped" (ci-principles
/// rule 6). Three ways a test can fail to produce a window, each covered:
/// nextest omits a filtered-out test entirely, a `<skipped/>` child is dropped
/// by selection, and a case with no `timestamp` has no anchor to place.
#[test]
fn a_test_that_did_not_run_has_no_span_at_all() {
    let (started, ended) = job_window();
    let skipped = case(
        "loom-daemon::lib",
        "tests::guarded",
        Some("2026-09-20T09:00:12Z"),
        5_000,
        TestOutcome::Skip,
    );
    assert!(nextest::select(std::slice::from_ref(&skipped)).is_empty());

    let anchorless = case("loom-daemon::lib", "tests::anchorless", None, 5_000, TestOutcome::Pass);
    assert!(nextest::window(&anchorless, started, ended).is_none());
    assert!(nextest::select(&[anchorless]).is_empty());

    // The fixture's own `<skipped/>` and no-timestamp cases parse, carry their
    // outcome, and are then dropped — so the rule is enforced on the real
    // document shape, not only on hand-built cases.
    let fixture_cases =
        nextest::parse(fixture()["artifacts"][ARTIFACT]["text"].as_str().unwrap()).unwrap();
    assert_eq!(fixture_cases.len(), 9);
    let skipped = fixture_cases
        .iter()
        .find(|c| c.name == "tests::skipped_one")
        .unwrap();
    assert_eq!(skipped.outcome, TestOutcome::Skip);
    assert!(fixture_cases
        .iter()
        .any(|c| c.name == "tests::no_timestamp" && c.started_at.is_none()));
    assert_eq!(nextest::select(&fixture_cases).len(), FIXTURE_TEST_SPANS);
}

/// Only an unexpired artifact is downloaded: GitHub answers 410 for an expired
/// one, which would be re-recorded as a failure on every cycle.
#[test]
fn an_expired_test_artifact_is_never_downloaded() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.edit("repos/fixture-org/alpha/actions/runs/1001/artifacts?per_page=100", |listing| {
        for artifact in listing["body"]["artifacts"].as_array_mut().unwrap() {
            if artifact["name"] == ARTIFACT {
                artifact["expired"] = serde_json::json!(true);
            }
        }
    });
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.test_spans_emitted, 0);
    assert_eq!(report.summary.test_artifact_failures, 0, "skipped, not failed");
    assert!(!api.requests().iter().any(|(p, _)| p.contains(ARTIFACT)));
}

/// A file over the byte cap is skipped rather than truncated into the parser:
/// half a document would turn a deliberate flood into a confusing partial
/// result instead of a named skip.
#[test]
fn a_junit_file_over_the_byte_cap_is_skipped_and_counted() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let mut xml = String::from("<testsuites name=\"nextest-run\"><testsuite name=\"x\">");
    while xml.len() as u64 <= nextest::MAX_JUNIT_BYTES {
        xml.push_str(
            "<testcase name=\"tests::pad\" classname=\"b\" \
             timestamp=\"2026-09-20T09:00:12.000+00:00\" time=\"1.0\"/>",
        );
    }
    xml.push_str("</testsuite></testsuites>");
    set_artifact_text(&api, ARTIFACT, &xml);
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.test_spans_emitted, 0);
    assert_eq!(report.summary.test_artifact_failures, 1);
    assert!(test_spans(dir.path()).is_empty());
}
