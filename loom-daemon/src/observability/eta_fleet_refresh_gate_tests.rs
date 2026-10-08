//! `observability::eta_fleet_refresh` (#10329): one refresher fleet-wide. The
//! per-tick `fleet.captain` gate (armed / standing down / fail-open with no
//! captain), and the reserve keyed per reader installation in the raw-event
//! sync.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::eta::fleet_fetch::Read;
use crate::fleet_captain::{armed_singleton_job_names, captainless_singleton_job_names};
use crate::forge_call_stats::ForgeOp;
use std::cell::{Cell, RefCell};
use std::sync::PoisonError;

/// The armed-singleton registry is process-global, and every test here arms
/// or disarms the same job name: run them one at a time.
static REGISTRY: Mutex<()> = Mutex::new(());

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()
}

fn reader(app: &str) -> Reader {
    Reader {
        app_id: app.to_string(),
        dir: PathBuf::from(format!("/readers/{app}")),
    }
}

fn target(root: &Path, repo: &str, app: &str) -> RepoTarget {
    RepoTarget {
        repo: repo.to_string(),
        host: Some("github.com".to_string()),
        cwd: root.to_path_buf(),
        reader: Ok(reader(app)),
    }
}

/// `.loom/config.json` under `root`, (re)written with `captain` (or none).
fn declare(root: &Path, captain: Option<&str>) {
    let path = root.join(crate::config_resolver::LEGACY_CONFIG_REL);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let config = captain
        .map_or_else(|| serde_json::json!({}), |c| serde_json::json!({"fleet": {"captain": c}}));
    std::fs::write(path, config.to_string()).unwrap();
}

/// A quiet forge (one empty listing page per repo) that counts its calls.
#[derive(Default)]
struct Counting {
    calls: usize,
}

impl ForgeRead for Counting {
    fn breaker_open(&self) -> bool {
        false
    }

    fn get(&mut self, _: &RepoTarget, _: &Reader, _: &str, _: Option<&str>, _: ForgeOp) -> Read {
        self.calls += 1;
        Read::Ok {
            status: 200,
            etag: Some("W/\"q\"".to_string()),
            body: "[]".to_string(),
            remaining: Some(4000),
        }
    }
}

/// One tick as `host` over `acme/alpha`: `(tick, forge calls, event syncs)`.
fn run_tick(root: &Path, host: &str, task: &mut TaskState) -> (Tick, usize, usize) {
    let mut forge = Counting::default();
    let synced = Cell::new(0);
    let mut events = |_: &RepoTarget, _: &Reader, _: ForgeEndpoint, _: u64, _: SyncMode| {
        synced.set(synced.get() + 1);
        (0, 0, None)
    };
    let targets = [target(root, "acme/alpha", "7")];
    let config = FleetRefreshConfig::default();
    let tick = tick(root, host, task, now(), |task| {
        cycle(root, &targets, &mut forge, &mut events, &config, task, now())
    });
    (tick, forge.calls, synced.get())
}

fn armed() -> bool {
    armed_singleton_job_names().contains(&SINGLETON_JOB_NAME.to_string())
}

// -- the captain gate ----------------------------------------------------------

#[test]
fn a_non_captain_makes_no_forge_call_and_no_record_but_still_fits() {
    let _serial = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    declare(root, Some("host-b"));
    // The captain's snapshot, as a host sharing its snapshot directory sees it.
    let mut snapshot = fleet::FleetSnapshot::empty("acme/alpha");
    snapshot.merge(&[], now());
    fleet::write(&fleet::snapshot_path(root, "acme/alpha"), &snapshot).unwrap();

    let (tick, calls, synced) = run_tick(root, "host-a", &mut TaskState::default());
    assert_eq!(
        tick.gate,
        RefreshGate::StandDown {
            owner: "host-b".to_string()
        }
    );
    assert_eq!((calls, synced), (0, 0), "no snapshot, backfill or raw-event call");
    assert!(tick.outcome.is_none(), "no cycle report, so no eta.fleet_refresh record");
    assert!(!tick.fit_held);
    assert!(!armed());
    let fitter = run::current_fitter();
    assert!(
        matches!(after_cycle(root, now(), tick.fit_held, true, &fitter), FitCheck::Wrote(_)),
        "refit_if_due still runs on the snapshots this host has"
    );
}

#[test]
fn the_captain_arms_the_job_and_runs_the_cycle_unchanged() {
    let _serial = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    declare(root, Some("host-a"));
    let (tick, calls, synced) = run_tick(root, "host-a", &mut TaskState::default());
    assert_eq!(tick.gate, RefreshGate::Captain);
    assert!(armed(), "listed in host.health.armed_singleton_jobs");
    let outcome = tick.outcome.expect("the captain refreshes");
    assert_eq!(outcome.report.repos[0].stop, StopReason::Complete);
    assert!(calls > 0 && synced > 0);
    crate::fleet_captain::disarm_singleton_job(SINGLETON_JOB_NAME);
}

