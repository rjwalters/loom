//! The workspace pass's own network decision, which the checkout step that
//! follows it takes as its own (#10869): a host that is not in H0, in an
//! outage hold, with `fleet.autoApply` off or paused makes no network call
//! from either half. Each case runs a real resync pass and then the checkout
//! step over the same temp clone.

use std::sync::Mutex;
use std::time::Instant;

use super::*;
use crate::fleet_sync::checkout_ff::{self, Stepped};

/// The checkout step after `pass`, over `root` alone, with fresh memory.
fn checkout_after(pass: &WorkspacePass, root: &Path) -> checkout_ff::CheckoutPass {
    let memory = Mutex::new(checkout_ff::Memory::default());
    let roots = || vec![root.to_path_buf()];
    let began = Instant::now();
    match checkout_ff::timer_step(&memory, &roots, true, &|| None, pass, began, None) {
        Stepped::Ran(found) => found,
        Stepped::Busy => panic!("nothing holds this memory"),
    }
}

/// Assert that neither half used the network and the clone learned nothing.
fn assert_offline(case: &str, host: &Host<'_>, pass: &WorkspacePass) {
    let before = git(&host.root, &["rev-parse", "refs/remotes/origin/main"]);
    assert!(!pass.online, "{case}: the resync pass is offline");
    assert_eq!(network(pass), (0, 0, 0), "{case}: the resync asked nothing");
    let step = checkout_after(pass, &host.root);
    assert_eq!((step.probes, step.fetches), (0, 0), "{case}: nor did the checkout step");
    let after = git(&host.root, &["rev-parse", "refs/remotes/origin/main"]);
    assert_eq!(after, before, "{case}: nothing was fetched");
}

#[test]
fn a_host_that_is_not_h0_makes_no_network_call_from_either_half() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    // The default branch moves after the clone: any ask would see it.
    fx.push_from_seed("a later commit", |seed| write(&seed.join("later.txt"), "x\n"));
    let cases = [
        ("non-release build", NotCurrent::NotAReleaseBuild),
        ("release not verified", NotCurrent::ReleaseUnverified),
        ("roll pending", NotCurrent::RollPending),
        ("staged binary", NotCurrent::Staged),
        ("below the floor", NotCurrent::FloorBelow),
        ("stalled self-update", NotCurrent::Stalled),
        ("paused", NotCurrent::Draining),
    ];
    for (case, why) in cases {
        let pass = host.pass_with(Mode::Write, &|| Err(why), None);
        assert_offline(case, &host, &pass);
    }
    // `fleet.autoApply` off: the timer runs the pass in `Mode::Check`.
    let pass = host.pass_with(Mode::Check, &|| Ok(()), None);
    assert_offline("autoApply off", &host, &pass);
    // In H0 the pass is online, and so is the step.
    assert!(host.pass().online, "H0");
}

#[test]
fn a_host_in_an_outage_hold_makes_no_network_call_from_either_half() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    git(&host.root, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    let first = host.pass();
    assert!(first.online, "the pass that finds the outage was allowed to ask");
    host.advance(Duration::from_secs(30));
    let held = host.pass();
    assert!(host.memory.borrow().outage_hold(host.now.get()).is_some(), "inside the hold");
    assert_offline("outage hold", &host, &held);
}

#[test]
fn a_repo_whose_remote_is_backed_off_is_listed_for_the_checkout_step() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let other = fx.clone_as("other");
    git(&host.root, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    let roots = [host.root.clone(), other.clone()];
    let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    assert!(pass.online, "one remote answered: no outage hold");
    assert_eq!(pass.backing_off, vec![host.root.clone()], "{pass:?}");
    // Past the backoff, it is asked again by both halves.
    host.advance(Duration::from_secs(7 * 60 * 60));
    git(&host.root, &["remote", "set-url", "origin", &fx.origin.to_string_lossy()]);
    let pass = host.pass_over(&roots, Mode::Write, &|| Ok(()), None);
    assert!(pass.backing_off.is_empty(), "{pass:?}");
}

/// Which backoffs keep the checkout step off a remote: one that did not
/// answer and one that refused the read. A refused push (`Protected`) still
/// serves reads, and `Other` is not a network signal, so both are still asked.
#[test]
fn only_an_unreachable_or_refused_backoff_is_listed_for_the_checkout_step() {
    let now = Utc::now();
    let cases = [
        ("unreachable", FailureKind::Unreachable, true),
        ("refused", FailureKind::Refused, true),
        ("protected", FailureKind::Protected, false),
        ("other", FailureKind::Other, false),
    ];
    let mut memory = Memory::default();
    let mut roots = Vec::new();
    for (name, kind, _) in cases {
        let root = PathBuf::from(format!("/nonexistent/{name}"));
        let (next_attempt, _) = memory.fail(&report_for(&root), kind, name, INTERVAL, now);
        assert!(next_attempt > now, "{name}: it is backed off");
        roots.push(root);
    }
    let listed = memory.remotes_backed_off(&roots, now);
    for ((name, _, expected), root) in cases.iter().zip(&roots) {
        assert_eq!(listed.contains(root), *expected, "{name}: {listed:?}");
    }
    // Every backoff has ended by then: nothing is listed.
    let later = now + ChronoDuration::hours(48);
    assert!(memory.remotes_backed_off(&roots, later).is_empty());
}

/// #10995 through both halves: the resync skips its fetch below the disk
/// floor, sets no backoff and stays online, and has already told the checkout
/// step the head it heard. The step that follows must not run that fetch.
#[test]
fn a_low_disk_resync_pass_is_followed_by_a_step_that_fetches_nothing() {
    use crate::fetch_headroom::test_override::with_free_gb;
    use crate::fleet_sync::checkout_ff::CheckoutState;
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    // The default branch moves after the clone, so the head the pass hears is
    // one the clone lacks.
    fx.push_from_seed("a later commit", |seed| write(&seed.join("later.txt"), "x\n"));
    let before = git(&host.root, &["rev-parse", "refs/remotes/origin/main"]);
    let memory = Mutex::new(checkout_ff::Memory::default());
    let roots = || vec![host.root.clone()];
    let step_after = |began: Instant, pass: &WorkspacePass| match checkout_ff::timer_step(
        &memory,
        &roots,
        true,
        &|| None,
        pass,
        began,
        None,
    ) {
        Stepped::Ran(found) => found,
        Stepped::Busy => panic!("nothing holds this memory"),
    };

    for tick in 0..4 {
        let (pass, step) = with_free_gb(1, || {
            // As `spawn_with` does: the step is handed the instant the pass
            // began, so it trusts the head that pass heard.
            let began = Instant::now();
            let pass = host.pass();
            let step = step_after(began, &pass);
            (pass, step)
        });
        let report = only(&pass);
        assert!(reason(report).starts_with("low-disk: "), "tick {tick}: {report:?}");
        assert!(pass.online, "a low-disk skip does not take the pass offline");
        assert!(pass.backing_off.is_empty(), "nor back the root off");
        assert_eq!(step.probes, 0, "tick {tick}: the resync's head is trusted: {step:?}");
        assert_eq!(step.fetches, 0, "tick {tick}: the step runs no fetch either");
        assert_eq!(step.low_disk, 1, "tick {tick}: it skipped the one that was due");
        assert_eq!(step.checkouts[0].state, CheckoutState::LowDisk, "tick {tick}: {step:?}");
        let after = git(&host.root, &["rev-parse", "refs/remotes/origin/main"]);
        assert_eq!(after, before, "tick {tick}: nothing was fetched by either half");
        host.advance(INTERVAL);
    }
}
