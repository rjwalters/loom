//! Tests for the CI telemetry poller (Issue #8824). Every test runs against
//! the committed recorded-fixture org under
//! `loom-daemon/tests/fixtures/ci_telemetry/` — no live network.
//!
//! Phase 2's job-log capture tests (#8825) live in the sibling
//! [`job_logs`] module (named to leave `logs::` resolving to
//! `ci_telemetry::logs`, the module under test).

mod job_logs;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};
use serde_json::Value;
use tempfile::TempDir;

use super::api::{
    classify, normalise_api_path, parse_next_link, parse_raw, ApiError, ApiResponse, GithubApi,
};
use super::journal::Journal;
use super::ledger::{self, Ledger, UnitDraft, UnitKey};
use super::poll::{backoff_until, run_cycle, CycleContext, CycleError};
use super::records::{
    envelope_identity, job_envelopes, run_envelopes, JobsPage, RepoJson, RunsPage,
};
use super::state::{self, classify as classify_health, Health, PollStatus};
use super::*;
use crate::telemetry::ci::{CI_LOG_ATTRIBUTE_KEYS, CI_METRIC_LABEL_KEYS, CI_SPAN_ATTRIBUTE_KEYS};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

const ORG: &str = "fixture-org";

fn fixture() -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/ci_telemetry/responses.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// Drop the `created=` watermark parameter so fixture keys are stable.
fn strip_created(path: &str) -> String {
    let Some((base, query)) = path.split_once('?') else {
        return path.to_string();
    };
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.starts_with("created="))
        .collect();
    format!("{base}?{}", kept.join("&"))
}

/// Serves the recorded fixture; records every request; can override a
/// response by key, or panic on the Nth request (a simulated process kill).
struct FixtureApi {
    responses: Mutex<BTreeMap<String, Value>>,
    overrides: Mutex<HashMap<String, ApiResponse>>,
    requests: Mutex<Vec<(String, Option<String>)>>,
    panic_at: Option<usize>,
    /// Job-log documents (#8825), keyed by request path.
    job_logs: Mutex<BTreeMap<String, Value>>,
    /// Paths whose next `get_document` call fails transiently (then heals) —
    /// the retry-after-failure seam.
    flaky_logs: Mutex<HashMap<String, usize>>,
}

impl FixtureApi {
    fn new() -> Self {
        let fx = fixture();
        let responses = fx["responses"]
            .as_object()
            .unwrap()
            .clone()
            .into_iter()
            .collect();
        let job_logs = fx["job_logs"]
            .as_object()
            .unwrap()
            .clone()
            .into_iter()
            .collect();
        FixtureApi {
            responses: Mutex::new(responses),
            overrides: Mutex::new(HashMap::new()),
            requests: Mutex::new(Vec::new()),
            panic_at: None,
            job_logs: Mutex::new(job_logs),
            flaky_logs: Mutex::new(HashMap::new()),
        }
    }

    fn panicking_at(n: usize) -> Self {
        FixtureApi {
            panic_at: Some(n),
            ..FixtureApi::new()
        }
    }

    /// Fail this job-log path's next `n` downloads, then serve it normally.
    fn fail_log_times(&self, path: &str, n: usize) {
        self.flaky_logs.lock().unwrap().insert(path.to_string(), n);
    }

    fn set_override(&self, key: &str, response: ApiResponse) {
        self.overrides
            .lock()
            .unwrap()
            .insert(key.to_string(), response);
    }

    fn edit(&self, key: &str, edit: impl FnOnce(&mut Value)) {
        let mut responses = self.responses.lock().unwrap();
        edit(responses.get_mut(key).unwrap());
    }

    fn requests(&self) -> Vec<(String, Option<String>)> {
        self.requests.lock().unwrap().clone()
    }
}

impl GithubApi for FixtureApi {
    fn get(&self, path: &str, etag: Option<&str>) -> Result<ApiResponse, ApiError> {
        let n = {
            let mut requests = self.requests.lock().unwrap();
            requests.push((path.to_string(), etag.map(str::to_string)));
            requests.len()
        };
        assert!(self.panic_at != Some(n), "simulated process kill at request {n}");
        let key = strip_created(path);
        if let Some(response) = self.overrides.lock().unwrap().get(&key) {
            return classify(response.clone(), path);
        }
        let responses = self.responses.lock().unwrap();
        let Some(entry) = responses.get(&key) else {
            return classify(
                ApiResponse {
                    status: 404,
                    body: "{\"message\":\"Not Found\"}".into(),
                    ..ApiResponse::default()
                },
                path,
            );
        };
        let entry_etag = entry.get("etag").and_then(Value::as_str);
        if etag.is_some() && etag == entry_etag {
            return Ok(ApiResponse {
                status: 304,
                etag: entry_etag.map(str::to_string),
                ..ApiResponse::default()
            });
        }
        Ok(ApiResponse {
            status: 200,
            etag: entry_etag.map(str::to_string),
            next: entry
                .get("next")
                .and_then(Value::as_str)
                .map(normalise_api_path),
            body: entry["body"].to_string(),
            ..ApiResponse::default()
        })
    }

