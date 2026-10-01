//! Tests for the live `session.output` emitter (Issue #9764), in the style
//! of `transcript_ingest/tests.rs`'s emission tests: fixtures seeded into a
//! temp `projects/` tree, a real sink over a real `DurableQueue`, assertions
//! read back through `peek_batch`.
//!
//! The sink global is a process-wide `OnceLock`, so every test in this file
//! shares one registration (first registration wins; it cannot be unset).
//! Isolation therefore comes from unique per-test `sessionId`s: each test
//! reads the shared queue file and filters to its own session's records.

use super::*;
use crate::activity::transcript_ingest::{LiveOutputConfig, TranscriptIngestConfig};
use crate::observability::queue::DurableQueue;
use crate::telemetry::TelemetryRecord;

const WORKSPACE: &str = "/home/ubuntu/GitHub/loom";

/// The one queue directory the process-global test sink writes to, shared by
/// every test here (see the module doc).
fn global_queue_dir() -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("loom-session-output-tests-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Install the process-global sink, pointing at the shared queue file.
/// Idempotent (first registration wins).
fn register_global_sink() {
    crate::observability::session_output::register_global_session_output_sink(
        SessionOutputSink::new(
            std::sync::Arc::new(DurableQueue::open(
                global_queue_dir().join("output-queue.jsonl"),
                100_000,
            )),
            "host-test",
        ),
    );
}

/// Every `session.output` record queued for `session_id`, in emission order.
fn queued_for(session_id: &str) -> Vec<SessionOutputRecord> {
    DurableQueue::open(global_queue_dir().join("output-queue.jsonl"), 100_000)
        .peek_batch(100_000)
        .into_iter()
        .filter_map(|e| match e.record {
            TelemetryRecord::SessionOutput(r) => Some(r),
            _ => None,
        })
        .filter(|r| r.session_id == session_id)
        .collect()
}

/// A transcript line whose `sessionId` is the test's own, so records can be
/// isolated per test despite the shared global sink.
fn seed_line(session_id: &str, ts: &str, text: &str) -> String {
    serde_json::json!({
        "type": "assistant",
        "timestamp": ts,
        "sessionId": session_id,
        "cwd": WORKSPACE,
        "message": {"role": "assistant", "content": text},
    })
    .to_string()
}

/// Seed `<projects>/<slug>/<session_id>.jsonl` in the layout Claude Code
/// writes, returning the file's path.
fn seed_file(projects: &Path, session_id: &str, lines: &[String]) -> PathBuf {
    let dir = projects.join(crate::transcript_tokens::project_slug(Path::new(WORKSPACE)));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{session_id}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    path
}

fn append(path: &Path, line: &str) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{line}").unwrap();
}

fn emitter(projects: &Path, cap: usize) -> LiveOutputEmitter {
    LiveOutputEmitter::new(projects.to_path_buf(), 30, cap)
}

/// One poll of a fresh transcript emits its new bytes as ordered chunks
/// carrying the source timestamp; the next poll emits only the bytes
/// appended since — no duplication, stable per-session sequence.
#[test]
fn a_poll_emits_new_bytes_and_the_next_poll_only_the_append() {
    register_global_sink();
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    let path = seed_file(
        &projects,
        "sess-append",
        &[seed_line("sess-append", "2026-09-18T04:00:00Z", "first")],
    );

    let mut e = emitter(&projects, 5 * 1024 * 1024);
    e.tick();
    let first = queued_for("sess-append");
    assert_eq!(first.len(), 1, "one chunk for the first poll: {first:?}");
    assert_eq!(first[0].chunk_index, 0);
    assert_eq!(first[0].chunk_count, 1);
    assert!(!first[0].truncated);
    assert!(first[0].text.starts_with('{'), "the transcript line is the body");
    assert_eq!(
        first[0].recorded_at.to_rfc3339(),
        "2026-09-18T04:00:00+00:00",
        "source timestamp, not the poll instant"
    );

    append(&path, &seed_line("sess-append", "2026-09-18T04:01:00Z", "second"));
    e.tick();
    let all = queued_for("sess-append");
    assert_eq!(all.len(), 2, "only the appended line is new: {all:?}");
    assert_eq!(all[1].chunk_index, 1);
    assert_eq!(all[1].chunk_count, 2);
    assert!(all[1].text.contains("second"));
    assert!(!all[..1].iter().any(|r| r.text.contains("second")), "no duplication");
}

