//! `loom-daemon status`'s session-container section against fake inputs: no
//! docker, no daemon (#10600).

use super::*;
use crate::session_reconcile::AccountOutcome;
use crate::tokens_pool::session_lifecycle::{
    profile_control_destination, PROFILE_CONTROLS, SESSION_POSTURE, SESSION_POSTURE_LABEL,
    WORKSPACE_LABEL,
};
use crate::tokens_pool::session_state::{Denials, DriftInputs, EffectiveDrift};
use serde_json::{json, Value};
use std::path::Path;

/// A canonical temp workspace with real repository dirs
/// (`workspace_mount_roots` only counts roots that exist).
struct Ws {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Ws {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for repo in ["a", "b", "old", "secret", "x"] {
            std::fs::create_dir_all(root.join(repo)).unwrap();
        }
        Self { _dir: dir, root }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn roots(&self, rels: &[&str]) -> Vec<PathBuf> {
        rels.iter().map(|r| self.p(r)).collect()
    }

    /// `<ws>/secret` is denied (as if `firewall: true`), whatever is
    /// registered.
    fn denials(&self) -> Denials {
        Denials {
            home: None,
            firewalled: vec![self.p("secret")],
        }
    }
}

fn seat(account: &str) -> Seat {
    Seat {
        account: account.into(),
        container: format!("loom-codex-session-{account}"),
        profiles: vec![PathBuf::from(format!("/profiles/{account}"))],
    }
}

/// A hardened host-mode container mounting `mounts` under `ws`.
fn container(ws: &Ws, account: &str, running: bool, restarting: bool, mounts: &[&str]) -> Value {
    let mut binds: Vec<Value> = mounts
        .iter()
        .map(|m| json!({"Type": "bind", "Destination": ws.p(m), "RW": true}))
        .collect();
    binds.extend(PROFILE_CONTROLS.iter().map(|name| {
        json!({"Type": "bind", "Destination": profile_control_destination(name), "RW": false})
    }));
    json!({
        "Name": format!("/loom-codex-session-{account}"),
        "Id": format!("id-{account}"),
        "State": {"Running": running, "Restarting": restarting},
        "Config": {"Labels": {WORKSPACE_LABEL: ws.root, SESSION_POSTURE_LABEL: SESSION_POSTURE}},
        "HostConfig": {
            "Privileged": false,
            "CapDrop": ["ALL"],
            "SecurityOpt": ["no-new-privileges"],
        },
        "Mounts": binds,
    })
}

fn snapshot(ws: &Ws, objects: Vec<Value>, registered: &[PathBuf]) -> Snapshot {
    let denials = |_: &Path| Ok(ws.denials());
    let inputs = DriftInputs {
        registered: Some(registered),
        denials_for: &denials,
    };
    Snapshot::Available(
        objects
            .into_iter()
            .map(|o| {
                let name = o["Name"]
                    .as_str()
                    .unwrap()
                    .trim_start_matches('/')
                    .to_string();
                (name, Observed::of(o, &inputs))
            })
            .collect(),
    )
}

struct Fixture {
    secret: PathBuf,
    seats: Vec<Seat>,
    snapshot: Option<(Duration, Snapshot)>,
    actions: BTreeMap<String, LastReconcile>,
    held: Vec<String>,
    removed: Vec<String>,
}

impl Fixture {
    fn new(ws: &Ws, accounts: &[&str], snapshot: Option<(Duration, Snapshot)>) -> Self {
        Self {
            secret: ws.p("secret"),
            seats: accounts.iter().map(|a| seat(a)).collect(),
            snapshot,
            actions: BTreeMap::new(),
            held: Vec::new(),
            removed: Vec::new(),
        }
    }

