//! Tests for the workspace dispatch hold (#10719).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use chrono::TimeZone;
use serial_test::serial;

use super::*;
use crate::install_compat::{classify, DaemonCompat, InstallMeta};
use crate::sweep_registry::SweepKind;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 8, 12, 0, 0).unwrap()
}

fn mins(m: i64) -> chrono::Duration {
    chrono::Duration::minutes(m)
}

fn daemon(running: &str, floor: Option<&str>) -> DaemonCompat {
    DaemonCompat {
        running: v(running),
        supports_installed: v("0.19.0"),
        floor: floor.map(v),
    }
}

fn meta(version: Option<&str>, requires: Option<&str>) -> InstallMeta {
    InstallMeta {
        loom_version: version.map(str::to_string),
        requires_daemon: requires.map(str::to_string),
    }
}

fn gate(m: &InstallMeta, d: &DaemonCompat) -> Result<Compat, ResyncRefusal> {
    crate::init::payload::resync_gate(m, d)
}

// ----------------------------------------------------------------------------
// decide_hold: the decision table
// ----------------------------------------------------------------------------

#[test]
fn the_decision_table() {
    let d = daemon("0.19.900", Some("0.19.850"));
    let held = |k| Verdict::Hold(k);
    for (version, requires, diff, want) in [
        // W4: requires_daemon above running, whatever else is true.
        (Some("0.19.950"), Some("0.19.901"), None, held(HoldKind::DaemonTooOld)),
        (Some("0.19.100"), Some("0.19.901"), Some(true), held(HoldKind::DaemonTooOld)),
        // Equal requires_daemon is fine.
        (Some("0.19.900"), Some("0.19.900"), None, Verdict::Clear),
        // Unorderable: may need a newer daemon.
        (Some("0.20.0-rc1"), Some("0.19.772"), None, held(HoldKind::DaemonTooOld)),
        (Some("0.19.900"), Some("soon"), None, held(HoldKind::DaemonTooOld)),
        // The ratchet guard: ahead but compatible, with or without a stamp.
        (Some("0.19.950"), Some("0.19.772"), None, Verdict::Clear),
        (Some("0.19.950"), None, None, Verdict::Clear),
        // W3 keys on the payload diff.
        (
            Some("0.19.800"),
            Some("0.19.772"),
            Some(true),
            held(HoldKind::InstallIncompatible),
        ),
        (Some("0.19.800"), Some("0.19.772"), Some(false), Verdict::Clear),
        (Some("0.19.800"), Some("0.19.772"), None, Verdict::Unknown),
        // Exactly at the floor is not too old.
        (Some("0.19.850"), Some("0.19.772"), Some(true), Verdict::Clear),
        // Below supports_installed with no floor in play.
        (Some("0.18.999"), None, Some(true), held(HoldKind::InstallIncompatible)),
        // Compatible, and the migration case (no requires_daemon).
        (Some("0.19.880"), Some("0.19.772"), Some(true), Verdict::Clear),
        (Some("0.19.880"), None, Some(true), Verdict::Clear),
        (None, None, Some(true), Verdict::Clear),
        (Some("unknown"), None, Some(true), Verdict::Clear),
    ] {
        let m = meta(version, requires);
        assert_eq!(decide_hold(&gate(&m, &d), diff), want, "{version:?} {requires:?} {diff:?}");
    }
}

#[test]
fn the_other_refusals() {
    for (refusal, want) in [
        (ResyncRefusal::UnreadableMetadata("x".into()), Verdict::Unknown),
        (ResyncRefusal::NotInstalled, Verdict::Clear),
        (ResyncRefusal::LoomSourceRepo, Verdict::Clear),
        (ResyncRefusal::NotAReleaseBuild, Verdict::Clear),
        (
            ResyncRefusal::PendingAheadOfDaemon {
                pending: v("0.19.950"),
                running: v("0.19.900"),
            },
            Verdict::Clear,
        ),
    ] {
        assert_eq!(decide_hold(&Err(refusal.clone()), Some(true)), want, "{refusal:?}");
    }
    // `classify` agrees on W4 over W3: a too-old stamp that needs a newer daemon.
    let d = daemon("0.19.900", Some("0.19.850"));
    assert_eq!(
        classify(&meta(Some("0.19.100"), Some("0.19.950")), &d),
        Compat::NeedsNewerDaemon
    );
}

