//! Claude live-output adapter tests (#9764).
//!
//! Every case drives a real file on disk through [`Cursor::advance`], because
//! the behaviours that matter — incremental reads, torn trailing lines,
//! truncation, attach-time backlog — are properties of reading a growing file,
//! not of a parser over a string.

#![allow(clippy::unwrap_used)]

use std::io::Write as _;
use std::path::PathBuf;

use chrono::{TimeZone, Utc};

use super::*;
use crate::telemetry::kinds::session_output::{OutputCategory, RunIdentity};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "loom-session-output-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn append(path: &std::path::Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

fn identity() -> RunIdentity {
    RunIdentity {
        repo: Some("rjwalters/loom".to_string()),
        visibility: crate::telemetry::RepoVisibility::Private,
        session_kind: Some(crate::telemetry::SessionKind::Sweep),
        issue: Some(9764),
        sweep_id: Some("sweep-issue-9764-1".to_string()),
        session_id: Some("sess".to_string()),
        attempt: Some(1),
        runtime: "claude".to_string(),
        role: Some("builder".to_string()),
        launch: crate::telemetry::kinds::session_output::Launch::Daemon,
    }
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap()
}

fn assistant_text(text: &str) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"2026-09-30T11:59:58.000Z","message":{{"content":[{{"type":"text","text":{}}}]}}}}"#,
        serde_json::to_string(text).unwrap()
    ) + "\n"
}

fn assistant_thinking(text: &str) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"2026-09-30T11:59:58.000Z","message":{{"content":[{{"type":"thinking","thinking":{},"signature":"x"}}]}}}}"#,
        serde_json::to_string(text).unwrap()
    ) + "\n"
}

fn assistant_tool_use(id: &str, name: &str, input: &str) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"2026-09-30T11:59:58.000Z","message":{{"content":[{{"type":"tool_use","id":"{id}","name":"{name}","input":{{"command":{}}}}}]}}}}"#,
        serde_json::to_string(input).unwrap()
    ) + "\n"
}

fn user_tool_result(id: &str, content: &str, is_error: bool) -> String {
    format!(
        r#"{{"type":"user","timestamp":"2026-09-30T11:59:59.000Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"{id}","is_error":{is_error},"content":{}}}]}}}}"#,
        serde_json::to_string(content).unwrap()
    ) + "\n"
}

fn user_prompt(text: &str) -> String {
    format!(
        r#"{{"type":"user","timestamp":"2026-09-30T11:59:57.000Z","message":{{"content":[{{"type":"text","text":{}}}]}}}}"#,
        serde_json::to_string(text).unwrap()
    ) + "\n"
}

#[test]
fn only_assistant_text_and_tool_metadata_ever_become_records() {
    let scratch = Scratch::new("content-boundary");
    let path = scratch.file("t.jsonl");
    append(&path, &user_prompt("MY SECRET PROMPT about acquisition plans"));
    append(&path, &assistant_thinking("INTERNAL REASONING nobody should see"));
    append(&path, &assistant_text("Reading the file now."));
    append(&path, &assistant_tool_use("tu_1", "Bash", "cat ~/.ssh/id_rsa"));
    append(&path, &user_tool_result("tu_1", "ssh-rsa AAAAB3NzaC1yc2EAAAA", false));
    append(
        &path,
        "{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"content\":\"/loom:sweep 9764\"}\n",
    );

    let mut cursor = Cursor::default();
    let pass = cursor.advance(&path, "sess", &identity(), now());

    let categories: Vec<_> = pass.records.iter().map(|r| r.category).collect();
    assert_eq!(
        categories,
        vec![
            OutputCategory::Output,
            OutputCategory::ToolStart,
            OutputCategory::ToolFinish
        ],
        "the prompt, the thinking block and the bookkeeping record produced nothing"
    );

    let all_text: String = pass
        .records
        .iter()
        .map(|r| {
            format!("{}|{}", r.text.clone().unwrap_or_default(), r.tool.clone().unwrap_or_default())
        })
        .collect();
    for excluded in [
        "MY SECRET PROMPT",
        "INTERNAL REASONING",
        "cat ~/.ssh/id_rsa",
        "ssh-rsa AAAAB3NzaC1yc2E",
    ] {
        assert!(!all_text.contains(excluded), "{excluded} leaked: {all_text}");
    }
    assert_eq!(pass.records[1].tool.as_deref(), Some("Bash"));
    assert_eq!(pass.records[2].tool.as_deref(), Some("Bash"));
    assert_eq!(pass.records[2].tool_ok, Some(true));
}

