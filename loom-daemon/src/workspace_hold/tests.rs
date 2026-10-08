//! Tests for the workspace dispatch hold (#10719).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;

use chrono::TimeZone;
use serial_test::serial;

use super::*;
use crate::install_compat::{classify, DaemonCompat, InstallMeta};
use crate::types::SweepKind;

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
            // Held, and (see the test further down) with no roll demand.
            Verdict::Hold(HoldKind::InstallIncompatible),
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
    // The roll's only input from here is `Holds::demand()`. Run it through
    // the self-update loop's own steps (`RepoAheadState`, then
    // `select_target`) with the newer release already out: the target stays
    // the ordinary, settle-gated one.
    use crate::auto_update::floor_roll::repo_ahead::{Demand, RepoAheadState};
    use crate::auto_update::floor_roll::{select_target, Release, TargetSource};
    let release = Release {
        tag: "v0.19.937".into(),
        version: "0.19.937".into(),
    };
    let roll = |holds: &Holds| {
        let demand = holds.demand().map(|d| Demand {
            version: d.version,
            workspace: d.repo.unwrap_or_default(),
        });
        let mut state = RepoAheadState::default();
        state.set_basis(demand, "0.19.930");
        let ahead = state.observe(Some(&release));
        let target = select_target("0.19.930", None, ahead.as_ref(), Some(&release)).unwrap();
        (state.driving(), target.source)
    };
    assert_eq!(
        roll(&holds),
        (false, TargetSource::AutoUpdate),
        "a compatible repo rolls nobody"
    );

    // The control: the same repo, once its files NEED the newer daemon. The
    // same wiring now yields a hold, a demand, and a repo-ahead target. So
    // the assertions above pass because of the guard, not because nothing
    // on this path can ever produce a demand.
    let needy = meta(Some("0.19.937"), Some("0.19.937"));
    let g = gate(&needy, &older);
    assert_eq!(decide_hold(&g, Some(true)), Verdict::Hold(HoldKind::DaemonTooOld));
    let finding =
        Finding::daemon_too_old(String::new(), needy.requires_daemon.as_deref(), older.running);
    let seen = Observation {
        root,
        repo: Some("acme/app".into()),
        default_branch: finding.clone(),
        checkout: finding,
    };
    let events = holds.step(&[seen], older.running, t0() + mins(1), ALERT_AFTER);
    assert_eq!(events.iter().map(|e| e.event).collect::<Vec<_>>(), ["set"]);
    assert_eq!(holds.demand().map(|d| d.version), Some("0.19.937".to_string()));
    assert_eq!(roll(&holds), (true, TargetSource::RepoAhead));
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

/// A hold nothing clears alerts at 30 minutes and again every 30 minutes:
/// never once and then silence, and never once per pass.
#[test]
fn a_standing_hold_alerts_every_thirty_minutes_while_it_stands() {
    let mut holds = Holds::default();
    let a = "/nonexistent/a";
    let pass = |holds: &mut Holds, at| {
        holds.step(&[obs(a, w4("0.19.950"), Finding::clear())], v(RUNNING), at, ALERT_AFTER)
    };
    assert_eq!(pass(&mut holds, t0()).len(), 1);
    let mut alerted = Vec::new();
    for minute in 1..=125 {
        let events = pass(&mut holds, t0() + mins(minute));
        assert!(events.iter().all(|e| e.event == "standing"), "{events:?}");
        if !events.is_empty() {
            assert_eq!(events[0].hold.since, t0(), "the hold's own clock is untouched");
            alerted.push(minute);
        }
    }
    assert_eq!(alerted, [30, 60, 90, 120], "bounded: one per 30 minutes over 125 passes");

    // A hold that changes kind is a new hold, with a new clock.
    let events =
        holds.step(&[obs(a, w3(), Finding::clear())], v(RUNNING), t0() + mins(126), ALERT_AFTER);
    assert_eq!(events.iter().map(|e| e.event).collect::<Vec<_>>(), ["set"]);
    let later = |holds: &mut Holds, m| {
        holds.step(&[obs(a, w3(), Finding::clear())], v(RUNNING), t0() + mins(m), ALERT_AFTER)
    };
    assert!(later(&mut holds, 150).is_empty(), "not 30 minutes into the new hold yet");
    assert_eq!(later(&mut holds, 156).len(), 1);
}

