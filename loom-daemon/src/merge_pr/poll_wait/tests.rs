//! Unit tests for the poll-wait decision. The differential
//! (`tests/merge_pr_poll_wait_differential.rs`) compares against the frozen
//! retired shell; these name the individual properties.

use super::*;

fn inp<'a>(kind: Kind, now: i64, deadline: i64, pending: &'a str) -> Inputs<'a> {
    Inputs {
        kind,
        pr: "42",
        now,
        deadline,
        timeout: "600",
        interval: "30",
        rc: "7",
        pending,
    }
}

#[test]
fn the_deadline_is_inclusive() {
    assert_eq!(decide(&inp(Kind::Pending, 99, 100, "a\n")).action, Action::Wait);
    assert_eq!(decide(&inp(Kind::Pending, 100, 100, "a\n")).action, Action::Timeout);
    assert_eq!(decide(&inp(Kind::Unfetchable, 100, 100, "")).action, Action::Timeout);
    assert_eq!(decide(&inp(Kind::Unfetchable, 99, 100, "")).action, Action::Wait);
}

#[test]
fn pending_names_are_counted_like_wc_l() {
    assert_eq!(pending_count("a\n"), 1);
    assert_eq!(pending_count("a\nb\nc\n"), 3);
    assert_eq!(pending_count(""), 0);
}

#[test]
fn a_pending_wait_narrates_at_info_with_the_count() {
    let d = decide(&inp(Kind::Pending, 0, 100, "a\nb\n"));
    assert_eq!(d.level, Level::Info);
    assert_eq!(
        d.message,
        "PR #42: 2 check(s) still running; waiting 30s for CI (timeout 600s)..."
    );
}

#[test]
fn an_unfetchable_wait_narrates_at_warning_with_the_rc() {
    let d = decide(&inp(Kind::Unfetchable, 0, 100, ""));
    assert_eq!(d.level, Level::Warning);
    assert!(d.message.contains("(rc=7)"));
}

#[test]
fn both_timeouts_say_exit_5_and_name_the_knob() {
    for kind in [Kind::Pending, Kind::Unfetchable] {
        let d = decide(&inp(kind, 100, 100, "a\n"));
        assert_eq!(d.level, Level::Warning);
        assert!(d.message.starts_with("Timed out after 600s"));
        assert!(d.message.contains("exiting 5"));
        assert!(d.message.ends_with("LOOM_AUTO_MERGE_TIMEOUT."));
    }
}

#[test]
fn the_rendered_line_is_one_sentinel_prefixed_line() {
    let line = decide(&inp(Kind::Pending, 0, 100, "a\n")).to_string();
    assert!(line.starts_with("LOOM-POLL-WAIT WAIT info PR #42:"));
    assert!(!line.contains('\n'));
}
