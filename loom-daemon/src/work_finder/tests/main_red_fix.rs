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
        1.into(),
        &[false],
        None,
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
    tick_with_lanes(src, &mut dispatcher, (1.into(), usize::MAX), false, false, RED).unwrap();
    assert_eq!(dispatcher.dispatched, vec![9]);
}

#[test]
fn a_red_main_fix_outranks_a_starred_issue() {
    // #11103 bridge: a verified red-main fix is `very-important`, the star
    // `important`, so the fix goes first.
    let starred = WorkItem::new(5, vec![OPERATOR_PRIORITY_LABEL.into()]);
    let mut multi = vec![(FakeSource::once(vec![fix(9), starred]), RecordingDispatcher::default())];
    tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        1.into(),
        &[false],
        None,
        usize::MAX,
        false,
        None,
        None,
        &[RED],
    );
    // The fix fills the cap of 1; the star then takes the overflow slot.
    assert_eq!(multi[0].1.dispatched, vec![9, 5]);
}

#[test]
fn a_halted_red_repo_admits_its_fixes_and_nothing_else() {
    let starred = WorkItem::new(5, vec![OPERATOR_PRIORITY_LABEL.into()]);
    let items = || vec![issue(1), starred.clone(), fix(9)];

    let mut multi = vec![(FakeSource::once(items()), RecordingDispatcher::default())];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        10.into(),
        &[true],
        None,
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
    let report =
        tick_with_lanes(src, &mut dispatcher, (10.into(), usize::MAX), true, false, RED).unwrap();
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
            10.into(),
            &[true],
            None,
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
        tick_with_lanes(src, &mut dispatcher, (10.into(), usize::MAX), true, false, lane).unwrap();
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
        2.into(),
        &[false],
        None,
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
            1.into(),
            &[false],
            None,
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

#[test]
fn the_ci_fallback_reads_the_repos_default_branch() {
    use crate::work_finder::main_red_fix::default_branch_for;
    let git = |dir: &std::path::Path, args: &[&str]| {
        let ok = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    };
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    // No `origin/HEAD`: fall back to `main`.
    assert_eq!(default_branch_for(dir.path()), "main");
    git(
        dir.path(),
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
        ],
    );
    assert_eq!(default_branch_for(dir.path()), "trunk");
}

// ---- #10118: unpromoted fixes (triage / curated) and escalation ----

use crate::comment_trust::{Author, TrustPolicy};
use crate::forge_identity::FleetLogins;
use crate::work_finder::main_red_fix::{merge_red_fix_candidates, RedFixWatch};

const MARKER_BODY: &str = "Fixes red main.\n\n<!-- loom:main-red-fix -->\n";

/// A marker-bearing fix still carrying `label` (`loom:triage` / `loom:curated`),
/// filed by the repo owner (a trusted author, #9548).
fn unpromoted(n: u32, label: &str) -> WorkItem {
    unpromoted_by(n, label, Author::new(Some("rjwalters"), Some("OWNER")))
}

/// [`unpromoted`], filed by `author`.
fn unpromoted_by(n: u32, label: &str, author: Author) -> WorkItem {
    WorkItem::new(n, vec![label.into()])
        .with_body(Some(MARKER_BODY.into()))
        .with_author(Some(author))
}

/// The trust rules a repo with one fleet App (`loom-fleet-dispatch`) resolves
/// to: insiders by association, the fleet App, nothing else.
fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::single("loom-fleet-dispatch"), None, Vec::new())
}

/// [`merge_red_fix_candidates`] under [`policy`].
fn merge(ready: Vec<WorkItem>, rows: Vec<WorkItem>) -> Vec<WorkItem> {
    let policy = policy();
    merge_red_fix_candidates(ready, rows, |a| policy.trusts(a))
}

