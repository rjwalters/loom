//! The tick deadline's gather walk (#12074): truncation, rotation, coverage.

use super::*;

/// Walk `n` repos from `start`, with the deadline passing once `budget` repos
/// have been gathered. Returns the gathered order and the walk.
fn walk(n: usize, start: usize, budget: usize) -> (Vec<usize>, GatherWalk) {
    let mut w = GatherWalk::new(n, start, Instant::now());
    let mut gathered = Vec::new();
    for idx in w.order().collect::<Vec<_>>() {
        let passed = gathered.len() >= budget;
        if w.admit(idx, passed) {
            gathered.push(idx);
        }
    }
    (gathered, w)
}

#[test]
fn no_deadline_gathers_every_repo_and_keeps_the_start() {
    let (gathered, w) = walk(5, 2, usize::MAX);
    assert_eq!(gathered, vec![2, 3, 4, 0, 1]);
    let stats = w.stats(Instant::now());
    assert_eq!((stats.repos_gathered, stats.repos_skipped, stats.truncated), (5, 0, false));
    assert_eq!(w.next_start(), 2);
}

#[test]
fn a_passed_deadline_truncates_and_the_next_tick_starts_at_the_skipped() {
    let (gathered, w) = walk(5, 0, 2);
    assert_eq!(gathered, vec![0, 1]);
    let stats = w.stats(Instant::now());
    assert_eq!((stats.repos_gathered, stats.repos_skipped, stats.truncated), (2, 3, true));
    assert_eq!(w.next_start(), 2, "the next tick resumes at the first repo skipped");
}

/// A run of truncated ticks covers every repo, never re-reading the first
/// few forever.
#[test]
fn successive_truncated_ticks_cover_every_repo() {
    let mut seen = std::collections::BTreeSet::new();
    let mut start = 0;
    for _ in 0..4 {
        let (gathered, w) = walk(7, start, 2);
        seen.extend(gathered);
        start = w.next_start();
    }
    assert_eq!(seen.len(), 7, "{seen:?}");
}

/// A tick whose deadline passed before the gather began still gathers one
/// repo, so it makes progress (its reads fail fast to the last good listing).
#[test]
fn a_deadline_passed_up_front_still_gathers_one_repo() {
    let (gathered, w) = walk(4, 3, 0);
    assert_eq!(gathered, vec![3]);
    assert_eq!(w.next_start(), 0);
    assert!(w.stats(Instant::now()).truncated);
}

#[test]
fn an_empty_walk_is_harmless() {
    let (gathered, w) = walk(0, 9, 0);
    assert!(gathered.is_empty());
    assert_eq!(w.next_start(), 0);
}

#[test]
fn deadline_passed_reads_the_instant() {
    let now = Instant::now();
    assert!(!deadline_passed(None, now));
    assert!(!deadline_passed(Some(now + Duration::from_secs(1)), now));
    assert!(deadline_passed(Some(now), now));
}

#[test]
fn the_deadline_config_defaults_and_overrides() {
    assert_eq!(tick_deadline_from(&serde_json::json!({})), DEFAULT_TICK_DEADLINE);
    let set = serde_json::json!({"autonomous": {"workFinder": {"tickDeadlineSecs": 30}}});
    assert_eq!(tick_deadline_from(&set), Duration::from_secs(30));
    let zero = serde_json::json!({"autonomous": {"workFinder": {"tickDeadlineSecs": 0}}});
    assert_eq!(tick_deadline_from(&zero), DEFAULT_TICK_DEADLINE);
}

/// #12074: the first repo admitted is the guaranteed one, whatever the
/// deadline; later ones are not.
#[test]
fn the_first_admitted_repo_is_the_guaranteed_one() {
    let mut w = GatherWalk::new(3, 0, Instant::now());
    assert!(w.admit(0, true));
    assert!(w.is_guaranteed());
    assert!(!w.admit(1, true));
    let mut w = GatherWalk::new(3, 0, Instant::now());
    assert!(w.admit(0, false) && w.is_guaranteed());
    assert!(w.admit(1, false) && !w.is_guaranteed());
}

