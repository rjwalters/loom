//! Live-output producer tests (#9764): the opt-in gate, the
//! cannot-enable-anything-else property, run identity/separation, and the
//! loss-reporting behaviour.

#![allow(clippy::unwrap_used)]

use super::*;
use crate::telemetry::kinds::session_output::OutputCategory;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "loom-live-output-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn now() -> DateTime<Utc> {
    Utc::now()
}

// ---------------------------------------------------------------------------
// The opt-in gate
// ---------------------------------------------------------------------------

#[test]
fn live_output_is_off_unless_something_asks_for_it() {
    // No env var is read here: `resolve_enabled` consults the env, so this
    // asserts the config+default half. The env half is asserted below with
    // an explicit value, never by mutating the ambient process env (which
    // would race every other test in this binary).
    assert!(!resolve_enabled(&LiveOutputConfig::default()));
    assert!(!resolve_enabled(&LiveOutputConfig {
        enabled: Some(false),
        ..LiveOutputConfig::default()
    }));
    assert!(resolve_enabled(&LiveOutputConfig {
        enabled: Some(true),
        ..LiveOutputConfig::default()
    }));
}

#[test]
fn an_absent_config_block_resolves_to_the_documented_defaults() {
    let scratch = Scratch::new("defaults");
    let config = read_config(&scratch.0);
    assert_eq!(config, LiveOutputConfig::default());
    let resolved = resolve(&config);
    assert_eq!(resolved.interval.as_millis(), u128::from(DEFAULT_INTERVAL_MS));
    assert_eq!(resolved.heartbeat.as_secs(), DEFAULT_HEARTBEAT_SECS);
    assert_eq!(resolved.max_runs, DEFAULT_MAX_RUNS);
}

#[test]
fn the_config_block_is_read_from_loom_config_json() {
    let scratch = Scratch::new("read-config");
    std::fs::create_dir_all(scratch.path(".loom")).unwrap();
    std::fs::write(
        scratch.path(".loom/config.json"),
        r#"{"observability":{"liveOutput":{"enabled":true,"intervalMs":1500,"heartbeatSecs":10,"maxRuns":8}}}"#,
    )
    .unwrap();
    let config = read_config(&scratch.0);
    assert_eq!(config.enabled, Some(true));
    let resolved = resolve(&config);
    assert_eq!(resolved.interval.as_millis(), 1_500);
    assert_eq!(resolved.heartbeat.as_secs(), 10);
    assert_eq!(resolved.max_runs, 8);
}

#[test]
fn nonsense_knob_values_fall_back_rather_than_producing_a_zero_interval() {
    let scratch = Scratch::new("bad-knobs");
    std::fs::create_dir_all(scratch.path(".loom")).unwrap();
    std::fs::write(
        scratch.path(".loom/config.json"),
        r#"{"observability":{"liveOutput":{"enabled":true,"intervalMs":0,"heartbeatSecs":0,"maxRuns":0}}}"#,
    )
    .unwrap();
    let resolved = resolve(&read_config(&scratch.0));
    assert_eq!(resolved.interval.as_millis(), u128::from(DEFAULT_INTERVAL_MS));
    assert_eq!(resolved.heartbeat.as_secs(), DEFAULT_HEARTBEAT_SECS);
    assert_eq!(resolved.max_runs, DEFAULT_MAX_RUNS);
}

// ---------------------------------------------------------------------------
// It cannot turn anything else on
// ---------------------------------------------------------------------------

#[test]
fn enabling_live_output_does_not_add_or_change_an_exporter() {
    // The whole safety argument for this feature rests on it selecting no
    // sink of its own. `resolve_exporters` must produce the identical list
    // with live output on and off.
    let with = super::super::ObservabilityConfig {
        enabled: Some(true),
        endpoint: Some("https://example.invalid/ingest".to_string()),
        ..Default::default()
    };
    let baseline = super::super::resolve_exporters(&with);
    let scratch = Scratch::new("no-side-effect");
    std::fs::create_dir_all(scratch.path(".loom")).unwrap();
    std::fs::write(
        scratch.path(".loom/config.json"),
        r#"{"observability":{"enabled":true,"endpoint":"https://example.invalid/ingest","liveOutput":{"enabled":true}}}"#,
    )
    .unwrap();
    let read = super::super::read_config(&scratch.0);
    assert_eq!(super::super::resolve_exporters(&read), baseline);
    // And the block genuinely was read, so the assertion above is not vacuous.
    assert_eq!(read_config(&scratch.0).enabled, Some(true));
}

