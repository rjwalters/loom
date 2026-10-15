//! Tests for the re-date commit vocabulary the measurement still reads.

use super::*;

#[test]
fn commit_message_names_the_pr_and_the_guard() {
    let msg = commit_message("8493");
    for needle in ["#8493", "#8248", "actions:write", "#8508"] {
        assert!(msg.contains(needle), "commit message should mention {needle:?}, got: {msg}");
    }
}

#[test]
fn commit_message_is_deterministic() {
    assert_eq!(commit_message("1"), commit_message("1"));
    assert_ne!(commit_message("1"), commit_message("2"));
}

#[test]
fn the_redate_subject_classifier_recognizes_exactly_what_commit_message_writes() {
    // Producer and classifier (#9337's `stale_main_bump`) must not drift.
    for pr in ["123", "8493", "1"] {
        let msg = commit_message(pr);
        assert!(is_redate_commit_subject(msg.lines().next().unwrap()), "{msg}");
    }
    for near_miss in [
        "chore: re-date required checks for PR #123",
        "chore: re-date required checks for PR # (#8248 guard, automated by #8508)",
        "chore: re-date required checks for PR #12a (#8248 guard, automated by #8508)",
        "chore: re-date required checks for PR #123 (#8248 guard, automated by #8508) ",
        " chore: re-date required checks for PR #123 (#8248 guard, automated by #8508)",
        "fix: something else (#123)",
        "",
    ] {
        assert!(!is_redate_commit_subject(near_miss), "{near_miss:?}");
    }
    // Only the first line is a subject; a whole message is not.
    assert!(!is_redate_commit_subject(&commit_message("123")));
}
