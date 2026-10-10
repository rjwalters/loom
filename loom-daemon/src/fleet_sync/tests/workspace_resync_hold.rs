//! The dispatch hold's view of a real pass (#10719): both copies of the
//! installed files, judged against real git and a synthetic payload.

use super::*;
use crate::workspace_hold::{HeldCopy, HoldKind, Holds, Observation, Verdict, ALERT_AFTER};

/// A pass by `host`, then what it tells the hold, folded into `holds`.
fn hold_pass(host: &Host<'_>, floor: Option<&str>, holds: &mut Holds) -> Vec<Observation> {
    hold_pass_in(host, Mode::Write, floor, holds).1
}

/// [`hold_pass`] in `mode`, with the pass itself.
fn hold_pass_in(
    host: &Host<'_>,
    mode: Mode,
    floor: Option<&str>,
    holds: &mut Holds,
) -> (WorkspacePass, Vec<Observation>) {
    let pass = host.pass_with(mode, &|| Ok(()), floor);
    let forge = host.fx.forge.clone();
    let env = Env {
        host: host.name,
        running: v(host.version),
        floor: floor.map(v),
        interval: INTERVAL,
        payload: &host.payload,
        nwo: &|_| Some(REPO.to_string()),
        may_write: &|_, _| Ok(()),
        forge: &move |_, _| -> Box<dyn ClaimForge> { Box::new(Shared(forge.clone())) },
        gate: &|| Ok(()),
        clock: &|| host.now.get(),
        spent: &|| false,
        overdue: &|| false,
        heads: &|asks| host.heads.answer(asks),
    };
    let seen = observations(&env, &pass, &mut host.memory.borrow_mut());
    holds.step(&seen, v(host.version), host.now.get(), ALERT_AFTER);
    (pass, seen)
}

fn verdicts(seen: &[Observation]) -> (Verdict, Verdict) {
    assert_eq!(seen.len(), 1);
    (seen[0].default_branch.verdict, seen[0].checkout.verdict)
}

/// The operator's ratchet-guard case, end to end: a host on 0.19.880 meets a
/// repo a newer host (0.19.900) stamped, whose files still work with
/// 0.19.880. It reports repo-ahead, never resyncs it, holds nothing and
/// demands no roll.
#[test]
fn a_repo_stamped_by_a_newer_compatible_host_is_neither_held_nor_a_roll_demand() {
    let fx = Fixture::new(Seed {
        version: "0.19.900",
        requires: Some("0.19.772"),
        ..STALE
    });
    let older = Host::new(&fx, "host-older");
    let before = fx.origin_head();
    let mut holds = Holds::default();
    let seen = hold_pass(&older, None, &mut holds);
    assert_eq!(verdicts(&seen), (Verdict::Clear, Verdict::Clear));
    assert!(holds.holds().is_empty(), "no dispatch hold");
    assert_eq!(holds.demand(), None, "no roll demand");
    assert_eq!(fx.origin_head(), before, "never resynced downward");
}

#[test]
fn a_repo_that_needs_a_newer_daemon_holds_both_copies_and_demands_it() {
    let fx = Fixture::new(Seed {
        version: "0.19.900",
        requires: Some("0.19.890"),
        ..STALE
    });
    let host = Host::new(&fx, "host-a");
    let mut holds = Holds::default();
    let seen = hold_pass(&host, None, &mut holds);
    let w4 = Verdict::Hold(HoldKind::DaemonTooOld);
    assert_eq!(verdicts(&seen), (w4, w4));
    let hold = holds.holds().into_values().next().unwrap();
    assert_eq!((hold.kind, hold.copy), (HoldKind::DaemonTooOld, HeldCopy::DefaultBranch));
    assert_eq!(holds.demand().unwrap().version, "0.19.890");
}