    fn build(&self) -> Option<SessionContainersReport> {
        let in_list = |list: &[String], profiles: &[PathBuf]| {
            profiles.iter().any(|p| list.iter().any(|a| p.ends_with(a)))
        };
        let held = |profiles: &[PathBuf]| in_list(&self.held, profiles);
        let removal = |profiles: &[PathBuf]| {
            in_list(&self.removed, profiles).then(|| DriftRemoval {
                schema_version: 1,
                workspace: PathBuf::from("/ws"),
                denied: vec![self.secret.clone()],
                reason: "refused".into(),
                removed_at_unix_ms: 1,
            })
        };
        build(&Inputs {
            seats: &self.seats,
            snapshot: self.snapshot.as_ref().map(|(age, s)| (*age, s)),
            actions: &self.actions,
            reconciler_enabled: Some(true),
            held: &held,
            removal: &removal,
        })
    }
}

fn fresh(ws: &Ws, objects: Vec<Value>) -> Option<(Duration, Snapshot)> {
    Some((Duration::from_secs(5), snapshot(ws, objects, &ws.roots(&["a"]))))
}

fn seat_of<'r>(report: &'r SessionContainersReport, account: &str) -> &'r SeatStatus {
    report
        .accounts
        .iter()
        .find(|s| s.account == account)
        .unwrap()
}

#[test]
fn a_host_without_seats_has_no_section() {
    let ws = Ws::new();
    assert!(Fixture::new(&ws, &[], fresh(&ws, vec![])).build().is_none());
}

#[test]
fn running_with_current_mounts_is_healthy() {
    let ws = Ws::new();
    let up = container(&ws, "a1", true, false, &["a"]);
    let report = Fixture::new(&ws, &["a1"], fresh(&ws, vec![up]))
        .build()
        .unwrap();
    assert!(!report.degraded, "{report:?}");
    assert_eq!(report.observation, "available");
    let s = seat_of(&report, "a1");
    assert_eq!((s.state.as_str(), s.mounts.verdict.as_str()), ("running", "ok"));
    assert_eq!(s.posture, "host");
    assert_eq!(s.describe(), "running, mounts ok, posture host, last reconcile: none");
}

#[test]
fn stopped_missing_and_restarting_degrade_naming_the_account() {
    let ws = Ws::new();
    let objects = vec![
        container(&ws, "up", true, false, &["a"]),
        container(&ws, "stopped", false, false, &["a"]),
        container(&ws, "looping", true, true, &["a"]),
    ];
    let report = Fixture::new(&ws, &["up", "stopped", "gone", "looping"], fresh(&ws, objects))
        .build()
        .unwrap();
    assert_eq!(seat_of(&report, "stopped").state, "stopped");
    assert_eq!(seat_of(&report, "gone").state, "missing");
    assert_eq!(seat_of(&report, "looping").state, "restarting");
    assert_eq!(seat_of(&report, "stopped").posture, "unverified");
    assert!(report.degraded);
    assert_eq!(
        report.degraded_reason.as_deref(),
        Some(
            "3 of 4 session seat(s) not serving: stopped stopped; gone missing; looping \
             restarting"
        )
    );
}

#[test]
fn mount_stale_shows_missing_extra_and_denied_separately() {
    let ws = Ws::new();
    // Registered: a, b (b not mounted), secret (registered but denied).
    // Mounted: a, old (deregistered), secret.
    let registered = ws.roots(&["a", "b", "secret"]);
    let object = container(&ws, "s", true, false, &["a", "old", "secret"]);
    let snap = snapshot(&ws, vec![object], &registered);
    let report = Fixture::new(&ws, &["s"], Some((Duration::from_secs(1), snap)))
        .build()
        .unwrap();
    let s = seat_of(&report, "s");
    assert_eq!(s.state, "running");
    assert_eq!(s.mounts.missing, vec![ws.p("b")]);
    assert_eq!(s.mounts.extra, vec![ws.p("old")]);
    assert_eq!(s.mounts.denied, vec![ws.p("secret")]);
    assert_eq!(s.mounts.describe(), "stale (missing 1, extra 1, denied 1)");
    assert_eq!(
        report.degraded_reason.as_deref(),
        Some("1 of 1 session seat(s) not serving: s mounts stale (missing 1, extra 1, denied 1)")
    );
}