    fn get_document(&self, path: &str) -> Result<ApiResponse, ApiError> {
        {
            let mut requests = self.requests.lock().unwrap();
            requests.push((path.to_string(), None));
            let n = requests.len();
            drop(requests);
            assert!(self.panic_at != Some(n), "simulated process kill at request {n}");
        }
        {
            let mut flaky = self.flaky_logs.lock().unwrap();
            if let Some(remaining) = flaky.get_mut(path) {
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err(ApiError::Transport(format!(
                        "synthetic transient failure for {path}"
                    )));
                }
            }
        }
        let logs = self.job_logs.lock().unwrap();
        let Some(entry) = logs.get(path) else {
            // Every other fixture job gets a one-line log, so a cycle with
            // capture on exercises all 24 of them rather than only the three
            // with committed bodies.
            return Ok(ApiResponse {
                status: 200,
                body: format!("2026-09-20T09:00:00.0000000Z synthesized log for {path}\n"),
                ..ApiResponse::default()
            });
        };
        if let Some(status) = entry.get("status").and_then(Value::as_u64) {
            return classify(
                ApiResponse {
                    status: u16::try_from(status).unwrap_or(500),
                    body: "{\"message\":\"Gone\"}".into(),
                    ..ApiResponse::default()
                },
                path,
            );
        }
        let body = if let Some(text) = entry.get("text").and_then(Value::as_str) {
            text.to_string()
        } else {
            let line = entry["repeat"].as_str().unwrap();
            let count = usize::try_from(entry["count"].as_u64().unwrap()).unwrap();
            (0..count)
                .map(|n| line.replace("{n}", &n.to_string()))
                .collect()
        };
        Ok(ApiResponse {
            status: 200,
            body,
            ..ApiResponse::default()
        })
    }
}

fn now() -> DateTime<Utc> {
    "2026-09-20T12:00:00Z".parse().unwrap()
}

fn ctx(root: &Path) -> CycleContext<'_> {
    CycleContext {
        root,
        org: ORG.to_string(),
        excluded_repos: Vec::new(),
        now: now(),
        host_id: "fixture-host".to_string(),
        initial_lookback: Duration::hours(24),
        log_capture: LogCaptureGate::Off,
        log_excluded_repos: Vec::new(),
        log_max_bytes: logs::DEFAULT_MAX_BYTES,
    }
}

/// The same cycle with job-log capture on (#8825). The cap is deliberately
/// small so the fixture's one large log truncates without committing
/// megabytes of fixture data.
const TEST_LOG_CAP: usize = 4 * 1024;

fn ctx_with_logs(root: &Path) -> CycleContext<'_> {
    CycleContext {
        log_capture: LogCaptureGate::On,
        log_max_bytes: TEST_LOG_CAP,
        ..ctx(root)
    }
}

fn journal(root: &Path) -> Vec<TelemetryEnvelope> {
    Journal::reader(journal_path(root)).read_all().unwrap()
}

/// Every CI envelope identity appears exactly once in the journal.
fn assert_no_duplicates(root: &Path) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for env in journal(root) {
        *counts.entry(envelope_identity(&env).unwrap()).or_default() += 1;
    }
    let dupes: Vec<_> = counts.iter().filter(|(_, n)| **n > 1).collect();
    assert!(dupes.is_empty(), "double-emitted: {dupes:?}");
}

fn kind_counts(root: &Path) -> (usize, usize, usize, usize) {
    let (mut runs, mut jobs, mut durations, mut spans) = (0, 0, 0, 0);
    for env in journal(root) {
        match env.record {
            TelemetryRecord::CiRun(_) => runs += 1,
            TelemetryRecord::CiJob(_) => jobs += 1,
            TelemetryRecord::CiDuration(_) => durations += 1,
            TelemetryRecord::Span(_) => spans += 1,
            TelemetryRecord::CiJobLog(_) => {}
            other => panic!("unexpected record in CI journal: {other:?}"),
        }
    }
    (runs, jobs, durations, spans)
}

/// Every `ci.job.log` chunk in the journal, grouped by `job_id` and ordered
/// by `chunk_index` — the reconstruction a SigNoz query performs (AC1).
fn reconstruct_logs(root: &Path) -> BTreeMap<u64, Vec<crate::telemetry::CiJobLogRecord>> {
    let mut by_job: BTreeMap<u64, Vec<crate::telemetry::CiJobLogRecord>> = BTreeMap::new();
    for env in journal(root) {
        if let TelemetryRecord::CiJobLog(record) = env.record {
            by_job.entry(record.job_id).or_default().push(record);
        }
    }
    for chunks in by_job.values_mut() {
        chunks.sort_by_key(|chunk| chunk.chunk_index);
    }
    by_job
}

// ---------------------------------------------------------------------------
// AC2: --once over the synthetic org
// ---------------------------------------------------------------------------

#[test]
fn once_emits_every_run_and_job_with_correct_durations() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert!(report.repo_errors.is_empty(), "{:?}", report.repo_errors);
    assert_eq!(report.summary.repos_polled, 2, "archived repo must be skipped");
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (6, 24));
    // Each run/job unit = its record + a duration sample + a span.
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30));
    assert_no_duplicates(dir.path());
    assert!(
        !api.requests()
            .iter()
            .any(|(p, _)| p.contains("old-archive")),
        "archived repos are never polled"
    );

    for env in journal(dir.path()) {
        match env.record {
            TelemetryRecord::CiRun(r) => {
                let k = i64::try_from(r.run_id % 10).unwrap();
                assert_eq!(r.duration_ms, k * 60_000, "run {} duration", r.run_id);
                assert_eq!(r.git_ref.as_deref(), Some("main"));
                assert_eq!(r.triggered_by.as_deref(), Some("octocat"));
                assert!(env.trace_context.is_some());
            }
            TelemetryRecord::CiJob(r) => {
                let j = i64::try_from(r.job_id % 10).unwrap();
                assert_eq!(r.duration_ms, j * 10_000, "job {} duration", r.job_id);
                assert_eq!(r.timed_out, r.conclusion.as_deref() == Some("timed_out"));
            }
            TelemetryRecord::CiDuration(r) => assert!(r.duration_ms > 0),
            TelemetryRecord::Span(s) => {
                s.validate().unwrap();
                assert_eq!(
                    s.clone().bounded().attributes,
                    s.attributes,
                    "span attributes survive bounding"
                );
            }
            _ => unreachable!(),
        }
    }
    let status = state::load_status(&state_dir(dir.path()));
    assert_eq!(status.last_ok_at, Some(now()));
    assert_eq!(status.consecutive_failures, 0);
}