#[test]
fn a_failed_tool_is_reported_as_such_without_its_output() {
    let scratch = Scratch::new("tool-error");
    let path = scratch.file("t.jsonl");
    append(&path, &assistant_tool_use("tu_9", "Read", "/etc/shadow"));
    append(&path, &user_tool_result("tu_9", "permission denied: /etc/shadow", true));
    let mut cursor = Cursor::default();
    let pass = cursor.advance(&path, "sess", &identity(), now());
    let finish = pass
        .records
        .iter()
        .find(|r| r.category == OutputCategory::ToolFinish)
        .unwrap();
    assert_eq!(finish.tool_ok, Some(false));
    assert_eq!(finish.text, None);
}

#[test]
fn successive_passes_read_only_what_was_appended() {
    let scratch = Scratch::new("incremental");
    let path = scratch.file("t.jsonl");
    append(&path, &assistant_text("first"));
    let mut cursor = Cursor::default();
    let first = cursor.advance(&path, "sess", &identity(), now());
    assert_eq!(first.records.len(), 1);
    assert_eq!(first.records[0].sequence, 0);

    // Nothing new: an empty pass, not a re-read.
    let quiet = cursor.advance(&path, "sess", &identity(), now());
    assert!(quiet.records.is_empty());

    append(&path, &assistant_text("second"));
    let second = cursor.advance(&path, "sess", &identity(), now());
    assert_eq!(second.records.len(), 1);
    assert_eq!(second.records[0].sequence, 1);
    assert_eq!(second.records[0].text.as_deref(), Some("second"));
    assert_ne!(first.records[0].event_id, second.records[0].event_id);
}

#[test]
fn a_torn_trailing_line_is_held_until_its_newline_arrives() {
    let scratch = Scratch::new("torn");
    let path = scratch.file("t.jsonl");
    let full = assistant_text("complete thought");
    let (head, tail) = full.split_at(full.len() / 2);
    append(&path, head);

    let mut cursor = Cursor::default();
    let partial = cursor.advance(&path, "sess", &identity(), now());
    assert!(partial.records.is_empty(), "a half-written line yields nothing");

    append(&path, tail);
    let complete = cursor.advance(&path, "sess", &identity(), now());
    assert_eq!(complete.records.len(), 1);
    assert_eq!(complete.records[0].text.as_deref(), Some("complete thought"));
    assert_eq!(complete.records[0].sequence, 0, "the line index is still 0");
}

#[test]
fn attaching_to_an_existing_transcript_keeps_the_tail_and_declares_the_gap() {
    let scratch = Scratch::new("backlog");
    let path = scratch.file("t.jsonl");
    for i in 0..(ATTACH_TAIL_EVENTS + 5) {
        append(&path, &assistant_text(&format!("line {i}")));
    }
    let mut cursor = Cursor::default();
    let pass = cursor.advance(&path, "sess", &identity(), now());
    assert_eq!(pass.gap, Some(("backlog_skipped".to_string(), 5)));
    assert_eq!(pass.records.len(), ATTACH_TAIL_EVENTS);
    // Sequences are still true line numbers, so an id computed here matches
    // one computed by a producer that saw the whole file.
    assert_eq!(pass.records[0].sequence, 5);
    assert_eq!(pass.records[0].text.as_deref(), Some("line 5"));
}

#[test]
fn a_transcript_that_shrinks_reports_a_gap_and_resumes() {
    let scratch = Scratch::new("truncate");
    let path = scratch.file("t.jsonl");
    append(&path, &assistant_text("before"));
    let mut cursor = Cursor::default();
    assert_eq!(
        cursor
            .advance(&path, "sess", &identity(), now())
            .records
            .len(),
        1
    );

    std::fs::write(&path, assistant_text("after")).unwrap();
    let pass = cursor.advance(&path, "sess", &identity(), now());
    assert_eq!(pass.gap, Some(("source_truncated".to_string(), 0)));
    assert_eq!(pass.records.len(), 1);
    assert_eq!(pass.records[0].text.as_deref(), Some("after"));
}

#[test]
fn a_replayed_read_reproduces_the_same_event_ids() {
    let scratch = Scratch::new("replay");
    let path = scratch.file("t.jsonl");
    for i in 0..3 {
        append(&path, &assistant_text(&format!("line {i}")));
    }
    let ids = |cursor: &mut Cursor| -> Vec<String> {
        cursor
            .advance(&path, "sess", &identity(), now())
            .records
            .iter()
            .map(|r| r.event_id.clone())
            .collect()
    };
    let first = ids(&mut Cursor::default());
    // A fresh producer (daemon restart) reading the same file from scratch.
    let second = ids(&mut Cursor::default());
    assert_eq!(first, second);
    assert_eq!(first.len(), 3);
    assert_eq!(
        first
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        3,
        "ids are unique within the stream"
    );
}

