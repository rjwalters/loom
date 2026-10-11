//! Exits of a vanished row with no resolved PR, from its issue's close
//! (#11398), beside the shared helpers of `outcomes_tests`.

use super::*;

fn held_curator() -> HeldSweep {
    HeldSweep {
        repo: REPO.to_string(),
        issue: 20,
        stage: FleetStage::SweepCurator,
        entered_at: t0() - Duration::minutes(2),
        entered_at_lower_bound: false,
        pr: None,
        overflow: false,
        start: Default::default(),
        sweep_id: None,
    }
}

/// Issue 20 closed at `t0 + 7m` as `facts` says.
fn closed_at_7(facts: fn(DateTime<Utc>) -> IssueFacts) -> (DateTime<Utc>, Forge) {
    let at = t0() + Duration::minutes(7);
    let mut forge = Forge::default();
    forge.issues.insert(20, facts(at));
    (at, forge)
}

/// A held sweep-curator row for issue 20, gone from pass 1 on.
fn vanishing(passes: usize) -> Vec<FleetInput> {
    let mut inputs = vec![input(&[], vec![held_curator()])];
    inputs.extend((0..passes).map(|_| input(&[], Vec::new())));
    inputs
}

#[test]
fn an_issue_closed_not_planned_mid_sweep_is_one_abandoned_exit_at_the_close_time() {
    let (closed, mut forge) = closed_at_7(IssueFacts::NotMerged);
    let out = run(&vanishing(2), Duration::minutes(10), &mut forge);
    let r = stages(&out[1]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!(r[0].stage, FleetStage::SweepCurator);
    assert_eq!(r[0].exit, StageExit::Abandoned);
    assert_eq!(r[0].next_stage, None);
    assert_eq!(r[0].forge_transition_at, Some(closed));
    assert_eq!(r[0].left_at, closed);
    assert_eq!(r[0].dwell_sec, Some(9 * 60));
    assert_eq!(r[0].resolution_sec, Some(0));
    assert!(stages(&out[2]).is_empty(), "no second exit");
}

/// Test plan (b): a merge closed the issue though the row never had a PR, so
/// the stay landed at the merge instant, never `abandoned`.
#[test]
fn a_merge_closed_issue_without_a_pr_row_is_one_landed_exit_at_the_merge_time() {
    let (merged, mut forge) = closed_at_7(IssueFacts::Merged);
    let out = run(&vanishing(2), Duration::minutes(10), &mut forge);
    let r = stages(&out[1]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!(r[0].exit, StageExit::Landed);
    assert_eq!(r[0].pr_number, None);
    assert_eq!(r[0].forge_transition_at, Some(merged));
    assert_eq!(r[0].left_at, merged);
    assert_eq!(r[0].dwell_sec, Some(9 * 60));
    assert!(stages(&out[2]).is_empty(), "no second exit");
}

/// A hand close as `completed` has no closing commit, so no merge closed
/// it: one `abandoned` exit at `closed_at`, with dwell and a forge key.
#[test]
fn a_hand_completed_close_is_one_abandoned_exit_at_the_close_time() {
    let at = t0() + Duration::minutes(7);
    let mut forge = Forge::default();
    let events = serde_json::json!([closed_event("2026-10-09T12:07:00Z", None)]);
    forge.issues.insert(20, IssueFacts::closed(at, &[events]));
    let out = run(&vanishing(2), Duration::minutes(10), &mut forge);
    let r = stages(&out[1]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!((r[0].exit, r[0].forge_transition_at), (StageExit::Abandoned, Some(at)));
    assert_eq!((r[0].left_at, r[0].dwell_sec), (at, Some(9 * 60)));
    assert!(r[0].has_fact_key());
    assert!(stages(&out[2]).is_empty(), "no second exit");
}

/// A close no `closed` event explains is `unknown`, but still ends at
/// `closed_at`, so it keeps a forge key.
#[test]
fn a_close_with_no_closed_event_is_unknown_at_the_close_time() {
    let (closed, mut forge) = closed_at_7(IssueFacts::ClosedUnresolved);
    let out = run(&vanishing(2), Duration::minutes(10), &mut forge);
    let r = stages(&out[1]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!((r[0].exit, r[0].forge_transition_at), (StageExit::Unknown, Some(closed)));
    assert_eq!((r[0].left_at, r[0].dwell_sec), (closed, None));
    assert!(r[0].has_fact_key());
    assert!(stages(&out[2]).is_empty(), "no second exit");
}

#[test]
fn two_hosts_diffing_one_close_emit_the_same_forge_instant() {
    for facts in [
        IssueFacts::NotMerged as fn(_) -> _,
        IssueFacts::Merged,
        IssueFacts::ClosedUnresolved,
    ] {
        let run_host = |step: i64| {
            let (_, mut forge) = closed_at_7(facts);
            let out = run(&vanishing(1), Duration::minutes(step), &mut forge);
            let r = stages(&out[1])[0].clone();
            (r.exit, r.stage, r.next_stage, r.forge_transition_at)
        };
        assert_eq!(run_host(10), run_host(25));
    }
}

#[test]
fn an_unread_issue_emits_nothing_until_it_reads_then_once() {
    let (closed, mut forge) = closed_at_7(IssueFacts::NotMerged);
    forge.issue_failures = 1;
    let out = run(&vanishing(3), Duration::minutes(10), &mut forge);
    assert!(stages(&out[1]).is_empty(), "no timeless exit on a failed read");
    let r = stages(&out[2]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!((r[0].exit, r[0].forge_transition_at), (StageExit::Abandoned, Some(closed)));
    assert!(stages(&out[3]).is_empty());
}

/// Test plan (f) and the cap: a transferred or deleted issue answers 404,
/// which is a failed read. It waits [`PENDING_ISSUE_MAX_PASSES`] failed
/// retries, then is one `unknown` stamped at the pass that first saw it gone,
/// with no forge instant (so no fact id), and is never emitted again.
#[test]
fn a_transferred_or_404_issue_is_one_unknown_after_the_cap_and_never_again() {
    let mut forge = Forge {
        issue_failures: usize::MAX,
        ..Forge::default()
    };
    let cap = PENDING_ISSUE_MAX_PASSES as usize;
    let out = run(&vanishing(cap + 5), Duration::minutes(5), &mut forge);
    // Pass 1 sees it gone; passes 2..=cap+1 are its failed retries.
    for (i, pass) in out.iter().enumerate() {
        let r = stages(pass);
        if i == cap + 1 {
            assert_eq!(r.len(), 1, "pass {i}: {out:?}");
            assert_eq!((r[0].exit, r[0].forge_transition_at), (StageExit::Unknown, None));
            assert_eq!(r[0].left_at, t0() + Duration::minutes(5), "first-seen-gone pass");
            assert_eq!(r[0].dwell_sec, None);
        } else {
            assert!(r.is_empty(), "pass {i}: {out:?}");
        }
    }
    assert_eq!(forge.issue_reads, cap + 1, "no read once it is emitted");
}

#[test]
fn a_vanished_row_whose_issue_is_still_open_stays_unknown() {
    let mut forge = Forge::default();
    let out = run(&vanishing(1), Duration::minutes(10), &mut forge);
    assert_eq!(stages(&out[1])[0].exit, StageExit::Unknown);
}

fn closed_event(at: &str, commit: Option<&str>) -> serde_json::Value {
    serde_json::json!({"event": "closed", "created_at": at, "commit_id": commit})
}

#[test]
fn a_closed_event_with_a_commit_is_a_merge() {
    let at = t0();
    let page = serde_json::json!([
        {"event": "labeled", "created_at": "2026-10-09T10:00:00Z", "label": {"name": "loom:issue"}},
        closed_event("2026-10-09T12:00:00Z", Some("abc123")),
    ]);
    assert_eq!(IssueFacts::closed(at, &[page]), IssueFacts::Merged(at));
}

#[test]
fn a_closed_event_with_no_commit_is_unmerged() {
    let at = t0();
    let page = serde_json::json!([closed_event("2026-10-09T12:00:00Z", None)]);
    assert_eq!(IssueFacts::closed(at, &[page]), IssueFacts::NotMerged(at));
}

#[test]
fn only_the_latest_close_says_how_it_closed() {
    let at = t0();
    // Merged once, reopened, then closed by hand.
    let pages = [
        serde_json::json!([closed_event("2026-10-09T09:00:00Z", Some("abc123"))]),
        serde_json::json!([
            {"event": "reopened", "created_at": "2026-10-09T10:00:00Z"},
            closed_event("2026-10-09T12:00:00Z", None),
        ]),
    ];
    assert_eq!(IssueFacts::closed(at, &pages), IssueFacts::NotMerged(at));
}

#[test]
fn no_closed_event_keeps_the_close_instant_unresolved() {
    let page = serde_json::json!([]);
    assert_eq!(IssueFacts::closed(t0(), &[page]), IssueFacts::ClosedUnresolved(t0()));
}
