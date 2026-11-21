//! The driver's per-workspace shaping, through the same functions its tick
//! loop calls.

use super::*;
use crate::fleet_dispatch::proto::{HeartbeatRequest, NextRequest};
use crate::step_dispatch::item_draw::Exclusion;
use crate::step_dispatch::tick::tests::{forge, workspace};
use crate::sweep_registry::step_spawn::draw_claims::DrawClaims;
use crate::work_finder::step_item::{PassReason, Step};

fn fresh() -> ListingState {
    ListingState {
        pulls_complete: true,
        stale_listing: false,
        fixes_only: false,
    }
}

fn builders(w: &crate::step_dispatch::item_draw::Weighing) -> usize {
    w.pool.iter().filter(|c| c.step == Step::Builder).count()
}

/// A Builder claim is a blind label flip: a last-good listing, or a failed
/// open-PR listing, draws no Builder step; a fresh listing does (#11909).
#[test]
fn a_stale_or_pr_blind_listing_draws_no_builder_step() {
    let mut fresh_pool = workspace(0, "acme/one", &forge()).weighing;
    assert_eq!(shape_pool(&mut fresh_pool, &DrawClaims::default(), fresh()), 0);
    assert_eq!(builders(&fresh_pool), 1, "#3 is a Builder step");

    for listing in [
        ListingState {
            stale_listing: true,
            ..fresh()
        },
        ListingState {
            pulls_complete: false,
            ..fresh()
        },
    ] {
        let mut w = workspace(0, "acme/one", &forge()).weighing;
        let others = w.pool.len() - builders(&w);
        assert_eq!(shape_pool(&mut w, &DrawClaims::default(), listing), 1, "{listing:?}");
        assert_eq!(builders(&w), 0, "{listing:?}");
        assert_eq!(w.pool.len(), others, "only the Builder step leaves: {listing:?}");
        assert_eq!(w.excluded[&Exclusion::Pass(PassReason::SharedPr)], 1);
    }
}

/// The actionable gauge is the shaped pool's size for a listed repo, and no
/// point at all for one the tick did not list (absent, never a 0).
#[test]
fn an_unlisted_repo_has_no_actionable_point() {
    let mut w = workspace(0, "acme/one", &forge()).weighing;
    let _ = shape_pool(&mut w, &DrawClaims::default(), fresh());
    let point = actionable_point("acme/one", Some(&w)).unwrap();
    assert_eq!(point.name, MetricName::StepActionable);
    let json = serde_json::to_value(&point).unwrap().to_string();
    assert!(json.contains("acme/one"), "{json}");
    assert!(actionable_point("acme/held", None).is_none());
    // A listed repo with nothing drawable is a measured 0, not absent.
    let mut empty = w.clone();
    empty.pool.clear();
    assert!(actionable_point("acme/one", Some(&empty)).is_some());
}

/// #12025: the step path's listing failures reach `loom.dispatch.error_kind`.
/// A failed item listing is a listing failure of its kind; a last-good item
/// listing is an incomplete one; held Builder steps are one `forge_listing`
/// error per repo; a fresh, whole tick records nothing.
#[test]
fn listing_failures_and_held_builders_name_their_error_kind() {
    let mut report = TickReport::default();
    note_listing(&mut report, 0, None, Some(fresh()), 0);
    assert_eq!((report.errors, report.error_kind()), (0, None));
    assert!(report.listing_not_whole().is_empty());

    let down = anyhow::anyhow!("gh: HTTP 502 Bad Gateway");
    note_listing(&mut report, 0, Some(&down), None, 0);
    let stale = ListingState {
        stale_listing: true,
        ..fresh()
    };
    note_listing(&mut report, 1, None, Some(stale), 1);
    let blind = ListingState {
        pulls_complete: false,
        ..fresh()
    };
    note_listing(&mut report, 2, None, Some(blind), 2);
    assert_eq!(report.errors, 3, "one per failed listing, one per repo holding Builders");
    assert_eq!(report.error_kind(), Some("forge_listing"));
    assert_eq!(report.listing_failed, vec![0]);
    assert_eq!(report.listing_incomplete, vec![1]);

    let mut limited = TickReport::default();
    let wall = anyhow::anyhow!("gh: API rate limit exceeded (HTTP 403)");
    note_listing(&mut limited, 0, Some(&wall), None, 0);
    assert_eq!(limited.error_kind(), Some("rate_limited"));
}

