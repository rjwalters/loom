//! #12027: comment markers and daemon-subcommand writes in the pick journal.

use super::*;

fn with_journal<T>(f: impl FnOnce() -> T) -> (T, Vec<JournalEntry>) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.jsonl");
    set_test_journal(Some(path.clone()));
    let out = f();
    set_test_journal(None);
    let entries = parse(&std::fs::read_to_string(&path).unwrap_or_default());
    (out, entries)
}

#[test]
fn a_comment_opening_with_a_marker_journals_it() {
    let ((), entries) = with_journal(|| {
        record_marker(
            11277,
            true,
            "<!-- champion:reject:ci-status:a,b -->\n**Champion: Cannot Auto-Merge**",
        );
    });
    match entries.as_slice() {
        [JournalEntry::Marker {
            number,
            is_pr,
            marker,
            ..
        }] => {
            assert_eq!(*number, 11277);
            assert!(*is_pr);
            assert_eq!(marker, "<!-- champion:reject:ci-status:a,b -->");
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn every_leading_marker_line_is_journaled_and_the_rest_are_not() {
    let body = "<!-- champion:proposal-verdict:body-ab12 -->\n\
                <!-- champion:revise:body-ab12 -->\n\
                **Champion Review: NEEDS REVISION**\n\
                <!-- champion:not-leading -->";
    assert_eq!(
        leading_markers(body),
        vec![
            "<!-- champion:proposal-verdict:body-ab12 -->".to_string(),
            "<!-- champion:revise:body-ab12 -->".to_string(),
        ]
    );
    let ((), entries) = with_journal(|| record_marker(42, false, body));
    assert_eq!(entries.len(), 2);
}

#[test]
fn a_body_without_a_leading_marker_journals_nothing() {
    for body in [
        "plain text\n<!-- champion:reject:ci-status:a -->",
        "<!-- just a comment -->",
        "<!--champion:reject:x-->",
        "",
    ] {
        assert!(leading_markers(body).is_empty(), "{body:?}");
        let ((), entries) = with_journal(|| record_marker(1, true, body));
        assert!(entries.is_empty(), "{body:?}");
    }
    assert_eq!(
        leading_markers("<!-- champion:hold-state head=abc -->"),
        vec!["<!-- champion:hold-state head=abc -->".to_string()]
    );
}

#[test]
fn a_long_marker_is_cut_at_the_bound() {
    let body = format!("<!-- champion:reject:ci-status:{} -->", "x".repeat(400));
    let markers = leading_markers(&body);
    assert_eq!(markers[0].chars().count(), MAX_MARKER_LEN);
}

#[test]
fn writers_are_no_ops_without_a_journal() {
    set_test_journal(None);
    if std::env::var_os(PICK_JOURNAL_ENV).is_none() {
        record_marker(1, true, "<!-- champion:reject:ci-status:a -->");
        record_applied(1, false, "blocked", "park_record");
    }
}

#[test]
fn an_applied_write_journals_one_entry() {
    let ((), entries) = with_journal(|| record_applied(42, false, "blocked", "park_record"));
    assert!(matches!(
        entries.as_slice(),
        [JournalEntry::Applied { number: 42, is_pr: false, action, by, .. }]
            if action == "blocked" && by == "park_record"
    ));
}
