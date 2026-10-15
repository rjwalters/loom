//! The forge gathers run on their own task (0.19.1163 regression, after
//! #12100): a gather that outlasts `snapshot_interval` must never delay the
//! host samples (host.health, tokens.snapshot, host.export).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use super::{forge_gather_loop, supervise_forge_gathers, AbortOnDrop};

const PERIOD: Duration = Duration::from_secs(300);

type Starts = Arc<Mutex<Vec<Instant>>>;

/// Spawn the real schedule with a gather that records its start instant and
/// then takes `hold` to finish.
fn spawn_gathers(first: Instant, hold: Duration, starts: &Starts) -> AbortOnDrop {
    let state = Arc::clone(starts);
    AbortOnDrop(tokio::spawn(forge_gather_loop(first, PERIOD, state, move |s| {
        let s = Arc::clone(s);
        Box::pin(async move {
            s.lock().unwrap().push(Instant::now());
            tokio::time::sleep(hold).await;
        })
    })))
}

#[tokio::test(start_paused = true)]
async fn a_hung_forge_gather_does_not_delay_host_samples() {
    let boot = Instant::now();
    let starts: Starts = Arc::default();
    // A gather that blocks for ten sample intervals — the shed /
    // breaker-bound shape that silenced host.export on the workers.
    let _task = spawn_gathers(boot, PERIOD * 10, &starts);

    // The collector's host-sample timer, built exactly as `run_collector`
    // builds `snapshot_timer`.
    let mut snapshot_timer = tokio::time::interval(PERIOD);
    snapshot_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    for n in 0..5u32 {
        snapshot_timer.tick().await;
        assert_eq!(
            Instant::now() - boot,
            PERIOD * n,
            "host sample {n} must fire on its own cadence while a gather hangs"
        );
    }
    assert_eq!(
        starts.lock().unwrap().len(),
        1,
        "one pass at a time: a hung gather is not stacked"
    );
}

#[tokio::test(start_paused = true)]
async fn first_gather_waits_for_the_jitter_gate_then_keeps_the_snapshot_cadence() {
    let boot = Instant::now();
    let jitter = Duration::from_secs(97);
    let starts: Starts = Arc::default();
    let _task = spawn_gathers(boot + jitter, Duration::from_secs(1), &starts);

    tokio::time::sleep(jitter + PERIOD * 2 + Duration::from_secs(10)).await;
    let offsets: Vec<Duration> = starts.lock().unwrap().iter().map(|t| *t - boot).collect();
    assert_eq!(offsets, vec![jitter, jitter + PERIOD, jitter + PERIOD * 2]);
}

#[tokio::test(start_paused = true)]
async fn a_long_gather_delays_the_next_pass_instead_of_bursting() {
    let boot = Instant::now();
    let starts: Starts = Arc::default();
    let hold = PERIOD + Duration::from_secs(60);
    let _task = spawn_gathers(boot, hold, &starts);

    tokio::time::sleep(hold * 2 + Duration::from_secs(10)).await;
    let offsets: Vec<Duration> = starts.lock().unwrap().iter().map(|t| *t - boot).collect();
    assert_eq!(offsets, vec![Duration::ZERO, hold, hold * 2]);
}

#[tokio::test(start_paused = true)]
async fn dropping_the_guard_stops_the_gathers() {
    let boot = Instant::now();
    let starts: Starts = Arc::default();
    let task = spawn_gathers(boot, Duration::from_secs(1), &starts);
    tokio::time::sleep(Duration::from_secs(10)).await;
    drop(task);
    tokio::time::sleep(PERIOD * 3).await;
    assert_eq!(
        starts.lock().unwrap().len(),
        1,
        "an aborted gather task must not keep gathering"
    );
}

/// The structural half of the regression: the collector's event / sample
/// loop must not await a forge gather itself. Awaited inline in the
/// `biased` select, a gather longer than `snapshot_interval` starved the
/// snapshot arm outright.
#[test]
fn run_collector_never_awaits_a_forge_gather_inline() {
    let src = include_str!("../collector.rs");
    let start = src
        .find("async fn run_collector(")
        .expect("run_collector exists");
    let body = &src[start..];
    let end = body.find("\n}\n").expect("run_collector ends");
    let body = &body[..end];
    let calls = body.matches("forge_gathers(").count();
    let spawns = body.matches("spawn_forge_gathers(").count();
    assert_eq!(
        calls, spawns,
        "run_collector must spawn the forge gathers (spawn_forge_gathers), never await them in its select"
    );
    assert!(body.contains("spawn_forge_gathers("), "run_collector spawns the forge gathers");
}

#[tokio::test(start_paused = true)]
async fn a_panicking_gather_is_restarted_after_one_interval() {
    let boot = Instant::now();
    let starts: Starts = Arc::default();
    let shared = Arc::clone(&starts);
    let _task = AbortOnDrop(tokio::spawn(supervise_forge_gathers(
        boot,
        PERIOD,
        move || Arc::clone(&shared),
        |s: &mut Starts| {
            let s = Arc::clone(s);
            Box::pin(async move {
                let pass = {
                    let mut v = s.lock().unwrap();
                    v.push(Instant::now());
                    v.len()
                };
                assert!(pass != 1, "pass 1 panics");
            })
        },
    )));

    tokio::time::sleep(PERIOD * 3 + Duration::from_secs(10)).await;
    let offsets: Vec<Duration> = starts.lock().unwrap().iter().map(|t| *t - boot).collect();
    // Pass 1 panics at 0; the supervisor waits one interval, then the
    // restarted loop gathers at P, 2P, 3P.
    assert_eq!(offsets, vec![Duration::ZERO, PERIOD, PERIOD * 2, PERIOD * 3]);
}