/// A red repo held for its fixes keeps every step that adds no code to `main`
/// for a non-fix item, and drops Builder and `ChampionMerge` unless the item
/// is a red-main fix (#12046).
#[test]
fn a_fixes_only_listing_keeps_the_steps_that_add_no_code() {
    let held = ListingState {
        fixes_only: true,
        ..fresh()
    };
    let steps = |w: &crate::step_dispatch::item_draw::Weighing| -> Vec<Step> {
        w.pool.iter().map(|c| c.step).collect()
    };

    let mut open = workspace(0, "acme/one", &forge()).weighing;
    let before = steps(&open);
    for step in [
        Step::Builder,
        Step::ChampionMerge,
        Step::Judge,
        Step::Doctor,
    ] {
        assert!(before.contains(&step), "fixture lacks {step:?}");
    }
    let _ = shape_pool(&mut open, &DrawClaims::default(), fresh());
    assert_eq!(steps(&open), before, "fixes_only: false is unchanged");

    let mut w = workspace(0, "acme/one", &forge()).weighing;
    let _ = shape_pool(&mut w, &DrawClaims::default(), held);
    let kept = steps(&w);
    assert!(!kept.contains(&Step::Builder) && !kept.contains(&Step::ChampionMerge));
    let expected: Vec<Step> = before
        .iter()
        .copied()
        .filter(|s| !matches!(s, Step::Builder | Step::ChampionMerge))
        .collect();
    assert_eq!(kept, expected, "every other step stays");

    let mut fix = workspace(0, "acme/one", &forge()).weighing;
    for c in &mut fix.pool {
        c.item.lane = Lane::RedMain;
    }
    let _ = shape_pool(&mut fix, &DrawClaims::default(), held);
    assert_eq!(steps(&fix), before, "red-main fixes keep every step");
}

/// #13367: a direct-mode repo keeps its configured merge limit, whatever its
/// eligible debt; only queue mode applies the demand rule.
#[test]
fn a_direct_mode_repo_keeps_the_configured_merge_limit() {
    for eligible in [0, 1, 11, 60] {
        assert_eq!(effective_merge_limit(false, 1, eligible, 10), 1, "{eligible}");
        assert_eq!(effective_merge_limit(false, 3, eligible, 10), 3, "{eligible}");
    }
    assert_eq!(effective_merge_limit(true, 1, 60, 10), DEFAULT_QUEUE_BUILD_CAP);
}

/// #13367: a queue-mode repo's merge limit scales with its eligible debt
/// (`laneK` 10), is bounded by the queue's build cap, and never drops below
/// the configured floor.
#[test]
fn the_queue_merge_limit_follows_the_eligible_debt() {
    assert_eq!(demand_merge_limit(1, 0, 10), 1);
    assert_eq!(demand_merge_limit(1, 1, 10), 1);
    assert_eq!(demand_merge_limit(1, 10, 10), 1);
    assert_eq!(demand_merge_limit(1, 11, 10), 2);
    assert_eq!(demand_merge_limit(1, 60, 10), DEFAULT_QUEUE_BUILD_CAP);
    assert_eq!(demand_merge_limit(3, 1, 10), 3, "the configured floor holds");
    assert_eq!(demand_merge_limit(8, 60, 10), 8, "an explicit value above the cap is kept");
    assert_eq!(demand_merge_limit(0, 5, 0), DEFAULT_QUEUE_BUILD_CAP, "zeros fail safe");
}

#[derive(Default)]
struct FleetCapacityCase {
    exhausted: [bool; 2],
    halted: [bool; 2],
    lanes: [RedMainLane; 2],
    truncate: bool,
    offer_exhausted: bool,
}

