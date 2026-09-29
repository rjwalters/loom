//! Extracted from `transcript_ingest.rs`'s inline test module (file-size
//! ratchet: the parent stays under the 1000-code-line threshold, #8757).

use super::*;
use crate::transcript_tokens::project_slug;

const WORKSPACE: &str = "/home/ubuntu/GitHub/loom";

fn assistant_line(id: &str, model: &str, ts: &str, input: i64, output: i64) -> String {
    serde_json::json!({
        "type": "assistant",
        "timestamp": ts,
        "sessionId": "uuid-a",
        "cwd": WORKSPACE,
        "gitBranch": "main",
        "message": {
            "model": model,
            "id": id,
            "usage": {
                "input_tokens": input,
                "output_tokens": output,
                "cache_read_input_tokens": 1000,
                "cache_creation_input_tokens": 100,
            },
        },
    })
    .to_string()
}

fn user_line(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "sessionId": "uuid-a",
        "cwd": WORKSPACE,
        "message": {"role": "user", "content": text},
    })
    .to_string()
}

/// Seed `<projects>/<slug>/<uuid>.jsonl` plus one subagent transcript, in
/// the layout Claude Code actually writes.
fn seed(projects: &Path, uuid: &str, parent: &[String], subagent: &[String]) {
    let dir = projects.join(project_slug(Path::new(WORKSPACE)));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{uuid}.jsonl")), parent.join("\n") + "\n").unwrap();
    if !subagent.is_empty() {
        let sub = dir.join(uuid).join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("agent-1.jsonl"), subagent.join("\n") + "\n").unwrap();
    }
}

fn opts(projects: &Path) -> IngestOptions {
    IngestOptions {
        projects_dir: projects.to_path_buf(),
        ..IngestOptions::default()
    }
}

fn open_db(dir: &Path) -> ActivityDb {
    ActivityDb::new(dir.join("activity.db")).unwrap()
}

fn count(db: &ActivityDb, sql: &str) -> i64 {
    db.conn.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn a_dispatch_sweeps_transcripts_populate_resource_usage_and_the_cost_views() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
            &projects,
            "uuid-a",
            &[
                user_line("<command-name>/loom:sweep</command-name>\n<command-args>8059 --claim-owned 8059</command-args>"),
                assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 10, 20),
                // A streamed repeat, which must not be counted twice.
                assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:01Z", 10, 20),
                assistant_line("msg_syn", "<synthetic>", "2026-09-18T04:00:02Z", 9999, 9999),
            ],
            &[
                user_line("You are the Loom Builder (Development Worker) for this repository."),
                assistant_line("msg_2", "claude-opus-5", "2026-09-18T04:05:00Z", 30, 40),
            ],
        );

    let db = open_db(home.path());
    let stats = ingest(&db, &opts(&projects)).unwrap();

    assert_eq!(stats.transcripts_seen, 2, "parent session + one subagent");
    assert_eq!(stats.transcripts_ingested, 2);
    assert_eq!(stats.rows_written, 2, "one (model, day) row per transcript");
    assert_eq!(stats.duplicate_records, 1, "the streamed repeat collapsed");
    assert_eq!(stats.synthetic_skipped, 1);
    assert_eq!(stats.tokens_input, 40, "10 (deduped) + 30");
    assert_eq!(stats.tokens_output, 60);

    // The table that was structurally empty on every dispatch-driven host.
    assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 2);
    // The superseded table stays unwritten (see the module doc).
    assert_eq!(count(&db, "SELECT COUNT(*) FROM token_usage"), 0);

    // Role attribution reaches resource_usage through agent_inputs, which
    // is exactly how cost_by_role groups.
    let roles: Vec<(String, i64)> = db
        .conn
        .prepare("SELECT agent_role, request_count FROM cost_by_role ORDER BY agent_role")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(roles, vec![("builder".to_string(), 1), ("sweep".to_string(), 1)]);

    let months: Vec<String> = db
        .conn
        .prepare("SELECT month FROM cost_by_month")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(months, vec!["2026-09".to_string()]);

    // Repo and issue are recoverable from the anchor row's context.
    let context: String = db
        .conn
        .query_row("SELECT context FROM agent_inputs WHERE agent_role = 'sweep'", [], |row| {
            row.get(0)
        })
        .unwrap();
    let context: serde_json::Value = serde_json::from_str(&context).unwrap();
    assert_eq!(context["repo"], "loom");
    assert_eq!(context["issue_number"], 8059);

    // Session id is recoverable from the anchor row's terminal_id.
    let terminals: Vec<String> = db
        .conn
        .prepare("SELECT terminal_id FROM agent_inputs ORDER BY terminal_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        terminals,
        vec![
            "transcript:uuid-a".to_string(),
            "transcript:uuid-a/agent-1".to_string()
        ]
    );
}

