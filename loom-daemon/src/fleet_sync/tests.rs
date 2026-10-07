//! Tests for the startup + timer fleet-store sync (#9596).
//!
//! Every forge interaction goes through the injected [`Transport`] seam
//! (`fleet_store::test_support::FakeForge`) — no network, no `gh`, no daemon.
//! Config tiers are rendered into a `tempfile::TempDir`, and the roster's
//! registry/clone lookups are injected closures, so nothing here touches the
//! host's real `~/.loom`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;

use super::*;
use crate::fleet_store::render::Tier;
use crate::fleet_store::test_support::{location, sample_files, FakeForge};

fn env_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    move |k: &str| map.get(k).cloned()
}

fn no_env(_: &str) -> Option<String> {
    None
}

/// Machine + local tier destinations inside a throwaway directory.
fn tier_paths(dir: &Path) -> (PathBuf, PathBuf) {
    (dir.join("defaults.json"), dir.join(".loom-local/local.json"))
}

// ------------------------------------------------------------------------
// Configuration
// ------------------------------------------------------------------------

#[test]
fn unset_fleet_repo_is_off() {
    // The whole feature's default: nothing configured -> `Ok(None)`, which is
    // what keeps a non-opted-in host byte-identical.
    let cfg = resolve_config(&json!({}), &no_env).expect("resolve");
    assert_eq!(cfg, None);
}

#[test]
fn config_repo_enables_with_defaults() {
    let cfg = resolve_config(&json!({"fleet": {"repo": "acme/fleet"}}), &no_env)
        .expect("resolve")
        .expect("enabled");
    assert_eq!(cfg.location.repo, "acme/fleet");
    assert_eq!(cfg.location.reference, "main");
    assert_eq!(cfg.interval.as_secs(), DEFAULT_SYNC_INTERVAL_SECS);
    assert!(!cfg.auto_apply, "autoApply must default off (roster --apply deregisters)");
}

#[test]
fn env_overrides_config_for_interval_and_auto_apply() {
    let config =
        json!({"fleet": {"repo": "acme/fleet", "syncIntervalSecs": 900, "autoApply": false}});
    let env = env_from(&[(SYNC_INTERVAL_ENV, "120"), (AUTO_APPLY_ENV, "1")]);
    let cfg = resolve_config(&config, &env)
        .expect("resolve")
        .expect("enabled");
    assert_eq!(cfg.interval.as_secs(), 120);
    assert!(cfg.auto_apply);
}

#[test]
fn interval_is_clamped_up_to_the_floor() {
    // A typo'd `1` must not turn a conditional 304 into a hot loop.
    let config = json!({"fleet": {"repo": "acme/fleet", "syncIntervalSecs": 1}});
    let cfg = resolve_config(&config, &no_env)
        .expect("resolve")
        .expect("enabled");
    assert_eq!(cfg.interval.as_secs(), MIN_SYNC_INTERVAL_SECS);
}

#[test]
fn non_truthy_auto_apply_env_reads_as_off() {
    let config = json!({"fleet": {"repo": "acme/fleet", "autoApply": true}});
    let env = env_from(&[(AUTO_APPLY_ENV, "0")]);
    let cfg = resolve_config(&config, &env)
        .expect("resolve")
        .expect("enabled");
    assert!(!cfg.auto_apply, "an explicit env 0 must disarm the write path");
}

#[test]
fn startup_timeout_zero_disables_the_cap() {
    assert_eq!(
        resolve_startup_timeout(&no_env),
        Some(Duration::from_secs(DEFAULT_STARTUP_TIMEOUT_SECS))
    );
    let env = env_from(&[(STARTUP_TIMEOUT_ENV, "0")]);
    assert_eq!(resolve_startup_timeout(&env), None);
    let env = env_from(&[(STARTUP_TIMEOUT_ENV, "5")]);
    assert_eq!(resolve_startup_timeout(&env), Some(Duration::from_secs(5)));
}

// ------------------------------------------------------------------------
// The config pass
// ------------------------------------------------------------------------

