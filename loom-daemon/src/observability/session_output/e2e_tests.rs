//! Live end-to-end verification for `session.output` (#9764).
//!
//! These tests are `#[ignore]`d: they need a reachable OTLP collector and a
//! backend behind it, which CI does not have. They are not decoration — they
//! are the only check that exercises the **whole** path rather than its pieces:
//!
//! ```text
//!   fixture transcript  (real file on disk)
//!     → claude::Cursor::advance        (real adapter)
//!     → SessionOutputRecord            (real redaction, real event ids)
//!     → TelemetryEnvelope              (real envelope)
//!     → OtlpExporter::emit_batch       (real OTLP/HTTP POST)
//!     → collector transform stages     (real gateway config)
//!     → backend                        (queryable)
//! ```
//!
//! Run them against a collector with:
//!
//! ```bash
//! LOOM_E2E_OTLP_ENDPOINT=http://127.0.0.1:14319 \
//!   cargo test -p loom-daemon --features otlp session_output::e2e \
//!   -- --ignored --nocapture
//! ```
//!
//! The `otlp` feature is required: [`OtlpExporter`] is behind it, and a plain
//! `cargo test` deliberately does not pull in `opentelemetry-proto`. That is
//! also why this module is `#[cfg(feature = "otlp")]` rather than merely
//! `#[ignore]`d — without the feature there is no real exporter to exercise,
//! and a version of this test that hand-rolled the HTTP POST would be
//! verifying its own plumbing instead of the daemon's.
//!
//! Each test prints the `event_id`s it published. Those are the join key for
//! the backend query in `defaults/docs/session-output.md` — the test asserts
//! that the **export** succeeded, and the printed ids are what a human (or a
//! follow-up query) uses to confirm the rows actually landed. Deliberately not
//! self-asserting against the backend: a ClickHouse client in this crate's
//! test deps, purely to verify a verification, is not a trade worth making.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, SecondsFormat, Utc};

use super::claude;
use crate::observability::exporter::Exporter;
use crate::observability::otlp::OtlpExporter;
use crate::telemetry::kinds::session_output::{
    Coverage, OutputCategory, RunIdentity, RunState, SessionOutputRecord,
};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// The collector to publish to. Absent means "skip" rather than "fail": an
/// ignored test that is run deliberately but without an endpoint should say
/// what is missing, not panic on a connection refused.
const ENDPOINT_ENV: &str = "LOOM_E2E_OTLP_ENDPOINT";

fn endpoint() -> Option<String> {
    std::env::var(ENDPOINT_ENV)
        .ok()
        .filter(|e| !e.trim().is_empty())
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("loom-so-e2e-{tag}-{}", std::process::id()));
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

/// A transcript whose head attributes it to `issue`, as the real discovery
/// scan requires.
fn transcript(projects: &Path, workspace: &Path, issue: u32, session: &str) -> PathBuf {
    let dir = projects.join(crate::transcript_tokens::project_slug(workspace));
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

fn append_text(path: &Path, text: &str, at: DateTime<Utc>) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}}}",
        at.to_rfc3339_opts(SecondsFormat::Millis, true),
        serde_json::to_string(text).unwrap()
    )
    .unwrap();
}

