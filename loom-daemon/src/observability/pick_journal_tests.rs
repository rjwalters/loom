//! Pick-journal tests (#10432): argv classification, label-derived reasons,
//! `pr-queue` rows, and the attach → append → take round trip.

use super::*;

fn argv(s: &str) -> Vec<String> {
    s.split_whitespace().map(str::to_string).collect()
}

#[test]
fn gh_edit_add_label_maps_each_workflow_label_to_an_action() {
    assert_eq!(
        gh_actions(&argv("pr edit 12 --add-label loom:reviewing")),
        vec![(12, "claimed")]
    );
    assert_eq!(
        gh_actions(&argv(
            "pr edit 12 --remove-label loom:review-requested --remove-label loom:reviewing --add-label loom:pr"
        )),
        vec![(12, "approved")]
    );
    assert_eq!(
        gh_actions(&argv("issue edit #7 -R o/r --add-label=loom:curated,loom:issue")),
        vec![(7, "curated"), (7, "promoted")]
    );
    // A non-Loom label and a removal are not actions.
    assert!(gh_actions(&argv("pr edit 12 --add-label bug --remove-label loom:pr")).is_empty());
    // A URL target.
    assert_eq!(
        gh_actions(&argv(
            "pr edit https://github.com/o/r/pull/99 --add-label loom:changes-requested"
        )),
        vec![(99, "changes_requested")]
    );
}

#[test]
fn gh_api_label_post_and_merge_put_are_actions_reads_and_deletes_are_not() {
    assert_eq!(
        gh_actions(&argv("api repos/o/r/issues/5/labels -f labels[]=loom:curating")),
        vec![(5, "claimed")]
    );
    assert_eq!(
        gh_actions(&argv("api -X PUT repos/o/r/pulls/8/merge -f merge_method=squash")),
        vec![(8, "merged")]
    );
    assert!(
        gh_actions(&argv("api repos/o/r/issues/5/labels/loom%3Areviewing -X DELETE")).is_empty()
    );
    assert!(gh_actions(&argv("api repos/o/r/issues/5/labels")).is_empty(), "a GET");
    assert!(gh_actions(&argv("pr view 5 --json labels")).is_empty());
}

#[test]
fn gh_pr_merge_and_review_are_actions() {
    assert_eq!(gh_actions(&argv("pr merge 3 --squash")), vec![(3, "merged")]);
    assert_eq!(gh_actions(&argv("pr review 4 --approve")), vec![(4, "approved")]);
    assert_eq!(gh_actions(&argv("pr review -r 4 -b no")), vec![(4, "changes_requested")]);
    assert!(gh_actions(&argv("pr review 4 --comment -b hi")).is_empty());
}

#[test]
fn every_label_action_is_in_the_closed_role_action_set() {
    for label in [
        "loom:reviewing",
        "loom:pr",
        "loom:changes-requested",
        "loom:curated",
        "loom:issue",
        "loom:blocked",
        "loom:operator",
        "loom:anything-else",
    ] {
        let action = action_for_label(label).unwrap();
        assert_eq!(known_action(action), Some(action), "{label}");
    }
    assert_eq!(known_action("dispatched"), None);
}

#[test]
fn label_derived_skip_reasons() {
    let l = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    assert_eq!(
        skip_reason_for_labels(&l(&["loom:operator"])),
        Some(PickSkipReason::OperatorHold)
    );
    assert_eq!(skip_reason_for_labels(&l(&["loom:blocked"])), Some(PickSkipReason::Blocked));
    assert_eq!(
        skip_reason_for_labels(&l(&["loom:sequenced"])),
        Some(PickSkipReason::OverlapChain)
    );
    assert_eq!(skip_reason_for_labels(&l(&["loom:treating"])), Some(PickSkipReason::InFlight));
    assert_eq!(
        skip_reason_for_labels(&l(&["loom:operator-priority"])),
        None,
        "a star is no hold"
    );
}

