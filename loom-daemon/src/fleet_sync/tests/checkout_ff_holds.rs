//! What stops the checkout step short of a failure (#10869): a fetch that is
//! due below the free-space floor is not run (#10995), and a host that is
//! paused, rolling or has a resume pending writes nothing (#11016).
//!
//! Real git in temp dirs only, as in the parent module. Every step is handed
//! its roots: none of these tests reads the workspace registry.

use std::sync::Mutex;
use std::time::Instant;

use super::super::host::write_hold_now;
use super::*;
use crate::fetch_headroom::test_override::with_free_gb;
use crate::fleet_state::{DrainFacts, Enforcer};
use crate::fleet_sync::workspace_resync::{host_gate, HostGateInputs, NotCurrent, WorkspacePass};

fn tracking(fx: &Fixture) -> String {
    git(&fx.host, &["rev-parse", "refs/remotes/origin/main"])
}

/// A host in H0, as the startup pass can know it.
fn h0() -> HostGateInputs {
    HostGateInputs {
        release_build: true,
        release_verified: true,
        verified: true,
        ..HostGateInputs::default()
    }
}

/// The workspace pass the step follows, as far as the step reads it.
fn resync(online: bool) -> WorkspacePass {
    WorkspacePass {
        online,
        ..WorkspacePass::default()
    }
}

/// A timer step over the fixture's host checkout, with `fleet.autoApply` on
/// and the host's facts as `gate` gives them, in `memory`.
fn step_in(
    memory: &Mutex<Memory>,
    fx: &Fixture,
    gate: &HostGateInputs,
    resync: &WorkspacePass,
) -> CheckoutPass {
    let roots = vec![fx.host.clone()];
    let held = || gate.write_hold().map(NotCurrent::as_str);
    let began = Instant::now();
    match timer_step(memory, &move || roots.clone(), true, &held, resync, began, None) {
        Stepped::Ran(found) => found,
        Stepped::Busy => panic!("nothing holds this memory"),
    }
}

// ----------------------------------------------------------------------------
// The free-space floor (#10995)
// ----------------------------------------------------------------------------

#[test]
fn below_the_disk_floor_no_fetch_is_run_and_no_failure_is_recorded() {
    // A root the resync does not cover, and every root at startup: the step
    // asks the remote itself, hears a head the clone lacks, and would fetch.
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    let before = snapshot(&fx.host);
    let known = tracking(&fx);
    let mut memory = Memory::default();
    let roots = std::slice::from_ref(&fx.host);

    // More ticks than `TRANSIENT_AFTER` and `UNREACHABLE_STOP`: the skip is
    // never a failure and never stops the pass asking.
    for tick in 0..4 {
        let found = with_free_gb(1, || pass_over(roots, &Knobs::default(), &mut memory));
        assert_eq!(found.probes, 1, "the remote is still asked, tick {tick}: {found:?}");
        assert_eq!(found.fetches, 0, "no fetch is spawned, tick {tick}");
        assert_eq!(found.low_disk, 1, "tick {tick}");
        assert_eq!(tracking(&fx), known, "origin/main did not move, tick {tick}");
        let report = only(&found);
        assert_eq!(report.state, CheckoutState::LowDisk, "{report:?}");
        assert!(!report.state.is_transient(), "not one of the failure states");
        let detail = report.detail.as_deref().unwrap_or_default();
        assert!(detail.contains("skipped git fetch") && detail.contains("below"), "{detail}");
        let entered = usize::from(tick == 0);
        assert_eq!(found.transitions.len(), entered, "reported once, on entry: {found:?}");
        // `loom-daemon status` shows it as a low-disk skip.
        let shown = lines(&found.checkouts);
        assert!(shown.len() == 1 && shown[0].contains(": low-disk ("), "{shown:?}");
    }
    assert_eq!(snapshot(&fx.host), before, "and the checkout was not touched");
}

#[test]
fn a_low_disk_skip_does_not_count_toward_the_three_in_a_row_stop() {
    // Four roots whose remotes all moved. Were a skipped fetch a miss, the
    // fourth would not be asked.
    let fx = Fixture::new();
    let roots: Vec<PathBuf> = ["one", "two", "three", "four"]
        .iter()
        .map(|name| fx.clone_as(name))
        .collect();
    fx.push("src/a.rs", "fn a() {}\n");
    let found = with_free_gb(1, || pass_over(&roots, &Knobs::default(), &mut Memory::default()));
    assert_eq!((found.probes, found.fetches, found.low_disk), (4, 0, 4), "{found:?}");
    assert!(found
        .checkouts
        .iter()
        .all(|c| c.state == CheckoutState::LowDisk));
}