#[test]
fn re_running_over_unchanged_transcripts_writes_nothing_new() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[
            user_line("<command-name>/loom:judge</command-name>"),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 10, 20),
        ],
        &[],
    );

    let db = open_db(home.path());
    let first = ingest(&db, &opts(&projects)).unwrap();
    assert_eq!(first.rows_written, 1);

    let second = ingest(&db, &opts(&projects)).unwrap();
    assert_eq!(second.skipped_unchanged, 1);
    assert_eq!(second.rows_written, 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM agent_inputs"), 1);

    // --force re-reads it, and still does not double-count.
    let forced = ingest(
        &db,
        &IngestOptions {
            force: true,
            ..opts(&projects)
        },
    )
    .unwrap();
    assert_eq!(forced.rows_written, 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM agent_inputs"), 1);
    assert_eq!(
        count(&db, "SELECT SUM(tokens_input) FROM resource_usage"),
        10,
        "re-ingestion replaces rows rather than adding to them"
    );
}

#[test]
fn a_still_growing_transcript_has_its_rows_replaced_not_appended() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    let lines = vec![
        user_line("<command-name>/loom:sweep</command-name>\n<command-args>42</command-args>"),
        assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 10, 20),
    ];
    seed(&projects, "uuid-a", &lines, &[]);

    let db = open_db(home.path());
    ingest(&db, &opts(&projects)).unwrap();
    assert_eq!(count(&db, "SELECT SUM(tokens_input) FROM resource_usage"), 10);

    // The sweep continues and the transcript grows.
    let mut grown = lines;
    grown.push(assistant_line("msg_2", "claude-sonnet-5", "2026-09-18T04:10:00Z", 5, 5));
    seed(&projects, "uuid-a", &grown, &[]);

    let second = ingest(
        &db,
        &IngestOptions {
            force: true,
            ..opts(&projects)
        },
    )
    .unwrap();
    assert_eq!(second.rows_written, 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 1);
    assert_eq!(
        count(&db, "SELECT SUM(tokens_input) FROM resource_usage"),
        15,
        "the whole file is re-read; the first pass's row is replaced"
    );
}

#[test]
fn a_dry_run_reports_without_writing() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            10,
            20,
        )],
        &[],
    );

    let db = open_db(home.path());
    let stats = ingest(
        &db,
        &IngestOptions {
            dry_run: true,
            ..opts(&projects)
        },
    )
    .unwrap();

    assert_eq!(stats.rows_written, 1, "it reports what it would write");
    assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM transcript_ingest"), 0);
}

#[test]
fn transcripts_outside_the_window_are_not_read() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            10,
            20,
        )],
        &[],
    );

    let db = open_db(home.path());
    let stats = ingest(
        &db,
        &IngestOptions {
            since: Some(Utc::now() + chrono::Duration::hours(1)),
            ..opts(&projects)
        },
    )
    .unwrap();

    assert_eq!(stats.skipped_by_window, 1);
    assert_eq!(stats.rows_written, 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 0);
}

#[test]
fn a_transcript_with_no_usage_is_remembered_but_writes_no_anchor_row() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(&projects, "uuid-a", &[user_line("hello with no reply")], &[]);

    let db = open_db(home.path());
    let stats = ingest(&db, &opts(&projects)).unwrap();

    assert_eq!(stats.transcripts_without_usage, 1);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM agent_inputs"), 0);
    assert_eq!(count(&db, "SELECT COUNT(*) FROM resource_usage"), 0);
    // Remembered, so the next pass does not re-parse it.
    assert_eq!(count(&db, "SELECT COUNT(*) FROM transcript_ingest"), 1);
    assert_eq!(ingest(&db, &opts(&projects)).unwrap().skipped_unchanged, 1);
}