#[test]
fn no_captain_fails_open_unarmed_and_logs_the_hint_once() {
    let _serial = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    declare(root, None);
    let mut task = TaskState::default();
    let (tick, calls, _) = run_tick(root, "host-a", &mut task);
    assert_eq!(tick.gate, RefreshGate::NoCaptain);
    assert!(tick.outcome.is_some() && calls > 0, "a single-host install keeps its refresh");
    assert!(!armed(), "fail-open is not an arm");
    assert!(
        !captainless_singleton_job_names().contains(&SINGLETON_JOB_NAME.to_string()),
        "and not a captainless refusal either: the job runs"
    );
    // The hint is logged when the gate changes, and it has not.
    assert_eq!(task.gate, Some(RefreshGate::NoCaptain));
    let (again, _, _) = run_tick(root, "host-a", &mut task);
    assert_eq!(again.gate, RefreshGate::NoCaptain);
}

#[test]
fn a_captain_edit_takes_effect_on_the_next_tick() {
    let _serial = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut task = TaskState::default();
    declare(root, Some("host-b"));
    let (first, calls, _) = run_tick(root, "host-a", &mut task);
    assert!(!first.gate.refreshes() && calls == 0);

    declare(root, Some("host-a"));
    let (second, calls, _) = run_tick(root, "host-a", &mut task);
    assert_eq!(second.gate, RefreshGate::Captain);
    assert!(calls > 0 && armed());

    declare(root, Some("host-b"));
    let (third, calls, _) = run_tick(root, "host-a", &mut task);
    assert!(!third.gate.refreshes() && calls == 0);
    assert!(!armed(), "a lost captaincy disarms within one tick");
}

#[test]
fn a_standing_down_host_honours_a_backfill_hold_beside_its_snapshots() {
    let _serial = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    declare(root, Some("host-b"));
    assert_eq!(backfill_in_progress_since(root), None);
    // The captain pended a backfill in the shared directory at `now`.
    fleet_refresh::pend_backfill(root, "acme/alpha", None, now(), 21, StopReason::Budget).unwrap();
    assert_eq!(backfill_in_progress_since(root), Some(now()));
    let (tick, calls, _) = run_tick(root, "host-a", &mut TaskState::default());
    assert_eq!(calls, 0);
    assert!(tick.fit_held, "the fit waits for the captain's backfill, as on the captain");
}

// -- the explicit ETA authority (#10918) ----------------------------------------

/// `.loom/config.json` under `root`, (re)written as `config`.
fn configure(root: &Path, config: &serde_json::Value) {
    let path = root.join(crate::config_resolver::LEGACY_CONFIG_REL);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, config.to_string()).unwrap();
}

#[test]
fn the_explicit_authority_refreshes_and_the_captain_stands_down() {
    let _serial = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    configure(root, &serde_json::json!({"fleet": {"captain": "cap", "etaAuthority": "w1"}}));

    let (captain, calls, synced) = run_tick(root, "cap", &mut TaskState::default());
    assert_eq!(
        captain.gate,
        RefreshGate::StandDown {
            owner: "w1".to_string()
        }
    );
    assert_eq!((calls, synced), (0, 0), "the captain makes no forge call");
    assert!(captain.outcome.is_none(), "and no eta.fleet_refresh record");
    assert!(!armed(), "not listed on the captain");
    assert_eq!(cycle_state(&captain, now(), 3600).captain.as_deref(), Some("w1"));

    let (authority, calls, synced) = run_tick(root, "w1", &mut TaskState::default());
    assert_eq!(authority.gate, RefreshGate::Authority);
    assert!(calls > 0 && synced > 0, "the authority makes the cycle's forge calls");
    assert!(authority.outcome.is_some());
    assert!(armed(), "host.health.armed_singleton_jobs lists it on the authority");
    assert!(!captainless_singleton_job_names().contains(&SINGLETON_JOB_NAME.to_string()));
    assert_eq!(cycle_state(&authority, now(), 3600).gate, "authority");

    // Every other host stands down too: one refresher fleet-wide.
    let (other, calls, _) = run_tick(root, "w2", &mut TaskState::default());
    assert!(!other.gate.refreshes() && calls == 0);
    assert!(!armed(), "a stand-down tick disarms");
    crate::fleet_captain::disarm_singleton_job(SINGLETON_JOB_NAME);
}

