//! The dispatch hold's view of a real pass (#10719): both copies of the
//! installed files, judged against real git and a synthetic payload.

use super::*;
use crate::workspace_hold::{HeldCopy, HoldKind, Holds, Observation, Verdict, ALERT_AFTER};

/// A pass by `host`, then what it tells the hold, folded into `holds`.
fn hold_pass(host: &Host<'_>, floor: Option<&str>, holds: &mut Holds) -> Vec<Observation> {
    let pass = host.pass_with(Mode::Write, &|| Ok(()), floor);
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
    seen
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

    // The checkout's files really are old: held, on the checkout copy.
    write(&host.root.join(".loom/scripts/a.sh"), "#!/bin/sh\necho old\n");
    write(&host.root.join(INSTALL_METADATA_PATH), &metadata("0.19.801", Some("0.19.772")));
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