#[test]
fn startup_write_renders_both_tiers_from_the_store() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let out = tempfile::tempdir().expect("out");
    let (machine, local) = tier_paths(out.path());

    let pass = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Write,
        Utc::now(),
    );

    assert_eq!(pass.error, None);
    assert!(!pass.cached, "a reachable forge is a live load");
    assert!(pass.wrote(), "a first render must write");
    assert_eq!(pass.tiers.len(), 2, "machine + host-local");
    let machine_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&machine).expect("machine tier"))
            .expect("json");
    // deep_merge of fleet/defaults.json with fleet/hosts/build-1/defaults.json.
    assert_eq!(machine_json["autonomous"]["workFinder"]["maxConcurrent"], json!(8));
    assert_eq!(machine_json["autonomous"]["autoUpdate"]["settleSecs"], json!(3600));
    assert_eq!(
        machine_json["autonomous"]["roleRunner"]["roles"],
        json!(["judge", "doctor"]),
        "the host overlay replaces the array wholesale"
    );
    let local_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&local).expect("local tier")).expect("json");
    assert_eq!(local_json["observability"]["enabled"], json!(true));
}

#[test]
fn check_mode_reports_drift_and_writes_nothing() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let out = tempfile::tempdir().expect("out");
    let (machine, local) = tier_paths(out.path());
    std::fs::write(&machine, r#"{"autonomous":{"workFinder":{"maxConcurrent":1}}}"#).expect("seed");

    let pass = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Check,
        Utc::now(),
    );

    assert_eq!(pass.error, None);
    assert!(pass.drifted(), "the seeded file differs from the store");
    assert!(!pass.wrote(), "check mode must never write");
    assert!(!local.exists(), "check mode must not create the local tier either");
    assert_eq!(
        std::fs::read_to_string(&machine).expect("machine tier"),
        r#"{"autonomous":{"workFinder":{"maxConcurrent":1}}}"#,
        "the on-disk tier is byte-identical after a check pass"
    );
    let machine_report = pass
        .tiers
        .iter()
        .find(|t| t.tier == Tier::Machine.name())
        .expect("machine tier report");
    assert!(machine_report.drifted);
    assert!(machine_report
        .detail
        .as_deref()
        .expect("drift detail")
        .contains("maxConcurrent"));
}

#[test]
fn write_mode_refuses_a_lossy_reduction_and_surfaces_it() {
    // 2am#1653's ask, automated-path half: a write pass must apply the same
    // `lost_top_level_keys` guard the manual `fleet-config render` CLI does
    // (loom-daemon/src/cli/fleet_config.rs's `cmd_render`) — there is no
    // operator present on this path to answer an `--allow-reduce` prompt, so
    // the write is skipped outright and the loss is surfaced via
    // `ConfigPass.error` / `TierReport.detail` instead.
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let out = tempfile::tempdir().expect("out");
    let (machine, local) = tier_paths(out.path());
    // `safehouse` is a top-level block the store's render for build-1 never
    // had (sample_files() only ever renders `autonomous`/`forge` there) — the
    // store never had it, so this is the exact clobber 2am#1653 hit.
    std::fs::write(
        &machine,
        r#"{"autonomous":{"workFinder":{"maxConcurrent":1}},"safehouse":{"enabled":true}}"#,
    )
    .expect("seed a file with a block the store doesn't have");

    let pass = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Write,
        Utc::now(),
    );

    assert!(pass.drifted(), "the seeded file differs from the store's render");
    assert_eq!(
        std::fs::read_to_string(&machine).expect("machine tier"),
        r#"{"autonomous":{"workFinder":{"maxConcurrent":1}},"safehouse":{"enabled":true}}"#,
        "the on-disk file must be untouched when the write is refused"
    );
    let err = pass
        .error
        .as_deref()
        .expect("the loss must be surfaced as an error");
    assert!(err.contains("safehouse"), "error names the dropped block: {err}");
    let machine_report = pass
        .tiers
        .iter()
        .find(|t| t.tier == Tier::Machine.name())
        .expect("machine tier report");
    assert!(!machine_report.wrote);
    assert!(machine_report
        .detail
        .as_deref()
        .expect("detail")
        .contains("safehouse"));
}