#[test]
fn job_spans_parent_to_their_run_span_in_one_trace() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let spans: Vec<_> = journal(dir.path())
        .into_iter()
        .filter_map(|e| match e.record {
            TelemetryRecord::Span(s) => Some(s),
            _ => None,
        })
        .collect();
    let roots: HashMap<String, _> = spans
        .iter()
        .filter(|s| s.parent_span_id.is_none())
        .map(|s| (s.attributes["loom.ci.run_id"].clone(), s))
        .collect();
    assert_eq!(roots.len(), 6);
    for job in spans.iter().filter(|s| s.parent_span_id.is_some()) {
        let root = roots[&job.attributes["loom.ci.run_id"]];
        assert_eq!(job.parent_span_id.as_ref(), Some(&root.context.span_id));
        assert_eq!(job.context.trace_id, root.context.trace_id);
    }
}

// ---------------------------------------------------------------------------
// AC3: idempotency — the money AC
// ---------------------------------------------------------------------------

#[test]
fn second_once_over_the_same_fixture_emits_zero_records() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let before = journal(dir.path()).len();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (0, 0));
    assert_eq!(journal(dir.path()).len(), before);
}

#[test]
fn killing_the_process_at_any_request_then_rerunning_emits_each_job_exactly_once() {
    // Count the requests a clean cycle makes, then kill a fresh workspace's
    // cycle at every one of them in turn (a panic unwinds out of
    // `run_cycle` mid-flight, exactly like a SIGKILL between two requests)
    // and re-run: the total must always be exactly 6 runs + 24 jobs.
    let probe = TempDir::new().unwrap();
    let clean = FixtureApi::new();
    run_cycle(&ctx(probe.path()), &clean).unwrap();
    let total = clean.requests().len();
    assert!(total > 10);
    for kill_at in 1..=total {
        let dir = TempDir::new().unwrap();
        let killer = FixtureApi::panicking_at(kill_at);
        let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = run_cycle(&ctx(dir.path()), &killer);
        }));
        assert!(crashed.is_err(), "kill point {kill_at} did not fire");
        run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
        assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30), "kill at request {kill_at}");
        assert_no_duplicates(dir.path());
    }
}

fn fixture_units(root_repo: &str, run_id: u64) -> Vec<UnitDraft> {
    let fx = fixture();
    let responses = &fx["responses"];
    let repo = RepoJson {
        name: root_repo.to_string(),
        full_name: format!("{ORG}/{root_repo}"),
        private: false,
        archived: false,
    };
    let runs: RunsPage = serde_json::from_value(
        responses[format!("repos/{ORG}/{root_repo}/actions/runs?per_page=100")]["body"].clone(),
    )
    .unwrap();
    let run = runs
        .workflow_runs
        .into_iter()
        .find(|r| r.id == run_id)
        .unwrap();
    let jobs: JobsPage = serde_json::from_value(
        responses
            [format!("repos/{ORG}/{root_repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100")]
            ["body"]
            .clone(),
    )
    .unwrap();
    let mut units: Vec<UnitDraft> = jobs
        .jobs
        .iter()
        .map(|job| UnitDraft {
            key: UnitKey::job(&repo.full_name, run.id, job.id, job.run_attempt),
            envelopes: job_envelopes(&repo, &run, job, "fixture-host"),
        })
        .collect();
    units.push(UnitDraft {
        key: UnitKey::run(&repo.full_name, run.id, run.run_attempt),
        envelopes: run_envelopes(&repo, &run, "fixture-host"),
    });
    units
}

#[test]
fn crash_between_ledger_commit_and_journal_emit_emits_exactly_once() {
    let dir = TempDir::new().unwrap();
    // Commit run 2003's units to the ledger, then "die" before emitting.
    let mut ledger = Ledger::open(state_dir(dir.path()).join("seen.jsonl")).unwrap();
    ledger.commit(fixture_units("beta", 2003)).unwrap();
    drop(ledger);
    assert!(journal(dir.path()).is_empty());

    let report = run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!(report.summary.recovered_units, 5);
    // Recovered units are not re-committed by the poll; totals are exact.
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30));
    assert_no_duplicates(dir.path());
}

#[test]
fn crash_mid_journal_write_is_repaired_and_emits_exactly_once() {
    let dir = TempDir::new().unwrap();
    let mut ledger = Ledger::open(state_dir(dir.path()).join("seen.jsonl")).unwrap();
    let committed = ledger.commit(fixture_units("alpha", 1002)).unwrap();
    // The journal write died part-way: two envelopes landed whole, the
    // third is a torn fragment with no trailing newline.
    let envelopes: Vec<_> = committed.iter().flat_map(|u| u.envelopes.clone()).collect();
    Journal::open(journal_path(dir.path()))
        .unwrap()
        .append(&envelopes[..2])
        .unwrap();
    let torn = serde_json::to_string(&envelopes[2]).unwrap();
    let mut bytes = std::fs::read(journal_path(dir.path())).unwrap();
    bytes.extend_from_slice(&torn.as_bytes()[..torn.len() / 2]);
    std::fs::write(journal_path(dir.path()), bytes).unwrap();
    drop(ledger);

    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30));
    assert_no_duplicates(dir.path());
}

// ---------------------------------------------------------------------------
// AC4: unit coverage
// ---------------------------------------------------------------------------

