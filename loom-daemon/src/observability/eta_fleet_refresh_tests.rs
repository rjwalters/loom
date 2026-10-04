//! `observability::eta_fleet_refresh` tests (#10263): spawn gating, the repo
//! set, the backoff and breaker gates, the raw-event budget, the fit hold, the
//! record fields, and the production reader's never-the-writer contract.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::eta::fleet_fetch::{Read, ReadFailure};
use crate::forge_call_stats::ForgeOp;
use chrono::Duration as Span;
use serde_json::json;
use std::cell::RefCell;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 4, 12, 0, 0).unwrap()
}

fn config() -> FleetRefreshConfig {
    FleetRefreshConfig::default()
}

fn reader(app: &str) -> Reader {
    Reader {
        app_id: app.to_string(),
        dir: PathBuf::from(format!("/readers/{app}")),
    }
}

fn target(root: &Path, repo: &str) -> RepoTarget {
    RepoTarget {
        repo: repo.to_string(),
        host: Some("github.com".to_string()),
        cwd: root.to_path_buf(),
        reader: Ok(reader("7")),
    }
}

/// A forge whose listing is one empty page (a quiet repo), optionally failing
/// every call, counting calls.
#[derive(Default)]
struct Quiet {
    calls: usize,
    fail: Option<(ReadFailure, Option<i64>)>,
    breaker: bool,
}

impl ForgeRead for Quiet {
    fn breaker_open(&self) -> bool {
        self.breaker
    }

    fn get(&mut self, _: &RepoTarget, _: &Reader, _: &str, _: Option<&str>, _: ForgeOp) -> Read {
        self.calls += 1;
        match self.fail {
            Some((failure, reset_epoch)) => Read::Failed {
                failure,
                remaining: Some(0),
                reset_epoch,
                detail: "fake".to_string(),
            },
            None => Read::Ok {
                status: 200,
                etag: Some("W/\"q\"".to_string()),
                body: "[]".to_string(),
                remaining: Some(4000),
            },
        }
    }
}

fn no_events() -> impl FnMut(&RepoTarget, &Reader, u64, SyncMode) -> (u64, u64, Option<StopReason>)
{
    |_: &RepoTarget, _: &Reader, _: u64, _: SyncMode| (0, 0, None)
}

// -- gating ---------------------------------------------------------------

#[test]
fn the_task_runs_only_when_eta_and_fleet_refresh_are_both_on() {
    let resolve = |v: serde_json::Value| crate::eta::config::resolve(&v, |_| None);
    assert!(should_spawn(&resolve(json!({}))), "default on");
    assert!(!should_spawn(&resolve(json!({"autonomous": {"eta": {"enabled": false}}}))));
    assert!(!should_spawn(&resolve(
        json!({"autonomous": {"eta": {"fleetRefresh": {"enabled": false}}}})
    )));
    let env_off = crate::eta::config::resolve(&json!({}), |k| {
        (k == "LOOM_ETA_FLEET_REFRESH_ENABLED").then(|| "0".to_string())
    });
    assert!(!should_spawn(&env_off));
    // The either/or with #10245's standalone fit task follows the same gate.
    assert!(owns_fit(&resolve(json!({}))));
    assert!(!owns_fit(&env_off));
}

// -- repo set ---------------------------------------------------------------

#[test]
fn the_repo_set_dedups_prefers_a_provisioned_root_and_adds_snapshot_repos() {
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let provisioned = dir.path().join("checkout-alpha");
    let gitea = dir.path().join("checkout-gitea");
    let mut cached = fleet::FleetSnapshot::empty("acme/cached");
    cached.merge(&[], now());
    fleet::write(&fleet::snapshot_path(&ws, "acme/cached"), &cached).unwrap();

    let identity = |root: &Path| -> Option<(String, String)> {
        let name = root.file_name()?.to_str()?;
        match name {
            "ws" | "checkout-alpha" => Some(("github.com".into(), "Acme/Alpha".into())),
            "checkout-gitea" => Some(("gitea.example.com".into(), "acme/gitea".into())),
            _ => None,
        }
    };
    let readers = |repo: &str, _host: Option<&str>| (repo != "acme/cached").then(|| reader("7"));
    let targets = repo_targets(&ws, &[provisioned.clone(), gitea], identity, readers);
    let repos: Vec<&str> = targets.iter().map(|t| t.repo.as_str()).collect();
    assert_eq!(repos, vec!["Acme/Alpha", "acme/cached", "acme/gitea"]);
    assert_eq!(targets[0].cwd, provisioned, "the repo's own checkout, not the daemon's");
    assert!(targets[0].reader.is_ok());
    assert_eq!(targets[1].reader, Err(NoReader::NoReader));
    assert_eq!(targets[1].cwd, ws);
    assert_eq!(targets[2].reader, Err(NoReader::UnsupportedForge));
}

