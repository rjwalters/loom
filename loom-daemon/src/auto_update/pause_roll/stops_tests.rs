//! H4's parallel, bounded stops (#11051). Scripted host only: no process is
//! signalled.

use super::tests::{begin, cand, item, load, spawn_parking_agent, tuning, FakeHost};
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// How long each slow teardown takes.
const SLOW: Duration = Duration::from_millis(300);

/// Several slow teardowns and one that hangs (a scope stop that never
/// returns). Serially this pause takes `4 + 1 + 8` slow stops past the budget
/// and then never ends; in parallel it ends at the budget plus the margin,
/// with the hung tree's process group killed and recorded.
#[test]
fn slow_and_hung_teardowns_stop_in_parallel_within_the_budget_plus_the_margin() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let mut t = tuning(1000);
    t.stop_margin = Duration::from_millis(2000);
    let plan = begin(&drain, dir.path(), t);

    let mut cands = Vec::new();
    for i in 0..4 {
        cands.push(cand(dir.path(), &format!("young{i}"), 10 + i, 5));
    }
    for i in 0..8 {
        cands.push(cand(dir.path(), &format!("missed{i}"), 20 + i, 900));
    }
    cands.push(cand(dir.path(), "hung", 40, 900));
    cands.push(cand(dir.path(), "parked", 41, 900));
    let parked_dir = cands.last().unwrap().pause_dir.clone().unwrap();

    let (running, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let release = Arc::new(AtomicBool::new(false));
    let (running_in, most_in, release_in) = (running.clone(), most.clone(), release.clone());
    let host = Arc::new(FakeHost {
        cands,
        on_teardown: Some(Box::new(move |id| {
            let now = running_in.fetch_add(1, Ordering::SeqCst) + 1;
            most_in.fetch_max(now, Ordering::SeqCst);
            if id == "hung" {
                while !release_in.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(10));
                }
            } else {
                std::thread::sleep(SLOW);
            }
            running_in.fetch_sub(1, Ordering::SeqCst);
        })),
        ..FakeHost::default()
    });
    let parker = spawn_parking_agent(parked_dir);

    let started = Instant::now();
    let outcome = run_h4(&drain, host.clone(), &plan);
    let took = started.elapsed();
    release.store(true, Ordering::SeqCst);
    parker.join().unwrap();

    assert!(matches!(outcome, H4Outcome::Paused { .. }), "{outcome:?}");
    let bound = t.stop_bound();
    let m = load(&plan);
    let stop_ms = m
        .roll
        .pause_duration_ms
        .expect("the pause duration is recorded");
    // The hung teardown holds the stop phase to its bound, and no longer.
    assert!(
        stop_ms >= u64::try_from(bound.as_millis()).unwrap() - 50,
        "the stop phase ended before its bound with a teardown still hung: {stop_ms}ms"
    );
    // A serial stop never ends here (the hung teardown never returns), so
    // any finite bound separates the two. The slack is generous because the
    // H4 thread's own manifest writes are slow on a loaded host.
    let slack = Duration::from_secs(10);
    assert!(
        Duration::from_millis(stop_ms) < bound + slack,
        "the stop phase took {stop_ms}ms against a {bound:?} bound"
    );
    assert!(took < bound + t.forge_floor + slack, "{took:?}");

    let most = most.load(Ordering::SeqCst);
    assert!(most > 1, "the teardowns ran one at a time");
    assert!(most <= DEFAULT_STOP_CONCURRENCY, "{most} teardowns at once");

    // The dispositions are the serial stop's.
    assert_eq!(*host.forced.lock().unwrap(), vec!["hung".to_string()]);
    let hung = item(&m, "hung");
    assert_eq!(hung.reason.as_deref(), Some(REASON_BUDGET_MISSED));
    assert_eq!(hung.disposition, Disposition::Requeue);
    assert!(hung.stopped_at.is_some());
    assert!(m.events.iter().any(|e| e.item.as_deref() == Some("hung")
        && e.event == "stopped"
        && e.detail
            .as_deref()
            .is_some_and(|d| d.contains("stop bound"))));
    for i in 0..4 {
        let y = item(&m, &format!("young{i}"));
        assert_eq!(y.reason.as_deref(), Some(pause_classify::REASON_YOUNG));
        assert_eq!(y.status, ItemStatus::Requeued);
    }
    for i in 0..8 {
        let x = item(&m, &format!("missed{i}"));
        assert_eq!(x.reason.as_deref(), Some(REASON_BUDGET_MISSED));
        assert_eq!(x.status, ItemStatus::Requeued);
        assert!(x.stopped_at.is_some());
    }
    let parked = item(&m, "parked");
    assert_eq!(parked.status, ItemStatus::Paused);
    assert!(parked.safe_point.is_some() && parked.stopped_at.is_some());
    let events = host.item_events();
    assert_eq!(events.len(), 14, "one daemon.roll.item per item");
    let pe = events.iter().find(|e| e["item_id"] == "parked").unwrap();
    assert!(pe["teardown_ms"].is_u64() && pe["safe_point_wait_ms"].is_u64(), "{pe}");
    assert!(m
        .events
        .iter()
        .any(|e| e.event == "pause_completed"
            && e.detail.as_deref().is_some_and(|d| d.contains("ms"))));
}

/// Every budget-missed item is handed over at the deadline at once, and up
/// to [`DEFAULT_STOP_CONCURRENCY`] of them are torn down together. Each
/// teardown waits until eight are running at the same time (or gives up
/// after 30 s): a serial stop, or a pool that does not reach its cap, makes
/// them give up. Load-independent: nothing is measured by the clock.
#[test]
fn budget_missed_items_are_stopped_together_at_the_deadline_up_to_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let drain = DrainState::new();
    let plan = begin(&drain, dir.path(), tuning(300));
    let cands: Vec<_> = (0..10)
        .map(|i| cand(dir.path(), &format!("missed{i}"), 50 + i, 900))
        .collect();
    let (running, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let gave_up = Arc::new(AtomicBool::new(false));
    let (running_in, most_in, gave_up_in) = (running.clone(), most.clone(), gave_up.clone());
    let host = Arc::new(FakeHost {
        cands,
        on_teardown: Some(Box::new(move |_| {
            let now = running_in.fetch_add(1, Ordering::SeqCst) + 1;
            most_in.fetch_max(now, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(30);
            // Nobody leaves before eight have been running at once.
            while most_in.load(Ordering::SeqCst) < DEFAULT_STOP_CONCURRENCY {
                if Instant::now() >= until {
                    gave_up_in.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            running_in.fetch_sub(1, Ordering::SeqCst);
        })),
        ..FakeHost::default()
    });
    let mut t = plan.tuning;
    // Far past the 30 s give-up, so the bound never decides this test.
    t.stop_margin = Duration::from_secs(120);
    let plan = PausePlan { tuning: t, ..plan };

    assert!(matches!(run_h4(&drain, host.clone(), &plan), H4Outcome::Paused { .. }));

    assert!(!gave_up.load(Ordering::SeqCst), "the teardowns never ran eight at once");
    assert_eq!(most.load(Ordering::SeqCst), DEFAULT_STOP_CONCURRENCY, "the cap");
    assert_eq!(host.torn.lock().unwrap().len(), 10);
    assert!(host.forced.lock().unwrap().is_empty(), "nothing hung");
    let m = load(&plan);
    assert!(m.roll.pause_duration_ms.is_some());
    assert!(m
        .items
        .iter()
        .all(|i| i.reason.as_deref() == Some(REASON_BUDGET_MISSED)));
    assert!(m.items.iter().all(|i| i.status == ItemStatus::Requeued));
}
