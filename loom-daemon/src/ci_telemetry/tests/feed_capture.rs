//! Feed-driven single-run capture and the correction-floor sweep (#9201).
//!
//! The "fake feed" is the real Phase 1 payload builder
//! ([`crate::forge_events::page_payload`]) published on a real [`EventBus`],
//! exactly as `FeedClient::apply_page` publishes a verified page; the fake
//! `GithubApi` is this module's parent's recorded-fixture [`FixtureApi`].

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use serde_json::json;
use tokio::time::Instant;

use super::*;
use crate::ci_telemetry::feed::{
    feed_is_live, resolve_floor_interval_secs, spawn_bridge, Action, Driver,
    DEFAULT_FEED_FLOOR_INTERVAL_SECS, FEED_FLOOR_INTERVAL_SECS_ENV, MAX_FEED_FLOOR_INTERVAL_SECS,
};
use crate::ci_telemetry::ledger::SEEN_RETENTION_DAYS;
use crate::ci_telemetry::poll::targeted::record_runs;
use crate::event_bus::EventBus;
use crate::forge_events::keys::RunKey;
use crate::forge_events::wake::WakeCounters;
use crate::forge_events::{page_payload, BUS_TOPIC};
use crate::types::{ForgeEventsState, ForgeEventsStatus};

fn key(repo: &str, run_id: u64) -> RunKey {
    RunKey {
        repo: repo.to_string(),
        run_id,
    }
}

