//! The checkout step's network decision and its hand-off from the workspace
//! pass (#10869): a host that is not in H0 makes no network call from it, a
//! repo the resync is backing off is not asked, the step never waits for its
//! memory, and `spawn_pass`'s hand-off runs it after the pass on the same
//! supervised thread.
//!
//! Being offline does not by itself stop the local fast-forward: a host that
//! is otherwise in H0 still moves a clean checkout to what its clone holds.
//! What does stop the write (a pause, a roll, a pending resume) is in
//! `checkout_ff_holds.rs`. Every step here runs with no write hold.
//!
//! Real git in temp dirs only, as in the parent module. Every step is handed
//! its roots: none of these tests reads the workspace registry.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::ThreadId;
use std::time::Instant;

use super::*;
use crate::fleet_sync::workspace_resync::{
    spawn_with, Ended, HostGateInputs, NotCurrent, WorkspacePass,
};

/// The workspace pass the step follows, as far as the step reads it.
fn resync(online: bool, backing_off: Vec<PathBuf>) -> WorkspacePass {
    WorkspacePass {
        online,
        backing_off,
        ..WorkspacePass::default()
    }
}

/// No write hold: the host is not paused, not rolling, and has no resume
/// pending.
fn unheld() -> Option<&'static str> {
    None
}

/// A timer step over `roots`, with its own fresh memory, on a host with no
/// write hold.
fn step(roots: &[PathBuf], resync: &WorkspacePass) -> CheckoutPass {
    let memory = Mutex::new(Memory::default());
    let roots = roots.to_vec();
    let began = Instant::now();
    match timer_step(&memory, &move || roots.clone(), true, &unheld, resync, began, None) {
        Stepped::Ran(found) => found,
        Stepped::Busy => panic!("nothing holds this memory"),
    }
}

/// A host checkout that is behind a remote it can no longer reach: any
/// network call is counted and fails, and the clone's own ref is one commit
/// behind the remote's.
fn behind_and_unplugged() -> (Fixture, String) {
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    let known = git(&fx.host, &["rev-parse", "refs/remotes/origin/main"]);
    fx.push("src/b.rs", "fn b() {}\n");
    unplug(&fx);
    (fx, known)
}

#[test]
fn a_step_after_an_offline_workspace_pass_asks_no_remote() {
    // Whatever kept the resync off the network (not in H0, an outage hold,
    // autoApply off, paused), its pass says so in `online`, and the step
    // takes that as its own.
    let (fx, known) = behind_and_unplugged();
    let found = step(std::slice::from_ref(&fx.host), &resync(false, Vec::new()));
    assert_eq!((found.probes, found.fetches), (0, 0));
    // This host is offline and nothing more: the network is down, and it is
    // not paused, not rolling and has no resume pending (`step` passes no
    // write hold). So the local merge against the clone's own ref still
    // happens. A paused host would not write: see `checkout_ff_holds.rs`.
    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert_eq!(fx.head(), known, "as far as this clone knows, and no further");

    // The same root after an online pass is asked.
    let (fx, _) = behind_and_unplugged();
    let found = step(std::slice::from_ref(&fx.host), &resync(true, Vec::new()));
    assert_eq!(found.probes, 1, "{found:?}");
}

#[test]
fn a_root_the_resync_is_backing_off_is_not_asked_by_the_step() {
    let (fx, _) = behind_and_unplugged();
    let other = fx.clone_as("other");
    let roots = [fx.host.clone(), other.clone()];
    let mut memory = Memory::default();
    let knobs = Knobs {
        backing_off: vec![fx.host.clone()],
        ..Knobs::default()
    };
    for tick in 0..3 {
        let found = pass_over(&roots, &knobs, &mut memory);
        assert_eq!(found.probes, 1, "only the other root, tick {tick}: {found:?}");
    }
    // And through the live step, from the resync pass's own list.
    let found = step(&roots, &resync(true, vec![fx.host.clone()]));
    assert_eq!(found.probes, 1, "{found:?}");
}