/// Run one multi-workspace tick over `items` with `lane`; return what was
/// dispatched and the tick's `seen` count.
fn multi_tick(
    items: Vec<WorkItem>,
    lane: RedMainLane,
    halted: bool,
    ci_red: bool,
) -> (Vec<u32>, usize) {
    let dispatcher = RecordingDispatcher {
        ci_red,
        ..RecordingDispatcher::default()
    };
    let mut multi = vec![(FakeSource::once(items), dispatcher)];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[100],
        1.into(),
        &[halted],
        None,
        usize::MAX,
        false,
        None,
        None,
        &[lane],
    );
    (multi[0].1.dispatched.clone(), report.seen)
}

#[test]
fn an_unpromoted_fix_is_buildable_on_a_red_repo() {
    for label in ["loom:triage", "loom:curated"] {
        let fix = unpromoted(9, label);
        assert!(fix.is_unpromoted_red_fix(), "{label}");
        let items = || {
            vec![
                dated(issue(1), "2026-01-01T00:00:00Z"),
                dated(fix.clone(), "2026-09-01T00:00:00Z"),
            ]
        };
        // Red: the fix is a candidate, ahead of the older ordinary issue.
        assert_eq!(multi_tick(items(), RED, false, false), (vec![9], 2), "{label}");
        // Red and halted by the gate: it is still admitted, alone.
        assert_eq!(multi_tick(items(), RED, true, false).0, vec![9], "{label}");

        let mut dispatcher = RecordingDispatcher::default();
        let src = &mut FakeSource::once(items());
        let report =
            tick_with_lanes(src, &mut dispatcher, (1.into(), usize::MAX), false, false, RED)
                .unwrap();
        assert_eq!((dispatcher.dispatched, report.seen), (vec![9], 2), "{label}");
    }
}

#[test]
fn an_unpromoted_fix_is_inert_on_a_green_repo() {
    let items = || vec![issue(1), unpromoted(9, "loom:triage")];
    // Not a candidate at all: the ordinary issue is dispatched and the fix is
    // not even counted as seen.
    assert_eq!(multi_tick(items(), RedMainLane::default(), false, false), (vec![1], 1));
    // A green repo the gate halted (a non-red hold) admits nothing.
    assert!(multi_tick(items(), RedMainLane::default(), true, false)
        .0
        .is_empty());

    let mut dispatcher = RecordingDispatcher::default();
    let src = &mut FakeSource::once(items());
    let lane = RedMainLane::default();
    let report =
        tick_with_lanes(src, &mut dispatcher, (5.into(), usize::MAX), false, false, lane).unwrap();
    assert_eq!((dispatcher.dispatched, report.seen), (vec![1], 1));
}

#[test]
fn with_the_gate_disabled_ci_decides_whether_an_unpromoted_fix_is_admitted() {
    let gate_off = RedMainLane {
        gate_disabled: true,
        ..RedMainLane::default()
    };
    let items = || vec![unpromoted(9, "loom:triage")];
    assert_eq!(multi_tick(items(), gate_off, false, true).0, vec![9], "CI red");
    assert!(
        multi_tick(items(), gate_off, false, false).0.is_empty(),
        "CI green or unavailable"
    );
}

#[test]
fn an_unpromoted_fix_still_goes_through_the_skip_filters() {
    for skip in [
        "loom:blocked",
        "loom:operator-only",
        "loom:operator",
        "loom:operator-decision",
    ] {
        let mut fix = unpromoted(9, "loom:triage");
        fix.labels.push(skip.into());
        assert!(multi_tick(vec![fix], RED, false, false).0.is_empty(), "{skip}");
    }
}