#[test]
fn a_second_write_pass_is_a_no_op_when_already_in_sync() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let out = tempfile::tempdir().expect("out");
    let (machine, local) = tier_paths(out.path());
    let args = |mode| (mode, Utc::now());

    let (mode, now) = args(Mode::Write);
    let first =
        config_pass(&forge, cache.path(), &location(), "build-1", &machine, &local, mode, now);
    assert!(first.wrote());

    let (mode, now) = args(Mode::Write);
    let second =
        config_pass(&forge, cache.path(), &location(), "build-1", &machine, &local, mode, now);
    assert!(!second.drifted(), "the tiers now match the store");
    assert!(!second.wrote(), "an in-sync render must not churn the file or its backup");
}

#[test]
fn unreachable_forge_at_startup_falls_back_to_the_last_good_snapshot() {
    // The boot-safety constraint: the daemon must still render (from cache)
    // and must not fail, when the forge cannot be reached.
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let out = tempfile::tempdir().expect("out");
    let (machine, local) = tier_paths(out.path());

    // Prime the cache while the forge is up, then take it away.
    let warm = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Write,
        Utc::now(),
    );
    assert_eq!(warm.error, None);
    std::fs::remove_file(&machine).expect("remove the rendered machine tier");
    forge.offline.set(true);

    let pass = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Write,
        Utc::now(),
    );

    assert_eq!(pass.error, None, "an unreachable forge is not a startup failure");
    assert!(pass.cached, "the snapshot came from the cache");
    assert!(pass.wrote(), "the cached snapshot still renders the tier back");
    assert!(machine.exists());
}

#[test]
fn unreachable_forge_with_no_cache_is_recorded_not_raised() {
    let forge = FakeForge::new(sample_files());
    forge.offline.set(true);
    let cache = tempfile::tempdir().expect("cache");
    let out = tempfile::tempdir().expect("out");
    let (machine, local) = tier_paths(out.path());

    let pass = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Write,
        Utc::now(),
    );

    assert!(pass.error.is_some(), "the failure is reported...");
    assert!(pass.tiers.is_empty());
    assert!(!machine.exists(), "...and nothing was invented on disk");
}

#[test]
fn an_unknown_host_is_an_error_not_a_partial_render() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let out = tempfile::tempdir().expect("out");
    let (machine, local) = tier_paths(out.path());

    let pass = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-99",
        &machine,
        &local,
        Mode::Write,
        Utc::now(),
    );

    assert!(pass.error.expect("error").contains("build-99"));
    assert!(!machine.exists());
}

#[test]
fn steady_state_is_one_conditional_request_per_pass() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let out = tempfile::tempdir().expect("out");
    let (machine, local) = tier_paths(out.path());

    let _ = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Check,
        Utc::now(),
    );
    let after_warmup = forge.calls.borrow().len();

    let _ = config_pass(
        &forge,
        cache.path(),
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Check,
        Utc::now(),
    );
    let calls = forge.calls.borrow().clone();
    let second_pass_calls = &calls[after_warmup..];
    assert_eq!(
        second_pass_calls,
        ["repos/acme/fleet/commits/main"],
        "after the first fetch a pass costs exactly one ref revalidation (304): {calls:?}"
    );
}

// ------------------------------------------------------------------------
// The roster pass (fail-closed)
// ------------------------------------------------------------------------

fn registered(path: &str, priority: u32) -> Registered {
    Registered {
        root: PathBuf::from(path),
        priority,
    }
}

fn plan_from(
    forge: &FakeForge,
    cache: &Path,
    registered: &[Registered],
) -> anyhow::Result<crate::fleet_store::roster::Plan> {
    roster_pass(
        forge,
        cache,
        &location(),
        Path::new("/home/op"),
        registered,
        &|p: &Path| p.to_path_buf(),
        &|_: &Path| true,
        Utc::now(),
    )
}

