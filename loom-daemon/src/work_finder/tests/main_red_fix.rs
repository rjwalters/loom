//! The red-main-fix lane (#9244 §6): key 3 only on a verified-red `main`,
//! a halted repo admits only its fixes, and the CI fallback when the gate is
//! disabled.

use super::*;
use crate::work_finder::main_red_fix::latest_run_is_failure;

fn fix(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into()])
        .with_body(Some("Fixes red main.\n\n<!-- loom:main-red-fix -->\n".into()))
}

fn dated(item: WorkItem, created_at: &str) -> WorkItem {
    WorkItem {
        created_at: Some(created_at.to_string()),
        ..item
    }
}

const RED: RedMainLane = RedMainLane {
    verified_red: true,
    gate_disabled: false,
    other_hold: false,
};

#[test]
fn the_marker_is_line_anchored() {
    assert!(fix(1).is_main_red_fix());
    assert!(!issue(1).is_main_red_fix());
    let quoted = issue(2).with_body(Some("Add `<!-- loom:main-red-fix -->` to it".into()));
    assert!(!quoted.is_main_red_fix(), "prose quoting the marker does not fire");
}

#[test]
fn a_fix_sorts_first_only_while_main_is_verified_red() {
    // The fix (#9) is newer than #1. On a red repo it goes first; on a green
    // repo the marker is inert and age decides.
    let items = || {
        vec![
            dated(issue(1), "2026-01-01T00:00:00Z"),
            dated(fix(9), "2026-09-01T00:00:00Z"),
        ]
    };
    let mut multi = vec![(FakeSource::once(items()), RecordingDispatcher::default())];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        1,
        &[false],
        usize::MAX,
        false,
        None,
        None,
        &[RED],
    );
    assert_eq!(report.dispatched, 1);
    assert_eq!(multi[0].1.dispatched, vec![9], "red main: the fix jumps the queue");

    let mut multi = vec![(FakeSource::once(items()), RecordingDispatcher::default())];
    tick_multi(&mut multi, &[100], 1, &[false]);
    assert_eq!(multi[0].1.dispatched, vec![1], "green main: no boost");

    // Single-workspace path.
    let mut dispatcher = RecordingDispatcher::default();
    let src = &mut FakeSource::once(items());
    tick_with_lanes(src, &mut dispatcher, (1, usize::MAX), false, false, RED).unwrap();
    assert_eq!(dispatcher.dispatched, vec![9]);
}

#[test]
fn a_starred_issue_still_outranks_a_red_main_fix() {
    let starred = WorkItem::new(5, vec![OPERATOR_PRIORITY_LABEL.into()]);
    let mut multi = vec![(FakeSource::once(vec![fix(9), starred]), RecordingDispatcher::default())];
    tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        1,
        &[false],
        usize::MAX,
        false,
        None,
        None,
        &[RED],
    );
    assert_eq!(multi[0].1.dispatched, vec![5]);
}

#[test]
fn a_halted_red_repo_admits_its_fixes_and_nothing_else() {
    let starred = WorkItem::new(5, vec![OPERATOR_PRIORITY_LABEL.into()]);
    let items = || vec![issue(1), starred.clone(), fix(9)];

    let mut multi = vec![(FakeSource::once(items()), RecordingDispatcher::default())];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        10,
        &[true],
        usize::MAX,
        false,
        None,
        None,
        &[RED],
    );
    assert!(report.halted, "report.halted semantics are unchanged");
    assert_eq!(multi[0].1.dispatched, vec![9], "only the fix is admitted");
    let rows = ready_queue::finish(&report.queue, &[]);
    let halted: Vec<u32> = rows
        .iter()
        .filter(|r| r.disposition == Qd::WorkspaceHalted)
        .map(|r| r.issue)
        .collect();
    assert_eq!(halted, vec![5, 1], "the starred issue waits with the rest");
    assert!(rows.iter().any(|r| r.issue == 9 && r.main_red_fix));

    let mut dispatcher = RecordingDispatcher::default();
    let src = &mut FakeSource::once(items());
    let report = tick_with_lanes(src, &mut dispatcher, (10, usize::MAX), true, false, RED).unwrap();
    assert!(report.halted);
    assert_eq!(dispatcher.dispatched, vec![9]);
}