// -- gates: backoff and breaker -----------------------------------------------

#[test]
fn a_rate_limit_backs_off_until_the_reset_with_zero_calls_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let targets = [target(root, "acme/alpha")];
    let mut task = TaskState::default();
    let reset = now() + Span::hours(3);
    let mut forge = Quiet {
        fail: Some((ReadFailure::RateLimited, Some(reset.timestamp()))),
        ..Quiet::default()
    };
    let mut events = no_events();
    let outcome = cycle(root, &targets, &mut forge, &mut events, &config(), &mut task, now());
    assert_eq!(outcome.report.repos[0].stop, StopReason::RateLimited);
    assert_eq!(task.backoff_until, Some(reset), "the reset is later than one interval");
    assert_eq!(forge.calls, 1);

    forge.fail = None;
    let outcome = cycle(
        root,
        &targets,
        &mut forge,
        &mut events,
        &config(),
        &mut task,
        now() + Span::hours(1),
    );
    assert_eq!(outcome.report.repos[0].stop, StopReason::Backoff);
    assert_eq!(forge.calls, 1, "no call inside the backoff");

    let outcome = cycle(
        root,
        &targets,
        &mut forge,
        &mut events,
        &config(),
        &mut task,
        now() + Span::hours(4),
    );
    assert_eq!(outcome.report.repos[0].stop, StopReason::Complete);
}

#[test]
fn a_rate_limit_without_a_reset_backs_off_one_interval() {
    let dir = tempfile::tempdir().unwrap();
    let mut task = TaskState::default();
    let mut forge = Quiet {
        fail: Some((ReadFailure::RateLimited, None)),
        ..Quiet::default()
    };
    let mut events = no_events();
    cycle(
        dir.path(),
        &[target(dir.path(), "acme/alpha")],
        &mut forge,
        &mut events,
        &config(),
        &mut task,
        now(),
    );
    assert_eq!(task.backoff_until, Some(now() + Span::seconds(3600)));
}

#[test]
fn an_open_breaker_records_every_repo_with_zero_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut b = target(root, "acme/beta");
    b.reader = Err(NoReader::NoReader);
    let mut forge = Quiet {
        breaker: true,
        ..Quiet::default()
    };
    let mut events = no_events();
    let outcome = cycle(
        root,
        &[target(root, "acme/alpha"), b],
        &mut forge,
        &mut events,
        &config(),
        &mut TaskState::default(),
        now(),
    );
    let stops: Vec<StopReason> = outcome.report.repos.iter().map(|r| r.stop).collect();
    assert_eq!(stops, vec![StopReason::BreakerOpen, StopReason::NoReader]);
    assert_eq!(forge.calls, 0);
}

#[test]
fn a_repo_without_a_reader_is_warned_about_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = target(dir.path(), "acme/alpha");
    t.reader = Err(NoReader::NoReader);
    let mut task = TaskState::default();
    let mut events = no_events();
    for _ in 0..3 {
        cycle(
            dir.path(),
            std::slice::from_ref(&t),
            &mut Quiet::default(),
            &mut events,
            &config(),
            &mut task,
            now(),
        );
    }
    assert_eq!(task.warned.len(), 1);
}

// -- raw events -------------------------------------------------------------