fn fixture_capacity_registry(root: &std::path::Path, repo: &str) -> SweepRegistry {
    use std::os::unix::fs::PermissionsExt;

    let gh = root.join("fixture-gh");
    let row = serde_json::json!([{
        "number": 7,
        "state": "open",
        "title": "Approved fixture",
        "body": "",
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
        "labels": [{"name": "loom:pr"}],
        "pull_request": {"url": "https://example.invalid/pulls/7"},
        "base": {"ref": "main"},
        "head": {"ref": "fixture", "sha": "fixture-head"},
        "user": {"login": "fixture"}
    }]);
    std::fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
case "$*" in
  'repo view'*) printf '%s\n' '{repo}' ;;
  *'/issues?state=open'*|*'/pulls?state=open'*)
    printf 'HTTP/2.0 200 OK\r\n\r\n%s\n' '{row}' ;;
  *) exit 1 ;;
esac
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config = crate::sweep_registry::SweepRegistryConfig::new(root.to_path_buf());
    config.gh_bin = Some(gh);
    config.journal_path = Some(root.join("sweeps.json"));
    config.outcomes_journal_path = Some(root.join("outcomes.jsonl"));
    config.outcome_telemetry_path = Some(root.join("outcomes-telemetry.jsonl"));
    SweepRegistry::new(config)
}

fn fleet_capacity_tick(case: FleetCapacityCase) -> (TickReport, HeartbeatRequest, [String; 2]) {
    use crate::fleet_dispatch::{proto, test_server::TestServer, worker};

    struct ResetWorker;
    impl Drop for ResetWorker {
        fn drop(&mut self) {
            worker::reset_for_test();
        }
    }

    worker::reset_for_test();
    let _reset_worker = ResetWorker;
    let _overrides = crate::env_test_support::EnvGuard::clear(&["LOOM_REPO", "LOOM_GH_BIN"]);
    let temp = tempfile::tempdir().unwrap();
    let roots = [temp.path().join("one"), temp.path().join("two")];
    let suffix = temp.path().file_name().unwrap().to_string_lossy();
    let repos = [
        format!("acme/capacity-{suffix}-0"),
        format!("acme/capacity-{suffix}-1"),
    ];
    let assignments = if case.offer_exhausted {
        vec![serde_json::json!({
            "assignment_id": "fixture-exhausted", "repo": repos[0], "item": 7,
            "step": "champion_merge", "ttl_s": 1200
        })]
    } else {
        Vec::new()
    };
    let captain = TestServer::start(move |path, _| match path {
        proto::HEARTBEAT_PATH => {
            (200, serde_json::json!({"server_epoch": "fixture", "grace": false}))
        }
        proto::NEXT_PATH => {
            (200, serde_json::json!({"server_epoch": "fixture", "assignments": assignments}))
        }
        proto::REFUSE_PATH => (200, serde_json::json!({"server_epoch": "fixture", "known": true})),
        // The worker only sends test traffic. Table decisions are checked on
        // the captured heartbeat without admitting a real step spawn.
        _ => (
            200,
            serde_json::json!({"server_epoch": "fixture", "granted": false, "reason": "held"}),
        ),
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let pool =
        WorkspacePool::new(Arc::new(crate::event_bus::EventBus::new()), runtime.handle().clone());
    for (idx, root) in roots.iter().enumerate() {
        crate::sweep_registry::test_support::write_config(
            root,
            &serde_json::json!({"fleetDispatch": {
                "enabled": true, "url": captain.url(), "request": {"jitter": 0, "startupSecs": 0}
            }})
            .to_string(),
        );
        let repo = &repos[idx];
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .status()
                .unwrap()
                .success());
        };
        git(&["-c", "init.templateDir=", "init", "-q"]);
        git(&[
            "remote",
            "add",
            "origin",
            &format!("https://github.com/{repo}.git"),
        ]);
        let mut registry = fixture_capacity_registry(root, repo);
        if case.exhausted[idx] {
            registry.record_role_tick_pool_exhausted("champion");
        }
        pool.seed(root.clone(), Arc::new(Mutex::new(registry)));
    }
    let mut driver = StepDriver {
        discovery: crate::discovery::Discovery::default(),
        provider: StageEntryProvider::load(None, 0),
        next_start: 0,
        starvation: Starvation::default(),
        requests: None,
        recheck: crate::discovery::recheck::Shared::default(),
    };
    let report = driver.tick(&TickInputs {
        pool: &pool,
        roots: &roots,
        weights: &[1, 1],
        halted: &case.halted,
        halt_causes: &[None, None],
        lanes: &case.lanes,
        build_backoff: &[false, false],
        max_concurrent: 2,
        deadline: case.truncate.then(Instant::now),
        gather_concurrency: 1,
        spawn_concurrency: 1,
        discovery: crate::discovery::Chain::standalone(),
    });
    worker::on_reaper_tick(&roots[0], &[]);
    let heartbeats = captain.wait_for(proto::HEARTBEAT_PATH, 1);
    let requests = captain.requests_to(proto::NEXT_PATH);
    assert_eq!(heartbeats.len(), 1, "one heartbeat");
    assert_eq!(requests.len(), 1, "one work request");
    let heartbeat: HeartbeatRequest = serde_json::from_value(heartbeats[0].clone()).unwrap();
    let next: NextRequest = serde_json::from_value(requests[0].clone()).unwrap();
    assert_eq!(heartbeat.roles_ok, next.roles_ok);
    assert_eq!(heartbeat.free_slots, next.free_slots);
    assert_eq!(heartbeat.host_id, next.host_id);
    if case.offer_exhausted {
        let refused = captain.wait_for(proto::REFUSE_PATH, 1);
        assert_eq!(refused.len(), 1, "the explicit offer to the exhausted workspace is refused");
        assert_eq!(refused[0]["assignment_id"], "fixture-exhausted");
        assert_eq!(refused[0]["reason"], "not_eligible");
    }
    (report, heartbeat, repos)
}