/// Without a configured exporter there is no sink, and a tick writes nothing
/// anywhere near the workspace — asserted against this test's own home dir,
/// which no registered sink points at.
#[test]
fn a_tick_never_writes_outside_the_registered_sink() {
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    seed_file(
        &projects,
        "sess-quiet",
        &[seed_line("sess-quiet", "2026-09-18T04:00:00Z", "quiet")],
    );

    emitter(&projects, 5 * 1024 * 1024).tick();
    assert!(
        !home.path().join("output-queue.jsonl").exists(),
        "the emitter only ever writes through the process-global sink"
    );
}

/// Chunks are at most 8 KiB (`ci_telemetry::logs`'s `CHUNK_BYTES`), split on
/// line boundaries (a single over-long line being the documented
/// char-boundary exception), in order, with `chunk_count` = chunks-so-far.
#[test]
fn chunks_respect_the_byte_bound_and_line_boundaries() {
    register_global_sink();
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    let long_line = "x".repeat(CHUNK_BYTES * 2);
    seed_file(
        &projects,
        "sess-bounds",
        &[
            seed_line("sess-bounds", "2026-09-18T04:00:00Z", "start"),
            seed_line("sess-bounds", "2026-09-18T04:00:01Z", &long_line),
            seed_line("sess-bounds", "2026-09-18T04:00:02Z", "end"),
        ],
    );

    emitter(&projects, 5 * 1024 * 1024).tick();
    let chunks = queued_for("sess-bounds");
    assert!(chunks.len() >= 3, "a 2-chunk-sized burst splits: {}", chunks.len());
    for chunk in &chunks {
        assert!(chunk.text.len() <= CHUNK_BYTES, "chunk over the bound: {}", chunk.text.len());
        assert_eq!(chunk.chunk_count, chunk.chunk_index + 1, "chunks-so-far, this one included");
    }
    let joined: String = chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .concat();
    assert!(joined.contains("\"start\"") && joined.contains("\"end\""));
    for (index, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.chunk_index, index as u32, "order is the session sequence");
    }
}

/// The per-session cap: the chunk that straddles `max_bytes_per_session`
/// carries `truncated`, everything beyond is dropped, and
/// `output_bytes_total` still admits the gap.
#[test]
fn the_per_session_cap_sets_truncated_and_drops_the_rest() {
    register_global_sink();
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    let line = "y".repeat(200);
    let path = seed_file(
        &projects,
        "sess-cap",
        &[
            seed_line("sess-cap", "2026-09-18T04:00:00Z", &line),
            seed_line("sess-cap", "2026-09-18T04:00:01Z", &line),
            seed_line("sess-cap", "2026-09-18T04:00:02Z", &line),
        ],
    );

    // The cap lands mid-line: one force-split chunk, then nothing.
    let mut e = emitter(&projects, 150);
    e.tick();
    let chunks = queued_for("sess-cap");
    assert_eq!(chunks.len(), 1, "the cap stops the burst: {chunks:?}");
    assert!(chunks[0].truncated, "the straddling chunk reads as truncated");
    let state = e.sessions.values().next().unwrap();
    assert_eq!(state.emitted_bytes, 150, "emitted text stops exactly at the cap");
    assert!(chunks[0].output_bytes_total > 150, "observed > emitted: the gap is explicit");

    // Later appends emit nothing further; the session is done.
    append(&path, &seed_line("sess-cap", "2026-09-18T04:01:00Z", "after the cap"));
    e.tick();
    assert_eq!(queued_for("sess-cap").len(), 1, "nothing is emitted past the cap");
}

/// A `TailSet` idle-drop comeback re-reads the file from byte 0; the
/// line-position guard must skip the already-absorbed prefix instead of
/// re-emitting it, then pick up cleanly at the new bytes.
#[test]
fn a_cursor_reset_re_reads_without_duplication() {
    register_global_sink();
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    let path = seed_file(
        &projects,
        "sess-reset",
        &[seed_line("sess-reset", "2026-09-18T04:00:00Z", "one")],
    );

    let mut e = emitter(&projects, 5 * 1024 * 1024);
    e.tick();
    assert_eq!(queued_for("sess-reset").len(), 1);

    // Simulate the comeback: a fresh TailSet (cursor forgotten) over the
    // same session state — exactly what an idle drop + rewrite produces.
    let mut e = LiveOutputEmitter {
        tail: TailSet::default(),
        ..e
    };
    append(&path, &seed_line("sess-reset", "2026-09-18T04:01:00Z", "two"));
    e.tick();
    let chunks = queued_for("sess-reset");
    assert_eq!(chunks.len(), 2, "the re-read emitted only the new line: {chunks:?}");
    assert!(chunks[1].text.contains("two"));
    assert!(!chunks[1].text.contains("one"), "the absorbed prefix was skipped");
}