#[test]
fn a_too_old_checkout_is_held_only_while_its_files_differ() {
    // The default branch is current; this host's checkout is behind it.
    let fx = Fixture::new(Seed {
        version: RUNNING,
        current: true,
        ..STALE
    });
    let host = Host::new(&fx, "host-a");
    let floor = Some("0.19.850");
    write(&host.root.join(INSTALL_METADATA_PATH), &metadata("0.19.800", Some("0.19.772")));
    let mut holds = Holds::default();

    // An old stamp over files equal to the payload: W0, not held.
    let seen = hold_pass(&host, floor, &mut holds);
    assert_eq!(verdicts(&seen), (Verdict::Clear, Verdict::Clear));
    assert!(holds.holds().is_empty());

    // The checkout's files really are old, but its requires_daemon is met:
    // behind only the floor, so not held (#11052).
    write(&host.root.join(".loom/scripts/a.sh"), "#!/bin/sh\necho old\n");
    write(&host.root.join(INSTALL_METADATA_PATH), &metadata("0.19.801", Some("0.19.772")));
    let seen = hold_pass(&host, floor, &mut holds);
    assert_eq!(verdicts(&seen), (Verdict::Clear, Verdict::Clear));
    assert!(holds.holds().is_empty());

    // The same with no requires_daemon on record: held, on the checkout copy.
    write(&host.root.join(INSTALL_METADATA_PATH), &metadata("0.19.802", None));
    let seen = hold_pass(&host, floor, &mut holds);
    assert_eq!(verdicts(&seen), (Verdict::Clear, Verdict::Hold(HoldKind::InstallIncompatible)));
    let hold = holds.holds().into_values().next().unwrap();
    assert_eq!((hold.kind, hold.copy), (HoldKind::InstallIncompatible, HeldCopy::Checkout));
    assert_eq!(holds.demand(), None, "W3 is cleared by a resync, not a roll");

    // The checkout is brought up to date: the next pass clears it.
    git(&host.root, &["checkout", "--", "."]);
    hold_pass(&host, floor, &mut holds);
    assert!(holds.holds().is_empty());
}

/// Install metadata a resync to `pending` was interrupted over: the stamp is
/// still the old release's, because it is written last.
fn interrupted(version: &str, requires: Option<&str>, pending: &str) -> String {
    metadata(version, requires).replacen(
        "{\n",
        &format!("{{\n  \"resync_pending\": \"{pending}\",\n"),
        1,
    )
}

/// A checkout whose resync to a release at or below the daemon was
/// interrupted still shows an old, compatible stamp plus `resync_pending`.
/// It must be held, not cleared by the behind-but-compatible shortcut
/// (#11109).
#[test]
fn a_checkout_with_resync_pending_is_held_whatever_its_versions_say() {
    let fx = Fixture::new(Seed {
        version: RUNNING,
        current: true,
        ..STALE
    });
    let host = Host::new(&fx, "host-a");
    let mut holds = Holds::default();
    assert_eq!(verdicts(&hold_pass(&host, None, &mut holds)), (Verdict::Clear, Verdict::Clear));

    write(
        &host.root.join(INSTALL_METADATA_PATH),
        &interrupted("0.19.801", Some("0.19.772"), "0.19.802"),
    );
    host.advance(INTERVAL);
    let seen = hold_pass(&host, Some("0.19.800"), &mut holds);
    assert_eq!(verdicts(&seen).1, Verdict::Hold(HoldKind::InstallIncompatible));
    assert_eq!(seen[0].checkout.demand, None);
}

/// A newer daemon started a resync on the default branch and did not finish
/// it. The files are a mix of two releases, so dispatch is held; the hold
/// asks for no roll, and this host never completes the run from its own
/// (older) payload.
#[test]
fn an_interrupted_newer_resync_is_held_and_demands_no_roll() {
    let fx = Fixture::new(Seed {
        version: RUNNING,
        current: true,
        ..STALE
    });
    let host = Host::new(&fx, "host-a");
    let mut holds = Holds::default();
    assert_eq!(verdicts(&hold_pass(&host, None, &mut holds)), (Verdict::Clear, Verdict::Clear));

    fx.push_from_seed("a resync to 0.19.950, interrupted", |seed| {
        write(
            &seed.join(INSTALL_METADATA_PATH),
            &interrupted(RUNNING, Some("0.19.772"), "0.19.950"),
        );
    });
    let before = fx.origin_head();
    host.advance(INTERVAL);
    let seen = hold_pass(&host, None, &mut holds);
    let held = Verdict::Hold(HoldKind::InstallIncompatible);
    // This host's checkout is still at the commit before the interruption.
    assert_eq!(verdicts(&seen), (held, Verdict::Clear));
    assert_eq!(seen[0].default_branch.demand, None, "an interrupted resync names no need");
    let hold = holds.holds().into_values().next().expect("held");
    assert_eq!((hold.kind, hold.copy), (HoldKind::InstallIncompatible, HeldCopy::DefaultBranch));
    assert!(hold.detail.contains("a resync to 0.19.950 was interrupted"), "{}", hold.detail);
    assert!(hold.detail.contains("mix of two releases"), "{}", hold.detail);
    assert_eq!(holds.demand(), None, "held without a roll demand: the ratchet guard stands");
    assert_eq!(fx.origin_head(), before, "and never completed from this older payload");

    // The same in this host's checkout (an interrupted CLI resync there).
    let own = fs::read_to_string(host.root.join(INSTALL_METADATA_PATH)).unwrap();
    write(
        &host.root.join(INSTALL_METADATA_PATH),
        &interrupted(RUNNING, Some("0.19.772"), "0.19.950"),
    );
    host.advance(INTERVAL);
    let seen = hold_pass(&host, None, &mut holds);
    assert_eq!(verdicts(&seen), (held, held));
    assert_eq!(seen[0].checkout.demand, None);
    assert_eq!(holds.demand(), None);

    // A host on that release finishes the run, and the checkout is repaired.
    fx.push_from_seed("the resync, finished", |seed| {
        write(&seed.join(INSTALL_METADATA_PATH), &metadata(RUNNING, Some("0.19.772")));
    });
    write(&host.root.join(INSTALL_METADATA_PATH), &own);
    host.advance(INTERVAL);
    assert_eq!(verdicts(&hold_pass(&host, None, &mut holds)), (Verdict::Clear, Verdict::Clear));
    assert!(holds.holds().is_empty(), "cleared on the first clean pass");
}

