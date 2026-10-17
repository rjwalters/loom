#![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
//! Issue #6262 AC4: an operator must be able to see, from `status`, that
//! the daemon seeded its capacity accounting with sweeps that survived a
//! restart — and how many — instead of inferring it from a log line.

use super::*;

fn report_with_adoptions(n: usize) -> DaemonStatusReport {
    DaemonStatusReport {
        journal_adopted_at_startup: n,
        ..DaemonStatusReport::default()
    }
}

fn no_update() -> loom_daemon::self_update::SelfUpdateStatus {
    loom_daemon::self_update::SelfUpdateStatus {
        built_commit: "abc".to_string(),
        source_commit: None,
        update_available: None,
        commits_behind: None,
        hours_behind: None,
    }
}

/// Non-zero is the case worth a line: the lock-based `reconstruct()` came
/// up short and the machine journal carried the difference.
#[test]
fn human_line_reports_a_non_zero_adoption_count() {
    let line = render_journal_adoption_line(&report_with_adoptions(3));
    assert!(line.contains("Adopted 3 surviving sweep(s)"), "{line}");
    assert!(line.contains("#6262"), "must cite the issue for a reader: {line}");
    assert!(
        line.contains("occupy capacity"),
        "must state the operational consequence, not just the count: {line}"
    );
}

/// Zero is BOTH "idle host" and "every lock survived" — the healthy shape
/// either way. Printing a line for it on every invocation is noise an
/// operator learns to skip, which is how a real warning gets missed.
#[test]
fn human_line_is_silent_when_nothing_was_adopted() {
    assert_eq!(render_journal_adoption_line(&report_with_adoptions(0)), "");
}

#[test]
fn json_carries_the_adoption_count() {
    let value =
        build_status_json_value(&report_with_adoptions(2), None, &no_update(), None, None, None);
    assert_eq!(value["journal_adopted_at_startup"], 2);
}

/// A pre-#6262 daemon never sent the field; its payload must still parse,
/// reporting `0` rather than failing the whole status read.
#[test]
fn adoption_count_survives_a_wire_round_trip_and_older_payloads() {
    let report = report_with_adoptions(5);
    let json = serde_json::to_string(&report).expect("serialize");
    let back: DaemonStatusReport = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.journal_adopted_at_startup, 5);

    let mut stripped: serde_json::Value = serde_json::from_str(&json).expect("value");
    stripped
        .as_object_mut()
        .expect("object")
        .remove("journal_adopted_at_startup");
    let older: DaemonStatusReport =
        serde_json::from_value(stripped).expect("pre-#6262 payload must still parse");
    assert_eq!(older.journal_adopted_at_startup, 0);
}