#[test]
fn torn_ledger_tail_is_detected_repaired_and_not_double_counted() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let ledger_path = state_dir(dir.path()).join("seen.jsonl");
    let units = Ledger::open_read_only(ledger_path.clone())
        .unwrap()
        .unit_count();
    let clean_len = std::fs::metadata(&ledger_path).unwrap().len();
    let mut bytes = std::fs::read(&ledger_path).unwrap();
    bytes.extend_from_slice(
        br#"{"type":"unit","seq":999,"repo":"fixture-org/alpha","run_id":1001,"job_id":1"#,
    );
    std::fs::write(&ledger_path, bytes).unwrap();

    // A reader never repairs (and never counts the fragment)...
    assert_eq!(
        Ledger::open_read_only(ledger_path.clone())
            .unwrap()
            .unit_count(),
        units
    );
    // ...the writer detects and truncates it back to the last full line.
    let repaired = Ledger::open(ledger_path.clone()).unwrap();
    assert!(repaired.repaired());
    assert_eq!(repaired.unit_count(), units);
    assert_eq!(std::fs::metadata(&ledger_path).unwrap().len(), clean_len);
    drop(repaired);

    let before = journal(dir.path()).len();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (0, 0));
    assert_eq!(journal(dir.path()).len(), before);
}

#[test]
fn pagination_is_followed_for_repos_runs_and_jobs() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let paths: Vec<String> = api.requests().into_iter().map(|(p, _)| p).collect();
    for page_two in [
        format!("orgs/{ORG}/repos?per_page=100&type=all&page=2"),
        format!("repos/{ORG}/alpha/actions/runs?per_page=100&page=2"),
        format!("repos/{ORG}/alpha/actions/runs/1001/jobs?filter=all&per_page=100&page=2"),
    ] {
        assert!(paths.iter().any(|p| strip_created(p) == page_two), "never followed {page_two}");
    }
}

#[test]
fn link_header_parsing_and_path_normalisation() {
    let link = r#"<https://api.github.com/organizations/9/repos?page=2>; rel="next", <https://api.github.com/organizations/9/repos?page=5>; rel="last""#;
    assert_eq!(parse_next_link(link).as_deref(), Some("organizations/9/repos?page=2"));
    assert_eq!(parse_next_link(r#"<https://x/y?page=1>; rel="prev""#), None);
    assert_eq!(normalise_api_path("https://ghe.example/api/v3/repos/o/r"), "repos/o/r");
    let raw = "HTTP/2.0 200 OK\r\nETag: W/\"e1\"\r\nLink: <https://api.github.com/repos/o/r/actions/runs?page=2>; rel=\"next\"\r\nX-RateLimit-Remaining: 42\r\n\r\n{\"workflow_runs\":[]}";
    let response = parse_raw(raw).unwrap();
    assert_eq!(response.etag.as_deref(), Some("W/\"e1\""));
    assert_eq!(response.next.as_deref(), Some("repos/o/r/actions/runs?page=2"));
    assert_eq!(response.ratelimit_remaining, Some(42));
    assert_eq!(response.body, "{\"workflow_runs\":[]}");
}

#[test]
fn created_watermark_advances_to_the_newest_run() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let first_runs = api
        .requests()
        .into_iter()
        .find(|(p, _)| p.starts_with(&format!("repos/{ORG}/alpha/actions/runs?")))
        .unwrap()
        .0;
    assert!(first_runs.contains("created=%3E%3D2026-09-19T12:00:00Z"), "{first_runs}");
    let ledger = Ledger::open_read_only(state_dir(dir.path()).join("seen.jsonl")).unwrap();
    let newest: DateTime<Utc> = "2026-09-20T11:00:00Z".parse().unwrap();
    assert_eq!(ledger.watermark(&format!("{ORG}/alpha")), Some(newest));
    assert_eq!(ledger.watermark(&format!("{ORG}/beta")), Some(newest));

    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    assert!(api
        .requests()
        .iter()
        .any(|(p, _)| p.starts_with(&format!("repos/{ORG}/alpha/actions/runs?"))
            && p.contains("created=%3E%3D2026-09-20T11:00:00Z")));
}

#[test]
fn an_in_progress_run_holds_the_watermark_and_is_emitted_once_it_completes() {
    let dir = TempDir::new().unwrap();
    let key = format!("repos/{ORG}/beta/actions/runs?per_page=100");
    let api = FixtureApi::new();
    // Newest first: index 1 is run 2002 (created 10:00).
    api.edit(&key, |entry| {
        entry["body"]["workflow_runs"][1]["status"] = Value::from("in_progress");
        entry["body"]["workflow_runs"][1]["conclusion"] = Value::Null;
    });
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.runs_emitted, 5);
    let ledger = Ledger::open_read_only(state_dir(dir.path()).join("seen.jsonl")).unwrap();
    let held: DateTime<Utc> = "2026-09-20T10:00:00Z".parse().unwrap();
    assert_eq!(ledger.watermark(&format!("{ORG}/beta")), Some(held));

    let report = run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (1, 4));
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30));
    assert_no_duplicates(dir.path());
}

#[test]
fn a_rerun_attempt_is_a_new_run_record_and_only_its_new_jobs_are_emitted() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let api = FixtureApi::new();
    let runs_key = format!("repos/{ORG}/beta/actions/runs?per_page=100");
    let jobs_key = format!("repos/{ORG}/beta/actions/runs/2003/jobs?filter=all&per_page=100");
    api.edit(&runs_key, |entry| {
        entry["body"]["workflow_runs"][0]["run_attempt"] = Value::from(2);
        entry["body"]["workflow_runs"][0]["conclusion"] = Value::from("success");
    });
    api.edit(&jobs_key, |entry| {
        let mut retry = entry["body"]["jobs"][3].clone();
        retry["id"] = Value::from(20035);
        retry["run_attempt"] = Value::from(2);
        retry["conclusion"] = Value::from("success");
        entry["body"]["jobs"].as_array_mut().unwrap().push(retry);
    });
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (1, 1));
    assert_no_duplicates(dir.path());
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!((report.summary.runs_emitted, report.summary.jobs_emitted), (0, 0));
}