#[test]
fn the_unpromoted_listing_keeps_only_unclaimed_marker_rows() {
    let ready = vec![fix(1)];
    let mut rows = vec![
        unpromoted(1, "loom:triage"), // already listed as loom:issue
        unpromoted(2, "loom:triage"),
        WorkItem::new(3, vec!["loom:triage".into()]), // no marker
        issue(4).with_body(Some("Quote `<!-- loom:main-red-fix -->` here".into())),
    ];
    for (n, extra) in [
        (5, "loom:building"),
        (6, "loom:curating"),
        (7, "loom:epic"),
        (8, "loom:architect"),
        (9, "loom:hermit"),
        (10, "loom:auditor"),
    ] {
        let mut row = unpromoted(n, "loom:curated");
        row.labels.push(extra.into());
        rows.push(row);
    }
    let merged = merge(ready, rows);
    let numbers: Vec<u32> = merged.iter().map(|i| i.number).collect();
    assert_eq!(numbers, vec![1, 2]);
    // The second listing dedups against the first.
    let merged = merge(merged, vec![unpromoted(2, "loom:curated")]);
    assert_eq!(merged.len(), 2);
    // A promoted or starred fix is not "unpromoted": it is a candidate anyway.
    assert!(!fix(1).is_unpromoted_red_fix());
    let mut starred = unpromoted(2, "loom:triage");
    starred.labels.push(OPERATOR_PRIORITY_LABEL.into());
    assert!(!starred.is_unpromoted_red_fix());
}

/// #10118 / #9548: the marker is content, not control, unless a trusted
/// identity filed the issue. An outsider's marker-bearing triage issue is not
/// admitted unpromoted (it would otherwise skip the human `loom:issue`
/// promotion on a red repo); the fleet App's and an insider's are.
#[test]
fn an_untrusted_authors_marker_is_not_admitted_unpromoted() {
    let rows = vec![
        unpromoted_by(1, "loom:triage", Author::new(Some("mallory"), Some("NONE"))),
        // A merged fork PR makes anyone a contributor: still untrusted.
        unpromoted_by(2, "loom:triage", Author::new(Some("eve"), Some("CONTRIBUTOR"))),
        // A user account named like the fleet App is not the App.
        unpromoted_by(3, "loom:curated", Author::new(Some("loom-fleet-dispatch"), Some("NONE"))),
        // Another installation's App is not ours.
        unpromoted_by(4, "loom:triage", Author::new(Some("other-fleet[bot]"), Some("NONE"))),
        // No author at all (a listing without one) is never trusted.
        WorkItem::new(5, vec!["loom:triage".into()]).with_body(Some(MARKER_BODY.into())),
        // Trusted: the fleet App, a member, the owner.
        unpromoted_by(
            6,
            "loom:triage",
            Author::new(Some("loom-fleet-dispatch[bot]"), Some("NONE")),
        ),
        unpromoted_by(7, "loom:curated", Author::new(Some("teammate"), Some("MEMBER"))),
        unpromoted(8, "loom:triage"),
    ];
    let merged = merge(Vec::new(), rows);
    let numbers: Vec<u32> = merged.iter().map(|i| i.number).collect();
    assert_eq!(numbers, vec![6, 7, 8]);

    // End to end on a red, halted repo: only the trusted fix is dispatched,
    // and the untrusted one is not even seen.
    let rows = vec![
        unpromoted_by(1, "loom:triage", Author::new(Some("mallory"), Some("NONE"))),
        unpromoted(2, "loom:triage"),
    ];
    let (dispatched, seen) = multi_tick(merge(Vec::new(), rows), RED, true, false);
    assert_eq!(dispatched, vec![2]);
    assert_eq!(seen, 1);
}

/// The trust policy is consulted only for marker-bearing rows that survive
/// the other filters, so a repo with no such row never resolves one.
#[test]
fn trust_is_consulted_only_for_marker_rows() {
    let mut asked = Vec::new();
    let rows = vec![
        WorkItem::new(1, vec!["loom:triage".into()]),
        unpromoted_by(2, "loom:triage", Author::new(Some("mallory"), Some("NONE"))),
    ];
    let merged = merge_red_fix_candidates(Vec::new(), rows, |a| {
        asked.push(a.login.clone());
        false
    });
    assert!(merged.is_empty());
    assert_eq!(asked, vec![Some("mallory".to_string())]);
}