#[test]
fn a_head_the_resync_learned_is_not_fetched_below_the_disk_floor() {
    // A covered root: the resync named the head, so the step asks nobody and
    // goes straight to the fetch. That fetch asks the floor too.
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    let known = tracking(&fx);
    let knobs = Knobs {
        confirmed: Some(tip.clone()),
        ..Knobs::default()
    };
    let found = with_free_gb(1, || pass(&fx, &knobs));
    assert_eq!((found.probes, found.fetches, found.low_disk), (0, 0, 1), "{found:?}");
    assert_eq!(only(&found).state, CheckoutState::LowDisk);
    assert_eq!(tracking(&fx), known);
}

#[test]
fn below_the_disk_floor_a_checkout_still_reaches_what_its_clone_holds() {
    // The clone already holds one commit the checkout lacks; the remote has a
    // second one. The first needs no fetch, so it is reached. The second is
    // not fetched.
    let fx = Fixture::new();
    let first = fx.push("src/a.rs", "fn a() {}\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    fx.push("src/b.rs", "fn b() {}\n");
    let mut memory = Memory::default();
    let roots = std::slice::from_ref(&fx.host);

    let found = with_free_gb(1, || pass_over(roots, &Knobs::default(), &mut memory));
    assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{found:?}");
    assert_eq!((found.fetches, found.low_disk), (0, 1));
    assert_eq!(fx.head(), first);

    let found = with_free_gb(1, || pass_over(roots, &Knobs::default(), &mut memory));
    assert_eq!(only(&found).state, CheckoutState::LowDisk, "{found:?}");
    assert_eq!(fx.head(), first);
}

#[test]
fn at_or_above_the_disk_floor_the_step_fetches_as_before() {
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    let mut memory = Memory::default();
    let roots = std::slice::from_ref(&fx.host);
    // Below the floor first, then the disk recovers: the very next pass
    // fetches and fast-forwards, and the low-disk state clears.
    let low = with_free_gb(1, || pass_over(roots, &Knobs::default(), &mut memory));
    assert_eq!(only(&low).state, CheckoutState::LowDisk);

    let found = with_free_gb(10_000, || pass_over(roots, &Knobs::default(), &mut memory));
    assert_eq!((found.probes, found.fetches, found.low_disk), (1, 1, 0), "{found:?}");
    assert_eq!(only(&found).state, CheckoutState::FastForwarded);
    assert_eq!(fx.head(), tip);
    assert_eq!(tracking(&fx), tip);

    // An unmeasurable volume never skips a fetch: unknown is not zero.
    let tip = fx.push("src/b.rs", "fn b() {}\n");
    let found = pass_over(roots, &Knobs::default(), &mut memory);
    assert_eq!((found.fetches, found.low_disk), (1, 0), "{found:?}");
    assert_eq!(fx.head(), tip);
}

// ----------------------------------------------------------------------------
// A paused host writes nothing
// ----------------------------------------------------------------------------

/// A clean host checkout one commit behind what its own clone holds: a
/// fast-forward needs no network at all.
fn behind_its_own_clone() -> (Fixture, String) {
    let fx = Fixture::new();
    let tip = fx.push("src/a.rs", "fn a() {}\n");
    git(&fx.host, &["fetch", "--quiet", "origin"]);
    (fx, tip)
}

#[test]
fn only_a_pause_a_roll_or_a_pending_resume_holds_the_write() {
    assert_eq!(h0().write_hold(), None);
    type Set = fn(&mut HostGateInputs);
    let held: [(Set, NotCurrent); 3] = [
        (|i| i.draining = true, NotCurrent::Draining),
        (|i| i.roll_pending = true, NotCurrent::RollPending),
        (|i| i.resume_pending = true, NotCurrent::ResumePending),
    ];
    for (set, why) in held {
        let mut gate = h0();
        set(&mut gate);
        assert_eq!(gate.write_hold(), Some(why));
        // And it holds whatever else is true of the host: the host gate's
        // first reason may be another one, the write hold is still there.
        gate.release_build = false;
        assert_eq!(host_gate(&gate), Err(NotCurrent::NotAReleaseBuild));
        assert_eq!(gate.write_hold(), Some(why));
    }
    // Every other reason a host is not in H0 keeps it off the network and
    // stops it pushing. None of them stops a local fast-forward.
    let not_held: [Set; 6] = [
        |i| i.release_build = false,
        |i| i.release_verified = false,
        |i| i.verified = false,
        |i| i.staged = true,
        |i| i.below_floor = true,
        |i| i.stalled = true,
    ];
    for set in not_held {
        let mut gate = h0();
        set(&mut gate);
        assert!(host_gate(&gate).is_err(), "{gate:?}");
        assert_eq!(gate.write_hold(), None, "{gate:?}");
    }
}

#[test]
fn a_draining_rolling_or_resume_pending_host_does_not_write() {
    type Set = fn(&mut HostGateInputs);
    let cases: [(&str, Set); 3] = [
        ("draining", |i| i.draining = true),
        ("roll pending", |i| i.roll_pending = true),
        ("resume-pending", |i| i.resume_pending = true),
    ];
    for (why, set) in cases {
        let (fx, tip) = behind_its_own_clone();
        let before = snapshot(&fx.host);
        let mut gate = h0();
        set(&mut gate);
        assert!(host_gate(&gate).is_err(), "{why}: the resync pass is offline");
        let memory = Mutex::new(Memory::default());

        // More than one tick: the hold is not a state that times out.
        for _ in 0..2 {
            let found = step_in(&memory, &fx, &gate, &resync(false));
            assert_eq!((found.probes, found.fetches), (0, 0), "{why}: no network");
            let report = only(&found);
            assert_eq!(report.state, CheckoutState::Behind, "{why}: {report:?}");
            assert_eq!(report.behind, Some(1), "{why}: still counted");
            let detail = report.detail.as_deref().unwrap_or_default();
            assert!(detail.contains(why) && detail.contains("nothing written"), "{detail}");
            assert_eq!(snapshot(&fx.host), before, "{why}: byte-for-byte unchanged");
        }

        // Nothing is lost: the first pass after the hold ends fast-forwards.
        let found = step_in(&memory, &fx, &h0(), &resync(false));
        assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{why}: {found:?}");
        assert_eq!(fx.head(), tip, "{why}");
    }
}

#[test]
fn a_pause_holds_the_write_even_on_a_pass_that_was_online() {
    // The pause begins after the workspace pass took its network decision:
    // the write hold is read live, right before the merge, not from the pass.
    let fx = Fixture::new();
    fx.push("src/a.rs", "fn a() {}\n");
    let old = fx.head();
    let mut gate = h0();
    gate.draining = true;
    let found = step_in(&Mutex::new(Memory::default()), &fx, &gate, &resync(true));
    assert_eq!(only(&found).state, CheckoutState::Behind, "{found:?}");
    assert_eq!(fx.head(), old, "HEAD did not move");
    assert_eq!(git(&fx.host, &["status", "--porcelain"]), "");
}

#[test]
fn a_host_in_h0_that_is_merely_offline_still_fast_forwards_locally() {
    // The network is down (an outage hold, a remote that does not answer) or
    // the host is not in H0 for a reason that is not a pause. It asks nobody,
    // and still moves a clean checkout to the commit its clone already has.
    type Set = fn(&mut HostGateInputs);
    let cases: [(&str, Set); 5] = [
        ("H0, in an outage hold", |_| {}),
        ("not a release build", |i| i.release_build = false),
        ("staged binary", |i| i.staged = true),
        ("below the floor", |i| i.below_floor = true),
        ("stalled self-update", |i| i.stalled = true),
    ];
    for (case, set) in cases {
        let (fx, tip) = behind_its_own_clone();
        // Unplugged: any network call would be counted, and would fail.
        let gone = fx.base().join("nowhere.git");
        git(&fx.host, &["remote", "set-url", "origin", gone.to_str().unwrap()]);
        let mut gate = h0();
        set(&mut gate);
        let found = step_in(&Mutex::new(Memory::default()), &fx, &gate, &resync(false));
        assert_eq!((found.probes, found.fetches), (0, 0), "{case}");
        assert_eq!(only(&found).state, CheckoutState::FastForwarded, "{case}: {found:?}");
        assert_eq!(fx.head(), tip, "{case}");
    }
}

// ----------------------------------------------------------------------------
// The startup pass, which runs before H5 is spawned
// ----------------------------------------------------------------------------

#[test]
fn a_resume_pending_at_boot_means_no_fetch_and_no_write() {
    // A daemon a pause roll just restarted: H5 has not verified it. The clone
    // holds one commit the checkout lacks (a write would be purely local) and
    // the remote has one more (a fetch would be due).
    let (fx, _) = behind_its_own_clone();
    fx.push("src/b.rs", "fn b() {}\n");
    let before = snapshot(&fx.host);
    let known = tracking(&fx);
    let roots = std::slice::from_ref(&fx.host);
    let pending = || HostGateInputs {
        resume_pending: true,
        ..h0()
    };
    assert_eq!(host_gate(&pending()), Err(NotCurrent::ResumePending));
    assert!(!online_at_boot(true, &pending()));
    let mut memory = Memory::default();

    let found = boot_step(&mut memory, roots, true, &pending);
    assert_eq!((found.probes, found.fetches), (0, 0), "no ls-remote, no fetch: {found:?}");
    let report = only(&found);
    assert_eq!(report.state, CheckoutState::Behind, "{report:?}");
    let detail = report.detail.as_deref().unwrap_or_default();
    assert!(detail.contains("resume-pending"), "{detail}");
    assert_eq!(tracking(&fx), known, "nothing was fetched");
    assert_eq!(snapshot(&fx.host), before, "and nothing was written");

    // A host that boots paused (a fleet hold) is the same.
    let paused = || HostGateInputs {
        draining: true,
        ..h0()
    };
    let found = boot_step(&mut memory, roots, true, &paused);
    assert_eq!((found.probes, found.fetches), (0, 0));
    assert_eq!(snapshot(&fx.host), before);

    // H5 has finished: the same pass asks, fetches and fast-forwards.
    let found = boot_step(&mut memory, roots, true, &h0);
    assert_eq!((found.probes, found.fetches), (1, 1), "{found:?}");
    assert_eq!(only(&found).state, CheckoutState::FastForwarded);
    assert_eq!(fx.head(), fx.origin_tip());
}

#[test]
fn the_hold_is_read_again_before_each_merge_at_boot() {
    // The facts change between the pass's network decision and its write.
    let (fx, _) = behind_its_own_clone();
    let before = snapshot(&fx.host);
    let reads = Cell::new(0);
    let gate = || {
        reads.set(reads.get() + 1);
        HostGateInputs {
            // In H0 when the pass starts; a pause lands before the merge.
            draining: reads.get() > 1,
            ..h0()
        }
    };
    let roots = std::slice::from_ref(&fx.host);
    let found = boot_step(&mut Memory::default(), roots, true, &gate);
    assert_eq!(only(&found).state, CheckoutState::Behind, "{found:?}");
    assert_eq!(snapshot(&fx.host), before);
    assert!(reads.get() >= 2, "read once for the network and again for the write");
}

#[test]
fn the_startup_facts_read_a_pause_manifest_h5_has_not_finished_with() {
    // What `arm_at_startup` does when it finds a live manifest, before
    // `fleet_sync::start` runs the startup pass.
    let id = "rp-checkout-ff-at-boot";
    crate::roll_pause::suppress::arm(id, Vec::new());
    let gate = HostGateInputs::at_boot(false, None);
    crate::roll_pause::suppress::disarm(id);
    assert!(gate.resume_pending, "{gate:?}");
    assert_eq!(gate.write_hold(), Some(NotCurrent::ResumePending));
    assert!(!online_at_boot(true, &gate), "no network before H5 either");
}

// ----------------------------------------------------------------------------
// The timer's live read
// ----------------------------------------------------------------------------

/// An enforcer that reports fixed drain facts and does nothing else.
struct Facts(DrainFacts);

impl Enforcer for Facts {
    fn hold(&self, _note: String) -> bool {
        false
    }
    fn release(&self) -> bool {
        false
    }
    fn is_held(&self) -> bool {
        false
    }
    fn stop(&self, _reason: String) -> bool {
        false
    }
    fn drain_facts(&self) -> DrainFacts {
        self.0
    }
}

#[test]
fn the_timer_reads_its_write_hold_from_the_live_drain_state() {
    let draining = Facts(DrainFacts {
        draining: true,
        roll_in_progress: false,
    });
    assert_eq!(write_hold_now(Some(&draining)), Some("draining"));
    let rolling = Facts(DrainFacts {
        draining: false,
        roll_in_progress: true,
    });
    assert_eq!(write_hold_now(Some(&rolling)), Some("roll pending"));
    // No drain state to read: there is no way to know the host is not
    // paused, so nothing is written.
    assert!(write_hold_now(None).is_some());
}