fn capacity_claim(
    heartbeat: &HeartbeatRequest,
    repo: &str,
) -> crate::fleet_dispatch::table::ClaimDecision {
    use crate::fleet_dispatch::table::{Mono, Policy, Table};

    let mut table = Table::new("fixture", Policy::default(), Mono(Duration::ZERO), BTreeSet::new());
    let now = Mono(Duration::from_secs(100));
    table.heartbeat(now, &heartbeat.host_id, heartbeat);
    table.claim(now, &heartbeat.host_id, repo, 7, "champion_merge")
}

#[test]
#[serial_test::serial]
fn a_positive_captain_retry_holds_local_role_draws_across_workspaces_until_expiry() {
    use crate::fleet_dispatch::{
        fallback::{Mode, ENTER_AFTER_FAILURES},
        proto::{self, RefusalReason},
        test_server::TestServer,
        worker::{self, Ask},
    };
    use crate::step_role::StepRole;
    use crate::types::QueueDisposition;

    struct ResetWorker;
    impl Drop for ResetWorker {
        fn drop(&mut self) {
            worker::reset_for_test();
        }
    }
    #[derive(Default)]
    struct Spawns(Vec<(usize, u32, StepRole)>);
    impl StepSpawner for Spawns {
        fn spawn(&mut self, idx: usize, request: &StepSpawnRequest) -> SpawnResult {
            self.0.push((idx, request.item, request.role));
            SpawnResult::Spawned
        }
    }

    worker::reset_for_test();
    let _reset = ResetWorker;
    let _overrides = crate::env_test_support::EnvGuard::clear(&["LOOM_REPO", "LOOM_GH_BIN"]);
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let captain = TestServer::start(|path, body| {
        assert_eq!(path, proto::CLAIM_PATH);
        let item = body["item"].as_u64().unwrap();
        if item >= 100 {
            return (500, serde_json::json!({}));
        }
        let mut response = serde_json::json!({
            "server_epoch": "fixture", "granted": false,
            "reason": if item == 4 { "held" } else { "not_eligible" },
        });
        if item != 1 {
            response["retry_after_s"] = serde_json::json!(if item == 2 { 0 } else { 60 });
        }
        (200, response)
    });
    let enable = |enabled| {
        crate::sweep_registry::test_support::write_config(
            first.path(),
            &serde_json::json!({"fleetDispatch": {"enabled": enabled, "url": captain.url()}})
                .to_string(),
        );
    };
    enable(true);
    worker::set_host_root(first.path());

    for (item, retry) in [(1, None), (2, Some(0))] {
        let answer = worker::claim(first.path(), item, StepRole::ChampionMerge);
        assert!(
            matches!(answer, Ask::Refused(RefusalReason::NotEligible, r) if r.retry_after_s == retry)
        );
        assert!(worker::local_role_holds(second.path(), Instant::now()).is_empty());
    }
    assert!(matches!(
        worker::claim(first.path(), 4, StepRole::Judge),
        Ask::Refused(RefusalReason::Held, _)
    ));
    let arm_champion = || {
        assert!(matches!(
            worker::claim(first.path(), 3, StepRole::ChampionMerge),
            Ask::Refused(RefusalReason::NotEligible, r) if r.retry_after_s == Some(60)
        ));
        worker::role_hold_deadline_for_test(StepRole::ChampionPromote).unwrap()
    };
    let before_claim = Instant::now();
    let expiry = arm_champion();
    assert!(expiry >= before_claim + Duration::from_secs(60));
    assert!(expiry <= Instant::now() + Duration::from_secs(60));
    let before_expiry = expiry - Duration::from_millis(1);
    assert_eq!(
        worker::local_role_holds(second.path(), before_expiry),
        BTreeSet::from(["champion"])
    );
    assert!(worker::roles_ok(&BTreeSet::new())
        .iter()
        .any(|r| r == "champion"));

    let draw = |now, offers: &[tick::Offer]| {
        let mut ws = workspace(1, "acme/second", &forge());
        ws.weighing
            .pool
            .retain(|c| matches!(c.target(), 2 | 3 | 60));
        let (free_slots, fallback) = worker::tick_budget(second.path(), 3, 3);
        let roles = worker::local_role_holds(second.path(), now);
        let mut spawner = Spawns::default();
        let result = tick::run_with_role_holds(
            vec![ws],
            StepTickLimits {
                free_slots,
                spawn_concurrency: 3,
                fallback,
            },
            &SeedBase {
                minute: 29_340_000,
                host: "fixture".into(),
                tick_counter: 3,
            },
            &PoolKnobs::default(),
            &mut spawner,
            offers,
            &roles,
        );
        (result, spawner.0)
    };
    let (held, spawns) = draw(before_expiry, &[]);
    assert_eq!(spawns, vec![(1, 3, StepRole::Builder)]);
    assert_eq!(held.report.skipped_host_constraint, 2);
    assert!(held.passes.is_empty(), "held roles are never drawn");
    assert_eq!(
        held.report
            .queue
            .iter()
            .filter(|r| {
                r.disposition == Some(QueueDisposition::HostConstraint)
                    && r.detail.as_deref() == Some(tick::REFUSAL_CAPTAIN_NOT_ELIGIBLE)
            })
            .count(),
        2
    );

    let offers = [tick::Offer {
        repo: "acme/second".into(),
        item: 60,
        role: StepRole::ChampionMerge,
    }];
    let (_, spawns) = draw(before_expiry, &offers);
    assert_eq!(spawns, vec![(1, 60, StepRole::ChampionMerge), (1, 3, StepRole::Builder)]);

    enable(false);
    assert_eq!(draw(before_expiry, &[]).1.len(), 3, "fleet-disabled ignores the hold");
    enable(true);
    assert_eq!(draw(expiry, &[]).1.len(), 3, "the role can be drawn at expiry");
    assert!(worker::role_hold_deadline_for_test(StepRole::ChampionMerge).is_none());

    let expiry = arm_champion();
    for item in 100..100 + ENTER_AFTER_FAILURES {
        assert_eq!(worker::claim(first.path(), item, StepRole::Judge), Ask::FailOpen);
    }
    assert_eq!(worker::mode(), Mode::Fallback);
    assert_eq!(
        draw(expiry - Duration::from_millis(1), &[]).1.len(),
        3,
        "fallback ignores the hold"
    );
    assert_eq!(worker::role_hold_deadline_for_test(StepRole::ChampionMerge), Some(expiry));
}