/// The repo object GitHub embeds in a single-run response, taken from the
/// fixture org's repo listing.
fn fixture_repo(full_name: &str) -> Value {
    let fx = fixture();
    ["", "&page=2"]
        .iter()
        .flat_map(|page| {
            fx["responses"][format!("orgs/{ORG}/repos?per_page=100&type=all{page}")]["body"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .find(|repo| repo["full_name"] == full_name)
        .unwrap()
}

/// Serve `GET repos/{repo}/actions/runs/{id}` from the fixture's runs listing
/// plus the repository object, the shape the real endpoint returns.
fn serve_single_runs(api: &FixtureApi) {
    let fx = fixture();
    let mut responses = api.responses.lock().unwrap();
    for (path, entry) in fx["responses"].as_object().unwrap() {
        let Some(runs) = entry["body"]["workflow_runs"].as_array() else {
            continue;
        };
        let repo = path.trim_start_matches("repos/");
        let repo = &repo[..repo.find("/actions").unwrap()];
        for run in runs {
            let mut body = run.clone();
            body["repository"] = fixture_repo(repo);
            let id = run["id"].as_u64().unwrap();
            responses.insert(format!("repos/{repo}/actions/runs/{id}"), json!({ "body": body }));
        }
    }
}

fn api_with_single_runs() -> FixtureApi {
    let api = FixtureApi::new();
    serve_single_runs(&api);
    api
}

fn jobs_in(repo: &str, run_id: u64) -> usize {
    let path = format!("repos/{repo}/actions/runs/{run_id}/jobs?filter=all&per_page=100");
    fixture()["responses"][path]["body"]["jobs"]
        .as_array()
        .unwrap()
        .len()
}

fn recorded_run_ids(root: &Path) -> Vec<u64> {
    journal(root)
        .into_iter()
        .filter_map(|env| match env.record {
            TelemetryRecord::CiRun(run) => Some(run.run_id),
            _ => None,
        })
        .collect()
}

/// A finished-run feed event, in the shape the operator's Worker forwards.
fn workflow_run_event(seq: u64, repo: &str, run_id: u64) -> Value {
    json!({"seq": seq, "type": "workflow_run", "action": "completed",
           "repo": repo, "run_id": run_id})
}

// ---------------------------------------------------------------------------
// Acceptance 1: a workflow_run.completed feed event records exactly that run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_workflow_run_completed_feed_event_records_exactly_that_run_within_one_feed_poll() {
    let dir = TempDir::new().unwrap();
    let bus = EventBus::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let counters = Arc::new(WakeCounters::default());
    let proven = Arc::new(AtomicBool::new(false));
    let _bridge = spawn_bridge(&bus, tx, counters.clone(), proven.clone());

    // One verified feed page, published exactly as Phase 1 publishes it.
    let events = vec![
        json!({"seq": 1, "type": "issues", "repo": "fixture-org/beta"}),
        workflow_run_event(2, "fixture-org/beta", 2002),
    ];
    bus.publish_generic(BUS_TOPIC, page_payload("fixture-host", &events))
        .unwrap();

    let keys = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("the page's run key reaches the poller without waiting for a sweep")
        .unwrap();
    assert_eq!(keys, vec![key("fixture-org/beta", 2002)]);
    assert!(proven.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(counters.prompts(), 1);

    let api = api_with_single_runs();
    let report = record_runs(&ctx(dir.path()), &api, &keys).unwrap();
    assert_eq!(report.recorded, 1, "{:?}", report.dropped);
    assert!(report.dropped.is_empty(), "{:?}", report.dropped);
    assert_eq!(recorded_run_ids(dir.path()), vec![2002], "exactly that run");
    let jobs = jobs_in("fixture-org/beta", 2002);
    let (runs, job_records, _, _) = kind_counts(dir.path());
    assert_eq!((runs, job_records), (1, jobs));
    assert_no_duplicates(dir.path());

    // Two requests — the run and its jobs — and no discovery, no runs listing.
    let paths: Vec<String> = api.requests().into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        paths,
        vec![
            "repos/fixture-org/beta/actions/runs/2002".to_string(),
            "repos/fixture-org/beta/actions/runs/2002/jobs?filter=all&per_page=100".to_string(),
        ]
    );
    assert_eq!(report.summary.requests, 2);

    // The same key again (a re-delivered page) is `Seen`: one request, no emit.
    let again = record_runs(&ctx(dir.path()), &api, &keys).unwrap();
    assert_eq!((again.recorded, again.already_seen), (0, 1));
    assert_eq!(recorded_run_ids(dir.path()), vec![2002]);
}

#[tokio::test]
async fn a_workflow_run_page_without_run_keys_proves_nothing() {
    let bus = EventBus::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let counters = Arc::new(WakeCounters::default());
    let proven = Arc::new(AtomicBool::new(false));
    let _bridge = spawn_bridge(&bus, tx, counters.clone(), proven.clone());
    // A Worker that forwards the type but no run id.
    let events = vec![json!({"seq": 1, "type": "workflow_run", "repo": "fixture-org/beta"})];
    bus.publish_generic(BUS_TOPIC, page_payload("fixture-host", &events))
        .unwrap();
    // A second page the bridge WILL forward, so the first is known processed.
    let marker = vec![workflow_run_event(2, "fixture-org/alpha", 1003)];
    bus.publish_generic(BUS_TOPIC, page_payload("fixture-host", &marker))
        .unwrap();
    let keys = rx.recv().await.unwrap();
    assert_eq!(keys, vec![key("fixture-org/alpha", 1003)]);
    assert_eq!(counters.prompts(), 2, "both pages qualify by type");
}

#[test]
fn feed_keys_outside_the_polled_scope_cost_no_request() {
    let dir = TempDir::new().unwrap();
    let api = api_with_single_runs();
    let ctx = CycleContext {
        excluded_repos: vec!["alpha".to_string()],
        ..ctx(dir.path())
    };
    let keys = [key("someone-else/repo", 1), key("fixture-org/alpha", 1003)];
    let report = record_runs(&ctx, &api, &keys).unwrap();
    assert_eq!(report.dropped.len(), 2, "{:?}", report.dropped);
    assert!(api.requests().is_empty());
    assert!(journal(dir.path()).is_empty());
}

#[test]
fn a_run_the_forge_says_is_unfinished_or_mismatched_is_not_recorded() {
    let dir = TempDir::new().unwrap();
    let api = api_with_single_runs();
    {
        let mut responses = api.responses.lock().unwrap();
        let path = "repos/fixture-org/beta/actions/runs/2001";
        responses.get_mut(path).unwrap()["body"]["status"] = json!("in_progress");
        // A feed key whose repo the forge contradicts (renamed/transferred).
        let path = "repos/fixture-org/beta/actions/runs/2003";
        responses.get_mut(path).unwrap()["body"]["repository"] = fixture_repo("fixture-org/alpha");
    }
    let keys = [
        key("fixture-org/beta", 2001),
        key("fixture-org/beta", 2003),
        key("fixture-org/beta", 9),
    ];
    let report = record_runs(&ctx(dir.path()), &api, &keys).unwrap();
    assert_eq!(report.recorded, 0);
    assert_eq!(report.dropped.len(), 3, "{:?}", report.dropped);
    assert!(journal(dir.path()).is_empty(), "the feed never supplies state");
}

// ---------------------------------------------------------------------------
// Acceptance 2: a slow correction floor loses nothing; exactly-once holds
// ---------------------------------------------------------------------------

#[test]
fn feed_capture_then_a_floor_sweep_an_hour_later_is_lossless_and_exactly_once() {
    let dir = TempDir::new().unwrap();
    let api = api_with_single_runs();
    // A lossy feed: only two of the six finished runs were ever reported.
    let delivered = [
        key("fixture-org/alpha", 1003),
        key("fixture-org/beta", 2002),
    ];
    let batch = record_runs(&ctx(dir.path()), &api, &delivered).unwrap();
    assert_eq!(batch.recorded, 2);

    // The correction floor runs 60 minutes later — past the 30-minute bar.
    let later = CycleContext {
        now: now() + Duration::minutes(60),
        ..ctx(dir.path())
    };
    let sweep = run_cycle(&later, &api).unwrap();
    assert_eq!(sweep.summary.runs_emitted, 4, "the four runs the feed dropped");
    assert_eq!(kind_counts(dir.path()), (6, 24, 30, FIXTURE_SPANS));
    assert_no_duplicates(dir.path());

    // Identical end state to pure polling.
    let polled = TempDir::new().unwrap();
    run_cycle(&ctx(polled.path()), &FixtureApi::new()).unwrap();
    let mut a = recorded_run_ids(dir.path());
    let mut b = recorded_run_ids(polled.path());
    a.sort_unstable();
    b.sort_unstable();
    assert_eq!(a, b);
}

#[test]
fn with_the_feed_dead_every_tick_sweeps_exactly_like_todays_interval() {
    let base = std::time::Duration::from_secs(120);
    let mut driver = Driver::new(base, std::time::Duration::from_secs(3600));
    let t0 = Instant::now();
    for n in 0..40 {
        assert_eq!(driver.on_tick(t0 + base * n, false), Action::Sweep, "tick {n}");
    }
}

#[test]
fn with_the_feed_driving_the_sweep_runs_at_the_floor_and_reverts_the_moment_it_dies() {
    let base = std::time::Duration::from_secs(120);
    let floor = std::time::Duration::from_secs(3600);
    let mut driver = Driver::new(base, floor);
    let t0 = Instant::now();
    let sweeps: Vec<u32> = (0..61)
        .filter(|n| driver.on_tick(t0 + base * *n, true) == Action::Sweep)
        .collect();
    assert_eq!(sweeps, vec![0, 30, 60], "one sweep per 60-minute floor");

    // The feed dies: the next tick is back on the base cadence, no grace.
    assert_eq!(driver.on_tick(t0 + base * 61, false), Action::Sweep);
    assert_eq!(driver.on_tick(t0 + base * 62, false), Action::Sweep);
}

#[test]
fn keys_batch_immediately_and_a_busy_lock_retries_them_on_the_next_tick() {
    let base = std::time::Duration::from_secs(120);
    let mut driver = Driver::new(base, std::time::Duration::from_secs(3600));
    let t0 = Instant::now();
    assert_eq!(driver.on_tick(t0, true), Action::Sweep);
    let keys = vec![key("o/r", 2), key("o/r", 1), key("o/r", 2)];
    assert_eq!(driver.on_keys(keys), Action::Batch(vec![key("o/r", 1), key("o/r", 2)]));
    driver.requeue(vec![key("o/r", 1)]);
    assert_eq!(driver.pending(), 1);
    assert_eq!(driver.on_tick(t0 + base, true), Action::Batch(vec![key("o/r", 1)]));
    assert_eq!(driver.on_tick(t0 + base * 2, true), Action::Idle);
    driver.requeue(vec![key("o/r", 3)]);
    driver.sweep_succeeded();
    assert_eq!(driver.pending(), 0, "a successful sweep covers pending keys");
}

#[test]
fn the_feed_counts_as_live_only_while_healthy_and_fresh() {
    let t = now();
    let healthy = ForgeEventsStatus {
        state: ForgeEventsState::Healthy,
        last_poll_at: Some(t - Duration::seconds(20)),
        poll_interval_secs: 10,
        ..ForgeEventsStatus::default()
    };
    assert!(feed_is_live(&healthy, t));
    let frozen = ForgeEventsStatus {
        last_poll_at: Some(t - Duration::seconds(61)),
        ..healthy.clone()
    };
    assert!(!feed_is_live(&frozen, t), "a dead feed task leaves `healthy` stale");
    for state in [
        ForgeEventsState::Backoff,
        ForgeEventsState::Failing,
        ForgeEventsState::Disabled,
    ] {
        let status = ForgeEventsStatus {
            state,
            ..healthy.clone()
        };
        assert!(!feed_is_live(&status, t), "{state:?}");
    }
}

#[test]
#[serial_test::serial]
fn the_floor_interval_resolves_and_is_clamped_inside_the_rescan_window() {
    let dir = TempDir::new().unwrap();
    std::env::remove_var(FEED_FLOOR_INTERVAL_SECS_ENV);
    assert_eq!(resolve_floor_interval_secs(dir.path(), 120), DEFAULT_FEED_FLOOR_INTERVAL_SECS);
    std::env::set_var(FEED_FLOOR_INTERVAL_SECS_ENV, "1800");
    assert_eq!(resolve_floor_interval_secs(dir.path(), 120), 1800);
    std::env::set_var(FEED_FLOOR_INTERVAL_SECS_ENV, "30");
    assert_eq!(resolve_floor_interval_secs(dir.path(), 120), 120, "never faster than base");
    std::env::set_var(FEED_FLOOR_INTERVAL_SECS_ENV, "999999");
    assert_eq!(resolve_floor_interval_secs(dir.path(), 120), MAX_FEED_FLOOR_INTERVAL_SECS);
    std::env::remove_var(FEED_FLOOR_INTERVAL_SECS_ENV);
}

#[test]
fn health_staleness_follows_the_recorded_sweep_cadence() {
    let t = now();
    let status = PollStatus {
        last_attempt_at: Some(t - Duration::minutes(50)),
        last_ok_at: Some(t - Duration::minutes(50)),
        ..PollStatus::default()
    };
    assert!(matches!(classify_health(&status, t, 120), Health::Stale { .. }));
    let floored = PollStatus {
        sweep_cadence_secs: Some(3600),
        ..status
    };
    assert!(matches!(classify_health(&floored, t, 120), Health::Ok { .. }));
}

// ---------------------------------------------------------------------------
// #11159: the feed path honours the ledger's retention boundary
// ---------------------------------------------------------------------------

const BETA: &str = "fixture-org/beta";

fn single_run_created_at(api: &FixtureApi, repo: &str, run_id: u64) -> DateTime<Utc> {
    let responses = api.responses.lock().unwrap();
    responses[&format!("repos/{repo}/actions/runs/{run_id}")]["body"]["created_at"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

/// Append a `watermark` line for `repo`, as a sweep that advanced it would.
fn advance_watermark(root: &Path, repo: &str, to: DateTime<Utc>) {
    let line = format!(
        "{{\"type\":\"watermark\",\"repo\":\"{repo}\",\"created_at\":\"{}\"}}\n",
        to.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    );
    crate::ci_telemetry::ledger::append_durable(
        &state_dir(root).join("seen.jsonl"),
        line.as_bytes(),
    )
    .unwrap();
}

#[test]
fn a_hint_for_a_run_past_the_ledgers_retention_is_refused_not_re_emitted() {
    let dir = TempDir::new().unwrap();
    let api = api_with_single_runs();
    let keys = [key(BETA, 2002)];
    let first = record_runs(&ctx_with_logs(dir.path()), &api, &keys).unwrap();
    assert_eq!(first.recorded, 1, "{:?}", first.dropped);
    let emitted = journal(dir.path()).len();

    // The sweep moves the repo's watermark far past retention; compaction
    // then drops the run's and jobs' keys, and a reopen no longer sees them.
    let path = state_dir(dir.path()).join("seen.jsonl");
    advance_watermark(dir.path(), BETA, Utc::now() + Duration::days(SEEN_RETENTION_DAYS + 15));
    let mut ledger = Ledger::open(path.clone()).unwrap();
    assert!(ledger.compact_if_large(0).unwrap(), "compaction must have run");
    drop(ledger);
    let reopened = Ledger::open(path).unwrap();
    assert!(!reopened.is_seen(&UnitKey::run(BETA, 2002, 1)), "the key expired");
    drop(reopened);

    // The same completed run is hinted again (the feed may over-report).
    let requests_before = api.requests().len();
    let again = record_runs(&ctx_with_logs(dir.path()), &api, &keys).unwrap();
    assert_eq!((again.recorded, again.already_seen), (0, 0));
    assert_eq!(again.dropped.len(), 1, "{:?}", again.dropped);
    assert!(again.dropped[0].contains("retention window"), "{:?}", again.dropped);
    assert_eq!(
        api.requests().len(),
        requests_before + 1,
        "only the run fetch: no jobs, no logs"
    );
    assert_eq!(journal(dir.path()).len(), emitted, "no run, job, artifact or log re-emitted");
    assert_no_duplicates(dir.path());
}

#[test]
fn a_recent_run_is_still_recorded_when_the_watermark_is_ahead_but_inside_retention() {
    let dir = TempDir::new().unwrap();
    let api = api_with_single_runs();
    let created = single_run_created_at(&api, BETA, 2002);
    // A watermark well ahead of the run, but within the retention window: a
    // legitimate not-yet-recorded (or re-run) run must still be captured.
    advance_watermark(dir.path(), BETA, created + Duration::days(SEEN_RETENTION_DAYS - 5));
    let report = record_runs(&ctx(dir.path()), &api, &[key(BETA, 2002)]).unwrap();
    assert_eq!(report.recorded, 1, "{:?}", report.dropped);
    assert_eq!(recorded_run_ids(dir.path()), vec![2002]);
}
