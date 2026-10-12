//! #11263: every tree H4 stops is noted, before it is signalled, with what the
//! roll did with it, so the run's own close reads `interrupted_by_roll` with
//! that disposition instead of `failure`. Scripted host only.

use super::tests::{begin, cand, item, load, spawn_parking_agent, tuning, FakeHost};
use super::*;
use crate::auto_update::pause_classify;
use crate::observability::lifecycle::roll_interruption::{noted, Interruption};

/// Issue numbers no other test uses, so the noted pids are this test's own.
const YOUNG: u32 = 1_126_301;
const PARKED: u32 = 1_126_302;
const MISSED: u32 = 1_126_303;

#[test]
fn every_stopped_tree_is_noted_with_the_disposition_the_manifest_records() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(1500));
    let cands = vec![
        cand(dir.path(), "young", YOUNG, 5),
        cand(dir.path(), "parked", PARKED, 900),
        cand(dir.path(), "missed", MISSED, 900),
    ];
    let pid = |issue: u32| {
        cands
            .iter()
            .find(|c| c.issue == Some(issue))
            .and_then(|c| c.pid)
            .unwrap()
    };
    let (young, parked, missed) = (pid(YOUNG), pid(PARKED), pid(MISSED));
    for p in [young, parked, missed] {
        assert_eq!(noted(p), None, "nothing is noted before the pause");
    }
    let parker = spawn_parking_agent(cands[1].pause_dir.clone().unwrap());
    let host = Arc::new(FakeHost {
        cands,
        ..FakeHost::default()
    });

    let outcome = run_h4(&drain, host, &plan);
    parker.join().unwrap();
    assert!(matches!(outcome, H4Outcome::Paused { .. }), "{outcome:?}");

    let m = load(&plan);
    assert_eq!(item(&m, "young").reason.as_deref(), Some(pause_classify::REASON_YOUNG));
    assert_eq!(item(&m, "parked").status, ItemStatus::Paused);
    assert_eq!(item(&m, "missed").reason.as_deref(), Some(REASON_BUDGET_MISSED));
    for (id, p, disposition) in [
        ("young", young, RollDisposition::Requeued),
        ("parked", parked, RollDisposition::Paused),
        ("missed", missed, RollDisposition::BudgetMissed),
    ] {
        let expected = Interruption {
            item_id: id.to_string(),
            disposition,
        };
        assert_eq!(noted(p), Some(expected), "{id}");
        // What recovery would read off the same manifest agrees.
        assert_eq!(RollDisposition::of_item(item(&m, id)), Some(disposition), "{id}");
    }
}

/// AC (#11263): a sweep whose merge is in flight (last checkpoint
/// `judge-done`) is classified like any other agent: `resume` (stopped only
/// at a safe point, i.e. between tool calls, so a running `merge-pr.sh` call
/// finishes first) or `requeue`. The checkpoint phase never selects a third,
/// kill-outside-the-budget path; `pause_classify::classify` does not read it.
#[test]
fn a_sweep_mid_merge_is_classified_resume_or_requeue_like_any_other() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    for (age, expected) in [(900, Disposition::Resume), (5, Disposition::Requeue)] {
        let mut merging = cand(dir.path(), "merging", 1_126_310, age);
        merging.checkpoint_phase = Some("judge-done".to_string());
        let mut building = merging.clone();
        building.checkpoint_phase = Some("builder".to_string());
        let (merging, building) =
            (manifest_item(&merging, now, 300), manifest_item(&building, now, 300));
        assert_eq!(merging.disposition, expected);
        assert_eq!(
            (merging.disposition, merging.reason),
            (building.disposition, building.reason),
            "the merge phase changes nothing"
        );
        assert_eq!(merging.checkpoint_phase.as_deref(), Some("judge-done"));
    }
}