#[test]
fn workspace_scoping_ignores_other_projects() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            10,
            20,
        )],
        &[],
    );
    // A second, unrelated project directory.
    let other = projects.join(project_slug(Path::new("/home/ubuntu/GitHub/other")));
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join("uuid-b.jsonl"),
        assistant_line("msg_9", "claude-sonnet-5", "2026-09-18T04:00:00Z", 77, 77) + "\n",
    )
    .unwrap();

    let db = open_db(home.path());
    let scoped = ingest(
        &db,
        &IngestOptions {
            workspace: Some(PathBuf::from(WORKSPACE)),
            ..opts(&projects)
        },
    )
    .unwrap();
    assert_eq!(scoped.transcripts_seen, 1);
    assert_eq!(count(&db, "SELECT SUM(tokens_input) FROM resource_usage"), 10);

    // Unscoped, both projects are ingested.
    let all = ingest(&db, &opts(&projects)).unwrap();
    assert_eq!(all.transcripts_seen, 2);
    assert_eq!(count(&db, "SELECT SUM(tokens_input) FROM resource_usage"), 87);
}

/// Every env var this file's `#[serial]` tests mutate — cleared before and
/// after each one so they never leak into an unrelated test.
fn clear_ingest_env() {
    for var in [
        "LOOM_TRANSCRIPT_INGEST",
        "LOOM_TRANSCRIPT_INGEST_INTERVAL",
        "LOOM_TRANSCRIPT_INGEST_WINDOW_HOURS",
    ] {
        std::env::remove_var(var);
    }
}

#[test]
#[serial_test::serial]
fn the_background_pass_is_on_by_default_issue_8477() {
    clear_ingest_env();
    assert!(
        resolve_enabled(&TranscriptIngestConfig::default()),
        "#8477: fleet cost history must not require a hand-set env var to survive Claude \
             Code's 30-day transcript retention fuse"
    );
    assert_eq!(
        resolve_settings(&TranscriptIngestConfig::default()),
        Some((900, 24)),
        "documented defaults"
    );
    clear_ingest_env();
}

#[test]
#[serial_test::serial]
fn env_explicit_off_overrides_the_default_on() {
    clear_ingest_env();
    std::env::set_var("LOOM_TRANSCRIPT_INGEST", "0");
    assert!(
        !resolve_enabled(&TranscriptIngestConfig::default()),
        "existing opt-out still works"
    );
    assert_eq!(resolve_settings(&TranscriptIngestConfig::default()), None);
    clear_ingest_env();
}

#[test]
#[serial_test::serial]
fn env_explicit_on_still_tunes_interval_and_window() {
    clear_ingest_env();
    std::env::set_var("LOOM_TRANSCRIPT_INGEST", "1");
    std::env::set_var("LOOM_TRANSCRIPT_INGEST_INTERVAL", "300");
    std::env::set_var("LOOM_TRANSCRIPT_INGEST_WINDOW_HOURS", "6");
    assert_eq!(resolve_settings(&TranscriptIngestConfig::default()), Some((300, 6)));
    clear_ingest_env();
}

#[test]
#[serial_test::serial]
fn config_can_opt_a_host_out_with_no_env_var_set() {
    clear_ingest_env();
    let config = TranscriptIngestConfig {
        enabled: Some(false),
        ..TranscriptIngestConfig::default()
    };
    assert!(!resolve_enabled(&config), "the config-tier opt-out this issue's AC requires");
    assert_eq!(resolve_settings(&config), None);
}

#[test]
#[serial_test::serial]
fn env_takes_precedence_over_a_conflicting_config_value() {
    clear_ingest_env();
    std::env::set_var("LOOM_TRANSCRIPT_INGEST", "1");
    let config = TranscriptIngestConfig {
        enabled: Some(false),
        ..TranscriptIngestConfig::default()
    };
    assert!(resolve_enabled(&config), "env > config");
    clear_ingest_env();
}