#[test]
fn no_otlp_queue_means_no_sink_and_therefore_no_publication() {
    assert!(SessionOutputSink::new(Vec::new(), "host").is_none());
}

#[test]
fn the_kind_is_refused_by_the_native_ingest_backend() {
    // Restated here, next to the producer, because this is the property that
    // makes "live output cannot start a managed-cloud export" true rather
    // than merely configured.
    let meta = crate::telemetry::TELEMETRY_KINDS
        .iter()
        .find(|m| m.kind == "session.output")
        .unwrap();
    assert!(!meta.native_ingest);
}

// ---------------------------------------------------------------------------
// Run identity and separation
// ---------------------------------------------------------------------------

#[test]
fn two_repos_with_the_same_issue_number_are_tracked_separately() {
    let mut tracker = Tracker::default();
    tracker
        .open("/a/loom", 42, Some("s-a".into()), Some("claude".into()), 8, now())
        .unwrap();
    tracker
        .open("/b/other", 42, Some("s-b".into()), Some("claude".into()), 8, now())
        .unwrap();
    assert_eq!(tracker.runs.len(), 2);
    assert!(tracker.close("/a/loom", 42).is_some());
    assert_eq!(tracker.runs.len(), 1);
    assert!(tracker.runs.contains_key(&("/b/other".to_string(), 42)));
}

#[test]
fn a_retry_of_the_same_issue_gets_a_fresh_attempt_index() {
    let mut tracker = Tracker::default();
    let first = tracker
        .open("/a/loom", 42, Some("s-1".into()), Some("claude".into()), 8, now())
        .unwrap()
        .identity
        .clone();
    tracker.close("/a/loom", 42);
    let second = tracker
        .open("/a/loom", 42, Some("s-2".into()), Some("claude".into()), 8, now())
        .unwrap()
        .identity
        .clone();
    assert_eq!(first.attempt, Some(1));
    assert_eq!(second.attempt, Some(2));
    assert_ne!(first.sweep_id, second.sweep_id);
}

#[test]
fn the_repo_starts_unresolved_rather_than_derived_from_the_path() {
    let mut tracker = Tracker::default();
    let run = tracker
        .open(
            "/Users/me/checkouts/loom-two/.loom/worktrees/issue-9764",
            9764,
            Some("s".into()),
            Some("claude".into()),
            8,
            now(),
        )
        .unwrap();
    assert_eq!(
        run.identity.repo, None,
        "a basename is never a forge identity; the slug is resolved from the forge"
    );
    assert_eq!(run.identity.issue, Some(9764));
}

#[test]
fn the_run_ceiling_stops_adding_runs_instead_of_growing_without_bound() {
    let mut tracker = Tracker::default();
    for issue in 0..3 {
        assert!(tracker
            .open("/a", issue, Some("s".into()), Some("claude".into()), 3, now())
            .is_some());
    }
    assert!(
        tracker
            .open("/a", 99, Some("s".into()), Some("claude".into()), 3, now())
            .is_none(),
        "a fourth run past a ceiling of 3 is refused"
    );
    // An already-tracked run is still refreshable at the ceiling.
    assert!(tracker
        .open("/a", 1, Some("s".into()), Some("claude".into()), 3, now())
        .is_some());
}

#[test]
fn an_unsupported_runtime_is_flagged_and_never_read() {
    let mut tracker = Tracker::default();
    let run = tracker
        .open("/a", 1, Some("s".into()), Some("codex".into()), 8, now())
        .unwrap();
    assert!(!run.supported());
    let scratch = Scratch::new("unsupported");
    assert!(
        advance_run(run, &scratch.0, now()).is_empty(),
        "an unsupported runtime produces no content records"
    );
    for runtime in SUPPORTED_RUNTIMES {
        let mut tracker = Tracker::default();
        let run = tracker
            .open("/a", 1, Some("s".into()), Some((*runtime).to_string()), 8, now())
            .unwrap();
        assert!(run.supported(), "{runtime}");
    }
}

#[test]
fn a_missing_runtime_degrades_to_unknown_and_is_unsupported() {
    let mut tracker = Tracker::default();
    let run = tracker.open("/a", 1, None, None, 8, now()).unwrap();
    assert_eq!(run.identity.runtime, "unknown");
    assert!(!run.supported());
}