#[test]
fn pr_queue_rows_keep_order_stage_and_sort_key() {
    let rows = serde_json::json!([
        {"number": 9, "labels": [{"name": "loom:pr"}], "mode": "workflow",
         "origin": "interactive", "priorityReason": "operator-priority", "operatorPriorityLevel": 1},
        {"number": 4, "labels": [], "mode": "fallback",
         "origin": "interactive", "priorityReason": "interactive", "operatorPriorityLevel": 0},
    ]);
    let out = pr_queue_rows(rows.as_array().unwrap());
    assert_eq!(out.iter().map(|r| r.number).collect::<Vec<_>>(), vec![9, 4]);
    assert_eq!(out[0].stage, "loom:pr");
    assert_eq!(out[1].stage, "fallback");
    assert_eq!(
        out[0].sort_key.as_ref().unwrap().value,
        "level=1,reason=operator-priority,origin=interactive,mode=workflow"
    );
}

#[test]
fn attach_append_take_round_trips_and_deletes_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let journal_dir = dir.path().join("j");
    let root = Path::new("/journal-test/root");
    discard();
    let mut cmd = Command::new("true");
    attach_in(&mut cmd, root, "judge", &journal_dir);
    let path = PathBuf::from(
        cmd.get_envs()
            .find(|(k, _)| *k == PICK_JOURNAL_ENV)
            .and_then(|(_, v)| v)
            .unwrap(),
    );
    // A retried launch in the same tick reuses the file.
    let mut again = Command::new("true");
    attach_in(&mut again, root, "judge", &journal_dir);
    assert_eq!(
        again
            .get_envs()
            .find(|(k, _)| *k == PICK_JOURNAL_ENV)
            .and_then(|(_, v)| v),
        Some(path.as_os_str())
    );
    let entry = JournalEntry::Act {
        at: Utc::now(),
        number: 3,
        action: "claimed".to_string(),
    };
    append_to(&path, &entry);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"not json\n")
        .unwrap();
    let got = take(root).unwrap();
    assert_eq!(got.entries, vec![entry], "the bad line is skipped");
    assert!(got.read);
    assert!(!path.exists(), "the journal is deleted once read");
    assert!(take(root).is_none(), "drained");
}

#[test]
fn a_tick_with_no_journal_attached_takes_none() {
    discard();
    assert!(take(Path::new("/journal-test/none")).is_none());
}

#[test]
fn writers_are_no_ops_outside_a_role_tick() {
    // PICK_JOURNAL_ENV is never set in the test process.
    if std::env::var_os(PICK_JOURNAL_ENV).is_none() {
        record_pr_queue("judge", &[]);
        record_gh_actions(&[OsString::from("pr")]);
    }
}

#[test]
fn a_missing_journal_file_is_unread_but_an_empty_one_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let journal_dir = dir.path().join("j");
    let root = Path::new("/journal-test/unread");
    let attach_path = |root: &Path| {
        discard();
        let mut cmd = Command::new("true");
        attach_in(&mut cmd, root, "judge", &journal_dir);
        PathBuf::from(
            cmd.get_envs()
                .find(|(k, _)| *k == PICK_JOURNAL_ENV)
                .and_then(|(_, v)| v)
                .unwrap(),
        )
    };
    // The agent never wrote: no file, so nothing was observed.
    let path = attach_path(root);
    assert!(!path.exists());
    let got = take(root).unwrap();
    assert!(!got.read && got.entries.is_empty());

    // An empty file was read.
    let path = attach_path(root);
    std::fs::write(&path, b"").unwrap();
    let got = take(root).unwrap();
    assert!(got.read && got.entries.is_empty());
}

#[test]
fn a_queue_line_written_before_the_total_field_still_parses() {
    let line = r#"{"kind":"queue","at":"2026-10-04T12:00:00Z","role":"judge","acts_observable":true,"rows":[]}"#;
    match parse(line).as_slice() {
        [JournalEntry::Queue { total, .. }] => assert_eq!(*total, 0),
        other => panic!("unexpected {other:?}"),
    }
}