// The next two tests mutate `config_resolver::PRIVATE_DEFAULTS_ENV`, which
// `config_resolver.rs`'s own tests already serialize under the *named*
// `loom_config_env` key (see that file's comment on issue #6177: a bare
// `#[serial]` does NOT exclude a `#[serial(loom_config_env)]` test, so
// both sides must use the same named key or they race).

#[test]
#[serial_test::serial(loom_config_env)]
fn read_transcript_ingest_config_soft_fails_on_a_repo_with_no_config_at_all() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let dir = tempfile::tempdir().unwrap();
    let config = read_transcript_ingest_config(dir.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(config, TranscriptIngestConfig::default());
}

#[test]
#[serial_test::serial(loom_config_env)]
fn read_transcript_ingest_config_reads_the_committed_block() {
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
            dir.path().join(crate::config_resolver::LEGACY_CONFIG_REL),
            r#"{"autonomous": {"transcriptIngest": {"enabled": false, "intervalSecs": 120, "windowHours": 0}}}"#,
        )
        .unwrap();
    let config = read_transcript_ingest_config(dir.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(
        config,
        TranscriptIngestConfig {
            enabled: Some(false),
            interval_secs: Some(120),
            window_hours: Some(0),
        }
    );
}

#[test]
fn collect_health_status_reports_disabled_without_touching_disk() {
    let home = tempfile::tempdir().unwrap();
    let config = TranscriptIngestConfig {
        enabled: Some(false),
        ..TranscriptIngestConfig::default()
    };
    let status = collect_health_status(
        &home.path().join("activity.db"),
        &home.path().join("projects"),
        &config,
    );
    assert!(!status.enabled);
    assert_eq!(status.newest_ingested_age_hours, None);
    assert_eq!(status.newest_transcript_age_hours, None);
}

#[test]
fn collect_health_status_reports_fresh_after_a_pass() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            10,
            20,
        )],
        &[],
    );
    let db_path = home.path().join("activity.db");
    let db = open_db(home.path());
    ingest(&db, &opts(&projects)).unwrap();
    drop(db);

    let status = collect_health_status(&db_path, &projects, &TranscriptIngestConfig::default());
    assert!(status.enabled);
    let ingested_age = status
        .newest_ingested_age_hours
        .expect("a ledger row exists after ingest()");
    assert!(ingested_age < 1.0, "just ingested: {ingested_age}");
    assert!(status.newest_transcript_age_hours.is_some());
}

#[test]
fn collect_health_status_is_none_when_nothing_was_ever_ingested() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            10,
            20,
        )],
        &[],
    );
    let status = collect_health_status(
        &home.path().join("activity.db"),
        &projects,
        &TranscriptIngestConfig::default(),
    );
    assert!(status.enabled);
    assert_eq!(status.newest_ingested_age_hours, None, "no pass has run yet");
    assert!(status.newest_transcript_age_hours.is_some(), "the transcript is still on disk");
}

/// A transcript being written right now can carry an mtime a few seconds
/// *ahead* of this process's clock — observed live on a fleet host as
/// `newestTranscriptAgeHours: -0.0039`. An age is never negative.
#[test]
fn a_future_timestamp_reports_a_zero_age_not_a_negative_one() {
    let now = Utc::now();
    assert_eq!(age_hours(now + chrono::Duration::seconds(14), now), 0.0);
    assert_eq!(age_hours(now, now), 0.0);
    let past = age_hours(now - chrono::Duration::hours(3), now);
    assert!((past - 3.0).abs() < 0.01, "an ordinary past timestamp is unaffected: {past}");
}

// ------------------------------------------------------------------
// `session.summary` emission (Issue #8757, G3 of #8714)
// ------------------------------------------------------------------

fn sink_for(dir: &Path) -> crate::observability::session_summary::SessionSummarySink {
    use crate::observability::queue::DurableQueue;
    crate::observability::session_summary::SessionSummarySink::new(
        std::sync::Arc::new(DurableQueue::open(dir.join("queue.jsonl"), 100)),
        "host-test",
    )
}