#[test]
fn status_records_have_their_own_sequence_independent_of_any_transcript() {
    let mut tracker = Tracker::default();
    let run = tracker
        .open("/a", 1, Some("sweep-1".into()), Some("claude".into()), 8, now())
        .unwrap();
    let first = run.status(OutputCategory::Coverage, now(), Coverage::Degraded, RunState::Running);
    let second = run.status(OutputCategory::Heartbeat, now(), Coverage::Live, RunState::Idle);
    assert_eq!(first.stream_id, "sweep-1");
    assert_eq!(first.sequence, 0);
    assert_eq!(second.sequence, 1);
    assert_ne!(first.event_id, second.event_id);
}

#[test]
fn a_run_without_a_sweep_id_still_gets_a_unique_status_stream() {
    let mut tracker = Tracker::default();
    let a = tracker
        .open("/a", 7, None, Some("claude".into()), 8, now())
        .unwrap()
        .status_stream
        .clone();
    tracker.close("/a", 7);
    let b = tracker
        .open("/a", 7, None, Some("claude".into()), 8, now())
        .unwrap()
        .status_stream
        .clone();
    assert_ne!(a, b, "two attempts never share a status stream");
    assert!(a.contains("issue-7"));
}

// ---------------------------------------------------------------------------
// End-to-end over a fixture transcript directory
// ---------------------------------------------------------------------------

/// Lay out a `projects/<slug>/<session>.jsonl` fixture for `workspace_root`
/// and `issue`, and return the transcript path.
fn fixture(projects: &Path, workspace_root: &Path, issue: u32, session: &str) -> PathBuf {
    let dir = projects.join(crate::transcript_tokens::project_slug(workspace_root));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session}.jsonl"));
    std::fs::write(
        &path,
        format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"<command-name>/loom:sweep</command-name><command-args>{issue}</command-args>\"}}}}\n"
        ),
    )
    .unwrap();
    path
}

fn append_assistant(path: &Path, text: &str) {
    append_assistant_at(path, text, "2026-09-30T12:00:00.000Z");
}

/// Append one assistant-text line carrying an explicit source timestamp, so a
/// test can place a source event before or after the producer's watch start.
fn append_assistant_at(path: &Path, text: &str, timestamp: &str) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{{\"type\":\"assistant\",\"timestamp\":\"{timestamp}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}",
        serde_json::to_string(text).unwrap()
    )
    .unwrap();
}

#[test]
fn an_in_flight_run_yields_several_updates_across_successive_passes() {
    let scratch = Scratch::new("in-flight");
    let projects = scratch.path("projects");
    let workspace = scratch.path("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let transcript = fixture(&projects, &workspace, 9764, "sess-1");

    let mut tracker = Tracker::default();
    let run = tracker
        .open(
            workspace.to_str().unwrap(),
            9764,
            Some("sweep-9764-1".into()),
            Some("claude".into()),
            8,
            now(),
        )
        .unwrap();
    // The forge resolves this; the fixture sets it directly because the test
    // must not shell out to `gh`.
    run.identity.repo = Some("rjwalters/loom".to_string());

    // First pass: attaching skips the single pre-existing prompt line.
    let first = advance_run(run, &projects, now());
    assert!(first.iter().all(|r| r.category != OutputCategory::Output));

    append_assistant(&transcript, "starting the build");
    let second = advance_run(run, &projects, now());
    let outputs: Vec<_> = second
        .iter()
        .filter(|r| r.category == OutputCategory::Output)
        .collect();
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].text.as_deref(), Some("starting the build"));
    assert_eq!(outputs[0].identity.repo.as_deref(), Some("rjwalters/loom"));
    assert_eq!(outputs[0].identity.issue, Some(9764));
    assert_eq!(outputs[0].identity.session_id.as_deref(), Some("sess-1"));
    assert_eq!(outputs[0].stream_id, "sess-1");

    append_assistant(&transcript, "build finished");
    let third = advance_run(run, &projects, now());
    let outputs: Vec<_> = third
        .iter()
        .filter(|r| r.category == OutputCategory::Output)
        .collect();
    assert_eq!(outputs.len(), 1, "two updates arrived during one run");
    assert_eq!(outputs[0].text.as_deref(), Some("build finished"));
    assert!(run.ever_covered);
}