#[test]
fn roster_drift_is_reported_without_applying_by_default() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let plan = plan_from(&forge, cache.path(), &[]).expect("plan");
    let mut applied: Vec<String> = Vec::new();
    let pass = summarize_roster(&plan, false, &mut |c| {
        applied.push(roster::describe(c));
        Ok(())
    });
    assert!(pass.drifted(), "/srv/src/app is desired but not registered");
    assert_eq!(pass.applied, 0);
    assert!(applied.is_empty(), "autoApply off must not call the apply seam at all");
    assert_eq!(pass.error, None);
}

#[test]
fn auto_apply_applies_every_appliable_change() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    // Registered at the wrong priority -> one SetPriority change.
    let plan = plan_from(&forge, cache.path(), &[registered("/srv/src/app", 100)]).expect("plan");
    let mut applied: Vec<String> = Vec::new();
    let pass = summarize_roster(&plan, true, &mut |c| {
        applied.push(roster::describe(c));
        Ok(())
    });
    assert_eq!(pass.applied, 1);
    assert_eq!(pass.unapplied, 0);
    assert_eq!(applied.len(), 1);
    assert!(applied[0].contains("priority"), "{applied:?}");
}

#[test]
fn a_missing_clone_is_counted_unapplied_and_never_applied() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    let plan = roster_pass(
        &forge,
        cache.path(),
        &location(),
        Path::new("/home/op"),
        &[],
        &|p: &Path| p.to_path_buf(),
        &|_: &Path| false, // nothing is cloned
        Utc::now(),
    )
    .expect("plan");
    let mut applied = 0usize;
    let pass = summarize_roster(&plan, true, &mut |_| {
        applied += 1;
        Ok(())
    });
    assert_eq!(applied, 0, "a missing clone is never cloned or registered");
    assert_eq!(pass.unapplied, 1);
    assert_eq!(pass.applied, 0);
}

#[test]
fn roster_fails_closed_when_the_forge_cannot_confirm_it() {
    let forge = FakeForge::new(sample_files());
    let cache = tempfile::tempdir().expect("cache");
    // Warm the cache, then take the forge away: `render` would serve this from
    // cache, the roster must refuse.
    plan_from(&forge, cache.path(), &[]).expect("warm");
    forge.offline.set(true);
    let err = plan_from(&forge, cache.path(), &[]).expect_err("must not serve a cached roster");
    assert!(format!("{err:#}").contains("could not reach the forge"), "{err:#}");
}

#[test]
fn a_both_flags_record_is_a_hard_error_for_the_whole_roster() {
    let mut files = sample_files();
    files.insert(
        "repos.yml".to_string(),
        "root: /srv/src\nrepos:\n  - name: app\n    fleet: true\n    firewall: true\n".to_string(),
    );
    let forge = FakeForge::new(files);
    let cache = tempfile::tempdir().expect("cache");
    let err = plan_from(&forge, cache.path(), &[]).expect_err("both flags must be refused");
    let msg = format!("{err:#}");
    assert!(msg.contains("firewall"), "{msg}");
    assert!(msg.contains("refusing the whole roster"), "{msg}");
}

// ------------------------------------------------------------------------
// The host-level snapshot and its `status` line
// ------------------------------------------------------------------------

fn sample_status() -> FleetSyncStatus {
    FleetSyncStatus {
        repo: "acme/fleet".to_string(),
        reference: "main".to_string(),
        host: "build-1".to_string(),
        pass: "timer".to_string(),
        at: Utc::now(),
        interval_secs: 300,
        auto_apply: false,
        config: ConfigPass::default(),
        roster: RosterPass::default(),
        state: StatePass::default(),
        enforced: Enforcement::Proceed,
        floor: FloorPass::default(),
    }
}

#[test]
fn snapshot_round_trips_through_the_status_file() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join(STATUS_FILENAME);
    assert_eq!(read_status(&path).expect("absent is not an error"), None);

    let mut status = sample_status();
    status.config.commit = Some("0123456789abcdef0123456789abcdef01234567".to_string());
    status.config.tiers.push(TierReport {
        tier: "machine tier".to_string(),
        path: PathBuf::from("/etc/loom/defaults.json"),
        drifted: true,
        wrote: false,
        detail: Some("~ autonomous.workFinder.maxConcurrent: 1 -> 8".to_string()),
    });
    write_status(&path, &status).expect("write");
    assert_eq!(read_status(&path).expect("read"), Some(status));
}