/// A W4 workspace whose remote starts failing is reported from its backoff.
/// It must stay the W4 it was found to be: the `requires_daemon` its roll
/// demand is made of, and the versions in the hold's detail. Losing them
/// made the demand "the release after this one", which can roll the host to
/// a release still below the real requirement, with no settle wait.
#[test]
fn a_w4_workspace_in_backoff_keeps_its_requirement_demand_and_detail() {
    let fx = Fixture::new(Seed {
        version: "0.19.900",
        requires: Some("0.19.890"),
        ..STALE
    });
    let host = Host::new(&fx, "host-a");
    // Only the default branch is W4, so the demand is its alone to carry.
    write(&host.root.join(INSTALL_METADATA_PATH), &metadata(RUNNING, Some("0.19.772")));
    let w4 = Verdict::Hold(HoldKind::DaemonTooOld);
    let healthy = hold_pass(&host, None, &mut Holds::default());
    assert_eq!(verdicts(&healthy), (w4, Verdict::Clear));
    let want = healthy[0].default_branch.clone();
    assert_eq!(want.demand, Some(v("0.19.890")));

    // The repo's remote starts refusing: a failure, and a backoff.
    let fault = super::super::heads::Fault::NotFound("no such repository".into());
    host.heads
        .faults
        .borrow_mut()
        .insert(host.root.clone(), fault);
    git(&host.root, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
    host.advance(INTERVAL);
    let failing = hold_pass(&host, None, &mut Holds::default());
    assert_eq!(host.memory.borrow().backoff[&host.root].failures, 1);
    assert_eq!(failing[0].default_branch, want, "the failing pass itself");

    // Inside the backoff nothing is read: the report is the remembered one.
    host.advance(Duration::from_secs(1));
    let waiting = host.pass();
    let report = only(&waiting);
    assert!(reason(report).starts_with("backoff until "), "{report:?}");
    assert_eq!(report.state, WState::W4);
    assert_eq!(report.installed.as_deref(), Some("0.19.900"));
    assert_eq!(report.requires_daemon.as_deref(), Some("0.19.890"));

    // A fresh `Holds`, so nothing is owed to what an earlier pass left.
    let mut holds = Holds::default();
    let seen = hold_pass(&host, None, &mut holds);
    assert_eq!(seen[0].default_branch, want, "same verdict, demand and detail as when healthy");
    assert_eq!(holds.demand().expect("still a roll demand").version, "0.19.890");
    let hold = holds.holds().into_values().next().unwrap();
    assert!(hold.detail.contains("0.19.890"), "{}", hold.detail);
    assert!(!hold.detail.contains("backoff"), "{}", hold.detail);
}

// ----------------------------------------------------------------------------
// #11052: behind but compatible, and the checkout re-judge
// ----------------------------------------------------------------------------

/// The floor roll: the default branch is one floor behind, its files differ,
/// and its `requires_daemon` is met. It is W3, so the resync still takes it,
/// but nothing is held while it waits.
#[test]
fn a_compatible_w3_repo_is_not_held_and_is_still_resynced() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let floor = Some("0.19.850");
    let mut holds = Holds::default();
    let (check, seen) = hold_pass_in(&host, Mode::Check, floor, &mut holds);
    assert_eq!(only(&check).state, WState::W3);
    assert_eq!(verdicts(&seen), (Verdict::Clear, Verdict::Clear));
    assert!(holds.holds().is_empty(), "no dispatch hold");
    assert_eq!(holds.demand(), None);
    // Still a resync candidate: the next writing pass resyncs it.
    let before = fx.origin_head();
    let pass = host.pass_with(Mode::Write, &|| Ok(()), floor);
    assert_eq!(only(&pass).state, WState::W0);
    assert_ne!(fx.origin_head(), before, "resynced");
}