/// Idle files are dropped from the tracking set: after a tick whose window
/// excludes the file, the session state is gone (bounded memory).
#[test]
fn idle_files_are_dropped_from_tracking() {
    register_global_sink();
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    let path =
        seed_file(&projects, "sess-idle", &[seed_line("sess-idle", "2026-09-18T04:00:00Z", "one")]);

    let mut e = emitter(&projects, 5 * 1024 * 1024);
    // Tick 1: a 30s interval reaches back 60s; the fresh file is tracked.
    e.tick();
    assert_eq!(e.sessions.len(), 1);

    // Rewrite the mtime beyond the window: TailSet drops it, and the
    // retention pass must drop the session with it.
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3 * 60);
    std::fs::File::options()
        .append(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    e.tick();
    assert!(e.sessions.is_empty(), "idle sessions are forgotten: {:?}", e.sessions.keys());
}

/// A `subagents/` transcript composes its identity exactly like
/// `session.summary` does: the file stem is the subagent's own id (its
/// records restate only the parent's `sessionId`), the parent attached.
#[test]
fn a_subagent_transcript_carries_its_parent_identity() {
    register_global_sink();
    let home = tempfile::tempdir().unwrap();
    let projects = home.path().join("projects");
    let dir = projects.join(crate::transcript_tokens::project_slug(Path::new(WORKSPACE)));
    std::fs::create_dir_all(dir.join("uuid-parent").join("subagents")).unwrap();
    // Discovery walks parent transcripts and descends into each one's
    // `subagents/` directory, so the parent file must exist too.
    std::fs::write(
        dir.join("uuid-parent.jsonl"),
        seed_line("uuid-parent", "2026-09-18T04:00:00Z", "parent") + "\n",
    )
    .unwrap();
    let sub = dir
        .join("uuid-parent")
        .join("subagents")
        .join("agent-1.jsonl");
    std::fs::write(
        &sub,
        seed_line("uuid-parent", "2026-09-18T04:00:01Z", "subagent output") + "\n",
    )
    .unwrap();

    emitter(&projects, 5 * 1024 * 1024).tick();
    let chunks = queued_for("agent-1");
    assert_eq!(chunks.len(), 1, "{chunks:?}");
    assert_eq!(chunks[0].session_id, "agent-1");
    assert_eq!(chunks[0].parent_session_id.as_deref(), Some("uuid-parent"));
    assert_eq!(chunks[0].runtime, "claude");
}

/// The env/config knob set resolves with the FLAGS-OFF default and the
/// 15-second floor.
#[test]
fn live_output_config_resolves_off_by_default_with_an_interval_floor() {
    let config = TranscriptIngestConfig {
        live_output: Some(LiveOutputConfig {
            enabled: Some(true),
            interval_secs: Some(5),
            max_bytes_per_session: Some(1024),
        }),
        ..Default::default()
    };
    let settings = resolve_live_output_settings(&config).expect("enabled");
    assert_eq!(settings.interval_secs, 15, "below the floor clamps up");
    assert_eq!(settings.max_bytes_per_session, 1024);

    assert!(
        resolve_live_output_settings(&TranscriptIngestConfig::default()).is_none(),
        "the default is off"
    );
}

/// `whole_line_prefix` mirrors `ci_telemetry::logs`'s byte-exact semantics:
/// never splitting inside a multi-byte character, `0` when the first line
/// alone exceeds the budget.
#[test]
fn whole_line_prefix_is_byte_exact() {
    assert_eq!(whole_line_prefix("abc\ndef\n", 100), 8);
    assert_eq!(whole_line_prefix("abc\ndef\n", 3), 0, "no newline within the budget");
    assert_eq!(whole_line_prefix("abc\ndef\n", 4), 4, "the newline itself ends a line");
    // A multi-byte character straddling the cap is not split.
    let multi = "ééé\n"; // 2 bytes each
    assert_eq!(whole_line_prefix(multi, 5), 0, "5 bytes lands inside the second é");
    assert_eq!(whole_line_prefix(multi, 7), 7);
}