#[test]
#[serial_test::serial]
fn fleet_role_capacity_advertises_champion_for_a_healthy_workspace() {
    use crate::fleet_dispatch::table::ClaimDecision;

    let (report, heartbeat, repos) = fleet_capacity_tick(FleetCapacityCase {
        exhausted: [true, false],
        offer_exhausted: true,
        ..FleetCapacityCase::default()
    });
    assert_eq!(report.gather.unwrap().repos_gathered, 2);
    assert_eq!(report.listing_failed, Vec::<usize>::new());
    let draw = report
        .item_draw
        .expect("the healthy merge is locally drawable");
    let drawn: Vec<_> = draw
        .draws
        .iter()
        .map(|item| (item.repo.as_str(), item.step.as_str()))
        .collect();
    assert_eq!(drawn, [(repos[1].as_str(), "champion_merge")]);
    assert!(draw
        .excluded
        .iter()
        .any(|item| item.repo == repos[0] && item.reason == "pool_exhausted"));
    let decision = capacity_claim(&heartbeat, &repos[1]);
    eprintln!("mixed pools: {:?}; {drawn:?}; {decision:?}", heartbeat.roles_ok);
    assert!(matches!(decision, ClaimDecision::Granted { .. }), "{decision:?}");
}

#[test]
#[serial_test::serial]
fn fleet_role_capacity_uses_all_participating_pool_facts_before_listing() {
    use crate::fleet_dispatch::{proto::RefusalReason, table::ClaimDecision};

    for (case, want_champion, expected_skips) in [
        (FleetCapacityCase::default(), true, 0),
        (
            FleetCapacityCase {
                exhausted: [true, true],
                ..FleetCapacityCase::default()
            },
            false,
            0,
        ),
        (
            FleetCapacityCase {
                exhausted: [true, false],
                truncate: true,
                ..FleetCapacityCase::default()
            },
            true,
            1,
        ),
        (
            FleetCapacityCase {
                exhausted: [true, true],
                truncate: true,
                ..FleetCapacityCase::default()
            },
            false,
            1,
        ),
        (
            FleetCapacityCase {
                exhausted: [true, false],
                halted: [false, true],
                ..FleetCapacityCase::default()
            },
            false,
            0,
        ),
        (
            FleetCapacityCase {
                exhausted: [true, false],
                halted: [false, true],
                lanes: [
                    RedMainLane::default(),
                    RedMainLane {
                        verified_red: true,
                        ..RedMainLane::default()
                    },
                ],
                truncate: true,
                ..FleetCapacityCase::default()
            },
            true,
            1,
        ),
        (
            FleetCapacityCase {
                halted: [true, true],
                ..FleetCapacityCase::default()
            },
            false,
            0,
        ),
    ] {
        let truncate = case.truncate;
        let all_halted = case.halted == [true, true];
        let (report, heartbeat, repos) = fleet_capacity_tick(case);
        assert_eq!(report.gather.unwrap().repos_skipped, expected_skips);
        assert_eq!(
            heartbeat
                .roles_ok
                .iter()
                .filter(|role| *role == "champion")
                .count(),
            usize::from(want_champion)
        );
        if all_halted {
            assert!(heartbeat.roles_ok.is_empty());
        }
        if truncate {
            assert!(report.item_draw.is_none(), "an unlisted root contributes no candidate");
        }
        let decision = capacity_claim(&heartbeat, &repos[1]);
        if want_champion {
            assert!(matches!(decision, ClaimDecision::Granted { .. }), "{decision:?}");
        } else {
            assert!(
                matches!(
                    decision,
                    ClaimDecision::Refused {
                        reason: RefusalReason::NotEligible,
                        ..
                    }
                ),
                "{decision:?}"
            );
        }
    }
}
