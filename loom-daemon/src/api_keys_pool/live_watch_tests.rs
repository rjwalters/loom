//! Tests for [`super`] — in-run exhaustion detection (#11286 item 3).
//!
//! The provider error lines below are **SYNTHETIC / UNVERIFIED**: no real Z.ai
//! exhausted-seat output has been captured (see `classify.rs`'s "Honest
//! limits"). They use the documented-shape needles the classifier already
//! carries, plus the captured OpenCode auth event where a real one exists.

use super::*;
use crate::api_keys_pool::classify::CAPTURED_OPENCODE_AUTH_EVENT;
use crate::api_keys_pool::ingest::{apply_mark, LAUNCH_RECORD_PREFIX};
use crate::api_keys_pool::{bad_marks, paths, registry};

const ANCHOR: &str = "sweep_id=sweep-issue-11286-1";
const PROVIDER: &str = "loomtest";

fn launch_line(source: &str) -> String {
    format!(
        "{LAUNCH_RECORD_PREFIX}{}",
        serde_json::json!({
            "schema": 1,
            "runtime": "opencode",
            "model": "glm-5.3-flash",
            "credentialSource": source,
            "credentialProvider": PROVIDER,
            "credentialAccount": "alpha",
        })
    )
}

fn header(source: &str) -> String {
    format!(
        "==== loom-daemon dispatch: {ANCHOR} ====\n{}\n# LOOM_CLI_START runtime=opencode\n",
        launch_line(source)
    )
}

#[test]
fn an_exhaustion_line_is_decided_while_the_run_is_still_live() {
    let mut watch = LiveWatch::new(ANCHOR);
    assert_eq!(watch.feed(&header("pool")), None);
    assert_eq!(watch.feed("{\"type\":\"text\",\"text\":\"working\"}\n"), None);
    // SYNTHETIC provider line.
    let decided = watch
        .feed("Error: insufficient balance. Your limit will reset at 2026-10-17 00:00:00\n")
        .expect("decided mid-run");
    assert_eq!(decided.classification, Classification::Exhausted);
    assert_eq!(decided.record.account.as_deref(), Some("alpha"));
    assert!(decided.provider_text.contains("will reset at"), "{decided:?}");
    // Never twice for the same launch.
    assert_eq!(watch.feed("Error: insufficient balance\n"), None);
    assert!(watch.decided());
}

#[test]
fn a_line_split_across_polls_is_read_whole() {
    let mut watch = LiveWatch::new(ANCHOR);
    watch.feed(&header("pool"));
    assert_eq!(watch.feed("Error: insufficient bal"), None);
    assert!(watch.feed("ance\n").is_some());
}

#[test]
fn a_rate_limit_is_left_to_the_exit_path() {
    let mut watch = LiveWatch::new(ANCHOR);
    watch.feed(&header("pool"));
    assert_eq!(watch.feed("{\"type\":\"error\",\"error\":{\"status\":429}}\n"), None);
    assert_eq!(watch.feed("HTTP 429 Too Many Requests\n"), None);
}

#[test]
fn the_agents_own_words_and_non_pool_launches_never_decide() {
    let mut watch = LiveWatch::new(ANCHOR);
    watch.feed(&header("pool"));
    assert_eq!(
        watch.feed("{\"type\":\"text\",\"text\":\"insufficient balance is the signature\"}\n"),
        None
    );
    let mut env = LiveWatch::new(ANCHOR);
    env.feed(&header("env"));
    assert_eq!(env.feed("Error: insufficient balance\n"), None);
}

#[test]
fn a_credential_failure_in_the_region_suppresses_the_mark() {
    let mut watch = LiveWatch::new(ANCHOR);
    watch.feed(&header("pool"));
    assert_eq!(
        watch.feed(&format!("{CAPTURED_OPENCODE_AUTH_EVENT}\nError: insufficient balance\n")),
        None
    );
}

#[test]
fn text_before_the_anchor_and_a_new_launch_record_start_fresh() {
    let mut watch = LiveWatch::new(ANCHOR);
    // A previous run's output in the same log, before this run's anchor.
    assert_eq!(
        watch.feed(&format!("{}\nError: insufficient balance\n", launch_line("pool"))),
        None
    );
    watch.feed(&header("pool"));
    assert_eq!(watch.feed("still fine\n"), None);
    // An exhaustion, then a re-dispatch inside the region: the new launch
    // gets its own decision.
    assert!(watch.feed("Error: insufficient balance\n").is_some());
    watch.feed(&format!("{}\n", launch_line("pool")));
    assert!(!watch.decided());
    assert!(watch.feed("Error: quota exceeded\n").is_some());
}

#[test]
fn polls_a_growing_file_by_offset_and_restarts_on_truncation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("sweep.log");
    std::fs::write(&path, header("pool")).unwrap();
    let mut watch = LiveWatch::new(ANCHOR);
    assert_eq!(watch.poll(&path), None);
    let mut contents = std::fs::read_to_string(&path).unwrap();
    contents.push_str("Error: insufficient balance\n");
    std::fs::write(&path, &contents).unwrap();
    assert!(watch.poll(&path).is_some());
    assert_eq!(watch.poll(&path), None);
    // Replaced by a shorter file: read again from the start.
    std::fs::write(&path, format!("{}Error: quota exceeded\n", header("pool"))).unwrap();
    assert!(watch.poll(&path).is_some());
    assert_eq!(watch.poll(&tmp.path().join("missing.log")), None);
}

/// End to end with the shared marking half: the live decision marks the seat
/// until the configured plan window, and the account leaves selection.
#[test]
fn a_live_decision_marks_the_seat_for_its_configured_window() {
    let tmp = tempfile::tempdir().unwrap();
    let root = paths::per_repo_api_keys_dir(tmp.path());
    registry::add(&root, PROVIDER, "alpha", "LOOM_TEST_KEY_11286", "fake-key", false).unwrap();
    crate::api_keys_pool::limits::set_exhaustion_window(&root, PROVIDER, "alpha", Some(604_800))
        .unwrap();
    let mut watch = LiveWatch::new(ANCHOR);
    watch.feed(&header("pool"));
    let decided = watch.feed("Error: insufficient balance\n").unwrap();
    let clock = bad_marks::test_clock::pin(1_791_590_400);
    let feedback = apply_mark(
        tmp.path(),
        decided.record,
        decided.classification,
        &decided.provider_text,
        "the live launch log",
    )
    .unwrap();
    let mark = feedback.mark.expect("marked");
    assert_eq!(mark.resets_at, Some(1_791_590_400 + 604_800));
    assert!(mark.reason.contains("configured plan window"), "{}", mark.reason);
    // Six hours later (the old fixed default) the seat is still held.
    clock.set(1_791_590_400 + 6 * 3600 + 1);
    assert!(bad_marks::is_bad_for_class(
        &root,
        PROVIDER,
        "alpha",
        Some("glm-5.3-flash"),
        bad_marks::epoch_now()
    )
    .unwrap());
}