#[test]
fn the_demand_is_requires_daemon_or_any_newer_release() {
    let running = v("0.19.900");
    assert_eq!(demanded(Some("0.19.950"), running), v("0.19.950"));
    assert_eq!(demanded(Some("0.20.0-rc1"), running), v("0.19.901"));
    assert_eq!(demanded(None, running), v("0.19.901"));
    assert_eq!(demanded(Some("0.19.800"), running), v("0.19.901"));
}

// ----------------------------------------------------------------------------
// The ratchet guard, as the operator asked for it
// ----------------------------------------------------------------------------

/// Two hosts on different, mutually compatible versions, and one repo stamped
/// by the newer host. The older host neither rolls nor holds dispatch.
#[test]
fn an_older_compatible_host_neither_rolls_nor_holds_for_a_repo_the_newer_host_stamped() {
    // What the newer host's release writes: its own version, and the oldest
    // daemon its files work with, which both hosts satisfy.
    let stamp = meta(Some("0.19.937"), Some("0.19.772"));
    let older = daemon("0.19.930", None);
    let newer = daemon("0.19.937", None);
    for (host, d) in [("older", &older), ("newer", &newer)] {
        let g = gate(&stamp, d);
        let verdict = decide_hold(&g, Some(true));
        assert_eq!(verdict, Verdict::Clear, "{host} host must keep dispatching");
    }
    // The older host's pass: both copies carry the newer stamp.
    let finding = |d: &DaemonCompat| match decide_hold(&gate(&stamp, d), Some(true)) {
        Verdict::Clear => Finding::clear(),
        other => panic!("{other:?}"),
    };
    let mut holds = Holds::default();
    let root = PathBuf::from("/nonexistent/acme-app");
    let seen = Observation {
        root: root.clone(),
        repo: Some("acme/app".into()),
        default_branch: finding(&older),
        checkout: finding(&older),
    };
    let events = holds.step(&[seen], older.running, t0(), ALERT_AFTER);
    assert!(events.is_empty(), "{events:?}");
    assert!(holds.holds().is_empty(), "no dispatch hold");
    assert_eq!(holds.demand(), None, "no roll demand");
    // And with no demand the self-update target is the ordinary one.
    let auto = crate::auto_update::floor_roll::Release {
        tag: "v0.19.937".into(),
        version: "0.19.937".into(),
    };
    let target =
        crate::auto_update::floor_roll::select_target("0.19.930", None, None, Some(&auto)).unwrap();
    assert_eq!(target.source, crate::auto_update::floor_roll::TargetSource::AutoUpdate);
}

// ----------------------------------------------------------------------------
// Holds::step: set, keep, clear, alert, demand
// ----------------------------------------------------------------------------

fn obs(root: &str, default_branch: Finding, checkout: Finding) -> Observation {
    Observation {
        root: PathBuf::from(root),
        repo: Some(format!("acme/{}", root.trim_start_matches("/nonexistent/"))),
        default_branch,
        checkout,
    }
}

fn w3() -> Finding {
    Finding::install_incompatible("too old".into())
}

fn w4(requires: &str) -> Finding {
    Finding::daemon_too_old("needs newer".into(), Some(requires), v("0.19.900"))
}

const RUNNING: &str = "0.19.900";