#[test]
fn etag_304_on_repo_discovery_is_a_no_op_that_still_serves_the_cached_repos() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    run_cycle(&ctx(dir.path()), &api).unwrap();
    let api = FixtureApi::new();
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.summary.repos_polled, 2, "the 304 must serve the cached pages");
    let discovery: Vec<_> = api
        .requests()
        .into_iter()
        .filter(|(p, _)| p.starts_with(&format!("orgs/{ORG}/repos")))
        .collect();
    assert_eq!(discovery.len(), 2, "one conditional request per cached page");
    assert!(discovery.iter().all(|(_, etag)| etag.is_some()), "{discovery:?}");
}

#[test]
fn a_rate_limit_backs_off_the_whole_org() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.set_override(
        &format!("repos/{ORG}/beta/actions/runs?per_page=100"),
        ApiResponse {
            status: 403,
            retry_after_secs: Some(600),
            body: "{\"message\":\"You have exceeded a secondary rate limit\"}".into(),
            ..ApiResponse::default()
        },
    );
    let Err(CycleError::RateLimited { until, .. }) = run_cycle(&ctx(dir.path()), &api) else {
        panic!("a 403 secondary limit must abort the whole cycle");
    };
    assert_eq!(until, now() + Duration::seconds(600));
    let status = state::load_status(&state_dir(dir.path()));
    assert_eq!(status.backoff_until, Some(until));
    assert!(matches!(classify_health(&status, now(), 120), Health::Failing { .. }));

    // Inside the window: zero requests, for every repo.
    let quiet = FixtureApi::new();
    assert!(matches!(
        run_cycle(&ctx(dir.path()), &quiet),
        Err(CycleError::BackingOff { .. })
    ));
    assert!(quiet.requests().is_empty());

    // After it: the org is polled again and the backoff clears.
    let mut later = ctx(dir.path());
    later.now = now() + Duration::seconds(601);
    run_cycle(&later, &FixtureApi::new()).unwrap();
    assert_eq!(state::load_status(&state_dir(dir.path())).backoff_until, None);
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, 30));
}

#[test]
fn rate_limit_classification_and_backoff_arithmetic() {
    let limited = |status: u16, body: &str| {
        classify(
            ApiResponse {
                status,
                body: body.into(),
                ..ApiResponse::default()
            },
            "p",
        )
    };
    assert!(matches!(limited(429, ""), Err(ApiError::RateLimited { .. })));
    assert!(matches!(
        limited(403, "API rate limit exceeded for user"),
        Err(ApiError::RateLimited { .. })
    ));
    assert!(matches!(
        limited(403, "Resource not accessible"),
        Err(ApiError::Http { status: 403, .. })
    ));
    assert!(matches!(limited(404, ""), Err(ApiError::Http { status: 404, .. })));
    assert!(limited(304, "").is_ok());

    let t = now();
    assert_eq!(backoff_until(t, Some(30), None, 0), t + Duration::seconds(30));
    let reset = t + Duration::seconds(900);
    assert_eq!(backoff_until(t, None, Some(reset.timestamp()), 0), reset);
    assert_eq!(backoff_until(t, None, None, 0), t + Duration::seconds(60));
    assert_eq!(backoff_until(t, None, None, 2), t + Duration::seconds(240));
    assert_eq!(backoff_until(t, None, None, 40), t + Duration::seconds(3600));
}

#[test]
fn a_non_rate_limit_repo_failure_is_named_and_does_not_stop_other_repos() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    api.set_override(
        &format!("repos/{ORG}/alpha/actions/runs?per_page=100"),
        ApiResponse {
            status: 500,
            body: "boom".into(),
            ..ApiResponse::default()
        },
    );
    let report = run_cycle(&ctx(dir.path()), &api).unwrap();
    assert_eq!(report.repo_errors.len(), 1);
    assert!(
        report.repo_errors[0].contains("fixture-org/alpha")
            && report.repo_errors[0].contains("HTTP 500")
    );
    assert_eq!(report.summary.runs_emitted, 3, "beta is still polled");
    let status = state::load_status(&state_dir(dir.path()));
    assert!(status.last_error.unwrap().contains("HTTP 500"));
}

#[test]
fn excluded_repos_are_never_polled() {
    let dir = TempDir::new().unwrap();
    let api = FixtureApi::new();
    let mut context = ctx(dir.path());
    context.excluded_repos = vec!["BETA".to_string()];
    let report = run_cycle(&context, &api).unwrap();
    assert_eq!(report.summary.repos_polled, 1);
    assert!(!api.requests().iter().any(|(p, _)| p.contains("/beta/")));
}

#[test]
fn a_held_lock_makes_the_cycle_busy() {
    let dir = TempDir::new().unwrap();
    let _held = state::CycleLock::try_acquire(&state_dir(dir.path()))
        .unwrap()
        .unwrap();
    assert!(matches!(run_cycle(&ctx(dir.path()), &FixtureApi::new()), Err(CycleError::Busy)));
}

// ---------------------------------------------------------------------------
// AC5: status never reads silence as healthy
// ---------------------------------------------------------------------------

