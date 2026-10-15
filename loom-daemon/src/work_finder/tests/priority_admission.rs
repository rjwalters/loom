//! Priority changes dispatch order, never admission (#11508, operator ruling
//! on #11103's rollout): `loom:important` / `loom:very-important` order the
//! queue (level, oldest, number), but no priority overflows a cap or makes an
//! issue eligible. The legacy star (`loom:operator-priority`) carries no
//! weight at all. (The repo-sharding slice these tests also covered is retired
//! from dispatch, #11682.)

use super::*;

fn vi(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into(), "loom:very-important".into()])
}

fn important(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into(), "loom:important".into()])
}

/// The retired operator star (#11510): the daemon no longer knows it, so it
/// is a plain label. The one dispatch-path fixture asserting that.
const RETIRED_STAR: &str = "loom:operator-priority";

fn starred(n: u32) -> WorkItem {
    WorkItem::new(n, vec!["loom:issue".into(), RETIRED_STAR.to_string()])
}

/// A dispatcher whose occupancy is fixed, so a cap can be "full" without any
/// in-flight dedup side effect on the candidates under test.
fn full(occupancy: u32) -> RecordingDispatcher {
    RecordingDispatcher {
        in_flight: (1000..1000 + occupancy).collect(),
        ..RecordingDispatcher::default()
    }
}

#[test]
fn levels_order_the_queue_on_both_tick_paths_and_the_star_is_ignored() {
    // One slot: the very-important issue wins it, whatever its number or age.
    let items = || vec![issue(1), starred(2), important(3), vi(4)];
    let mut dispatcher = full(0);
    tick(&mut FakeSource::once(items()), &mut dispatcher, 1, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![4]);

    let mut multi = vec![(FakeSource::once(items()), full(0))];
    tick_multi(&mut multi, &[1], 1, &[false]);
    assert_eq!(multi[0].1.dispatched, vec![4]);

    // Room for all: level, then number (no createdAt); the star is plain.
    let mut dispatcher = full(0);
    tick(&mut FakeSource::once(items()), &mut dispatcher, 10, false).unwrap();
    assert_eq!(dispatcher.dispatched, vec![4, 3, 1, 2]);
}

#[test]
fn no_priority_overflows_the_global_cap() {
    // Cap 2, occupancy 2: nothing is admitted, on either path, and the slot
    // the star used to take stays unused.
    let items = || vec![vi(1), important(2), starred(3), issue(4)];
    let mut multi = vec![(FakeSource::once(items()), full(2))];
    let report = tick_multi(&mut multi, &[1], 2, &[false]);
    assert_eq!((report.dispatched, report.dispatched_overflow), (0, 0));
    assert_eq!(report.deferred_capacity, 4);
    assert!(multi[0].1.dispatched_overflow.is_empty());
    assert_eq!(report.overflow_free, Some(true));

    let mut dispatcher = full(2);
    let report = tick(&mut FakeSource::once(items()), &mut dispatcher, 2, false).unwrap();
    assert_eq!((report.dispatched, report.deferred_capacity), (0, 4));
    assert!(dispatcher.dispatched_overflow.is_empty());

    // Headroom to spare above the configured cap changes nothing either.
    let terms = CapTerms {
        configured: 2,
        headroom: 5,
    };
    let mut multi = vec![(FakeSource::once(items()), full(2))];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[1],
        terms,
        &[false],
        None,
        usize::MAX,
        false,
        None,
        &[],
    );
    assert_eq!((report.dispatched, report.deferred_capacity), (0, 4));
}

#[test]
fn a_very_important_candidate_is_deferred_by_the_per_repo_cap() {
    // Global room (1 of 4) but this repo is at its per-repo cap of 1.
    let mut multi = vec![(FakeSource::once(vec![vi(1), issue(2)]), full(1))];
    let report = tick_multi_with_repo_cap(
        &mut multi,
        &[1],
        4.into(),
        &[false],
        None,
        usize::MAX,
        false,
        Some(1),
        &[],
    );
    assert_eq!((report.dispatched, report.dispatched_overflow), (0, 0));
    assert_eq!(report.deferred_repo_cap, 2);
}

#[test]
fn the_published_row_keys_are_main_red_fix_level_created_at_number() {
    let mut multi = vec![(FakeSource::once(vec![issue(1), vi(2)]), full(0))];
    let report = tick_multi(&mut multi, &[1], 10, &[false]);
    let rows = ready_queue::finish(&report.queue, &[]);
    assert_eq!(rows[0].issue, 2, "rank 1 is the very-important issue");
    assert_eq!(rows[0].level, crate::priority_pick::Level::VeryImportant, "#11510");
    assert!(!rows[0].main_red_fix, "no verified red-main fix here");
    assert_eq!(
        ready_queue::ordering_names(),
        vec![
            "main_red_fix".to_string(),
            "level".into(),
            "created_at".into(),
            "number".into()
        ]
    );
}

/// #12090: the per-tick admission (ramp) cap is re-read every tick, like
/// `maxConcurrent`, so a fleet-store edit between ticks changes how many new
/// sweeps the NEXT tick admits — no daemon restart.
mod live_admission_cap {
    use super::*;

    fn write_admissions(root: &std::path::Path, n: usize) {
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        let body = format!(
            r#"{{"autonomous":{{"workFinder":{{"maxConcurrent":10,"maxAdmissionsPerTick":{n}}}}}}}"#
        );
        std::fs::write(root.join(".loom/config.json"), body).unwrap();
    }

    fn admitted_this_tick(reloader: &mut ConfiguredMaxReloader) -> usize {
        let cap = reloader.refresh();
        let admissions = reloader.max_admissions_per_tick();
        let mut source = FakeSource::once((1..=8).map(issue).collect());
        let mut disp = RecordingDispatcher::default();
        let report =
            tick_with_admission_cap(&mut source, &mut disp, cap, false, admissions).unwrap();
        report.dispatched
    }

    #[test]
    #[serial]
    fn a_config_edit_between_ticks_changes_the_next_ticks_admissions() {
        std::env::remove_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV);
        std::env::remove_var(WORK_FINDER_MAX_CONCURRENT_ENV);
        let dir = tempfile::tempdir().unwrap();
        write_admissions(dir.path(), 2);
        let initial = ConfiguredMax::resolve(&read_work_finder_config(dir.path()));
        assert_eq!(initial.max_admissions_per_tick, (2, ConfigSource::Config));
        let mut reloader = ConfiguredMaxReloader::new(dir.path().to_path_buf(), initial);
        assert_eq!(admitted_this_tick(&mut reloader), 2);

        write_admissions(dir.path(), 5);
        assert_eq!(admitted_this_tick(&mut reloader), 5, "raised on the next tick");

        write_admissions(dir.path(), 1);
        assert_eq!(admitted_this_tick(&mut reloader), 1, "and lowered");

        // An env override still wins over a config edit (read at start only).
        std::env::set_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV, "3");
        write_admissions(dir.path(), 6);
        assert_eq!(admitted_this_tick(&mut reloader), 3);
        std::env::remove_var(WORK_FINDER_MAX_ADMISSIONS_PER_TICK_ENV);
    }
}