#[test]
fn a_worktree_whose_directory_name_is_unrelated_still_reports_the_forge_repo() {
    let scratch = Scratch::new("odd-dir");
    let projects = scratch.path("projects");
    // Deliberately nothing like `loom`.
    let workspace = scratch.path("scratch-checkout-42");
    std::fs::create_dir_all(&workspace).unwrap();
    let transcript = fixture(&projects, &workspace, 9764, "sess-x");
    append_assistant(&transcript, "hello from an oddly named checkout");

    let mut tracker = Tracker::default();
    let run = tracker
        .open(
            workspace.to_str().unwrap(),
            9764,
            Some("sweep-1".into()),
            Some("claude".into()),
            8,
            now(),
        )
        .unwrap();
    run.identity.repo = Some("rjwalters/loom".to_string());
    // Attach, then read the appended line.
    advance_run(run, &projects, now());
    append_assistant(&transcript, "second line");
    let records = advance_run(run, &projects, now());
    let output = records
        .iter()
        .find(|r| r.category == OutputCategory::Output)
        .unwrap();
    assert_eq!(output.identity.repo.as_deref(), Some("rjwalters/loom"));
    assert!(
        !output.stream_id.contains("scratch-checkout"),
        "a stream id is a session key, not a path: {}",
        output.stream_id
    );
}

#[test]
fn two_concurrent_runs_produce_disjoint_streams() {
    let scratch = Scratch::new("concurrent");
    let projects = scratch.path("projects");
    let ws_a = scratch.path("a");
    let ws_b = scratch.path("b");
    std::fs::create_dir_all(&ws_a).unwrap();
    std::fs::create_dir_all(&ws_b).unwrap();
    let t_a = fixture(&projects, &ws_a, 100, "sess-a");
    let t_b = fixture(&projects, &ws_b, 200, "sess-b");

    let mut tracker = Tracker::default();
    for (ws, issue, repo) in [
        (&ws_a, 100_u32, "rjwalters/loom"),
        (&ws_b, 200_u32, "rjwalters/other"),
    ] {
        let run = tracker
            .open(
                ws.to_str().unwrap(),
                issue,
                Some(format!("sweep-{issue}")),
                Some("claude".into()),
                8,
                now(),
            )
            .unwrap();
        run.identity.repo = Some(repo.to_string());
        advance_run(run, &projects, now());
    }
    append_assistant(&t_a, "work on 100");
    append_assistant(&t_b, "work on 200");

    let mut by_issue = std::collections::BTreeMap::new();
    for run in tracker.runs.values_mut() {
        for record in advance_run(run, &projects, now()) {
            if record.category == OutputCategory::Output {
                by_issue.insert(record.identity.issue, record);
            }
        }
    }
    let a = &by_issue[&Some(100)];
    let b = &by_issue[&Some(200)];
    assert_eq!(a.text.as_deref(), Some("work on 100"));
    assert_eq!(b.text.as_deref(), Some("work on 200"));
    assert_ne!(a.stream_id, b.stream_id);
    assert_ne!(a.identity.repo, b.identity.repo);
    assert_ne!(a.event_id, b.event_id);
}