#[test]
fn status_distinguishes_never_polled_ok_stale_and_failing() {
    let t = now();
    assert_eq!(classify_health(&PollStatus::default(), t, 120), Health::NeverPolled);
    let ok = PollStatus {
        last_attempt_at: Some(t - Duration::seconds(30)),
        last_ok_at: Some(t - Duration::seconds(30)),
        ..PollStatus::default()
    };
    assert_eq!(classify_health(&ok, t, 120), Health::Ok { age_secs: 30 });
    let quiet = PollStatus {
        last_attempt_at: Some(t - Duration::seconds(3600)),
        last_ok_at: Some(t - Duration::seconds(3600)),
        ..PollStatus::default()
    };
    assert_eq!(classify_health(&quiet, t, 120), Health::Stale { age_secs: 3600 });
    let failing = PollStatus {
        last_attempt_at: Some(t),
        last_ok_at: Some(t - Duration::seconds(600)),
        last_error: Some("discovery-failed: HTTP 404".into()),
        last_error_at: Some(t),
        consecutive_failures: 3,
        ..PollStatus::default()
    };
    let Health::Failing {
        error,
        consecutive_failures,
        since,
        ..
    } = classify_health(&failing, t, 120)
    else {
        panic!("a failed last attempt must read as failing");
    };
    assert_eq!((error.as_str(), consecutive_failures), ("discovery-failed: HTTP 404", 3));
    assert_eq!(since, Some(t - Duration::seconds(600)));
}

// ---------------------------------------------------------------------------
// AC7: the emitted vocabulary matches the gateway allowlist byte-for-byte
// ---------------------------------------------------------------------------

const COLLECTOR_CONFIG: &str =
    include_str!("../../../defaults/observability/collector/config.yaml");

/// The quoted keys of the `keep_keys` list under `- context: <context>`.
fn keep_keys(context: &str) -> BTreeSet<String> {
    let mut current = String::new();
    let mut keys = BTreeSet::new();
    for line in COLLECTOR_CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            current = rest.trim().to_string();
        }
        if current == context && trimmed.contains("keep_keys(") {
            keys.extend(trimmed.split('"').skip(1).step_by(2).map(str::to_string));
        }
    }
    keys
}

fn ci_keys(keys: &BTreeSet<String>) -> BTreeSet<String> {
    keys.iter()
        .filter(|k| k.starts_with("loom.ci."))
        .cloned()
        .collect()
}

fn set(keys: &[&str]) -> BTreeSet<String> {
    keys.iter().map(|k| (*k).to_string()).collect()
}

#[test]
fn collector_allowlist_matches_the_ci_vocabulary_exactly() {
    assert_eq!(ci_keys(&keep_keys("log")), set(CI_LOG_ATTRIBUTE_KEYS));
    assert_eq!(ci_keys(&keep_keys("span")), set(CI_SPAN_ATTRIBUTE_KEYS));
    let datapoint = keep_keys("datapoint");
    for label in CI_METRIC_LABEL_KEYS {
        assert!(datapoint.contains(*label), "datapoint keep_keys lacks {label}");
    }
    for shared in ["loom.repo", "loom.repo.visibility"] {
        assert!(keep_keys("log").contains(shared) && keep_keys("span").contains(shared));
    }
    // #8825: the gateway's scrub stage is scoped by this one key. If the
    // allowlist ever dropped it, ingest would still succeed but a job log
    // would become unreconstructable (chunk ordering is the only contract).
    assert!(keep_keys("log").contains(crate::telemetry::ci::CI_LOG_CHUNK_MARKER_KEY));
    assert!(CI_LOG_ATTRIBUTE_KEYS.contains(&crate::telemetry::ci::CI_LOG_CHUNK_MARKER_KEY));
}

/// The scrub-class list in the collector config and `CI_LOG_SCRUB_CLASSES`
/// must agree, in order — the daemon-side half of #8825's "the list lives in
/// the repo, reviewable" rule (the integration test
/// `collector_fanout::gateway_scrubs_exactly_the_declared_ci_log_classes`
/// asserts the same from the other side, including the scope guards).
#[test]
fn collector_scrub_classes_match_the_declared_list() {
    // A class may need more than one pattern (github-token covers both the
    // `gh*_` prefixes and `github_pat_`), so consecutive repeats collapse —
    // but the ORDER of classes is load-bearing and is compared exactly.
    let mut markers: Vec<String> = Vec::new();
    for line in COLLECTOR_CONFIG
        .lines()
        .filter(|line| line.contains("replace_pattern(body,"))
    {
        let start = line.find("[REDACTED:").expect("a replacement marker");
        let end = line[start..].find(']').expect("a closed marker") + start;
        let class = line[start + "[REDACTED:".len()..end].to_string();
        if markers.last() != Some(&class) {
            markers.push(class);
        }
    }
    assert_eq!(
        markers,
        crate::telemetry::ci::CI_LOG_SCRUB_CLASSES
            .iter()
            .map(|c| (*c).to_string())
            .collect::<Vec<_>>()
    );
    for class in crate::telemetry::ci::CI_LOG_SCRUB_CLASSES {
        assert_eq!(crate::telemetry::ci::scrub_marker(class), format!("[REDACTED:{class}]"));
    }
}

