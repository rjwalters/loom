//! Per-cause dispatch-backoff streaks and log wording (Issue #8865).

use super::*;
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};

fn registry() -> (SweepRegistry, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.skip_label_flip = true;
    config.journal_path = Some(dir.path().join("journal.json"));
    (SweepRegistry::new(config), dir)
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The acceptance criterion: an open-PR refusal is not logged as a failed
/// dispatch. The generic line is byte-for-byte what it was.
#[test]
fn an_open_pr_refusal_is_not_worded_as_a_failed_dispatch() {
    let open_pr = armed_line(8136, 1, secs(60), DispatchBackoffCause::OpenPrGuard);
    assert!(open_pr.contains("issue #8136 dispatch backoff armed"), "{open_pr}");
    assert!(open_pr.contains("cause=OpenPrGuard"), "{open_pr}");
    assert!(open_pr.contains("next attempt allowed in 60s"), "{open_pr}");
    assert!(!open_pr.contains("consecutive failed dispatch(es)"), "{open_pr}");
    assert!(!open_pr.contains("failed dispatch"), "{open_pr}");

    assert_eq!(
        armed_line(7, 3, secs(240), DispatchBackoffCause::Generic),
        "sweep_registry: issue #7 dispatch backoff armed — 3 consecutive failed dispatch(es), \
         next attempt allowed in 240s (#4485; cause=Generic)"
    );
}

/// Step 2.8's refusal inside a window names what armed it.
#[test]
fn a_refusal_inside_a_window_names_its_cause() {
    let (mut reg, _dir) = registry();
    reg.record_open_pr_guard_backoff(1);
    reg.record_dispatch_failure(2);

    let open_pr = refused_line(1, reg.dispatch_backoff.get(&1), secs(42));
    assert!(open_pr.contains("open-PR guard refusal(s)"), "{open_pr}");
    assert!(open_pr.contains("42s of backoff remaining"), "{open_pr}");
    assert!(!open_pr.contains("failed dispatch"), "{open_pr}");

    assert_eq!(
        refused_line(2, reg.dispatch_backoff.get(&2), secs(42)),
        "sweep_registry: refusing to dispatch issue #2 — 1 consecutive failed dispatch(es), \
         42s of backoff remaining (#4485)"
    );
}

/// The logged lines are the ones the registry actually emits.
#[test]
fn the_registry_logs_the_cause_specific_armed_line() {
    let (mut reg, _dir) = registry();
    let logs = crate::test_log_capture::capture_logs(|| {
        reg.record_open_pr_guard_backoff(8136);
        reg.record_dispatch_failure(8137);
    });
    let armed: Vec<&String> = logs
        .iter()
        .filter(|(level, msg)| *level == log::Level::Info && msg.contains("dispatch backoff armed"))
        .map(|(_, msg)| msg)
        .collect();
    assert_eq!(armed.len(), 2, "{logs:?}");
    assert!(armed[0].contains("#8136") && !armed[0].contains("consecutive failed dispatch(es)"));
    assert!(armed[1].contains("#8137") && armed[1].contains("1 consecutive failed dispatch(es)"));
}

/// Each cause keeps its own ladder: unchanged doubling within one cause.
#[test]
fn a_streak_of_one_cause_still_doubles() {
    let (mut reg, _dir) = registry();
    for want in 1..=4 {
        reg.record_open_pr_guard_backoff(10);
        assert_eq!(reg.dispatch_failure_count(10), want);
    }
    for want in 1..=4 {
        reg.record_dispatch_failure(11);
        assert_eq!(reg.dispatch_failure_count(11), want);
    }
}

/// The acceptance criterion: a genuine dispatch failure after open-PR
/// refusals starts its streak at 1 instead of inheriting them — and the
/// reverse, so a refusal is never counted on top of real failures either.
#[test]
fn a_failure_does_not_inherit_open_pr_refusals() {
    let (mut reg, _dir) = registry();
    for _ in 0..4 {
        reg.record_open_pr_guard_backoff(20);
    }
    assert_eq!(reg.dispatch_failure_count(20), 4);
    reg.record_dispatch_failure(20);
    assert_eq!(reg.dispatch_failure_count(20), 1, "a fresh failure streak");
    assert!(
        !reg.open_pr_backoff_issues(Utc::now()).contains(&20),
        "the window is now a failure window"
    );
    reg.record_dispatch_failure(20);
    assert_eq!(reg.dispatch_failure_count(20), 2);

    reg.record_open_pr_guard_backoff(20);
    assert_eq!(reg.dispatch_failure_count(20), 1, "a fresh refusal streak");
    assert!(reg.open_pr_backoff_issues(Utc::now()).contains(&20));
}

/// `continues` is the whole rule: same cause, and not gone cold.
#[test]
fn continues_requires_the_same_cause_and_a_warm_streak() {
    let now = Utc::now();
    let prev = |cause, age_secs| DispatchBackoffState {
        consecutive: 3,
        last_failure_at: now - chrono::Duration::seconds(age_secs),
        until: now,
        cause,
    };
    use DispatchBackoffCause::{Generic, OpenPrGuard};
    assert!(continues(&prev(Generic, 10), Generic, now, 900));
    assert!(continues(&prev(OpenPrGuard, 900), OpenPrGuard, now, 900));
    assert!(!continues(&prev(OpenPrGuard, 901), OpenPrGuard, now, 900), "gone cold");
    assert!(!continues(&prev(OpenPrGuard, 10), Generic, now, 900));
    assert!(!continues(&prev(Generic, 10), OpenPrGuard, now, 900));
}