#[test]
fn no_snapshot_renders_no_status_line() {
    // The byte-identical guarantee at the `status` surface: a host that never
    // ran a pass adds nothing to the report.
    assert_eq!(render_line(None, Utc::now()), None);
}

#[test]
fn status_line_names_drift_and_that_nothing_was_written() {
    let now = Utc::now();
    let mut status = sample_status();
    status.at = now - ChronoDuration::seconds(90);
    status.config.commit = Some("abcdef0123456789abcdef0123456789abcdef01".to_string());
    status.config.tiers.push(TierReport {
        tier: "machine tier".to_string(),
        path: PathBuf::from("/etc/loom/defaults.json"),
        drifted: true,
        wrote: false,
        detail: Some("~ autonomous.workFinder.maxConcurrent: 1 -> 8".to_string()),
    });
    status
        .roster
        .drift
        .push("+ add app /srv/src/app".to_string());

    let line = render_line(Some(&status), now).expect("a line");
    assert!(line.contains("acme/fleet @ main"), "{line}");
    assert!(line.contains("abcdef012345"), "the short commit: {line}");
    assert!(line.contains("machine tier: DRIFT"), "{line}");
    assert!(line.contains("roster: DRIFT"), "{line}");
    assert!(line.contains("nothing was written"), "{line}");
    assert!(line.contains("fleet.autoApply"), "the remedy is named: {line}");
}

#[test]
fn status_line_says_so_when_the_host_matches_the_store() {
    let now = Utc::now();
    let mut status = sample_status();
    status.config.commit = Some("abcdef0123456789abcdef0123456789abcdef01".to_string());
    status.config.tiers.push(TierReport {
        tier: "machine tier".to_string(),
        path: PathBuf::from("/etc/loom/defaults.json"),
        drifted: false,
        wrote: false,
        detail: None,
    });
    let line = render_line(Some(&status), now).expect("a line");
    assert!(line.contains("host matches the store"), "{line}");
    assert!(!line.contains("DRIFT"), "{line}");
    assert!(line.contains("roster: in sync"), "{line}");
}

#[test]
fn status_line_distinguishes_a_skipped_roster_from_a_clean_one() {
    let now = Utc::now();
    let mut status = sample_status();
    status.config.cached = true;
    status.roster.skipped = Some("the forge was unreachable this pass".to_string());
    let line = render_line(Some(&status), now).expect("a line");
    assert!(line.contains("CACHED snapshot"), "{line}");
    assert!(line.contains("roster: not checked"), "{line}");
    assert!(!line.contains("roster: in sync"), "a skipped roster is not a clean one: {line}");
}

// ------------------------------------------------------------------------
// Run-state enforcement on the timer pass (#9598)
// ------------------------------------------------------------------------

/// An [`Enforcer`] that records what it was asked to do. `stop_accepts=false`
/// models the one production divergence: a host with no supervisor, where the
/// drain-and-exit is refused and `IpcEnforcer` holds dispatch instead.
#[derive(Default)]
struct RecordingEnforcer {
    calls: std::sync::Mutex<Vec<&'static str>>,
    held: std::sync::Mutex<bool>,
    stop_accepts: bool,
}

impl RecordingEnforcer {
    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }
}

impl Enforcer for RecordingEnforcer {
    fn hold(&self, _note: String) -> bool {
        self.calls.lock().unwrap().push("hold");
        let mut held = self.held.lock().unwrap();
        !std::mem::replace(&mut held, true)
    }

    fn release(&self) -> bool {
        self.calls.lock().unwrap().push("release");
        std::mem::replace(&mut self.held.lock().unwrap(), false)
    }

    fn is_held(&self) -> bool {
        *self.held.lock().unwrap()
    }

    fn stop(&self, _reason: String) -> bool {
        self.calls.lock().unwrap().push("stop");
        if !self.stop_accepts {
            *self.held.lock().unwrap() = true;
        }
        self.stop_accepts
    }
}