#[test]
fn a_hold_is_set_kept_on_unknown_and_cleared() {
    let mut holds = Holds::default();
    let a = "/nonexistent/a";
    let events = holds.step(&[obs(a, w3(), Finding::clear())], v(RUNNING), t0(), ALERT_AFTER);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event, "set");
    let hold = holds.holds().remove(Path::new(a)).unwrap();
    assert_eq!(
        (hold.kind, hold.copy, hold.since),
        (HoldKind::InstallIncompatible, HeldCopy::DefaultBranch, t0())
    );
    // A read failure in between keeps it, and says nothing.
    let later = t0() + mins(5);
    let events = holds.step(
        &[obs(a, Finding::unknown(), Finding::unknown())],
        v(RUNNING),
        later,
        ALERT_AFTER,
    );
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(holds.holds().get(Path::new(a)).unwrap().since, t0());
    // W3 to W0: cleared on the next pass.
    let events = holds.step(
        &[obs(a, Finding::clear(), Finding::clear())],
        v(RUNNING),
        later + mins(5),
        ALERT_AFTER,
    );
    assert_eq!(events.iter().map(|e| e.event).collect::<Vec<_>>(), ["cleared"]);
    assert!(holds.holds().is_empty());
}

#[test]
fn unknown_with_no_previous_verdict_is_not_a_hold() {
    let mut holds = Holds::default();
    let events = holds.step(
        &[obs(
            "/nonexistent/a",
            Finding::unknown(),
            Finding::unknown(),
        )],
        v(RUNNING),
        t0(),
        ALERT_AFTER,
    );
    assert!(events.is_empty());
    assert!(holds.holds().is_empty());
}

#[test]
fn the_checkout_copy_holds_on_its_own_and_w4_wins_over_w3() {
    let mut holds = Holds::default();
    let a = "/nonexistent/a";
    holds.step(&[obs(a, Finding::clear(), w3())], v(RUNNING), t0(), ALERT_AFTER);
    let hold = holds.holds().remove(Path::new(a)).unwrap();
    assert_eq!((hold.kind, hold.copy), (HoldKind::InstallIncompatible, HeldCopy::Checkout));
    // The default branch now needs a newer daemon: W4 decides, and its clock
    // starts again.
    let events =
        holds.step(&[obs(a, w4("0.19.950"), w3())], v(RUNNING), t0() + mins(1), ALERT_AFTER);
    assert_eq!(events[0].event, "set");
    assert_eq!(events[0].demand.as_deref(), Some("0.19.950"));
    let hold = holds.holds().remove(Path::new(a)).unwrap();
    assert_eq!(
        (hold.kind, hold.copy, hold.since),
        (HoldKind::DaemonTooOld, HeldCopy::DefaultBranch, t0() + mins(1))
    );
}

#[test]
fn a_standing_hold_alerts_once() {
    let mut holds = Holds::default();
    let a = "/nonexistent/a";
    let pass = |holds: &mut Holds, at| {
        holds.step(&[obs(a, w4("0.19.950"), Finding::clear())], v(RUNNING), at, ALERT_AFTER)
    };
    assert_eq!(pass(&mut holds, t0()).len(), 1);
    assert!(pass(&mut holds, t0() + mins(29)).is_empty());
    let events = pass(&mut holds, t0() + mins(30));
    assert_eq!(events.iter().map(|e| e.event).collect::<Vec<_>>(), ["standing"]);
    assert!(pass(&mut holds, t0() + mins(31)).is_empty());
    assert!(pass(&mut holds, t0() + mins(120)).is_empty(), "once per hold");
}

#[test]
fn a_workspace_that_leaves_the_registry_is_dropped() {
    let mut holds = Holds::default();
    holds.step(&[obs("/nonexistent/a", w3(), Finding::clear())], v(RUNNING), t0(), ALERT_AFTER);
    let events = holds.step(&[], v(RUNNING), t0() + mins(1), ALERT_AFTER);
    assert_eq!(events.iter().map(|e| e.event).collect::<Vec<_>>(), ["cleared"]);
    assert!(holds.holds().is_empty());
}