/// Issue #8757 acceptance: a fixture transcript produces exactly one
/// `session.summary` record with the documented fields populated — and
/// the subagent transcript's record carries `parent_session_id`.
#[test]
fn a_pass_emits_one_session_summary_per_ingested_transcript() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[
            user_line(
                "<command-name>/loom:sweep</command-name>\n<command-args>8757</command-args>",
            ),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 10, 20),
        ],
        &[assistant_line(
            "msg_2",
            "claude-opus-5",
            "2026-09-18T04:05:00Z",
            30,
            40,
        )],
    );

    let sink = sink_for(home.path());
    let db = open_db(home.path());
    let stats = ingest(
        &db,
        &IngestOptions {
            summary_sink: Some(sink),
            ..opts(&projects)
        },
    )
    .unwrap();

    assert_eq!(stats.session_summaries, 2, "parent + subagent");
    let envelopes = {
        use crate::observability::queue::DurableQueue;
        let queue = DurableQueue::open(home.path().join("queue.jsonl"), 100);
        queue.peek_batch(50)
    };
    let summaries: Vec<_> = envelopes
        .iter()
        .filter_map(|e| match &e.record {
            crate::telemetry::TelemetryRecord::SessionSummary(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(summaries.len(), 2, "exactly one per transcript");

    let parent = summaries
        .iter()
        .find(|r| r.parent_session_id.is_none())
        .expect("parent record");
    assert_eq!(parent.session_id, "uuid-a");
    assert_eq!(parent.role.as_deref(), Some("sweep"));
    assert_eq!(parent.issue, Some(8757));
    assert_eq!(parent.tokens_input, 10);

    let subagent = summaries
        .iter()
        .find(|r| r.parent_session_id.is_some())
        .expect("subagent record");
    assert_eq!(subagent.parent_session_id.as_deref(), Some("uuid-a"));
    assert_eq!(subagent.tokens_input, 30);

    // Every envelope carries the sink's host id and the per-kind
    // schema version.
    for envelope in &envelopes {
        assert_eq!(envelope.host_id, "host-test");
        assert_eq!(envelope.schema_version, 5);
    }
}

/// No sink configured (CLI pass / observability off): nothing is queued,
/// and the DB pass is unchanged.
#[test]
fn without_a_sink_no_session_summary_is_emitted() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            10,
            20,
        )],
        &[],
    );

    let db = open_db(home.path());
    let stats = ingest(&db, &opts(&projects)).unwrap();
    assert_eq!(stats.session_summaries, 0);
    assert!(!home.path().join("queue.jsonl").exists());
}

// ------------------------------------------------------------------
// `session.analysis` emission (Issue #8760, G3 part 2 of #8714)
// ------------------------------------------------------------------

fn analysis_sink_for(dir: &Path) -> crate::observability::session_analysis::SessionAnalysisSink {
    use crate::observability::queue::DurableQueue;
    crate::observability::session_analysis::SessionAnalysisSink::new(
        std::sync::Arc::new(DurableQueue::open(dir.join("analysis-queue.jsonl"), 100)),
        "host-test",
    )
}

/// A `session.analysis` record is pushed alongside `session.summary` when
/// both sinks are configured — one per ingested transcript, sharing the
/// same identity (`session_id`/`parent_session_id`) its sibling summary
/// carries.
#[test]
fn a_pass_emits_one_session_analysis_per_ingested_transcript() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[
            user_line(
                "<command-name>/loom:sweep</command-name>\n<command-args>8760</command-args>",
            ),
            assistant_line("msg_1", "claude-sonnet-5", "2026-09-18T04:00:00Z", 10, 20),
        ],
        &[],
    );

    let summary_sink = sink_for(home.path());
    let analysis_sink = analysis_sink_for(home.path());
    let db = open_db(home.path());
    let stats = ingest(
        &db,
        &IngestOptions {
            summary_sink: Some(summary_sink),
            analysis_sink: Some(analysis_sink),
            ..opts(&projects)
        },
    )
    .unwrap();

    assert_eq!(stats.session_summaries, 1);
    assert_eq!(stats.session_analyses, 1);

    let envelopes = {
        use crate::observability::queue::DurableQueue;
        let queue = DurableQueue::open(home.path().join("analysis-queue.jsonl"), 100);
        queue.peek_batch(50)
    };
    let analyses: Vec<_> = envelopes
        .iter()
        .filter_map(|e| match &e.record {
            crate::telemetry::TelemetryRecord::SessionAnalysis(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(analyses.len(), 1);
    assert_eq!(analyses[0].session_id, "uuid-a");
    assert_eq!(analyses[0].parent_session_id, None);
    // A single-call session has no cost of zero (an assistant usage record
    // exists) and no retry loop.
    assert!(analyses[0].cost_usd.unwrap() > 0.0);
    assert_eq!(analyses[0].retry_loops, Vec::new());

    for envelope in &envelopes {
        assert_eq!(envelope.host_id, "host-test");
        assert_eq!(envelope.schema_version, 6);
    }
}

/// `analysis_sink` and `summary_sink` are independently optional: a config
/// that only supplies one gets exactly that record kind.
#[test]
fn analysis_sink_alone_emits_no_session_summary() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            10,
            20,
        )],
        &[],
    );

    let analysis_sink = analysis_sink_for(home.path());
    let db = open_db(home.path());
    let stats = ingest(
        &db,
        &IngestOptions {
            analysis_sink: Some(analysis_sink),
            ..opts(&projects)
        },
    )
    .unwrap();

    assert_eq!(stats.session_analyses, 1);
    assert_eq!(stats.session_summaries, 0, "no summary_sink was configured");
    assert!(!home.path().join("queue.jsonl").exists());
}