fn status_desiring(state: crate::fleet_store::state::RunState) -> FleetSyncStatus {
    let mut status = sample_status();
    status.state = StatePass {
        desired: Some(state),
        source: Some("host".to_string()),
        ..StatePass::default()
    };
    status.enforced = status.state.enforcement();
    status
}

#[test]
fn a_running_store_leaves_an_unheld_host_completely_alone() {
    let enforcer = RecordingEnforcer::default();
    let mut status = status_desiring(crate::fleet_store::state::RunState::Running);
    enforce(&mut status, &enforcer, None);
    assert_eq!(status.enforced, Enforcement::Proceed);
    assert!(enforcer.calls().is_empty(), "no drain primitive was touched");
}

#[test]
fn a_paused_store_holds_once_and_is_silent_thereafter() {
    let enforcer = RecordingEnforcer::default();
    let mut status = status_desiring(crate::fleet_store::state::RunState::Paused);
    enforce(&mut status, &enforcer, None);
    assert_eq!(status.enforced, Enforcement::Hold);
    assert!(enforcer.is_held());

    let mut again = status_desiring(crate::fleet_store::state::RunState::Paused);
    enforce(&mut again, &enforcer, None);
    assert_eq!(again.enforced, Enforcement::Hold);
    assert_eq!(enforcer.calls(), vec!["hold"], "the steady state re-requests nothing");
}

#[test]
fn a_store_flipping_back_to_running_releases_the_fleet_hold() {
    let enforcer = RecordingEnforcer::default();
    let mut paused = status_desiring(crate::fleet_store::state::RunState::Paused);
    enforce(&mut paused, &enforcer, None);

    let mut running = status_desiring(crate::fleet_store::state::RunState::Running);
    enforce(&mut running, &enforcer, None);
    assert_eq!(running.enforced, Enforcement::Proceed);
    assert!(!enforcer.is_held());
    assert_eq!(enforcer.calls(), vec!["hold", "release"]);
}

#[test]
fn a_refused_stop_is_reported_as_the_hold_it_actually_became() {
    let enforcer = RecordingEnforcer::default(); // stop_accepts = false
    let mut status = status_desiring(crate::fleet_store::state::RunState::Stopped);
    assert_eq!(status.enforced, Enforcement::Stop, "the store asked for a stop");
    enforce(&mut status, &enforcer, None);
    assert_eq!(
        status.enforced,
        Enforcement::Hold,
        "the snapshot `status` reads must not claim a stop that did not happen"
    );
}

#[test]
fn an_accepted_stop_stays_a_stop_in_the_snapshot() {
    let enforcer = RecordingEnforcer {
        stop_accepts: true,
        ..RecordingEnforcer::default()
    };
    let mut status = status_desiring(crate::fleet_store::state::RunState::Stopped);
    enforce(&mut status, &enforcer, None);
    assert_eq!(status.enforced, Enforcement::Stop);
    assert_eq!(enforcer.calls(), vec!["stop"]);
}

// ------------------------------------------------------------------------
// The desired-vs-actual `status` lines (#9598)
// ------------------------------------------------------------------------

#[test]
fn the_status_block_pairs_desired_with_actual() {
    let now = Utc::now();
    let mut status = status_desiring(crate::fleet_store::state::RunState::Paused);
    status.state.by = Some("operator".to_string());
    status.state.reason = Some("disk replacement".to_string());
    let line = render_line(Some(&status), now).expect("a line");
    assert!(line.contains("run state: desired paused (host entry)"), "{line}");
    assert!(line.contains("-> new dispatch HELD"), "{line}");
    assert!(line.contains("reason: disk replacement"), "{line}");
}