#[test]
fn the_authority_overrides_its_own_fleet_refresh_off_and_keeps_its_fit_inputs_current() {
    let _serial = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // fleet-gitops' worker shape: refresh off in this host's config.
    configure(
        root,
        &serde_json::json!({
            "fleet": {"captain": "cap", "etaAuthority": "w1"},
            "autonomous": {"eta": {"fleetRefresh": {"enabled": false}}}
        }),
    );
    // A stale snapshot, three days old: what the authority had been fitting on.
    let stale = now() - chrono::Duration::days(3);
    let mut snapshot = fleet::FleetSnapshot::empty("acme/alpha");
    snapshot.merge(&[], stale);
    fleet::write(&fleet::snapshot_path(root, "acme/alpha"), &snapshot).unwrap();

    let (tick, calls, _) = run_tick(root, "w1", &mut TaskState::default());
    assert_eq!(tick.gate, RefreshGate::Authority);
    assert!(calls > 0);
    let as_of = fleet::read(&fleet::snapshot_path(root, "acme/alpha"))
        .unwrap()
        .as_of;
    assert!(as_of > stale, "one cycle advances the snapshot: {as_of}");
    let fitter = run::current_fitter();
    assert!(
        matches!(after_cycle(root, now(), tick.fit_held, true, &fitter), FitCheck::Wrote(_)),
        "the fit reads `written`, not no_snapshots or stale"
    );
    crate::fleet_captain::disarm_singleton_job(SINGLETON_JOB_NAME);

    // A host that is not the authority keeps its config `false`.
    let (worker, calls, _) = run_tick(root, "w2", &mut TaskState::default());
    assert_eq!(worker.gate, RefreshGate::Disabled);
    assert_eq!(calls, 0);
    assert_eq!(cycle_state(&worker, now(), 3600).gate, "disabled");
}

#[test]
fn the_gate_is_pure_over_the_owner_and_the_hosts_switch() {
    use crate::eta::job_owner::resolve;
    let gate = |explicit, captain, host, on| decide(&resolve(explicit, captain, host), on);
    // No explicit authority: exactly the captain gate, as before.
    assert_eq!(gate(None, Some("cap"), "cap", true), RefreshGate::Captain);
    assert_eq!(
        gate(None, Some("cap"), "w1", true),
        RefreshGate::StandDown {
            owner: "cap".into()
        }
    );
    assert_eq!(gate(None, None, "w1", true), RefreshGate::NoCaptain);
    assert_eq!(gate(None, Some("cap"), "cap", false), RefreshGate::Disabled);
    // The authority is the captain: unchanged in effect, it refreshes.
    assert_eq!(gate(Some("cap"), Some("cap"), "cap", true), RefreshGate::Authority);
    // The authority refreshes even with its own switch off; nobody else does.
    assert_eq!(gate(Some("w1"), Some("cap"), "w1", false), RefreshGate::Authority);
    assert_eq!(gate(Some("w1"), Some("cap"), "cap", false), RefreshGate::Disabled);
    let names: Vec<&str> = [
        RefreshGate::Captain,
        RefreshGate::Authority,
        RefreshGate::NoCaptain,
        RefreshGate::StandDown { owner: "x".into() },
        RefreshGate::Disabled,
    ]
    .iter()
    .map(RefreshGate::as_str)
    .collect();
    assert_eq!(
        names,
        [
            "captain",
            "authority",
            "no_captain",
            "stand_down",
            "disabled"
        ]
    );
}

#[test]
fn moving_the_authority_takes_effect_on_the_next_tick() {
    let _serial = REGISTRY.lock().unwrap_or_else(PoisonError::into_inner);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut task = TaskState::default();
    configure(root, &serde_json::json!({"fleet": {"captain": "cap"}}));
    let (first, _, _) = run_tick(root, "cap", &mut task);
    assert_eq!(first.gate, RefreshGate::Captain);
    configure(root, &serde_json::json!({"fleet": {"captain": "cap", "etaAuthority": "w1"}}));
    let (second, calls, _) = run_tick(root, "cap", &mut task);
    assert!(!second.gate.refreshes() && calls == 0);
    assert!(!armed(), "the captain disarms within one tick, with no restart");
}

// -- the reserve, per installation (raw events) ---------------------------------

#[test]
fn an_event_sync_reserve_skips_that_installation_not_the_apps_other_owners() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let asked: RefCell<Vec<(String, ForgeEndpoint)>> = RefCell::new(Vec::new());
    let mut events = |t: &RepoTarget, _: &Reader, e: ForgeEndpoint, _: u64, _: SyncMode| {
        asked.borrow_mut().push((t.repo.clone(), e));
        let stop = (t.repo == "acme/alpha").then_some(StopReason::Reserve);
        (1, 1, stop)
    };
    // One App (7), two owners: two installations, two buckets.
    let targets = [
        target(root, "acme/alpha", "7"),
        target(root, "Acme/beta", "7"),
        target(root, "other/gamma", "7"),
    ];
    let outcome = cycle(
        root,
        &targets,
        &mut Counting::default(),
        &mut events,
        &FleetRefreshConfig::default(),
        &mut TaskState::default(),
        now(),
    );
    let gamma = "other/gamma".to_string();
    assert_eq!(
        *asked.borrow(),
        vec![
            ("acme/alpha".to_string(), ForgeEndpoint::IssuesEvents),
            (gamma.clone(), ForgeEndpoint::IssuesEvents),
            (gamma, ForgeEndpoint::Pulls),
        ],
        "beta shares (7, acme) — the owner match is case-insensitive; gamma is (7, other)"
    );
    assert_eq!(outcome.report.repos[1].raw_events_added, None);
    assert_eq!(outcome.report.repos[2].raw_events_added, Some(2));
}
