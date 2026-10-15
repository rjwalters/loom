#![allow(clippy::unwrap_used)]
use super::*;
use serial_test::serial;
use std::process::Command;

fn clear_env() {
    std::env::remove_var(PR_FILE_ENV);
    std::env::remove_var(CONTEXT_FILE_ENV);
}

#[test]
fn the_marker_vocabulary_is_snake_cased_and_nothing_else_is_a_verdict() {
    assert_eq!(snake_verdict("approved"), Some("approved"));
    assert_eq!(snake_verdict("changes-requested"), Some("changes_requested"));
    assert_eq!(snake_verdict("rejected"), None);
}

#[test]
#[serial]
fn a_verdict_is_handed_to_the_tick_and_read_back_once() {
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::new("true");
    super::super::role_target::export(&mut cmd, dir.path(), "role-judge-1");
    let claim = cmd
        .get_envs()
        .find(|(k, _)| *k == PR_FILE_ENV)
        .and_then(|(_, v)| v)
        .map(PathBuf::from)
        .unwrap();
    std::env::set_var(PR_FILE_ENV, &claim);
    record(7, "ABCDEF1234", "changes-requested");
    record(8, "0123456789abcdef", "approved");
    record(9, "not-a-sha", "approved");
    record(9, "abcdef1", "rejected");
    clear_env();
    let entries = take_tick(dir.path(), "role-judge-1");
    assert_eq!(entries.len(), 2, "malformed writes are not recorded");
    assert_eq!(entries[0].verdict, "changes_requested");
    assert_eq!(entries[0].head_sha, "abcdef1234");
    assert_eq!(entries[1].pr_number, 8);
    assert!(take_tick(dir.path(), "role-judge-1").is_empty(), "read once");
}

#[test]
#[serial]
fn a_sweep_phase_reads_the_newest_verdict_for_its_pr() {
    clear_env();
    let dir = tempfile::tempdir().unwrap();
    let context = dir.path().join("sweep.json");
    std::env::set_var(CONTEXT_FILE_ENV, &context);
    record(7, "aaaaaaa", "changes-requested");
    record(7, "bbbbbbb", "approved");
    clear_env();
    assert_eq!(for_pr(&context, 7).unwrap().head_sha, "bbbbbbb");
    assert!(for_pr(&context, 8).is_none());
    remove_for(&context);
    assert!(for_pr(&context, 7).is_none());
}

#[test]
#[serial]
fn nothing_is_written_outside_a_handoff() {
    clear_env();
    record(7, "aaaaaaa", "approved"); // no env: a silent no-op
}

#[test]
fn a_spawned_judge_tick_says_whether_it_found_work() {
    let idle = observation(None, &[]);
    assert_eq!(idle.found_work, Some(false));
    assert_eq!(idle.noop_reason.as_deref(), Some(NO_CLAIM_NO_VERDICT));
    let claimed = observation(Some(12), &[]);
    assert_eq!(claimed.found_work, Some(true));
    assert_eq!((claimed.pr_number, claimed.verdict), (Some(12), None));
    assert!(claimed.noop_reason.is_none());
    let at = Utc::now();
    let two = observation(
        Some(12),
        &[
            Entry {
                pr_number: 12,
                head_sha: "aaaaaaa".into(),
                verdict: "changes_requested".into(),
                at,
            },
            Entry {
                pr_number: 13,
                head_sha: "bbbbbbb".into(),
                verdict: "approved".into(),
                at,
            },
        ],
    );
    assert_eq!(two.found_work, Some(true));
    assert_eq!((two.pr_number, two.verdict.as_deref()), (Some(13), Some("approved")));
    assert_eq!(two.verdicts.as_ref().unwrap().len(), 2, "every verdict is kept");
    let attrs = span_attributes(&two);
    assert_eq!(attrs["loom.found_work"], "true");
    assert_eq!(attrs["loom.judge_verdict"], "approved");
    assert_eq!(attrs["loom.head_sha"], "bbbbbbb");
    assert_eq!(attrs["loom.pr_number"], "13", "the newest verdict's PR wins");
    let attrs = span_attributes(&idle);
    assert_eq!(attrs["loom.found_work"], "false");
    assert_eq!(attrs["loom.noop_reason"], NO_CLAIM_NO_VERDICT);
    assert!(!attrs.contains_key("loom.pr_number"));
    assert!(span_attributes(&RoleTickObserved::default()).is_empty());
}