#[test]
fn a_hold_that_is_not_a_verified_red_main_admits_nothing() {
    // Gate in flight / drain / breaker / pool hold: `halted` without (or on
    // top of) a verified red main. Nothing is admitted, fixes included.
    for lane in [
        RedMainLane::default(),
        RedMainLane {
            other_hold: true,
            ..RED
        },
    ] {
        let mut multi = vec![(FakeSource::once(vec![fix(9)]), RecordingDispatcher::default())];
        let report = tick_multi_with_repo_cap(
            &mut multi,
            &[100],
            10,
            &[true],
            usize::MAX,
            false,
            None,
            None,
            &[lane],
        );
        assert!(multi[0].1.dispatched.is_empty(), "{lane:?}");
        assert!(report.halted);

        let mut dispatcher = RecordingDispatcher::default();
        let src = &mut FakeSource::once(vec![fix(9)]);
        tick_with_lanes(src, &mut dispatcher, (10, usize::MAX), true, false, lane).unwrap();
        assert!(dispatcher.dispatched.is_empty(), "{lane:?}");
    }
}

#[test]
fn a_red_main_fix_does_not_get_the_overflow_slot() {
    let mut multi = vec![(
        FakeSource::once(vec![fix(9)]),
        RecordingDispatcher {
            in_flight: [1000, 1001].into_iter().collect(),
            ..RecordingDispatcher::default()
        },
    )];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        2,
        &[false],
        usize::MAX,
        false,
        None,
        None,
        &[RED],
    );
    assert_eq!((report.dispatched, report.deferred_capacity), (0, 1));
}

#[test]
fn with_the_gate_disabled_ci_decides_red() {
    let gate_off = RedMainLane {
        gate_disabled: true,
        ..RedMainLane::default()
    };
    let items = || vec![issue(1), fix(9)];
    for (ci_red, expect) in [(true, 9), (false, 1)] {
        let dispatcher = RecordingDispatcher {
            ci_red,
            ..RecordingDispatcher::default()
        };
        let mut multi = vec![(FakeSource::once(items()), dispatcher)];
        tick_multi_with_repo_cap(
            &mut multi,
            &[100],
            1,
            &[false],
            usize::MAX,
            false,
            None,
            None,
            &[gate_off],
        );
        assert_eq!(multi[0].1.dispatched, vec![expect], "ci_red={ci_red}");
    }
    // The CI read happens only when a marker-bearing candidate exists.
    let mut asked = false;
    assert!(!gate_off.is_red(&[issue(1)], || {
        asked = true;
        true
    }));
    assert!(!asked, "no marker, no forge read");
    // An enabled gate never consults CI.
    assert!(!RedMainLane::default().is_red(&[fix(9)], || true));
}

#[test]
fn the_ci_fallback_reads_the_newest_commits_runs() {
    let red = r#"[
        {"headSha":"new","status":"completed","conclusion":"failure","workflowName":"CI"},
        {"headSha":"new","status":"completed","conclusion":"success","workflowName":"Lint"},
        {"headSha":"old","status":"completed","conclusion":"success","workflowName":"CI"}
    ]"#;
    assert!(latest_run_is_failure(red));
    let green = r#"[
        {"headSha":"new","status":"completed","conclusion":"success","workflowName":"CI"},
        {"headSha":"old","status":"completed","conclusion":"failure","workflowName":"CI"}
    ]"#;
    assert!(!latest_run_is_failure(green), "an older red commit is not today's main");
    assert!(!latest_run_is_failure("[]"));
    assert!(!latest_run_is_failure("not json"));
}