#[test]
fn the_event_cache_syncs_after_the_snapshots_from_the_budget_left() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let seen: RefCell<Vec<(String, u64, SyncMode)>> = RefCell::new(Vec::new());
    let mut events = |t: &RepoTarget, _: &Reader, left: u64, mode: SyncMode| {
        seen.borrow_mut().push((t.repo.clone(), left, mode));
        (12, 3, None)
    };
    let config = FleetRefreshConfig {
        backfill_max_calls_per_cycle: 100,
        ..config()
    };
    let mut forge = Quiet::default();
    let outcome = cycle(
        root,
        &[target(root, "acme/alpha")],
        &mut forge,
        &mut events,
        &config,
        &mut TaskState::default(),
        now(),
    );
    // The snapshot backfill spent one call (an empty listing page); the
    // never-backfilled event cache gets the 99 the backfill budget has left.
    assert_eq!(seen.borrow().as_slice(), &[("acme/alpha".to_string(), 99, SyncMode::Backfill)]);
    let r = &outcome.report.repos[0];
    assert_eq!(r.raw_events_added, Some(12));
    assert_eq!(r.forge_calls, 1 + 3);
    assert_eq!(outcome.report.remaining.1, 96);
}

#[test]
fn a_rate_limited_event_sync_backs_the_task_off() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut events =
        |_: &RepoTarget, _: &Reader, _: u64, _: SyncMode| (0, 1, Some(StopReason::RateLimited));
    let mut task = TaskState::default();
    cycle(
        root,
        &[target(root, "acme/alpha")],
        &mut Quiet::default(),
        &mut events,
        &config(),
        &mut task,
        now(),
    );
    assert!(task.backoff_until.is_some_and(|until| until > now()));
}

// -- fit hold -------------------------------------------------------------------

#[test]
fn an_unfinished_backfill_holds_the_fit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut events = no_events();
    let config = FleetRefreshConfig {
        backfill_max_calls_per_cycle: 0,
        ..config()
    };
    // Plant an in-progress backfill begun at `now`.
    let mut state = fleet_refresh::RefreshState::new("acme/alpha");
    state.pass = Some(fleet_refresh::Pass {
        kind: fleet_refresh::PassKind::Backfill,
        listed_at: now(),
        since: now() - Span::days(21),
        next_page: 1,
        done: Vec::new(),
        head_etag: None,
    });
    fleet::write(
        &fleet_refresh::staging_path(root, "acme/alpha"),
        &fleet::FleetSnapshot::empty("acme/alpha"),
    )
    .unwrap();
    fleet_refresh::write_state(&fleet_refresh::state_path(root, "acme/alpha"), &state).unwrap();
    let targets = [target(root, "acme/alpha")];
    let mut task = TaskState::default();
    let held = cycle(
        root,
        &targets,
        &mut Quiet::default(),
        &mut events,
        &config,
        &mut task,
        now() + Span::hours(2),
    );
    assert!(held.fit_held);
    let free = cycle(
        root,
        &targets,
        &mut Quiet::default(),
        &mut events,
        &config,
        &mut task,
        now() + Span::hours(7),
    );
    assert!(!free.fit_held, "the hold lapses six hours after the backfill began");
}

// -- records ----------------------------------------------------------------

#[test]
fn one_record_per_repo_with_a_derived_cycle_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut b = target(root, "acme/beta");
    b.reader = Err(NoReader::UnsupportedForge);
    let mut events = no_events();
    let outcome = cycle(
        root,
        &[target(root, "acme/alpha"), b],
        &mut Quiet::default(),
        &mut events,
        &config(),
        &mut TaskState::default(),
        now(),
    );
    let loom = Provenance {
        version: "0.19.683".to_string(),
        revision: "9d8e226ce0123456789abcdef0123456789abcde".to_string(),
        tree_state: "clean".to_string(),
        complete: true,
    };
    let rows = records(&outcome.report, "host-a", outcome.started_at, &loom);
    assert_eq!(rows.len(), 2);
    let again = records(&outcome.report, "host-a", outcome.started_at, &loom);
    assert_eq!(rows[0].cycle_id, again[0].cycle_id, "derived, never random");
    assert_eq!(rows[0].cycle_id, rows[1].cycle_id, "one id per cycle");
    let other_host = records(&outcome.report, "host-b", outcome.started_at, &loom);
    assert_ne!(rows[0].cycle_id, other_host[0].cycle_id);

    let alpha = &rows[0];
    assert_eq!(alpha.repo, "acme/alpha");
    assert_eq!((alpha.pass.as_str(), alpha.stop_reason.as_str()), ("backfill", "complete"));
    assert!(alpha.promoted);
    assert_eq!(alpha.forge_calls, 1);
    assert_eq!(alpha.reader_app.as_deref(), Some("7"));
    assert!(alpha.snapshot_id.is_some() && alpha.as_of == Some(now()));
    assert!(alpha.has_provenance());
    let beta = &rows[1];
    assert_eq!((beta.pass.as_str(), beta.stop_reason.as_str()), ("none", "unsupported_forge"));
    assert_eq!(beta.forge_calls, 0);
}