#[test]
fn a_missing_file_is_an_empty_pass_not_an_error() {
    let scratch = Scratch::new("missing");
    let mut cursor = Cursor::default();
    let pass = cursor.advance(&scratch.file("nope.jsonl"), "sess", &identity(), now());
    assert!(pass.records.is_empty());
    assert_eq!(pass.gap, None);
}

#[test]
fn malformed_lines_are_skipped_without_disturbing_line_numbering() {
    let scratch = Scratch::new("malformed");
    let path = scratch.file("t.jsonl");
    append(&path, "{not json at all\n");
    append(&path, &assistant_text("survivor"));
    let mut cursor = Cursor::default();
    let pass = cursor.advance(&path, "sess", &identity(), now());
    assert_eq!(pass.records.len(), 1);
    assert_eq!(pass.records[0].sequence, 1, "the bad line still consumed index 0");
}

#[test]
fn a_huge_backlog_is_bounded_per_pass_and_caught_up_over_several() {
    let scratch = Scratch::new("bounded");
    let path = scratch.file("t.jsonl");
    // Enough bytes that one pass cannot consume them all.
    let line = assistant_text(&"y".repeat(4_000));
    let lines = (MAX_BYTES_PER_PASS as usize / line.len()) + 5;
    for _ in 0..lines {
        append(&path, &line);
    }
    let mut cursor = Cursor::default();
    // First pass attaches (so it skips the backlog); append more and confirm
    // the per-pass budget is respected on a genuinely incremental read.
    cursor.advance(&path, "sess", &identity(), now());
    for _ in 0..lines {
        append(&path, &line);
    }
    let pass = cursor.advance(&path, "sess", &identity(), now());
    assert!(pass.more_available, "a pass over a large backlog reports more to come");
    assert!(
        (pass.records.len() as u64) * (line.len() as u64) <= MAX_BYTES_PER_PASS + line.len() as u64,
        "one pass stayed inside its byte budget"
    );
    let second = cursor.advance(&path, "sess", &identity(), now());
    assert!(!second.records.is_empty(), "the next pass continues where it stopped");
}

#[test]
fn a_secret_printed_by_the_agent_is_scrubbed_before_it_leaves_the_adapter() {
    let scratch = Scratch::new("scrub");
    let path = scratch.file("t.jsonl");
    append(
        &path,
        &assistant_text("I set GH_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123 for the push"),
    );
    let mut cursor = Cursor::default();
    let pass = cursor.advance(&path, "sess", &identity(), now());
    let text = pass.records[0].text.as_deref().unwrap();
    assert!(!text.contains("ghp_"), "{text}");
    assert!(text.contains("[REDACTED"), "{text}");
}

#[test]
fn source_time_comes_from_the_record_and_observed_time_from_the_reader() {
    let scratch = Scratch::new("times");
    let path = scratch.file("t.jsonl");
    append(&path, &assistant_text("hello"));
    let mut cursor = Cursor::default();
    let read_at = Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 5).unwrap();
    let pass = cursor.advance(&path, "sess", &identity(), read_at);
    let record = &pass.records[0];
    assert_eq!(record.source_at, Utc.with_ymd_and_hms(2026, 9, 30, 11, 59, 58).unwrap());
    assert_eq!(record.observed_at, read_at);
    assert_eq!(record.producer_lag_ms(), 7_000);
}

#[test]
fn a_record_without_a_timestamp_falls_back_to_the_read_time_not_to_zero() {
    let scratch = Scratch::new("no-ts");
    let path = scratch.file("t.jsonl");
    append(
        &path,
        "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
    );
    let mut cursor = Cursor::default();
    let read_at = now();
    let pass = cursor.advance(&path, "sess", &identity(), read_at);
    // An epoch-zero source time would be indistinguishable from a genuine
    // historical record and would poison any latency measurement.
    assert_eq!(pass.records[0].source_at, read_at);
    assert_eq!(pass.records[0].producer_lag_ms(), 0);
}