/// #12074: a repo starved for N ticks in a row escalates to the writer for
/// one tick, then counts again; a healthy tick resets it.
#[test]
fn a_starved_repo_escalates_to_the_writer_once_every_n_ticks() {
    let root = std::path::Path::new("/w/one");
    let mut s = Starvation::default();
    let mut escalated_at = Vec::new();
    for tick in 0..8 {
        let escalate = s.escalate(root);
        if escalate {
            escalated_at.push(tick);
        }
        s.record(root, true, escalate);
    }
    let n = usize::try_from(STARVED_TICKS_BEFORE_WRITER).unwrap();
    assert_eq!(escalated_at, vec![n, 2 * n + 1], "bounded: every N+1 ticks at worst");
    s.record(root, true, false);
    s.record(root, false, false);
    assert!(!s.escalate(root), "a healthy gather resets the count");
}

// ---- #12256: the concurrent gather --------------------------------------

#[test]
fn run_ordered_returns_job_order_whatever_finishes_first() {
    // Earlier jobs sleep longer, so they finish last on a pool of workers.
    let jobs: Vec<u64> = (0..16).collect();
    for workers in [1, 3, 8, 32] {
        let out = run_ordered(
            jobs.clone(),
            workers,
            || (),
            |pos, j| {
                std::thread::sleep(Duration::from_millis(16 - j));
                Some((pos, j * 10))
            },
        );
        let got: Vec<(usize, (usize, u64))> = out;
        let want: Vec<(usize, (usize, u64))> = (0..16)
            .map(|i| (i, (i, u64::try_from(i).unwrap() * 10)))
            .collect();
        assert_eq!(got, want, "workers={workers}");
    }
}

#[test]
fn run_ordered_runs_concurrently_and_bounds_workers() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let live = AtomicUsize::new(0);
    let peak = AtomicUsize::new(0);
    let started = Instant::now();
    let out = run_ordered(
        (0..12).collect::<Vec<u32>>(),
        4,
        || (),
        |_, j| {
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(40));
            live.fetch_sub(1, Ordering::SeqCst);
            Some(j)
        },
    );
    assert_eq!(out.len(), 12);
    let peak = peak.load(Ordering::SeqCst);
    assert!(peak <= 4, "peak {peak} exceeds the bound");
    assert!(peak > 1, "the jobs never overlapped");
    // 12 jobs × 40 ms serially is 480 ms; four at a time is about 120 ms.
    assert!(started.elapsed() < Duration::from_millis(400));
}

#[test]
fn run_ordered_drops_skipped_jobs_and_runs_setup_per_worker() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let setups = AtomicUsize::new(0);
    let out = run_ordered(
        vec!["a", "b", "c", "d"],
        2,
        || {
            setups.fetch_add(1, Ordering::SeqCst);
        },
        |pos, j| (pos % 2 == 0).then_some(j),
    );
    assert_eq!(out, vec![(0, "a"), (2, "c")]);
    assert_eq!(setups.load(Ordering::SeqCst), 2);
    // No jobs: no worker, no result.
    let none: Vec<(usize, u8)> = run_ordered(Vec::<u8>::new(), 8, || (), |_, j| Some(j));
    assert!(none.is_empty());
}

#[test]
fn run_ordered_takes_jobs_in_order_so_a_deadline_skips_from_the_end() {
    // The walk admits under one lock in take order: with a budget of 3
    // admissions, exactly the first three jobs run, on any worker count.
    for workers in [1, 4] {
        let admitted = std::sync::Mutex::new(0usize);
        let out = run_ordered(
            (0..10).collect::<Vec<usize>>(),
            workers,
            || (),
            |_, j| {
                let mut a = admitted.lock().unwrap();
                if *a >= 3 {
                    return None;
                }
                *a += 1;
                Some(j)
            },
        );
        assert_eq!(out.iter().map(|(_, j)| *j).collect::<Vec<_>>(), vec![0, 1, 2]);
    }
}

#[test]
fn gather_concurrency_reads_a_bounded_positive_integer() {
    use serde_json::json;
    let at = |v| json!({"autonomous": {"workFinder": {"gatherConcurrency": v}}});
    assert_eq!(gather_concurrency_from(&json!({})), DEFAULT_GATHER_CONCURRENCY);
    assert_eq!(gather_concurrency_from(&at(json!(1))), 1);
    assert_eq!(gather_concurrency_from(&at(json!(12))), 12);
    assert_eq!(gather_concurrency_from(&at(json!(1000))), MAX_GATHER_CONCURRENCY);
    for bad in [json!(0), json!(-3), json!("8"), json!(2.5)] {
        assert_eq!(gather_concurrency_from(&at(bad)), DEFAULT_GATHER_CONCURRENCY);
    }
}