/// A copy that cannot be read keeps its hold, and keeps alerting: the hold
/// says how old the verdict it stands on is, in the alert and on `status`.
#[test]
fn an_unreadable_copy_keeps_alerting_and_says_how_old_its_verdict_is() {
    let mut holds = Holds::default();
    let a = "/nonexistent/a";
    holds.step(&[obs(a, w4("0.19.950"), Finding::clear())], v(RUNNING), t0(), ALERT_AFTER);
    let unreadable = |holds: &mut Holds, m| {
        let seen = [obs(a, Finding::unknown(), Finding::unknown())];
        holds.step(&seen, v(RUNNING), t0() + mins(m), ALERT_AFTER)
    };
    let mut alerted = Vec::new();
    for minute in (5..=95).step_by(5) {
        if !unreadable(&mut holds, minute).is_empty() {
            alerted.push(minute);
        }
    }
    assert_eq!(alerted, [30, 60, 90], "the previous verdict still alerts on the cadence");
    let hold = holds.holds().into_values().next().unwrap();
    assert_eq!(hold.verdict_at, t0(), "no pass has judged it since the first");
    let now = t0() + mins(95);
    let interval = Duration::from_secs(300);
    assert_eq!(
        hold.stale_note(now, interval * 3).as_deref(),
        Some("hold verdict is 95 minutes old")
    );
    assert!(unread_note(&hold, now).contains("could not read the default-branch copy"));
    assert!(unread_note(&hold, now).contains("95 minutes ago"));

    // Read again: the verdict is the latest pass's, and nothing is said.
    holds.step(&[obs(a, w4("0.19.950"), Finding::clear())], v(RUNNING), now, ALERT_AFTER);
    let hold = holds.holds().into_values().next().unwrap();
    assert_eq!((hold.since, hold.verdict_at), (t0(), now));
    assert_eq!(hold.stale_note(now + mins(10), interval * 3), None);
    assert_eq!(unread_note(&hold, now), "");
    // Passes stop altogether: `status` reads the last snapshot later and says so.
    assert_eq!(
        hold.stale_note(now + mins(40), interval * 3).as_deref(),
        Some("hold verdict is 40 minutes old")
    );
}

/// An interrupted resync to a newer release holds dispatch and asks for no
/// roll, on either copy, beside a W4 copy whose demand is untouched by it.
#[test]
fn an_interrupted_newer_resync_is_held_without_a_roll_demand() {
    let d = daemon("0.19.900", None);
    let raw =
        r#"{"loom_version":"0.19.900","requires_daemon":"0.19.772","resync_pending":"0.19.950"}"#;
    let gate = crate::init::payload::gate_metadata(raw, &d);
    assert!(matches!(gate, Err(ResyncRefusal::PendingAheadOfDaemon { .. })), "{gate:?}");
    let verdict = decide_hold(&gate, None);
    assert_eq!(verdict, Verdict::Hold(HoldKind::InstallIncompatible));
    // A pending run at or below this daemon is this daemon's to finish.
    let own = raw.replace("0.19.950", "0.19.900");
    assert_eq!(
        decide_hold(&crate::init::payload::gate_metadata(&own, &d), None),
        Verdict::Clear
    );

    let pending = || Finding::install_incompatible("a resync to 0.19.950 was interrupted".into());
    assert_eq!(pending().demand, None);
    let mut holds = Holds::default();
    let seen = [
        obs("/nonexistent/a", pending(), Finding::clear()),
        obs("/nonexistent/b", Finding::clear(), pending()),
    ];
    let events = holds.step(&seen, d.running, t0(), ALERT_AFTER);
    assert_eq!(events.iter().map(|e| e.event).collect::<Vec<_>>(), ["set", "set"]);
    assert!(events.iter().all(|e| e.demand.is_none()), "{events:?}");
    assert_eq!(holds.holds().len(), 2, "both held");
    assert_eq!(holds.demand(), None, "and no roll demand from either");

    // Beside a real W4, the demand is the W4's and only the W4's.
    let seen = [
        obs("/nonexistent/a", pending(), Finding::clear()),
        obs("/nonexistent/c", w4("0.19.920"), Finding::clear()),
    ];
    holds.step(&seen, d.running, t0() + mins(1), ALERT_AFTER);
    let demand = holds.demand().unwrap();
    assert_eq!(
        (demand.version.as_str(), demand.root),
        ("0.19.920", PathBuf::from("/nonexistent/c"))
    );
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
        verdict_at: t0(),
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
    assert!(held.list(None).is_empty(), "no registry entry");

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

/// The work finder's per-root pre-filter: the held workspace is held with the
/// hold's cause and gets no recovery probe; its sibling is not held by it.
#[tokio::test]
async fn the_work_finder_holds_only_the_held_root_and_names_the_cause() {
    use crate::work_finder::pool_preflight::preflight_held_causes_per_root;
    let held_dir = tempfile::tempdir().unwrap();
    let free_dir = tempfile::tempdir().unwrap();
    let roots = vec![held_dir.path().to_path_buf(), free_dir.path().to_path_buf()];
    let pool = crate::workspace_pool::WorkspacePool::new(
        std::sync::Arc::new(crate::event_bus::EventBus::new()),
        tokio::runtime::Handle::current(),
    );
    set_for_test(held_dir.path(), Some(hold(HoldKind::DaemonTooOld)));
    let (held, causes, probes) = preflight_held_causes_per_root(&pool, &roots, t0());
    assert!(held[0]);
    assert_eq!(causes[0], Some(HaltCause::DaemonTooOld));
    assert!(!probes.contains(&roots[0]), "no recovery probe into a held workspace");
    let hold_causes = [HaltCause::DaemonTooOld, HaltCause::InstallIncompatible];
    assert!(causes[1].is_none_or(|c| !hold_causes.contains(&c)), "{:?}", causes[1]);

    set_for_test(held_dir.path(), Some(hold(HoldKind::InstallIncompatible)));
    let (_, causes, _) = preflight_held_causes_per_root(&pool, &roots, t0());
    assert_eq!(causes[0], Some(HaltCause::InstallIncompatible));

    set_for_test(held_dir.path(), None);
    let (_, causes, _) = preflight_held_causes_per_root(&pool, &roots, t0());
    assert!(causes[0].is_none_or(|c| !hold_causes.contains(&c)), "{:?}", causes[0]);
}