/// No analysis sink configured: nothing is queued for `session.analysis`,
/// independent of `summary_sink`'s own configuration.
#[test]
fn without_an_analysis_sink_no_session_analysis_is_emitted() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed(
        &projects,
        "uuid-a",
        &[assistant_line(
            "msg_1",
            "claude-sonnet-5",
            "2026-09-18T04:00:00Z",
            10,
            20,
        )],
        &[],
    );

    let db = open_db(home.path());
    let stats = ingest(&db, &opts(&projects)).unwrap();
    assert_eq!(stats.session_analyses, 0);
    assert!(!home.path().join("analysis-queue.jsonl").exists());
}

// ------------------------------------------------------------------
// Join keys: repo slug / issue / PR / session kind / trace (Issue #9445)
// ------------------------------------------------------------------

/// Issue #9445 end-to-end, on the shape that produced the live defect: a
/// Builder subagent session whose first user message is a role prompt (no
/// `/loom:<role> <N>` argument), running in an issue worktree under a
/// checkout whose **directory name has nothing to do with the repo**.
///
/// Pre-#9445 that record carried `repo: "issue-9445"` (the cwd basename), no
/// issue, no PR, and — because the #8908 trace join keys on the issue — no
/// trace id. Every one of the four is now resolved.
#[test]
fn a_worktree_session_carries_the_repo_slug_issue_pr_and_sweep_trace() {
    use crate::observability::runtime_usage::join::{JoinEntry, JOIN_DIR};
    use crate::telemetry::trace::TraceContext;

    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");

    // A checkout named nothing like its repo (the "Superset-style" worktree
    // of the issue's acceptance criterion), plus the issue worktree in it.
    let root = home.path().join("wood-reward");
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "remote",
            "add",
            "origin",
            "git@github.com:apache/superset.git",
        ],
    ] {
        let status = std::process::Command::new("git")
            .args(&args)
            .current_dir(&root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }
    let worktree = root.join(".loom/worktrees/issue-9445");
    std::fs::create_dir_all(&worktree).unwrap();

    // The sweep's own artifacts: the checkpoint naming its PR, and the
    // persisted trace context the #8908 join reads.
    let checkpoint = root.join(".loom/sweep-checkpoint");
    std::fs::create_dir_all(&checkpoint).unwrap();
    std::fs::write(
        checkpoint.join("issue-9445.json"),
        r#"{"phase":"builder-done","pr_number":9460}"#,
    )
    .unwrap();
    let context = TraceContext::root(true);
    let joins = root.join(JOIN_DIR);
    std::fs::create_dir_all(&joins).unwrap();
    std::fs::write(
        joins.join(format!("{}-{}.json", context.trace_id.as_str(), context.span_id.as_str())),
        serde_json::to_vec(&JoinEntry {
            issue: 9445,
            context: context.clone(),
            started_at: Utc::now() - chrono::Duration::hours(1),
            ended_at: None,
        })
        .unwrap(),
    )
    .unwrap();

    // The transcript: a Builder subagent prompt, no slash-command argument,
    // cwd inside the worktree, on the issue's feature branch.
    let cwd = worktree.to_string_lossy().into_owned();
    let ts = (Utc::now() - chrono::Duration::minutes(30)).to_rfc3339();
    let line = |value: serde_json::Value| value.to_string();
    let dir = projects.join(project_slug(&worktree));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("uuid-b.jsonl"),
        [
            line(serde_json::json!({
                "type": "user", "timestamp": ts, "sessionId": "uuid-b",
                "cwd": cwd, "gitBranch": "feature/issue-9445",
                "message": {"role": "user", "content":
                    "You are the Loom Builder (Development Worker) for this repository."},
            })),
            line(serde_json::json!({
                "type": "assistant", "timestamp": ts, "sessionId": "uuid-b",
                "cwd": cwd, "gitBranch": "feature/issue-9445",
                "message": {"model": "claude-opus-5", "id": "msg_1",
                            "usage": {"input_tokens": 10, "output_tokens": 20}},
            })),
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();

    let sink = sink_for(home.path());
    let db = open_db(home.path());
    let stats = ingest(
        &db,
        &IngestOptions {
            summary_sink: Some(sink),
            ..opts(&projects)
        },
    )
    .unwrap();
    assert_eq!(stats.session_summaries, 1);

    let envelopes = {
        use crate::observability::queue::DurableQueue;
        DurableQueue::open(home.path().join("queue.jsonl"), 100).peek_batch(50)
    };
    let envelope = envelopes.first().expect("one envelope");
    let crate::telemetry::TelemetryRecord::SessionSummary(record) = &envelope.record else {
        panic!("expected a session.summary record");
    };

    assert_eq!(
        record.repo.as_deref(),
        Some("apache/superset"),
        "the forge slug, never the `wood-reward` / `issue-9445` directory names"
    );
    assert_eq!(record.issue, Some(9445), "attributed from the worktree it ran in");
    assert_eq!(record.pr_number, Some(9460), "from the issue's own sweep checkpoint");
    assert_eq!(record.session_kind, Some(crate::telemetry::SessionKind::Sweep));
    assert_eq!(record.role.as_deref(), Some("builder"));
    assert_eq!(
        envelope
            .trace_context
            .as_ref()
            .map(|c| c.trace_id.as_str().to_string()),
        Some(context.trace_id.as_str().to_string()),
        "the session nests under the sweep's own trace"
    );
}

/// The counterpart: an ordinary interactive session in a directory that is not
/// a checkout at all. No repo is invented from its name, no issue is guessed,
/// and `session_kind` says the exclusion was deliberate.
#[test]
fn an_interactive_session_is_labelled_rather_than_silently_unjoined() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    let cwd = home.path().join("Downloads");
    std::fs::create_dir_all(&cwd).unwrap();
    let dir = projects.join(project_slug(&cwd));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("uuid-c.jsonl"),
        serde_json::json!({
            "type": "assistant", "timestamp": "2026-09-18T04:00:00Z", "sessionId": "uuid-c",
            "cwd": cwd.to_string_lossy(), "gitBranch": "main",
            "message": {"model": "claude-sonnet-5", "id": "msg_1",
                        "usage": {"input_tokens": 1, "output_tokens": 2}},
        })
        .to_string()
            + "\n",
    )
    .unwrap();

    let sink = sink_for(home.path());
    let db = open_db(home.path());
    ingest(
        &db,
        &IngestOptions {
            summary_sink: Some(sink),
            ..opts(&projects)
        },
    )
    .unwrap();

    let envelopes = {
        use crate::observability::queue::DurableQueue;
        DurableQueue::open(home.path().join("queue.jsonl"), 100).peek_batch(50)
    };
    let crate::telemetry::TelemetryRecord::SessionSummary(record) =
        &envelopes.first().expect("one envelope").record
    else {
        panic!("expected a session.summary record");
    };
    assert_eq!(record.repo, None, "`Downloads` is not a repo name");
    assert_eq!(record.issue, None);
    assert_eq!(record.session_kind, Some(crate::telemetry::SessionKind::Interactive));
}