#[test]
fn the_watch_alerts_once_per_fix_past_the_threshold() {
    let after = std::time::Duration::from_secs(1800);
    let t0 = std::time::Instant::now();
    let at = |mins: u64| t0 + std::time::Duration::from_secs(mins * 60);
    let mut watch = RedFixWatch::default();
    assert!(watch.observe(at(0), true, &[9], after).is_empty());
    assert!(watch.observe(at(29), true, &[9], after).is_empty());
    let due = watch.observe(at(30), true, &[9], after);
    assert_eq!(due.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![9]);
    watch.record_attempt(at(30), 9, true);
    assert!(watch.observe(at(45), true, &[9], after).is_empty(), "once");

    // Claimed (left the waiting set), then released again: the clock restarts.
    assert!(watch.observe(at(46), true, &[], after).is_empty());
    assert!(watch.observe(at(47), true, &[9], after).is_empty());
    assert!(watch.observe(at(76), true, &[9], after).is_empty());
    assert_eq!(watch.observe(at(77), true, &[9], after).len(), 1);
    watch.record_attempt(at(77), 9, true);

    // A green tick resets everything; a green repo never alerts.
    let mut watch = RedFixWatch::default();
    assert!(watch.observe(at(0), true, &[9], after).is_empty());
    assert!(watch.observe(at(20), false, &[9], after).is_empty());
    assert!(watch.observe(at(40), true, &[9], after).is_empty(), "clock restarted at 40");
    assert!(watch.observe(at(100), false, &[9], after).is_empty());
}

#[test]
fn a_failed_alert_filing_is_retried_with_backoff_until_it_succeeds() {
    let after = std::time::Duration::from_secs(30 * 60);
    let t0 = std::time::Instant::now();
    let at = |secs: u64| t0 + std::time::Duration::from_secs(secs);
    let mut watch = RedFixWatch::default();
    assert!(watch.observe(at(0), true, &[9], after).is_empty());
    assert_eq!(watch.observe(at(1800), true, &[9], after).len(), 1);
    watch.record_attempt(at(1800), 9, false); // sink failed once

    assert!(watch.observe(at(1830), true, &[9], after).is_empty(), "backing off 60s");
    assert_eq!(watch.observe(at(1860), true, &[9], after).len(), 1, "retried");
    watch.record_attempt(at(1860), 9, false); // failed again: 120s
    assert!(watch.observe(at(1950), true, &[9], after).is_empty());
    assert_eq!(watch.observe(at(1980), true, &[9], after).len(), 1);
    watch.record_attempt(at(1980), 9, true); // finally filed

    assert!(watch.observe(at(2100), true, &[9], after).is_empty(), "no refile after success");
    assert!(watch.observe(at(9000), true, &[9], after).is_empty());
}

/// A dispatcher recording the escalation calls the tick makes.
#[derive(Default)]
struct EscalationRecorder {
    inner: RecordingDispatcher,
    calls: Vec<(bool, Vec<u32>)>,
}

impl WorkDispatcher for EscalationRecorder {
    fn in_flight(&self) -> HashSet<u32> {
        self.inner.in_flight()
    }
    fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool> {
        self.inner.dispatch(issue, complexity)
    }
    fn escalate_red_fix(&mut self, red: bool, waiting: &[u32]) {
        self.calls.push((red, waiting.to_vec()));
    }
}

#[test]
fn the_tick_feeds_the_escalation_with_the_fixes_waiting_on_a_red_main() {
    let items = || vec![issue(1), unpromoted(9, "loom:triage"), fix(7)];
    for (lane, halted, expect) in [
        (RED, false, (true, vec![9, 7])),
        (RED, true, (true, vec![9, 7])),
        (RedMainLane::default(), false, (false, vec![])),
    ] {
        let mut multi = vec![(FakeSource::once(items()), EscalationRecorder::default())];
        tick_multi_with_repo_cap(
            &mut multi,
            &[100],
            0.into(),
            &[halted],
            None,
            usize::MAX,
            false,
            None,
            None,
            &[lane],
        );
        assert_eq!(multi[0].1.calls, vec![expect.clone()], "{lane:?} halted={halted}");

        let mut dispatcher = EscalationRecorder::default();
        let src = &mut FakeSource::once(items());
        tick_with_lanes(src, &mut dispatcher, (0.into(), usize::MAX), halted, false, lane).unwrap();
        assert_eq!(dispatcher.calls, vec![expect], "single: {lane:?} halted={halted}");
    }
}