#[test]
fn records_emit_exactly_the_declared_vocabulary() {
    let dir = TempDir::new().unwrap();
    // Capture on, so `ci.job.log`'s keys (including the truncation marker's)
    // are in the union too — the fixture exercises every optional field.
    run_cycle(&ctx_with_logs(dir.path()), &FixtureApi::new()).unwrap();
    let (mut log_keys, mut span_keys, mut labels) =
        (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
    for env in journal(dir.path()) {
        match env.record {
            TelemetryRecord::CiRun(r) => {
                log_keys.extend(r.log_attributes().into_iter().map(|(k, _)| k.to_string()))
            }
            TelemetryRecord::CiJob(r) => {
                log_keys.extend(r.log_attributes().into_iter().map(|(k, _)| k.to_string()))
            }
            TelemetryRecord::CiJobLog(r) => {
                log_keys.extend(r.log_attributes().into_iter().map(|(k, _)| k.to_string()))
            }
            TelemetryRecord::CiDuration(r) => {
                labels.extend(r.metric_labels().into_iter().map(|(k, _)| k.to_string()))
            }
            TelemetryRecord::Span(s) => {
                span_keys.extend(ci_keys(&s.attributes.keys().cloned().collect()))
            }
            _ => {}
        }
    }
    // The fixture exercises every optional field, so the union is the full set.
    assert_eq!(log_keys, set(CI_LOG_ATTRIBUTE_KEYS));
    assert_eq!(span_keys, set(CI_SPAN_ATTRIBUTE_KEYS));
    assert_eq!(labels, set(CI_METRIC_LABEL_KEYS));
}

// ---------------------------------------------------------------------------
// Config, the log-capture gate, and export
// ---------------------------------------------------------------------------

#[test]
fn config_defaults_are_flags_off_and_config_values_resolve() {
    let resolved = resolve(&CiTelemetryConfig::default());
    assert!(!resolved.enabled);
    assert_eq!(resolved.org, DEFAULT_ORG);
    assert_eq!(resolved.interval_secs, DEFAULT_INTERVAL_SECS);
    assert!(resolved.excluded_repos.is_empty());
    assert_eq!(log_capture_gate(&resolved), LogCaptureGate::Off);
    assert_eq!(resolved.log_capture_max_bytes, logs::DEFAULT_MAX_BYTES);
    assert!(resolved.log_capture_excluded_repos.is_empty());

    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"autonomous":{"ciTelemetry":{"enabled":true,"org":"acme","intervalSecs":300,"excludedRepos":[{"repo":"infra","reason":"mirror of upstream CI"}],"logCaptureEnabled":true,"logCaptureMaxBytes":131072,"logCaptureExcludedRepos":[{"repo":"vendored","reason":"third-party logs are not ours to store"}]}}}"#,
    )
    .unwrap();
    let config = read_config(dir.path());
    assert_eq!(config.org.as_deref(), Some("acme"));
    assert_eq!(config.interval_secs, Some(300));
    assert_eq!(
        config.excluded_repos,
        Some(vec![RepoExclusion {
            repo: "infra".into(),
            reason: "mirror of upstream CI".into()
        }])
    );
    assert!(config.refused_exclusions.is_empty());
    let resolved = resolve(&config);
    assert!(resolved.enabled || std::env::var(ENABLED_ENV).is_ok());
    // Phase 2 (#8825) honours the gate phase 1 refused by name.
    if std::env::var(LOG_CAPTURE_ENABLED_ENV).is_err() {
        assert_eq!(log_capture_gate(&resolved), LogCaptureGate::On);
    }
    if std::env::var(LOG_CAPTURE_MAX_BYTES_ENV).is_err() {
        assert_eq!(resolved.log_capture_max_bytes, 131_072);
    }
    assert_eq!(
        resolved.log_capture_excluded_repos,
        vec![RepoExclusion {
            repo: "vendored".into(),
            reason: "third-party logs are not ours to store".into()
        }]
    );
}

#[test]
#[serial_test::serial]
fn env_overrides_config() {
    let exclusion = RepoExclusion {
        repo: "a".into(),
        reason: "why".into(),
    };
    let config = CiTelemetryConfig {
        enabled: Some(false),
        org: Some("from-config".into()),
        interval_secs: Some(300),
        excluded_repos: Some(vec![exclusion.clone()]),
        refused_exclusions: Vec::new(),
        log_capture_enabled: Some(false),
        log_capture_max_bytes: Some(1024),
        log_capture_excluded_repos: Some(vec![exclusion.clone()]),
    };
    std::env::set_var(ENABLED_ENV, "1");
    std::env::set_var(ORG_ENV, "from-env");
    std::env::set_var(INTERVAL_SECS_ENV, "45");
    std::env::set_var(LOG_CAPTURE_ENABLED_ENV, "1");
    std::env::set_var(LOG_CAPTURE_MAX_BYTES_ENV, "2048");
    // Not recognised overrides: exclusions are committed-config-only, both
    // the record-level list and #8825's log-only one.
    std::env::set_var("LOOM_CI_TELEMETRY_EXCLUDED_REPOS", "x, y");
    std::env::set_var("LOOM_CI_TELEMETRY_LOG_CAPTURE_EXCLUDED_REPOS", "x, y");
    let resolved = resolve(&config);
    for name in [
        ENABLED_ENV,
        ORG_ENV,
        INTERVAL_SECS_ENV,
        LOG_CAPTURE_ENABLED_ENV,
        LOG_CAPTURE_MAX_BYTES_ENV,
        "LOOM_CI_TELEMETRY_EXCLUDED_REPOS",
        "LOOM_CI_TELEMETRY_LOG_CAPTURE_EXCLUDED_REPOS",
    ] {
        std::env::remove_var(name);
    }
    assert!(resolved.enabled);
    assert_eq!(resolved.org, "from-env");
    assert_eq!(resolved.interval_secs, 45);
    assert_eq!(resolved.excluded_repos, vec![exclusion.clone()]);
    assert!(resolved.log_capture_requested);
    assert_eq!(resolved.log_capture_max_bytes, 2048);
    assert_eq!(resolved.log_capture_excluded_repos, vec![exclusion]);
}