#[test]
fn the_status_block_names_a_cached_or_last_recorded_answer() {
    let now = Utc::now();
    let mut cached = status_desiring(crate::fleet_store::state::RunState::Stopped);
    cached.state.cached = true;
    let line = render_line(Some(&cached), now).expect("a line");
    assert!(line.contains("from a CACHED snapshot"), "{line}");
    assert!(line.contains("desired stopped"), "{line}");

    let mut recorded = status_desiring(crate::fleet_store::state::RunState::Stopped);
    recorded.state.cached = true;
    recorded.state.from_last_recorded = true;
    let line = render_line(Some(&recorded), now).expect("a line");
    assert!(line.contains("LAST RECORDED state"), "{line}");
}

#[test]
fn a_state_that_could_not_be_read_is_never_silent() {
    let now = Utc::now();
    let mut status = sample_status();
    status.state.error = Some("the store has no fleet/state.yml".to_string());
    let line = render_line(Some(&status), now).expect("a line");
    assert!(line.contains("run state: NOT ENFORCED"), "{line}");

    // …and a host whose store says nothing at all about run state, with no
    // error either, adds no run-state line (pre-#9598 output, byte for byte).
    let quiet = render_line(Some(&sample_status()), now).expect("a line");
    assert!(!quiet.contains("run state"), "{quiet}");
}

// ------------------------------------------------------------------------
// The fleet version floor (#10711)
// ------------------------------------------------------------------------

fn valid(v: &str) -> Result<FloorRead, String> {
    Ok(FloorRead::Valid {
        version: v.to_string(),
        source: "repos.yml",
    })
}

fn malformed() -> Result<FloorRead, String> {
    Ok(FloorRead::Malformed {
        source: "repos.yml",
        detail: "got 1.2".to_string(),
    })
}

#[test]
fn an_absent_floor_is_unset_and_reports_nothing() {
    let pass = resolve_floor(Ok(FloorRead::Absent), Some("0.19.830"));
    assert_eq!(pass, FloorPass::default(), "removing the key clears the floor");
    assert!(pass.is_unset());
}

#[test]
fn a_valid_floor_is_set_with_its_source() {
    let pass = resolve_floor(valid("0.19.831"), Some("0.19.830"));
    assert_eq!(pass.floor.as_deref(), Some("0.19.831"));
    assert_eq!(pass.source.as_deref(), Some("repos.yml"));
    assert!(!pass.carried);
    assert_eq!(pass.error, None);
}

#[test]
fn a_malformed_floor_keeps_the_last_good_one_and_alerts() {
    let pass = resolve_floor(malformed(), Some("0.19.830"));
    assert_eq!(pass.floor.as_deref(), Some("0.19.830"), "never read as no floor");
    assert!(pass.carried);
    let err = pass.error.as_deref().expect("an alert");
    assert!(err.contains("malformed"), "{err}");
    assert!(err.contains("keeping the last good floor 0.19.830"), "{err}");
}

#[test]
fn a_first_ever_malformed_floor_leaves_it_unset_and_alerts() {
    let pass = resolve_floor(malformed(), None);
    assert_eq!(pass.floor, None);
    assert!(!pass.carried);
    assert!(pass.error.is_some());
    assert!(!pass.is_unset(), "the alert is reported");
}

#[test]
fn an_unreadable_snapshot_keeps_the_last_good_floor() {
    let pass = resolve_floor(Err("no cached snapshot".to_string()), Some("0.19.830"));
    assert_eq!(pass.floor.as_deref(), Some("0.19.830"));
    assert!(pass.error.is_some());
}

#[test]
fn the_last_good_floor_survives_across_passes() {
    // valid -> malformed -> malformed -> valid -> absent, threading each
    // pass's result into the next exactly as `run_pass` does.
    let mut last: Option<String> = None;
    let mut step = |read: Result<FloorRead, String>| {
        let pass = resolve_floor(read, last.as_deref());
        last = pass.floor.clone();
        pass
    };
    assert_eq!(step(valid("0.19.830")).floor.as_deref(), Some("0.19.830"));
    assert_eq!(step(malformed()).floor.as_deref(), Some("0.19.830"));
    assert_eq!(step(malformed()).floor.as_deref(), Some("0.19.830"));
    assert_eq!(step(valid("0.19.840")).floor.as_deref(), Some("0.19.840"));
    assert_eq!(step(Ok(FloorRead::Absent)).floor, None);
}