#[test]
fn a_transcript_gap_is_reported_against_the_run_that_lost_it() {
    let scratch = Scratch::new("gap");
    let projects = scratch.path("projects");
    let workspace = scratch.path("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let transcript = fixture(&projects, &workspace, 9764, "sess-g");
    // Enough history that attaching has to skip some.
    for i in 0..(claude::ATTACH_TAIL_EVENTS + 3) {
        append_assistant(&transcript, &format!("line {i}"));
    }
    let mut tracker = Tracker::default();
    let run = tracker
        .open(
            workspace.to_str().unwrap(),
            9764,
            Some("sweep-1".into()),
            Some("claude".into()),
            8,
            now(),
        )
        .unwrap();
    run.identity.repo = Some("rjwalters/loom".to_string());
    let records = advance_run(run, &projects, now());
    let gap = records
        .iter()
        .find(|r| r.category == OutputCategory::Gap)
        .expect("attaching mid-run declares the backlog it skipped");
    assert_eq!(gap.gap_reason.as_deref(), Some("backlog_skipped"));
    assert!(gap.dropped_events > 0);
    assert_eq!(gap.coverage, Coverage::Degraded);
    assert_eq!(run.dropped_events, gap.dropped_events);
}

// ---------------------------------------------------------------------------
// Latency measurement (#9764 acceptance: p50/p95/max from source AND
// observation times, with historical timestamps excluded)
// ---------------------------------------------------------------------------

/// Truncate to whole milliseconds — the precision a transcript timestamp is
/// written at. A test that compares a record's `source_at` against the instant
/// it wrote must use the same precision, or it fails on the microseconds
/// `Utc::now()` carries and the RFC-3339 millis serialization drops.
fn to_millis(at: DateTime<Utc>) -> DateTime<Utc> {
    at.with_timezone(&Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        .parse::<DateTime<Utc>>()
        .unwrap()
}

/// An RFC-3339 millisecond timestamp, the shape a Claude transcript writes.
fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Open a run on a fixture transcript and return the tracker plus the paths,
/// with `repo` pre-resolved (the tests must never shell out to `gh`).
fn lag_fixture(tag: &str, session: &str) -> (Scratch, PathBuf, PathBuf, Tracker) {
    let scratch = Scratch::new(tag);
    let projects = scratch.path("projects");
    let workspace = scratch.path("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let transcript = fixture(&projects, &workspace, 9764, session);
    let tracker = Tracker::default();
    (scratch, projects, transcript, tracker)
}

#[test]
fn a_fresh_in_flight_event_is_measured_from_its_own_source_time() {
    let (_scratch, projects, transcript, mut tracker) = lag_fixture("lag-fresh", "sess-l1");
    let workspace = _scratch.path("ws");
    let opened = to_millis(now());
    let run = tracker
        .open(
            workspace.to_str().unwrap(),
            9764,
            Some("sweep-1".into()),
            Some("claude".into()),
            8,
            opened,
        )
        .unwrap();
    run.identity.repo = Some("rjwalters/loom".to_string());
    advance_run(run, &projects, opened);

    // A source event one second after the producer began watching, read two
    // seconds after it was written.
    let source = opened + chrono::Duration::seconds(1);
    append_assistant_at(&transcript, "live line", &stamp(source));
    let read_at = source + chrono::Duration::seconds(2);
    let records = advance_run(run, &projects, read_at);
    let output = records
        .iter()
        .find(|r| r.category == OutputCategory::Output)
        .unwrap();
    // The record itself carries both times unmerged.
    assert_eq!(output.source_at, source);
    assert_eq!(output.observed_at, read_at);
    assert_eq!(output.producer_lag_ms(), 2_000);

    // ...and the run's window measured exactly that one sample.
    let stats = run.lag.snapshot().expect("a fresh sample was admitted");
    assert_eq!(stats.samples, 1);
    assert_eq!(stats.p50_ms, 2_000);
    assert_eq!(stats.p95_ms, 2_000);
    assert_eq!(stats.max_ms, 2_000);
    assert_eq!(stats.historical_excluded, 0);
}

#[test]
fn a_replayed_backlog_line_is_excluded_from_the_latency_measurement() {
    let (_scratch, projects, transcript, mut tracker) = lag_fixture("lag-old", "sess-l2");
    let workspace = _scratch.path("ws");

    // History written long before this producer ever ran. Its retained tail is
    // republished on attach, but its age is not a latency.
    let ancient = to_millis(now()) - chrono::Duration::hours(3);
    for i in 0..3 {
        append_assistant_at(&transcript, &format!("old line {i}"), &stamp(ancient));
    }
    let opened = to_millis(now());
    let run = tracker
        .open(
            workspace.to_str().unwrap(),
            9764,
            Some("sweep-1".into()),
            Some("claude".into()),
            8,
            opened,
        )
        .unwrap();
    run.identity.repo = Some("rjwalters/loom".to_string());

    // Attach republishes the retained tail. Those records exist on the wire...
    let replayed = advance_run(run, &projects, opened);
    assert!(
        replayed
            .iter()
            .any(|r| r.category == OutputCategory::Output),
        "the retained tail is still published"
    );
    // ...but nothing was *measured*, because a three-hour-old timestamp is an
    // age, not a pipeline latency. Reporting p95 = 3h here would be a false
    // stall, which is exactly what the acceptance criterion forbids.
    assert_eq!(
        run.lag.snapshot(),
        None,
        "a historical timestamp was treated as a latency measurement"
    );
    assert_eq!(run.lag.historical_excluded(), 3);

    // A genuinely live event afterwards measures normally, and the exclusion
    // count stays visible beside it.
    let source = opened + chrono::Duration::seconds(1);
    append_assistant_at(&transcript, "live line", &stamp(source));
    advance_run(run, &projects, source + chrono::Duration::milliseconds(400));
    let stats = run.lag.snapshot().unwrap();
    assert_eq!(stats.samples, 1);
    assert_eq!(stats.max_ms, 400);
    assert!(stats.p95_ms < 10_000, "{stats:?} was polluted by the three-hour backlog");
    assert_eq!(stats.historical_excluded, 3);
}

#[test]
fn a_status_record_carries_the_run_latency_and_a_content_record_does_not() {
    let (_scratch, projects, transcript, mut tracker) = lag_fixture("lag-status", "sess-l3");
    let workspace = _scratch.path("ws");
    let opened = to_millis(now());
    let run = tracker
        .open(
            workspace.to_str().unwrap(),
            9764,
            Some("sweep-1".into()),
            Some("claude".into()),
            8,
            opened,
        )
        .unwrap();
    run.identity.repo = Some("rjwalters/loom".to_string());
    advance_run(run, &projects, opened);

    // Before any fresh sample a status record reports no latency at all,
    // rather than a zero that would claim one.
    let cold = run.status(OutputCategory::Heartbeat, opened, Coverage::Degraded, RunState::Idle);
    assert_eq!(cold.lag, None);

    let source = opened + chrono::Duration::seconds(1);
    append_assistant_at(&transcript, "live line", &stamp(source));
    let records = advance_run(run, &projects, source + chrono::Duration::milliseconds(750));
    let content = records
        .iter()
        .find(|r| r.category == OutputCategory::Output)
        .unwrap();
    assert_eq!(content.lag, None, "a content record must not carry the window summary");

    let warm = run.status(OutputCategory::Heartbeat, now(), Coverage::Live, RunState::Idle);
    let stats = warm.lag.expect("a heartbeat reports the run's latency");
    assert_eq!(stats.samples, 1);
    assert_eq!(stats.p95_ms, 750);
}

#[test]
fn two_attempts_of_one_issue_measure_latency_independently() {
    // A retry must not inherit the previous attempt's percentiles: each run is
    // its own window, which is what lets a dashboard show that attempt 2 is
    // healthy while attempt 1 stalled.
    let scratch = Scratch::new("lag-attempts");
    let projects = scratch.path("projects");
    let workspace = scratch.path("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let transcript = fixture(&projects, &workspace, 9764, "sess-l4");
    let mut tracker = Tracker::default();

    let opened = to_millis(now());
    {
        let run = tracker
            .open(
                workspace.to_str().unwrap(),
                9764,
                Some("sweep-a".into()),
                Some("claude".into()),
                8,
                opened,
            )
            .unwrap();
        run.identity.repo = Some("rjwalters/loom".to_string());
        advance_run(run, &projects, opened);
        let source = opened + chrono::Duration::seconds(1);
        append_assistant_at(&transcript, "slow attempt", &stamp(source));
        advance_run(run, &projects, source + chrono::Duration::seconds(8));
        assert_eq!(run.lag.snapshot().unwrap().max_ms, 8_000);
        assert_eq!(run.identity.attempt, Some(1));
    }

    // Re-opening the same (workspace, issue) is attempt 2 with a fresh window.
    let retry_opened = to_millis(now());
    let run = tracker
        .open(
            workspace.to_str().unwrap(),
            9764,
            Some("sweep-b".into()),
            Some("claude".into()),
            8,
            retry_opened,
        )
        .unwrap();
    assert_eq!(run.identity.attempt, Some(2));
    assert_eq!(run.lag.snapshot(), None, "attempt 2 inherited attempt 1's samples");
}

#[test]
fn a_run_with_no_locatable_source_is_degraded_not_silently_live() {
    let scratch = Scratch::new("no-source");
    let projects = scratch.path("projects");
    std::fs::create_dir_all(&projects).unwrap();
    let mut tracker = Tracker::default();
    let run = tracker
        .open("/nonexistent/ws", 9764, Some("s".into()), Some("claude".into()), 8, now())
        .unwrap();
    assert!(advance_run(run, &projects, now()).is_empty());
    assert!(!run.ever_covered, "no stream was found, so coverage must not claim live");
}