/// The log-only exclusion key gets the same discipline as `excludedRepos`:
/// committed config, a stated reason, no env tier (#8825 + the
/// ci-observability policy, which names log capture as the ONLY excludable
/// signal — so it needs its own key rather than reusing the coarse one that
/// would also drop the repo's unconditional metrics).
#[test]
fn log_capture_exclusions_need_a_reason_and_committed_config() {
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::create_dir_all(dir.path().join(".loom-local")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"autonomous":{"ciTelemetry":{"logCaptureExcludedRepos":[{"repo":"kept"},{"repo":"ok","reason":"logs carry third-party secrets"}]}}}"#,
    )
    .unwrap();
    let resolved = resolve(&read_config(dir.path()));
    assert_eq!(
        resolved.log_capture_excluded_repos,
        vec![RepoExclusion {
            repo: "ok".into(),
            reason: "logs carry third-party secrets".into()
        }]
    );
    assert!(resolved
        .refused_exclusions
        .iter()
        .any(|refusal| refusal.starts_with("kept:")));

    // A host-local tier cannot add one.
    std::fs::write(
        dir.path().join(".loom-local/local.json"),
        r#"{"autonomous":{"ciTelemetry":{"logCaptureExcludedRepos":[{"repo":"sneaky","reason":"local"}]}}}"#,
    )
    .unwrap();
    let resolved = resolve(&read_config(dir.path()));
    assert!(resolved
        .refused_exclusions
        .iter()
        .any(|refusal| refusal.contains(LOG_CAPTURE_EXCLUDED_REPOS_KEY)
            && refusal.contains("committed config")));
    assert!(resolved
        .log_capture_excluded_repos
        .iter()
        .all(|exclusion| exclusion.repo != "sneaky"));
}

/// ci-observability policy: excluding a repo suppresses its unconditional
/// run/job records, so it is a policy exception that must carry a reason in
/// committed config. Anything else is refused by name and the repo stays
/// polled — the failure direction is "capture", never "silently drop".
#[test]
fn exclusions_without_a_reason_or_outside_committed_config_are_refused() {
    let (admitted, refused) = parse_exclusions(&serde_json::json!([
        {"repo": "kept-out", "reason": "vendored mirror"},
        "bare-string",
        {"repo": "no-reason"},
        {"repo": "blank-reason", "reason": "  "},
        {"reason": "no repo"},
    ]));
    assert_eq!(
        admitted,
        vec![RepoExclusion {
            repo: "kept-out".into(),
            reason: "vendored mirror".into()
        }]
    );
    assert_eq!(refused.len(), 4, "{refused:?}");
    assert!(refused.iter().any(|r| r.contains("bare-string")));
    assert!(refused.iter().any(|r| r.starts_with("no-reason:")));
    assert!(refused.iter().any(|r| r.starts_with("blank-reason:")));
    let (admitted, refused) = parse_exclusions(&serde_json::json!("infra"));
    assert!(admitted.is_empty());
    assert_eq!(refused.len(), 1);

    // A host-local tier (`.loom-local/local.json`) cannot add an exclusion.
    let dir = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::create_dir_all(dir.path().join(".loom-local")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"autonomous":{"ciTelemetry":{"org":"acme"}}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join(".loom-local/local.json"),
        r#"{"autonomous":{"ciTelemetry":{"excludedRepos":[{"repo":"sneaky","reason":"local"}]}}}"#,
    )
    .unwrap();
    let resolved = resolve(&read_config(dir.path()));
    assert!(resolved.excluded_repos.is_empty(), "{:?}", resolved.excluded_repos);
    assert_eq!(resolved.refused_exclusions.len(), 1);
    assert!(resolved.refused_exclusions[0].contains("committed config"));
    let ctx = CycleContext::new(dir.path(), &resolved);
    assert!(ctx.excluded_repos.is_empty(), "a refused entry must exclude nothing");
}

#[test]
fn spawn_task_is_inert_when_disabled() {
    if std::env::var(ENABLED_ENV).is_ok() {
        return;
    }
    let dir = TempDir::new().unwrap();
    assert!(spawn_task(dir.path().to_path_buf()).is_none());
    assert!(!state_dir(dir.path()).exists(), "disabled means zero side effects");
}

struct VecSink(Mutex<Vec<TelemetryEnvelope>>);

impl crate::observability::queue::QueueSink for VecSink {
    fn offer(&self, envelope: TelemetryEnvelope) {
        self.0.lock().unwrap().push(envelope);
    }
    fn offer_durable(&self, envelope: TelemetryEnvelope) -> std::io::Result<()> {
        self.offer(envelope);
        Ok(())
    }
}

#[test]
fn export_backfill_offers_each_journal_line_once_and_skips_a_partial_tail() {
    let dir = TempDir::new().unwrap();
    run_cycle(&ctx(dir.path()), &FixtureApi::new()).unwrap();
    let total = journal(dir.path()).len();
    assert_eq!(export::pending_count(dir.path()), total);
    let sink = VecSink(Mutex::new(Vec::new()));
    assert_eq!(export::backfill(dir.path(), &sink), total);
    assert_eq!(export::backfill(dir.path(), &sink), 0);
    assert_eq!(export::pending_count(dir.path()), 0);
    assert_eq!(export::load_cursor(dir.path()).exported, total as u64);

    // An in-flight (newline-less) append is never half-consumed.
    let mut bytes = std::fs::read(journal_path(dir.path())).unwrap();
    bytes.extend_from_slice(b"{\"schema_version\":8,");
    std::fs::write(journal_path(dir.path()), bytes).unwrap();
    assert_eq!(export::backfill(dir.path(), &sink), 0);
    assert_eq!(sink.0.lock().unwrap().len(), total);
}