// -- the production reader -----------------------------------------------------

/// [`ReaderForge`] runs under the reader dir with every token var removed,
/// and a credential failure is returned — never retried on the writer.
#[test]
fn the_production_reader_never_falls_back_to_the_writer() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("calls.log");
    let gh = dir.path().join("gh-reader-stub");
    std::fs::write(
        &gh,
        format!(
            "#!/bin/sh\necho \"cfg=$GH_CONFIG_DIR token=${{GH_TOKEN:-unset}} gtoken=${{GITHUB_TOKEN:-unset}} $*\" >> {}\n\
             printf 'HTTP/2.0 401 Unauthorized\\r\\nX-Ratelimit-Remaining: 4000\\r\\n\\r\\n{{\"message\":\"Bad credentials\"}}'\n\
             echo 'gh: Bad credentials (HTTP 401)' >&2\nexit 1\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut forge = ReaderForge::with_gh_bin(&gh);
    let reader_dir = dir.path().join("reader-cfg");
    let t = RepoTarget {
        repo: "acme/reader-only-10263".to_string(),
        host: Some("github.com".to_string()),
        cwd: dir.path().to_path_buf(),
        reader: Ok(Reader {
            app_id: "test-app-10263".to_string(),
            dir: reader_dir.clone(),
        }),
    };
    let r = t.reader.clone().unwrap();
    let read = forge.get(
        &t,
        &r,
        "repos/acme/reader-only-10263/issues?page=1",
        None,
        crate::forge_call_stats::ops::ISSUE_LIST,
    );
    let Read::Failed { failure, .. } = read else {
        panic!("{read:?}");
    };
    assert_eq!(failure, ReadFailure::RateLimited, "bad credentials are App-wide");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.lines().count(), 1, "exactly one call, no writer retry: {calls}");
    let line = calls.lines().next().unwrap();
    assert!(line.contains(&format!("cfg={}", reader_dir.display())), "{line}");
    assert!(line.contains("token=unset gtoken=unset"), "{line}");
}

// -- the fit, after the cycle (#10245) -------------------------------------------

/// A fresh host: one cycle publishes the snapshot as of now, and that cycle's
/// fit check writes today's coefficient file; the next check is not due.
#[test]
fn a_fresh_hosts_first_cycle_feeds_todays_fit() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut events = no_events();
    let outcome = cycle(
        root,
        &[target(root, "acme/alpha")],
        &mut Quiet::default(),
        &mut events,
        &config(),
        &mut TaskState::default(),
        now(),
    );
    assert_eq!(outcome.report.repos[0].stop, StopReason::Complete);
    assert!(!outcome.fit_held);
    let fitter = run::current_fitter();
    let FitCheck::Wrote(path) = after_cycle(root, now(), outcome.fit_held, true, &fitter) else {
        panic!("today's fit is due on a snapshot as of now");
    };
    assert!(path.exists());
    assert_eq!(
        path,
        crate::eta::fit::coeffs::fit_dir(root)
            .join(crate::eta::fit::coeffs::path_for(run::midnight(now())))
    );
    assert_eq!(
        after_cycle(root, now() + Span::hours(1), false, true, &fitter),
        FitCheck::NotDue
    );
}

#[test]
fn the_fit_check_honours_fit_enabled_and_the_hold() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut snapshot = fleet::FleetSnapshot::empty("acme/alpha");
    snapshot.merge(&[], now());
    fleet::write(&fleet::snapshot_path(root, "acme/alpha"), &snapshot).unwrap();
    let fitter = run::current_fitter();
    assert_eq!(after_cycle(root, now(), false, false, &fitter), FitCheck::Disabled);
    assert_eq!(after_cycle(root, now(), true, true, &fitter), FitCheck::Held);
    assert!(
        !crate::eta::fit::coeffs::fit_dir(root).exists(),
        "neither a disabled nor a held check writes anything"
    );
    assert!(matches!(after_cycle(root, now(), false, true, &fitter), FitCheck::Wrote(_)));
}