#[test]
fn the_floor_half_reads_the_refreshed_cache_without_a_forge_request() {
    let mut files = sample_files();
    let roster = files.get("repos.yml").cloned().expect("roster");
    files.insert("repos.yml".to_string(), format!("loom_min_version: \"0.19.830\"\n{roster}"));
    let forge = FakeForge::new(files);
    let dir = tempfile::tempdir().expect("dir");
    let (machine, local) = tier_paths(dir.path());
    let cache = dir.path().join("cache");
    config_pass(
        &forge,
        &cache,
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Check,
        Utc::now(),
    );
    forge.calls.borrow_mut().clear();

    let pass = floor_half(&cache, &location(), None, Utc::now());
    assert_eq!(pass.floor.as_deref(), Some("0.19.830"));
    assert_eq!(pass.source.as_deref(), Some("repos.yml"));
    assert!(forge.calls.borrow().is_empty(), "no extra forge request");

    // A store change lands on the next pass with no restart.
    let mut files = forge.files.borrow().clone();
    files.insert("fleet.json".to_string(), r#"{"loom_min_version":"0.19.900"}"#.to_string());
    *forge.files.borrow_mut() = files;
    config_pass(
        &forge,
        &cache,
        &location(),
        "build-1",
        &machine,
        &local,
        Mode::Check,
        Utc::now(),
    );
    let pass = floor_half(&cache, &location(), pass.floor.as_deref(), Utc::now());
    assert_eq!(pass.floor.as_deref(), Some("0.19.900"));
    assert_eq!(pass.source.as_deref(), Some("fleet.json"));
}

#[test]
fn the_floor_half_with_no_cache_keeps_the_last_good_floor() {
    let dir = tempfile::tempdir().expect("dir");
    let pass = floor_half(&dir.path().join("none"), &location(), Some("0.19.830"), Utc::now());
    assert_eq!(pass.floor.as_deref(), Some("0.19.830"));
    assert!(pass.error.is_some());
}

#[test]
fn a_snapshot_without_a_floor_omits_the_field_and_still_round_trips() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join(STATUS_FILENAME);
    let status = sample_status();
    write_status(&path, &status).expect("write");
    let raw = std::fs::read_to_string(&path).expect("raw");
    assert!(!raw.contains("\"floor\""), "no floor, no field: {raw}");
    assert_eq!(read_status(&path).expect("read"), Some(status));
}

#[test]
fn a_pre_floor_snapshot_still_deserializes() {
    let mut value = serde_json::to_value(sample_status()).expect("value");
    value.as_object_mut().expect("object").remove("floor");
    let status: FleetSyncStatus = serde_json::from_value(value).expect("old shape reads");
    assert_eq!(status.floor, FloorPass::default());
}

#[test]
fn a_snapshot_with_a_floor_round_trips() {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join(STATUS_FILENAME);
    let mut status = sample_status();
    status.floor = resolve_floor(malformed(), Some("0.19.830"));
    write_status(&path, &status).expect("write");
    assert_eq!(read_status(&path).expect("read"), Some(status));
}

#[test]
fn status_line_shows_the_floor_and_a_malformed_one_is_an_error() {
    let mut status = sample_status();
    status.floor = resolve_floor(valid("0.19.830"), None);
    let block = render_line(Some(&status), Utc::now()).expect("block");
    assert!(block.contains("loom_min_version: 0.19.830 (from repos.yml)"), "{block}");
    assert!(!status.errored());

    status.floor = resolve_floor(malformed(), Some("0.19.830"));
    let block = render_line(Some(&status), Utc::now()).expect("block");
    assert!(block.contains("LAST GOOD"), "{block}");
    assert!(block.contains("loom_min_version: ERROR"), "{block}");
    assert!(status.errored(), "a malformed floor alerts through the drift topic");
}

#[test]
fn no_floor_leaves_the_status_line_unchanged() {
    let status = sample_status();
    let block = render_line(Some(&status), Utc::now()).expect("block");
    assert!(!block.contains("loom_min_version"), "{block}");
}