#[test]
fn a_firewalled_but_registered_mount_is_stale_not_running() {
    // The case the single definition exists for: registry-only drift sees
    // nothing wrong, the reconciler would remove the container.
    let ws = Ws::new();
    let registered = ws.roots(&["a", "secret"]);
    let object = container(&ws, "f", true, false, &["a", "secret"]);
    assert!(
        crate::tokens_pool::session_state::mount_drift(&object, &registered).is_empty(),
        "registry-only drift sees nothing"
    );
    let snap = snapshot(&ws, vec![object], &registered);
    let Snapshot::Available(map) = &snap else {
        unreachable!()
    };
    assert_eq!(
        map["loom-codex-session-f"].state,
        crate::tokens_pool::session_state::SessionState::StaleMounts,
        "the gauge's state reads stale_mounts too"
    );
    let report = Fixture::new(&ws, &["f"], Some((Duration::from_secs(1), snap)))
        .build()
        .unwrap();
    assert_eq!(seat_of(&report, "f").mounts.verdict, "stale");
    assert_eq!(seat_of(&report, "f").mounts.denied, vec![ws.p("secret")]);
    assert!(report.degraded);
}

#[test]
fn a_removed_seat_reads_removed_not_just_missing() {
    let ws = Ws::new();
    let mut fixture = Fixture::new(&ws, &["r"], fresh(&ws, vec![]));
    fixture.removed = vec!["r".into()];
    let report = fixture.build().unwrap();
    let s = seat_of(&report, "r");
    assert_eq!(s.state, "missing");
    let removed = format!("removed (denied mount: {})", ws.p("secret").display());
    assert_eq!(s.removal.as_ref().unwrap().describe(), removed);
    assert_eq!(
        report.degraded_reason,
        Some(format!("1 of 1 session seat(s) not serving: r {removed}"))
    );
}

#[test]
fn a_held_seat_is_listed_but_does_not_degrade() {
    let ws = Ws::new();
    let down = container(&ws, "h", false, false, &["a"]);
    let mut fixture = Fixture::new(&ws, &["h"], fresh(&ws, vec![down]));
    fixture.held = vec!["h".into()];
    let report = fixture.build().unwrap();
    assert!(!report.degraded, "an operator stop is deliberate");
    assert!(seat_of(&report, "h").held);
    assert!(seat_of(&report, "h")
        .describe()
        .contains("held (operator stop)"));
}

#[test]
fn deferred_shows_the_last_reconcile_action() {
    let ws = Ws::new();
    let drifted = container(&ws, "d", true, false, &["a", "x"]);
    let mut fixture = Fixture::new(&ws, &["d"], fresh(&ws, vec![drifted]));
    fold_pass(
        &mut fixture.actions,
        &[AccountOutcome {
            name: "d".into(),
            container: "loom-codex-session-d".into(),
            outcome: Outcome::DriftDeferred {
                reason: DeferReason::Busy,
            },
        }],
        100,
    );
    let report = fixture.build().unwrap();
    let s = seat_of(&report, "d");
    assert_eq!(s.mounts.describe(), "stale (missing 0, extra 1, denied 0)");
    let last = s.last_reconcile.clone().unwrap();
    assert_eq!(last.action, "deferred (in-flight)");
    assert_eq!(last.at_unix, 100);
}

#[test]
fn no_snapshot_or_a_stale_one_is_unavailable_never_a_state() {
    let ws = Ws::new();
    let old = snapshot(&ws, vec![], &ws.roots(&["a"]));
    for (snapshot, why) in [
        (None, "no container snapshot has been published yet"),
        (
            Some((LATEST_MAX_AGE + Duration::from_secs(1), old)),
            "the newest container snapshot is 121s old",
        ),
        (
            Some((Duration::from_secs(1), Snapshot::Unavailable("timed out".into()))),
            "docker could not be queried: timed out",
        ),
    ] {
        let report = Fixture::new(&ws, &["a1"], snapshot).build().unwrap();
        assert_eq!(report.observation, "unavailable");
        assert!(
            report
                .unavailable_reason
                .as_deref()
                .unwrap()
                .starts_with(why),
            "{report:?}"
        );
        assert_eq!(seat_of(&report, "a1").state, "unavailable");
        assert!(!seat_of(&report, "a1").degraded, "no state is claimed");
        assert!(report.degraded, "selection is blind: degraded");
        assert!(report
            .degraded_reason
            .as_deref()
            .unwrap()
            .starts_with("session containers unobservable: "));
    }
}