#[test]
fn a_step_whose_memory_is_held_skips_the_tick_instead_of_waiting() {
    // An earlier step abandoned inside a git child still holds the memory.
    let memory = Arc::new(Mutex::new(Memory::default()));
    let held = memory.lock().unwrap();
    let (tx, rx) = mpsc::channel();
    let shared = memory.clone();
    std::thread::spawn(move || {
        let never = || -> Vec<PathBuf> { panic!("a busy step reads no roots") };
        let pass = resync(true, Vec::new());
        let stepped = timer_step(&shared, &never, true, &unheld, &pass, Instant::now(), None);
        let _ = tx.send(stepped);
    });
    let stepped = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the step must return at once, not wait for the memory");
    assert!(matches!(stepped, Stepped::Busy), "{stepped:?}");
    drop(held);
    // Once the old step lets go, the next tick runs.
    let pass = resync(false, Vec::new());
    let none = Vec::<PathBuf>::new;
    let free = timer_step(&memory, &none, true, &unheld, &pass, Instant::now(), None);
    assert!(matches!(free, Stepped::Ran(_)), "{free:?}");
}

/// A host in H0, as the startup pass can know it.
fn h0_at_boot() -> HostGateInputs {
    HostGateInputs {
        release_build: true,
        release_verified: true,
        verified: true,
        ..HostGateInputs::default()
    }
}

#[test]
fn the_startup_step_is_offline_wherever_the_host_gate_would_be() {
    assert!(online_at_boot(true, &h0_at_boot()));
    type Set = fn(&mut HostGateInputs);
    let offline: [(&str, Set); 9] = [
        ("non-release build", |i| i.release_build = false),
        ("release not verified", |i| i.release_verified = false),
        ("roll pending", |i| i.roll_pending = true),
        ("resume pending", |i| i.resume_pending = true),
        ("staged binary", |i| i.staged = true),
        ("below the floor", |i| i.below_floor = true),
        ("stalled self-update", |i| i.stalled = true),
        ("paused", |i| i.draining = true),
        ("autoApply off", |_| {}),
    ];
    for (case, set) in offline {
        let mut gate = h0_at_boot();
        set(&mut gate);
        let auto_apply = case != "autoApply off";
        assert!(!online_at_boot(auto_apply, &gate), "{case}");
    }
    // Only `Unverified` is exempt, and only because the startup pass is the
    // one that verifies: `at_boot` reads every other fact.
    let at_boot = HostGateInputs::at_boot(false, None);
    assert!(at_boot.verified, "a fresh daemon is not held for being unverified");
    let mut unverified = h0_at_boot();
    unverified.verified = false;
    assert_eq!(host_gate_reason(&unverified), Some(NotCurrent::Unverified));
    assert!(HostGateInputs::at_boot(true, None).draining, "paused at boot");
    assert!(!HostGateInputs::at_boot(false, None).roll_pending, "no roll retained yet");
    assert!(HostGateInputs::at_boot(false, Some("9999.0.0")).below_floor);
    assert!(!HostGateInputs::at_boot(false, Some("0.0.1")).below_floor);
}

fn host_gate_reason(gate: &HostGateInputs) -> Option<NotCurrent> {
    crate::fleet_sync::workspace_resync::host_gate(gate).err()
}

// ----------------------------------------------------------------------------
// The hand-off from the workspace pass (`spawn_pass` -> `spawn_with`)
// ----------------------------------------------------------------------------

/// What one supervised pass and its step saw.
#[derive(Debug, Default)]
struct Seen {
    pass: Option<(ThreadId, Instant, Instant)>,
    then: Option<(ThreadId, Instant, Instant, bool)>,
}