#[test]
fn the_demand_is_the_highest_need_over_every_copy() {
    let mut holds = Holds::default();
    holds.step(
        &[
            obs("/nonexistent/a", w4("0.19.920"), w4("0.19.960")),
            obs("/nonexistent/b", w4("0.19.950"), Finding::clear()),
            // W3 asks for nothing: a resync clears it, not a roll.
            obs("/nonexistent/c", w3(), w3()),
            obs("/nonexistent/d", Finding::clear(), Finding::clear()),
        ],
        v(RUNNING),
        t0(),
        ALERT_AFTER,
    );
    let demand = holds.demand().unwrap();
    assert_eq!(demand.version, "0.19.960");
    assert_eq!(demand.root, PathBuf::from("/nonexistent/a"));
    assert_eq!(demand.copy, HeldCopy::Checkout);
    assert_eq!(holds.holds().len(), 3);
    // Once the copies are compatible (the host rolled), no demand is left.
    holds.step(
        &[obs("/nonexistent/a", Finding::clear(), Finding::clear())],
        v("0.19.960"),
        t0() + mins(1),
        ALERT_AFTER,
    );
    assert_eq!(holds.demand(), None);
}

// ----------------------------------------------------------------------------
// The dispatch side
// ----------------------------------------------------------------------------

fn hold(kind: HoldKind) -> WorkspaceHold {
    WorkspaceHold {
        kind,
        copy: HeldCopy::DefaultBranch,
        since: t0(),
        detail: "requires daemon 0.19.950 > running 0.19.900".into(),
    }
}

/// The registry refuses issue and PR-set dispatch into a held workspace with
/// the typed error, spawning nothing; an unheld sibling dispatches.
#[test]
#[serial]
fn the_registry_refuses_a_held_workspace_and_not_its_sibling() {
    let held_dir = tempfile::tempdir().unwrap();
    let free_dir = tempfile::tempdir().unwrap();
    let (mut held, held_log) =
        crate::sweep_registry::test_support::fixture_registry(held_dir.path());
    let (mut free, _) = crate::sweep_registry::test_support::fixture_registry(free_dir.path());
    set_for_test(held_dir.path(), Some(hold(HoldKind::DaemonTooOld)));

    for kind in [SweepKind::Issue(10719), SweepKind::PrSet(vec![1, 2])] {
        let err = held
            .dispatch(&kind, None, None, None, None)
            .expect_err("held");
        let typed = err
            .downcast_ref::<WorkspaceHeldDispatchError>()
            .expect("the typed refusal");
        assert_eq!(typed.kind, HoldKind::DaemonTooOld);
        assert!(err.to_string().contains("daemon-too-old"), "{err}");
        assert!(err.to_string().contains("`force` does not override"), "{err}");
    }
    assert!(!held_log.exists(), "nothing was spawned");
    assert!(held.list().is_empty(), "no registry entry");

    let out = free.dispatch(&SweepKind::Issue(10720), None, None, None, None);
    assert!(out.is_ok(), "the sibling dispatches: {out:?}");

    // The hold lifts: the same workspace dispatches again.
    set_for_test(held_dir.path(), None);
    let out = held.dispatch(&SweepKind::Issue(10721), None, None, None, None);
    assert!(out.is_ok(), "{out:?}");
}

#[test]
fn the_role_runner_skips_held_roots_and_logs_once_per_change() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let roots = vec![a.path().to_path_buf(), b.path().to_path_buf()];
    let mut logged = HashSet::new();
    set_for_test(a.path(), Some(hold(HoldKind::InstallIncompatible)));
    for _ in 0..3 {
        let free = filter_held(roots.clone(), &mut logged);
        assert_eq!(free, vec![b.path().to_path_buf()]);
        assert_eq!(logged.len(), 1, "remembered, so logged once");
    }
    set_for_test(a.path(), None);
    assert_eq!(filter_held(roots.clone(), &mut logged), roots);
    assert!(logged.is_empty());
}

#[test]
fn the_work_finder_cause_tokens_round_trip() {
    for kind in [HoldKind::InstallIncompatible, HoldKind::DaemonTooOld] {
        let cause = kind.halt_cause();
        assert_eq!(HaltCause::from_wire(cause.as_str()), Some(cause));
    }
    assert_eq!(HoldKind::InstallIncompatible.halt_cause().as_str(), "install_incompatible");
    assert_eq!(HoldKind::DaemonTooOld.halt_cause().as_str(), "daemon_too_old");
}