/// The same repo with no `requires_daemon` on record: nothing vouches for
/// its files, so it is held until the resync lands.
#[test]
fn a_w3_repo_whose_requires_daemon_cannot_be_read_is_held() {
    let fx = Fixture::new(Seed {
        requires: None,
        ..STALE
    });
    let host = Host::new(&fx, "host-a");
    let mut holds = Holds::default();
    let (check, seen) = hold_pass_in(&host, Mode::Check, Some("0.19.850"), &mut holds);
    assert_eq!(only(&check).state, WState::W3);
    let held = Verdict::Hold(HoldKind::InstallIncompatible);
    assert_eq!(verdicts(&seen), (held, held));
    let hold = holds.holds().into_values().next().unwrap();
    assert_eq!((hold.kind, hold.copy), (HoldKind::InstallIncompatible, HeldCopy::DefaultBranch));
    assert!(hold.detail.contains("no requires_daemon"), "{}", hold.detail);
    assert_eq!(holds.demand(), None);
}

/// A checkout held for its old files clears in the same pass the checkout
/// step fast-forwards it: the step's moved roots are judged again at once.
#[test]
fn a_checkout_hold_clears_when_the_checkout_fast_forwards_in_the_same_pass() {
    use crate::fleet_sync::checkout_ff;
    let fx = Fixture::new(Seed {
        requires: None,
        ..STALE
    });
    let host = Host::new(&fx, "host-a");
    let floor = Some("0.19.850");
    let mut holds = Holds::default();
    // The writing pass resyncs the default branch; this host's checkout is
    // still the old install, and holds.
    let (pass, seen) = hold_pass_in(&host, Mode::Write, floor, &mut holds);
    assert_eq!(only(&pass).state, WState::W0);
    let held = Verdict::Hold(HoldKind::InstallIncompatible);
    assert_eq!(verdicts(&seen), (Verdict::Clear, held));
    let hold = holds.holds().into_values().next().unwrap();
    assert_eq!(hold.copy, HeldCopy::Checkout);

    // The checkout step of that same pass fast-forwards the checkout.
    let env = checkout_ff::Env {
        write: true,
        held: &|| None,
        gate_in_flight: &|_| false,
        hold: &|_| Some(checkout_ff::MoveHold::free()),
        network: true,
        backing_off: &|_| false,
        confirmed: &|_, _| None,
        breaker_open: &|| false,
        budget: None,
        elapsed: &|| Duration::ZERO,
        clock: &|| host.now.get(),
    };
    let roots = [host.root.clone()];
    let moved = checkout_ff::run(&env, &roots, &mut checkout_ff::Memory::default());
    assert_eq!(moved.fast_forwarded(), roots, "{:?}", moved.checkouts);

    // And re-judges what it moved, without waiting for the next pass.
    let judge = super::super::hold::Judge {
        running: v(host.version),
        floor: floor.map(v),
        payload: &host.payload,
    };
    let seen = super::super::hold::checkout_observations(
        &judge,
        &moved.fast_forwarded(),
        Ok(()),
        &mut host.memory.borrow_mut(),
        &|_| Some(REPO.to_string()),
    );
    assert_eq!(verdicts(&seen), (Verdict::Unknown, Verdict::Clear));
    let events = holds.rejudge(&seen, v(host.version), host.now.get(), ALERT_AFTER);
    assert_eq!(events.iter().map(|e| e.event).collect::<Vec<_>>(), ["cleared"]);
    assert!(holds.holds().is_empty(), "cleared in the same pass");
}

/// #11186: a maintain-only workspace is maintained exactly like any other.
/// The pass resyncs it (it never asks about dispatch holds), the hold the
/// pass judges stays clear, and nothing raises a roll demand; dispatch is
/// refused all the while.
#[test]
fn a_maintain_only_workspace_is_still_resynced() {
    use crate::workspace_hold::{hold_for, set_maintain_only_for_test};
    use crate::workspace_registry::{MaintainOnly, MaintainOnlySource};
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let mark = MaintainOnly {
        by: MaintainOnlySource::FleetStore,
        since: t0(),
    };
    set_maintain_only_for_test(&host.root, Some(mark));
    let commits = fx.origin_commits();

    let mut holds = Holds::default();
    let (pass, _) = hold_pass_in(&host, Mode::Write, None, &mut holds);
    assert_eq!(only(&pass).state, WState::W0, "{pass:?}");
    assert!(reason(only(&pass)).starts_with("resynced to v0.19.880"), "{pass:?}");
    assert_eq!(fx.origin_commits(), commits + 1, "resynced");
    assert!(holds.holds().is_empty(), "no pass hold");
    assert_eq!(holds.demand(), None, "no roll demand");
    assert_eq!(hold_for(&host.root).unwrap().kind, HoldKind::MaintainOnly);

    set_maintain_only_for_test(&host.root, None);
}
