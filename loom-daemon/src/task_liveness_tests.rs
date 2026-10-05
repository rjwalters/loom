use super::*;

const MIN: Duration = Duration::from_secs(60);

fn entry(registry: &Registry, task: &str, now: Instant) -> TaskLivenessEntry {
    registry
        .snapshot_at(now)
        .into_iter()
        .find(|e| e.task == task)
        .unwrap()
}

#[test]
fn default_window_is_two_intervals_plus_grace() {
    assert_eq!(default_stale_after(5 * MIN), 10 * MIN + GRACE);
}

#[test]
fn a_registered_task_is_alive_until_its_window_passes_without_a_beat() {
    let registry = Registry::default();
    let t0 = Instant::now();
    registry.register("auto_update", 15 * MIN, 31 * MIN, t0);
    assert!(entry(&registry, "auto_update", t0).alive);
    assert!(entry(&registry, "auto_update", t0 + 31 * MIN).alive);
    let late = entry(&registry, "auto_update", t0 + 31 * MIN + Duration::from_secs(1));
    assert!(!late.alive, "never beat inside its window: dead");
    assert_eq!(late.last_beat, None);
    assert_eq!(late.stale_after_secs, 31 * 60);
}

/// Issue #10414's acceptance test: a task that stops beating (it exited,
/// panicked past its handler, or wedged inside an iteration) flips to dead
/// within one staleness window of its last beat, and no later.
#[test]
fn a_task_that_stops_beating_is_dead_within_one_staleness_window() {
    let registry = Registry::default();
    let t0 = Instant::now();
    let interval = 5 * MIN;
    let wall = Utc::now();
    for tick in 0..4 {
        registry.beat("eta_pass", interval, t0 + interval * tick, wall);
    }
    let last_beat = t0 + interval * 3;
    // Killed right after its last beat: alive for exactly one window.
    let window = default_stale_after(interval);
    assert!(entry(&registry, "eta_pass", last_beat + window).alive);
    let dead = entry(&registry, "eta_pass", last_beat + window + Duration::from_secs(1));
    assert!(!dead.alive);
    assert_eq!(dead.silent_secs, (window + Duration::from_secs(1)).as_secs());
    assert_eq!(dead.last_beat, Some(wall));
}

#[test]
fn mark_dead_flips_immediately_and_a_new_beat_revives() {
    let registry = Registry::default();
    let t0 = Instant::now();
    registry.beat("auto_update", 15 * MIN, t0, Utc::now());
    registry.mark_dead("auto_update", "tick task panicked");
    let dead = entry(&registry, "auto_update", t0);
    assert!(!dead.alive);
    assert_eq!(dead.dead_reason.as_deref(), Some("tick task panicked"));
    registry.beat("auto_update", 15 * MIN, t0 + MIN, Utc::now());
    assert!(entry(&registry, "auto_update", t0 + MIN).alive);
}

#[test]
fn beat_if_registered_beats_a_registered_task_and_ignores_an_unknown_one() {
    let registry = Registry::default();
    let t0 = Instant::now();
    registry.register("eta_pass", 5 * MIN, 11 * MIN, t0);
    registry.beat_if_registered("eta_pass", t0 + 10 * MIN, Utc::now());
    registry.beat_if_registered("eta_disabled", t0, Utc::now());
    let e = entry(&registry, "eta_pass", t0 + 20 * MIN);
    assert!(e.alive, "the beat at +10m restarted the 11m window");
    assert_eq!(e.stale_after_secs, 11 * 60, "the registered window is kept");
    assert_eq!(registry.snapshot_at(t0).len(), 1, "no slot for an unregistered task");
}

#[test]
fn mark_dead_on_an_unregistered_task_is_a_no_op() {
    let registry = Registry::default();
    registry.mark_dead("never_registered", "gone");
    assert!(registry.snapshot_at(Instant::now()).is_empty());
}

#[test]
fn snapshot_is_sorted_by_task_name() {
    let registry = Registry::default();
    let now = Instant::now();
    for task in ["role_runner.judge", "auto_update", "eta_pass"] {
        registry.register(task, MIN, 3 * MIN, now);
    }
    let names: Vec<String> = registry
        .snapshot_at(now)
        .into_iter()
        .map(|e| e.task)
        .collect();
    assert_eq!(names, ["auto_update", "eta_pass", "role_runner.judge"]);
}

#[test]
fn role_beats_are_namespaced_under_the_role_runner_prefix() {
    beat_role("liveness-test-role", MIN);
    let found = snapshot()
        .into_iter()
        .find(|e| e.task == "role_runner.liveness-test-role")
        .unwrap();
    assert!(found.alive);
    assert_eq!(found.interval_secs, 60);
}

/// End to end with a real tokio task: the loop beats, then panics; the global
/// registry reads it dead once its window passes without a beat.
#[tokio::test]
async fn a_spawned_loop_that_panics_reads_dead_after_its_window() {
    let task = "liveness-test-panicking-loop";
    let interval = Duration::from_millis(20);
    register(task, interval, Duration::from_millis(100));
    let handle = tokio::spawn(async move {
        for _ in 0..3 {
            beat(task, interval);
            tokio::time::sleep(interval).await;
        }
        panic!("simulated loop death");
    });
    assert!(handle.await.is_err(), "the loop panicked");
    let alive_now = |name: &str| {
        snapshot()
            .into_iter()
            .find(|e| e.task == name)
            .unwrap()
            .alive
    };
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert!(!alive_now(task), "silent past its window: task_alive = 0");
}