fn append_tool(path: &Path, name: &str, id: &str, at: DateTime<Utc>) {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{{\"type\":\"assistant\",\"timestamp\":\"{}\",\"message\":{{\"content\":[{{\"type\":\"tool_use\",\"id\":\"{id}\",\"name\":\"{name}\",\"input\":{{\"secret_argument\":\"ghp_abcdefghijklmnopqrstuvwxyz0123\"}}}}]}}}}",
        at.to_rfc3339_opts(SecondsFormat::Millis, true)
    )
    .unwrap();
}

fn identity(repo: &str, issue: u32, attempt: u32, sweep: &str) -> RunIdentity {
    RunIdentity {
        repo: Some(repo.to_string()),
        visibility: crate::telemetry::RepoVisibility::Private,
        session_kind: Some(crate::telemetry::SessionKind::Sweep),
        issue: Some(issue),
        sweep_id: Some(sweep.to_string()),
        session_id: None,
        attempt: Some(attempt),
        runtime: "claude".to_string(),
        role: Some("builder".to_string()),
    }
}

/// Publish `records` through the real OTLP exporter, printing the event ids.
async fn publish(label: &str, endpoint: &str, records: Vec<SessionOutputRecord>) {
    assert!(!records.is_empty(), "{label}: nothing to publish");
    for record in &records {
        println!(
            "[{label}] event_id={} category={} repo={:?} issue={:?} attempt={:?} \
             source_at={} observed_at={} lag_ms={} body={:?}",
            record.event_id,
            record.category.as_str(),
            record.identity.repo,
            record.identity.issue,
            record.identity.attempt,
            record
                .source_at
                .to_rfc3339_opts(SecondsFormat::Millis, true),
            record
                .observed_at
                .to_rfc3339_opts(SecondsFormat::Millis, true),
            record.producer_lag_ms(),
            record.text
        );
    }
    let envelopes: Vec<TelemetryEnvelope> = records
        .into_iter()
        .map(|r| TelemetryEnvelope::new("e2e-host", TelemetryRecord::SessionOutput(r)))
        .collect();
    let exporter = OtlpExporter::new(endpoint.to_string(), String::new()).unwrap();
    exporter
        .emit_batch(&envelopes)
        .await
        .unwrap_or_else(|e| panic!("{label}: export to {endpoint} failed: {e:?}"));
    println!("[{label}] exported {} records to {endpoint}", envelopes.len());
}

/// Acceptance: a real in-flight run emits at least two correlated, readable
/// output updates into an OTLP pipeline **before the run finishes**, with a
/// `coverage` record opening it and tool metadata alongside.
#[tokio::test]
#[ignore = "needs a reachable OTLP collector; set LOOM_E2E_OTLP_ENDPOINT"]
async fn an_in_flight_run_publishes_several_updates_before_it_ends() {
    let Some(endpoint) = endpoint() else {
        println!("skipped: set {ENDPOINT_ENV} to a collector base URL");
        return;
    };
    let scratch = Scratch::new("in-flight");
    let projects = scratch.path("projects");
    // Deliberately nothing like the forge repo name: the published `loom.repo`
    // must be the slug passed in, never this basename.
    let workspace = scratch.path("unrelated-checkout-name");
    std::fs::create_dir_all(&workspace).unwrap();
    let path = transcript(&projects, &workspace, 9764, "e2e-session-1");

    let streams = claude::discover(&projects, &workspace, 9764);
    assert_eq!(streams.len(), 1, "discovery found {streams:?}");
    let (stream_id, found) = streams.into_iter().next().unwrap();
    assert_eq!(found, path);

    let id = identity("rjwalters/loom", 9764, 1, "e2e-sweep-a");
    let mut cursor = claude::Cursor::default();
    let mut published = Vec::new();

    // The run opens with an explicit coverage statement, as a real run does.
    published.push(SessionOutputRecord::status(
        id.clone(),
        OutputCategory::Coverage,
        "e2e-sweep-a",
        0,
        Utc::now(),
        Coverage::Live,
        RunState::Running,
    ));
    // Attach (consumes the head line).
    cursor.advance(&path, &stream_id, &id, Utc::now());

    // Update 1, while the run is still going.
    let t1 = Utc::now();
    append_text(&path, "Update one: reading the adapter.", t1);
    let pass = cursor.advance(&path, &stream_id, &id, Utc::now());
    assert_eq!(pass.records.len(), 1, "{:?}", pass.records);
    published.extend(pass.records);

    // Update 2 — plus a tool call whose ARGUMENTS contain a token. The
    // argument must have no representation at all in what gets published.
    let t2 = t1 + Duration::milliseconds(500);
    append_text(&path, "Update two: still running.", t2);
    append_tool(&path, "Bash", "toolu_e2e_1", t2);
    let pass = cursor.advance(&path, &stream_id, &id, Utc::now());
    let categories: Vec<&str> = pass.records.iter().map(|r| r.category.as_str()).collect();
    assert_eq!(categories, vec!["output", "tool_start"], "{categories:?}");
    for record in &pass.records {
        let body = record.text.clone().unwrap_or_default();
        assert!(!body.contains("ghp_"), "a tool argument leaked: {body}");
        assert!(!body.contains("secret_argument"), "a tool argument leaked: {body}");
    }
    published.extend(pass.records);

    let outputs = published
        .iter()
        .filter(|r| r.category == OutputCategory::Output)
        .count();
    assert!(outputs >= 2, "only {outputs} output updates during one run");

    // Every content record must be distinguishable and correlated.
    let ids: std::collections::BTreeSet<&str> =
        published.iter().map(|r| r.event_id.as_str()).collect();
    assert_eq!(ids.len(), published.len(), "duplicate event ids: {ids:?}");
    for record in &published {
        assert_eq!(record.identity.repo.as_deref(), Some("rjwalters/loom"));
        assert_eq!(record.identity.issue, Some(9764));
    }

    publish("in-flight", &endpoint, published).await;
}

/// Acceptance: repo/issue/run filters separate two **concurrent issues** and
/// two **attempts of the same issue**, including a worktree whose directory
/// name is unrelated to its forge repo.
#[tokio::test]
#[ignore = "needs a reachable OTLP collector; set LOOM_E2E_OTLP_ENDPOINT"]
async fn concurrent_issues_and_repeated_attempts_stay_separable() {
    let Some(endpoint) = endpoint() else {
        println!("skipped: set {ENDPOINT_ENV} to a collector base URL");
        return;
    };
    let scratch = Scratch::new("separation");
    let projects = scratch.path("projects");
    let mut published = Vec::new();

    // Two concurrent issues, in two differently-named checkouts.
    for (dir, issue, session, sweep) in [
        ("totally-unrelated-dir", 9764_u32, "e2e-sep-a", "e2e-sweep-9764-1"),
        ("another-odd-name", 9765_u32, "e2e-sep-b", "e2e-sweep-9765-1"),
    ] {
        let workspace = scratch.path(dir);
        std::fs::create_dir_all(&workspace).unwrap();
        let path = transcript(&projects, &workspace, issue, session);
        let id = identity("rjwalters/loom", issue, 1, sweep);
        let mut cursor = claude::Cursor::default();
        cursor.advance(&path, session, &id, Utc::now());
        append_text(&path, &format!("work on issue {issue}"), Utc::now());
        let pass = cursor.advance(&path, session, &id, Utc::now());
        assert_eq!(pass.records.len(), 1);
        published.extend(pass.records);
    }

    // Two attempts of the SAME issue: same repo, same issue, different
    // attempt + sweep + session.
    let workspace = scratch.path("retry-checkout");
    std::fs::create_dir_all(&workspace).unwrap();
    for attempt in 1_u32..=2 {
        let session = format!("e2e-retry-{attempt}");
        let path = transcript(&projects, &workspace, 9766, &session);
        let id = identity("rjwalters/loom", 9766, attempt, &format!("e2e-sweep-9766-{attempt}"));
        let mut cursor = claude::Cursor::default();
        cursor.advance(&path, &session, &id, Utc::now());
        append_text(&path, &format!("attempt {attempt} of issue 9766"), Utc::now());
        let pass = cursor.advance(&path, &session, &id, Utc::now());
        assert_eq!(pass.records.len(), 1);
        published.extend(pass.records);
    }

    // Issue filter separates the concurrent pair.
    let issues: std::collections::BTreeSet<Option<u32>> =
        published.iter().map(|r| r.identity.issue).collect();
    assert_eq!(issues, [Some(9764), Some(9765), Some(9766)].into_iter().collect());

    // Attempt filter separates the retries of issue 9766 — same repo, same
    // issue, so `loom.attempt` is the only thing that can do it.
    let retries: Vec<&SessionOutputRecord> = published
        .iter()
        .filter(|r| r.identity.issue == Some(9766))
        .collect();
    assert_eq!(retries.len(), 2);
    assert_ne!(retries[0].identity.attempt, retries[1].identity.attempt);
    assert_ne!(retries[0].identity.sweep_id, retries[1].identity.sweep_id);
    assert_ne!(retries[0].stream_id, retries[1].stream_id);
    assert_ne!(retries[0].event_id, retries[1].event_id);

    // And no record's repo was ever derived from its directory name.
    for record in &published {
        assert_eq!(record.identity.repo.as_deref(), Some("rjwalters/loom"));
        assert!(
            !record.stream_id.contains("checkout") && !record.stream_id.contains("dir"),
            "a stream id leaked a path: {}",
            record.stream_id
        );
    }

    publish("separation", &endpoint, published).await;
}

/// Acceptance: a run whose runtime has no adapter publishes an explicit
/// unsupported-coverage record and no output, so it can never be read as live.
#[tokio::test]
#[ignore = "needs a reachable OTLP collector; set LOOM_E2E_OTLP_ENDPOINT"]
async fn unsupported_runtimes_publish_an_explicit_coverage_status() {
    let Some(endpoint) = endpoint() else {
        println!("skipped: set {ENDPOINT_ENV} to a collector base URL");
        return;
    };
    let mut published = Vec::new();
    for runtime in ["codex", "pi", "opencode"] {
        assert!(
            !super::SUPPORTED_RUNTIMES.contains(&runtime),
            "{runtime} is now supported; this test needs updating"
        );
        let mut id = identity("rjwalters/loom", 9764, 1, &format!("e2e-{runtime}"));
        id.runtime = runtime.to_string();
        let record = SessionOutputRecord::status(
            id,
            OutputCategory::Coverage,
            format!("e2e-{runtime}"),
            0,
            Utc::now(),
            Coverage::Unsupported,
            RunState::Running,
        );
        assert_eq!(record.coverage, Coverage::Unsupported);
        assert_eq!(record.text, None, "an unsupported run publishes no content");
        published.push(record);
    }
    // And the one runtime that IS covered says so.
    published.push(SessionOutputRecord::status(
        identity("rjwalters/loom", 9764, 1, "e2e-claude"),
        OutputCategory::Coverage,
        "e2e-claude",
        0,
        Utc::now(),
        Coverage::Live,
        RunState::Running,
    ));
    publish("coverage", &endpoint, published).await;
}

/// Acceptance: source-to-queryable latency is measured from **source and
/// observation** times, and a deliberately old source timestamp is reported as
/// a gap/backlog rather than entering the latency distribution.
#[tokio::test]
#[ignore = "needs a reachable OTLP collector; set LOOM_E2E_OTLP_ENDPOINT"]
async fn latency_is_measured_from_both_clocks_and_excludes_history() {
    let Some(endpoint) = endpoint() else {
        println!("skipped: set {ENDPOINT_ENV} to a collector base URL");
        return;
    };
    let scratch = Scratch::new("latency");
    let projects = scratch.path("projects");
    let workspace = scratch.path("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let path = transcript(&projects, &workspace, 9764, "e2e-latency");
    let id = identity("rjwalters/loom", 9764, 1, "e2e-sweep-latency");
    let mut cursor = claude::Cursor::default();
    let watch_since = Utc::now();
    cursor.advance(&path, "e2e-latency", &id, watch_since);

    let mut window = super::latency::LagWindow::default();
    let mut published = Vec::new();

    // A genuinely live event: written now, read now.
    let fresh_at = Utc::now();
    append_text(&path, "a live line", fresh_at);
    let read_at = Utc::now();
    let pass = cursor.advance(&path, "e2e-latency", &id, read_at);
    assert_eq!(pass.records.len(), 1);
    for record in &pass.records {
        // Both clocks are on the record, unmerged — this is what the backend
        // query in the docs joins on.
        assert!(record.observed_at >= record.source_at || record.producer_lag_ms() == 0);
        assert!(
            window.observe(record.source_at, record.observed_at, watch_since),
            "a live event was refused as historical"
        );
    }
    published.extend(pass.records);

    // A deliberately ancient source timestamp. It is published (it happened),
    // but its age must NOT be counted as pipeline latency.
    let ancient = watch_since - Duration::hours(6);
    append_text(&path, "a line with a six-hour-old timestamp", ancient);
    let pass = cursor.advance(&path, "e2e-latency", &id, Utc::now());
    assert_eq!(pass.records.len(), 1);
    for record in &pass.records {
        assert!(
            !window.observe(record.source_at, record.observed_at, watch_since),
            "a six-hour-old timestamp was admitted as a latency sample"
        );
    }
    published.extend(pass.records);

    let stats = window.snapshot().expect("one fresh sample was measured");
    assert_eq!(stats.samples, 1, "{stats:?}");
    assert_eq!(stats.historical_excluded, 1, "{stats:?}");
    assert!(stats.max_ms < 6 * 3_600_000, "{stats:?} was polluted by the historical event");
    println!(
        "[latency] producer p50={}ms p95={}ms max={}ms samples={} historical_excluded={}",
        stats.p50_ms, stats.p95_ms, stats.max_ms, stats.samples, stats.historical_excluded
    );

    // A heartbeat carries the distribution to the backend.
    published.push(
        SessionOutputRecord::status(
            id.clone(),
            OutputCategory::Heartbeat,
            "e2e-sweep-latency",
            1,
            Utc::now(),
            Coverage::Live,
            RunState::Idle,
        )
        .with_lag(Some(stats)),
    );

    publish("latency", &endpoint, published).await;
}

/// Acceptance (content boundary, over the wire): a session that prints real
/// credential shapes publishes none of them, and a prompt never appears at all.
#[tokio::test]
#[ignore = "needs a reachable OTLP collector; set LOOM_E2E_OTLP_ENDPOINT"]
async fn credentials_and_prompts_do_not_survive_to_the_wire() {
    let Some(endpoint) = endpoint() else {
        println!("skipped: set {ENDPOINT_ENV} to a collector base URL");
        return;
    };
    let scratch = Scratch::new("redaction");
    let projects = scratch.path("projects");
    let workspace = scratch.path("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let path = transcript(&projects, &workspace, 9764, "e2e-redact");
    let id = identity("rjwalters/loom", 9764, 1, "e2e-sweep-redact");
    let mut cursor = claude::Cursor::default();
    cursor.advance(&path, "e2e-redact", &id, Utc::now());

    const SECRETS: &[&str] = &[
        "ghp_abcdefghijklmnopqrstuvwxyz0123",
        "sk-ant-api03-abcdefghijklmnopqrstuvwxyz",
        "AKIAIOSFODNN7EXAMPLE",
        "github_pat_11ABCDEFG0abcdefghijklmnop",
    ];
    let now = Utc::now();
    append_text(
        &path,
        &format!(
            "Here is the token {} and the key {} and {} and {}",
            SECRETS[0], SECRETS[1], SECRETS[2], SECRETS[3]
        ),
        now,
    );
    // A user prompt with a secret in it. A prompt has no record shape at all,
    // so this line must produce nothing whatsoever.
    {
        use std::io::Write as _;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "{{\"type\":\"user\",\"timestamp\":\"{}\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"PROMPT_CANARY_DO_NOT_EXPORT {}\"}}]}}}}",
            now.to_rfc3339_opts(SecondsFormat::Millis, true),
            SECRETS[0]
        )
        .unwrap();
    }
    let pass = cursor.advance(&path, "e2e-redact", &id, Utc::now());
    assert_eq!(
        pass.records.len(),
        1,
        "the prompt line must produce no record: {:?}",
        pass.records
    );
    let body = pass.records[0].text.clone().unwrap();
    for secret in SECRETS {
        assert!(!body.contains(secret), "{secret} survived: {body}");
    }
    assert!(!body.contains("PROMPT_CANARY_DO_NOT_EXPORT"), "{body}");
    assert!(body.contains("[REDACTED:"), "nothing was marked: {body}");
    println!("[redaction] published body: {body}");

    publish("redaction", &endpoint, pass.records).await;
}