#[test]
fn discovery_attributes_a_session_to_its_issue_and_names_its_stream() {
    let scratch = Scratch::new("discover");
    let projects = scratch.file("projects");
    let workspace = scratch.file("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let slug = crate::transcript_tokens::project_slug(&workspace);
    let project_dir = projects.join(&slug);
    std::fs::create_dir_all(&project_dir).unwrap();

    let mine = project_dir.join("11111111-2222-3333-4444-555555555555.jsonl");
    std::fs::write(
        &mine,
        "{\"type\":\"user\",\"message\":{\"content\":\"<command-name>/loom:sweep</command-name><command-args>9764</command-args>\"}}\n",
    )
    .unwrap();
    let other = project_dir.join("99999999-2222-3333-4444-555555555555.jsonl");
    std::fs::write(
        &other,
        "{\"type\":\"user\",\"message\":{\"content\":\"<command-name>/loom:sweep</command-name><command-args>1234</command-args>\"}}\n",
    )
    .unwrap();

    let found = discover(&projects, &workspace, 9764);
    assert_eq!(found.len(), 1, "only this issue's session: {found:?}");
    assert_eq!(found[0].0, "11111111-2222-3333-4444-555555555555");
    assert_eq!(found[0].1, mine);
    assert!(discover(&projects, &workspace, 4321).is_empty());
}

#[test]
fn discovery_is_unaffected_by_the_worktree_directory_name() {
    // The forge repo identity never comes from a path; discovery keys on the
    // project slug, and the slug is only ever a lookup key — it is not
    // exported, and `loom.repo` is resolved separately from the forge.
    let scratch = Scratch::new("odd-name");
    let projects = scratch.file("projects");
    let workspace = scratch.file("totally-unrelated-name");
    std::fs::create_dir_all(&workspace).unwrap();
    let project_dir = projects.join(crate::transcript_tokens::project_slug(&workspace));
    std::fs::create_dir_all(&project_dir).unwrap();
    let path = project_dir.join("abcd.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"user\",\"message\":{\"content\":\"<command-name>/loom:sweep</command-name><command-args>9764</command-args>\"}}\n",
    )
    .unwrap();
    let found = discover(&projects, &workspace, 9764);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "abcd");
}

#[test]
fn a_missing_projects_directory_yields_no_streams() {
    let scratch = Scratch::new("no-projects");
    assert!(discover(&scratch.file("nope"), &scratch.file("ws"), 9764).is_empty());
}

// ---- #10124: narration filed as `thinking` ----------------------------------
//
// Real-shaped (sanitized) fixtures. In Claude Code 2.1.288 transcripts every
// `thinking` block has exactly `type`, `thinking` (empty) and `signature`
// (populated), whether the turn ends in `tool_use` or `end_turn`. Nothing
// distinguishes narration from reasoning, so none is emitted; the loss is
// counted instead.

fn real_shaped_thinking(stop_reason: &str, thinking: &str) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"2026-09-30T11:59:58.000Z","message":{{"stop_reason":"{stop_reason}","content":[{{"type":"thinking","thinking":{},"signature":"EqQBCkYIARgCKkD0c2lnbmF0dXJl"}}]}}}}"#,
        serde_json::to_string(thinking).unwrap()
    ) + "\n"
}

#[test]
fn narration_shaped_and_reasoning_shaped_thinking_are_both_withheld_and_counted() {
    let scratch = Scratch::new("thinking-withheld");
    let path = scratch.file("t.jsonl");
    // Narration-as-thinking: the text a user would have seen before a tool call.
    append(
        &path,
        &real_shaped_thinking("tool_use", "NARRATION-MARKER I will read the config next."),
    );
    append(&path, &assistant_tool_use("tu_1", "Read", "x"));
    // Genuine reasoning, plus the empty-with-signature shape real transcripts carry.
    append(&path, &real_shaped_thinking("end_turn", "REASONING-MARKER weighing options."));
    append(&path, &real_shaped_thinking("end_turn", ""));

    let mut cursor = Cursor::default();
    let pass = cursor.advance(&path, "sess", &identity(), now());

    assert_eq!(pass.thinking_withheld, 3);
    let categories: Vec<_> = pass.records.iter().map(|r| r.category).collect();
    assert_eq!(categories, vec![OutputCategory::ToolStart]);
    let dump = format!("{:?}", pass.records);
    assert!(!dump.contains("NARRATION-MARKER"));
    assert!(!dump.contains("REASONING-MARKER"));
}

#[test]
fn no_thinking_means_no_coverage_gap_signal() {
    let scratch = Scratch::new("no-thinking");
    let path = scratch.file("t.jsonl");
    append(&path, &assistant_text("Reading the file now."));
    append(&path, &assistant_tool_use("tu_1", "Read", "x"));

    let mut cursor = Cursor::default();
    let pass = cursor.advance(&path, "sess", &identity(), now());
    assert_eq!(pass.thinking_withheld, 0);
    assert_eq!(pass.records.len(), 2);
}
