use chrono::Utc;

use super::*;

fn verdict(number: u32, action: &str, sha: Option<&str>) -> JournalEntry {
    JournalEntry::Applied {
        at: Utc::now(),
        number,
        is_pr: true,
        action: action.to_string(),
        by: "verdict-labels".to_string(),
        head_sha: sha.map(str::to_string),
    }
}

#[test]
fn a_spawned_tick_with_no_claim_and_no_verdict_found_no_work() {
    let tick = from_entries(&[], None, None);
    assert!(!tick.found_work);
    assert_eq!(tick.noop_reason.as_deref(), Some("no_claim_no_verdict"));
    assert!(tick.verdicts.is_empty());
    assert_eq!(tick.latest(), None);
}

#[test]
fn the_sessions_own_reason_wins_over_the_daemons() {
    let tick = from_entries(&[], None, Some("already_judged".into()));
    assert_eq!(tick.noop_reason.as_deref(), Some("already_judged"));
}

#[test]
fn a_claim_alone_is_work_without_a_verdict() {
    let tick = from_entries(&[], Some(42), None);
    assert!(tick.found_work);
    assert_eq!(tick.noop_reason, None);
    assert!(tick.verdicts.is_empty());
}

#[test]
fn a_verdict_write_is_work_and_names_the_pr_head_and_verdict() {
    let tick = from_entries(&[verdict(7, "changes_requested", Some("abc123"))], None, None);
    assert!(tick.found_work);
    assert_eq!(tick.noop_reason, None);
    let latest = tick.latest().unwrap();
    assert_eq!(
        (latest.pr_number, latest.head_sha.as_deref(), latest.verdict.as_str()),
        (7, Some("abc123"), "changes_requested")
    );
    assert_eq!(tick.verdict_pr(), Some(7));
}

#[test]
fn the_newest_write_per_pr_wins_and_each_pr_is_one_entry() {
    let tick = from_entries(
        &[
            verdict(9, "approved", Some("a")),
            verdict(3, "approved", Some("b")),
            verdict(9, "changes_requested", Some("a")),
        ],
        None,
        None,
    );
    let got: Vec<_> = tick
        .verdicts
        .iter()
        .map(|v| (v.pr_number, v.verdict.as_str()))
        .collect();
    assert_eq!(got, [(3, "approved"), (9, "changes_requested")]);
    assert_eq!(tick.latest().unwrap().pr_number, 9);
}

#[test]
fn other_applied_writes_are_not_verdicts() {
    let park = JournalEntry::Applied {
        at: Utc::now(),
        number: 5,
        is_pr: true,
        action: "blocked".into(),
        by: "park_record".into(),
        head_sha: None,
    };
    let tick = from_entries(&[park], None, None);
    assert!(!tick.found_work);
}

#[test]
fn only_the_last_ticks_section_of_the_log_is_read_for_a_reason() {
    let marker = crate::launch_record::RUNTIME_RESOLVED_MARKER;
    let log = format!(
        "{marker}claude\nLOOM_STEP_OUTCOME role=judge target=1 class=noop reason=old_tick\n\
         {marker}claude\nprose\n"
    );
    assert_eq!(session_reason(&log), None, "the old tick's line is not this tick's");
    let log =
        format!("{log}LOOM_STEP_OUTCOME role=judge target=2 class=noop reason=already_judged\n");
    assert_eq!(session_reason(&log).as_deref(), Some("already_judged"));
    assert_eq!(session_reason("no marker\n"), None);
}

#[test]
fn observe_reads_the_live_journal_without_consuming_it() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let path = tmp.path().join("j.jsonl");
    let line = serde_json::to_string(&verdict(11, "approved", Some("deadbeef"))).unwrap();
    std::fs::write(&path, format!("{line}\n")).unwrap();
    pick_journal::adopt(root, path.clone());
    let tick = observe(root, None);
    assert!(tick.found_work);
    assert_eq!(tick.latest().unwrap().head_sha.as_deref(), Some("deadbeef"));
    assert!(path.exists(), "peek must leave the file for the drain");
    assert!(pick_journal::take(root).is_some(), "the drain still gets the journal");
    // With no journal attached at all, nothing was observed.
    assert!(!observe(root, None).found_work);
}