#[tokio::test]
async fn the_step_runs_after_the_pass_on_its_thread_and_takes_its_network_decision() {
    static SLOT: AtomicBool = AtomicBool::new(false);
    let (fx, _) = behind_and_unplugged();
    let roots = vec![fx.host.clone()];
    let seen = Arc::new(Mutex::new(Seen::default()));
    let (by_pass, by_then) = (seen.clone(), seen.clone());
    let pass = move || {
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        let found = WorkspacePass {
            running: "found".into(),
            ..resync(false, Vec::new())
        };
        by_pass.lock().unwrap().pass = Some((std::thread::current().id(), start, Instant::now()));
        found
    };
    let then = move |began: Instant, resync: &WorkspacePass| {
        let at = Instant::now();
        by_then.lock().unwrap().then =
            Some((std::thread::current().id(), began, at, resync.online));
        let memory = Mutex::new(Memory::default());
        timer_step(&memory, &move || roots.clone(), true, &unheld, resync, began, None)
    };
    let (tx, rx) = mpsc::channel();
    let task = spawn_with(&SLOT, Duration::from_secs(120), pass, then, move |ended| {
        let _ = tx.send(ended);
    })
    .expect("the slot is free");
    task.await.unwrap();

    let Ok(Ended::Done((found, Some(Stepped::Ran(checked))))) = rx.try_recv() else {
        panic!("the pass and its step both ended");
    };
    assert_eq!(found.running, "found", "the pass's own result is handed on");
    assert_eq!((checked.probes, checked.fetches), (0, 0), "offline pass, offline step");
    let seen = seen.lock().unwrap();
    let (pass_thread, pass_start, pass_end) = seen.pass.unwrap();
    let (then_thread, began, then_start, online) = seen.then.unwrap();
    assert_eq!(pass_thread, then_thread, "on the pass's own supervised thread");
    assert!(began <= pass_start, "handed the instant the pass began");
    assert!(then_start >= pass_end, "after the pass, never during it");
    assert!(!online);
}

#[tokio::test]
async fn a_step_that_panics_does_not_lose_the_pass() {
    static SLOT: AtomicBool = AtomicBool::new(false);
    let pass = || WorkspacePass {
        running: "kept".into(),
        ..WorkspacePass::default()
    };
    let then = |_: Instant, _: &WorkspacePass| -> Stepped { panic!("a defect in the step") };
    let (tx, rx) = mpsc::channel();
    let task = spawn_with(&SLOT, Duration::from_secs(120), pass, then, move |ended| {
        let _ = tx.send(ended);
    })
    .expect("the slot is free");
    task.await.unwrap();
    let Ok(Ended::Done((found, after))) = rx.try_recv() else {
        panic!("the pass ended normally");
    };
    assert_eq!(found.running, "kept", "the resync result reaches `finish`");
    assert!(after.is_none(), "only the step's own result is lost");
    assert!(!SLOT.load(Ordering::Acquire), "the slot is free again");
}

// Multi-threaded: this test blocks its own thread while the first pass runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tick_refused_by_the_single_flight_drops_its_step() {
    static SLOT: AtomicBool = AtomicBool::new(false);
    let (release, wait) = mpsc::channel::<()>();
    let (started, has_started) = mpsc::channel::<()>();
    let first = spawn_with(
        &SLOT,
        Duration::from_secs(120),
        move || {
            let _ = started.send(());
            let _ = wait.recv_timeout(Duration::from_secs(60));
            WorkspacePass::default()
        },
        |_: Instant, _: &WorkspacePass| (),
        |_| {},
    )
    .expect("the slot is free");
    has_started
        .recv_timeout(Duration::from_secs(60))
        .expect("the first pass is running");

    let ran = Arc::new(AtomicBool::new(false));
    let (pass_ran, then_ran) = (ran.clone(), ran.clone());
    let refused = spawn_with(
        &SLOT,
        Duration::from_secs(120),
        move || {
            pass_ran.store(true, Ordering::Release);
            WorkspacePass::default()
        },
        move |_: Instant, _: &WorkspacePass| then_ran.store(true, Ordering::Release),
        |_| {},
    );
    assert!(refused.is_none(), "single flight: the tick starts nothing");

    release.send(()).unwrap();
    first.await.unwrap();
    assert!(!ran.load(Ordering::Acquire), "the refused tick's pass and step never ran");
    // The next tick starts normally.
    let next = spawn_with(
        &SLOT,
        Duration::from_secs(120),
        WorkspacePass::default,
        |_: Instant, _: &WorkspacePass| (),
        |_| {},
    );
    next.expect("free again").await.unwrap();
}