#[test]
fn an_unreadable_registry_gives_no_verdict_never_stale() {
    let ws = Ws::new();
    let denials = |_: &Path| Ok(ws.denials());
    let inputs = DriftInputs {
        registered: None,
        denials_for: &denials,
    };
    let object = container(&ws, "u", true, false, &["a", "secret"]);
    let snap = Snapshot::Available(BTreeMap::from([(
        "loom-codex-session-u".to_string(),
        Observed::of(object, &inputs),
    )]));
    let report = Fixture::new(&ws, &["u"], Some((Duration::from_secs(1), snap)))
        .build()
        .unwrap();
    assert_eq!(seat_of(&report, "u").state, "running");
    assert_eq!(seat_of(&report, "u").mounts.verdict, "unknown");
    assert!(!report.degraded);
}

#[test]
fn an_unreadable_roster_denies_nothing() {
    // Unknown is not denied: the registered `secret` mount is fine.
    let ws = Ws::new();
    let unreadable = |_: &Path| -> anyhow::Result<Denials> { anyhow::bail!("roster unreadable") };
    let registered = ws.roots(&["a", "secret"]);
    let inputs = DriftInputs {
        registered: Some(&registered),
        denials_for: &unreadable,
    };
    let observed = Observed::of(container(&ws, "r", true, false, &["a", "secret"]), &inputs);
    assert_eq!(observed.drift, Some(EffectiveDrift::default()));
    assert_eq!(observed.state, crate::tokens_pool::session_state::SessionState::Running);
}

#[test]
fn backoff_keeps_the_failure_that_caused_it() {
    let mut actions = BTreeMap::new();
    let pass = |outcome: Outcome| {
        vec![AccountOutcome {
            name: "a".into(),
            container: "loom-codex-session-a".into(),
            outcome,
        }]
    };
    fold_pass(
        &mut actions,
        &pass(Outcome::Failed {
            error: "docker run: no such image\nmore".into(),
            retry_at: 0,
        }),
        10,
    );
    fold_pass(&mut actions, &pass(Outcome::BackingOff { retry_at: 0 }), 20);
    assert_eq!(
        actions["a"].action,
        "failed; backoff until 1970-01-01T00:00:00Z: docker run: no such image"
    );
    assert_eq!(actions["a"].at_unix, 10);
    fold_pass(&mut actions, &pass(Outcome::Running), 30);
    assert_eq!(actions["a"].at_unix, 10, "a healthy pass keeps the last action");
    fold_pass(&mut actions, &pass(Outcome::Resumed), 40);
    assert_eq!(actions["a"].action, "started");
    fold_pass(&mut actions, &pass(Outcome::BackingOff { retry_at: 7_200 }), 50);
    assert_eq!(actions["a"].action, "backoff until 1970-01-01T02:00:00Z");
}

#[test]
fn the_report_round_trips_as_json() {
    let ws = Ws::new();
    let mut fixture = Fixture::new(&ws, &["r"], fresh(&ws, vec![]));
    fixture.removed = vec!["r".into()];
    let report = fixture.build().unwrap();
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["accounts"][0]["state"], "missing");
    assert_eq!(
        json["accounts"][0]["removal"]["denied"][0],
        ws.p("secret").display().to_string()
    );
    assert_eq!(json["degraded"], true);
    let back: SessionContainersReport = serde_json::from_value(json).unwrap();
    assert_eq!(back, report);
}
