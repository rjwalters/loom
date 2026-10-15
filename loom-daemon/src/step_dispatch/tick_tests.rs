//! The step tick end to end: forge rows through the gather's builders, the
//! classifier and the weighing, into the tick with a fake spawner.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use super::*;
use crate::forge_listing::RestIssue;
use crate::step_dispatch::gather::{build_inputs, draw_inputs, pull_from_row, IssueFacts};
use crate::step_dispatch::item_draw::{replay_record, weigh, ItemWeights, WeighContext};
use crate::work_finder::step_item::{self, HostInput, PullMarkers, RepoInput};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-14T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn row(number: u32, labels: &[&str], body: &str, pr: bool) -> RestIssue {
    RestIssue {
        number,
        title: None,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
        created_at: Some("2026-10-01T00:00:00Z".to_string()),
        updated_at: None,
        closed_at: None,
        state: "open".to_string(),
        body: Some(body.to_string()),
        author: None,
        author_association: None,
        is_pull_request: pr,
        comments: 0,
        open_sub_issues: None,
        labels_at: None,
    }
}

/// One repo with an item at every stage, plus a claimed, a parked and an
/// operator-held one.
fn forge() -> Vec<RestIssue> {
    vec![
        row(1, &["loom:triage"], "", false),
        row(2, &["loom:curated"], "", false),
        row(3, &["loom:issue"], "", false),
        row(4, &["loom:building"], "", false),
        row(40, &["loom:review-requested"], "Closes #4", true),
        row(5, &["loom:building"], "", false),
        row(50, &["loom:changes-requested"], "Closes #5", true),
        row(6, &["loom:building"], "", false),
        row(60, &["loom:pr"], "Closes #6", true),
        row(7, &["loom:issue", "loom:building"], "", false),
        row(8, &["loom:issue", "loom:blocked"], "", false),
        row(9, &["loom:issue", "loom:operator"], "", false),
        row(10, &["loom:issue"], "", false),
        row(100, &["loom:review-requested", "loom:reviewing"], "Closes #10", true),
    ]
}

fn workspace(idx: usize, repo: &str, rows: &[RestIssue]) -> StepWorkspace {
    let pulls = rows
        .iter()
        .filter(|r| r.is_pull_request)
        .map(|r| pull_from_row(r, PullMarkers::default()))
        .collect();
    let inputs = build_inputs(
        rows,
        pulls,
        &IssueFacts::default(),
        RepoInput::default(),
        HostInput::default(),
    );
    let items = draw_inputs(step_item::classify(&inputs), rows, |_, _| None);
    let ctx = WeighContext {
        now: now(),
        build_backoff_engaged: false,
        pool_exhausted_roles: BTreeSet::new(),
        workspace_idx: idx,
        cooldowns: Default::default(),
    };
    StepWorkspace {
        idx,
        repo: repo.to_string(),
        weight: 1,
        weighing: weigh(&items, &ctx, &ItemWeights::default()),
        halted: false,
        created_at: HashMap::new(),
    }
}

/// Spawns everything except the items it is told to refuse, once each.
#[derive(Default)]
struct FakeSpawner {
    refuse: Vec<(u32, &'static str)>,
    refuse_all: Option<&'static str>,
    asked: Vec<(usize, u32, StepRole, u32)>,
}

impl StepSpawner for FakeSpawner {
    fn spawn(&mut self, idx: usize, request: &StepSpawnRequest) -> SpawnResult {
        self.asked
            .push((idx, request.item, request.role, request.draw_attempt));
        if let Some(reason) = self.refuse_all {
            return SpawnResult::Refused(reason.to_string());
        }
        if let Some(i) = self.refuse.iter().position(|(n, _)| *n == request.item) {
            let (_, reason) = self.refuse.remove(i);
            return SpawnResult::Refused(reason.to_string());
        }
        SpawnResult::Spawned
    }
}

fn seed() -> SeedBase {
    SeedBase {
        minute: 29_340_000,
        host: "host-a".to_string(),
        tick_counter: 3,
    }
}

fn limits(free_slots: usize) -> StepTickLimits {
    StepTickLimits {
        free_slots,
        max_admissions: free_slots,
        saturation_held: false,
    }
}

fn run_with(
    workspaces: Vec<StepWorkspace>,
    limits: StepTickLimits,
    cap: &mut RepoCap,
    spawner: &mut FakeSpawner,
) -> StepTick {
    run(
        workspaces,
        limits,
        &seed(),
        (11_103, None),
        &WeightBonus::default(),
        cap,
        spawner,
    )
}

#[test]
fn every_stage_gets_its_step_and_claimed_and_parked_items_are_skipped() {
    let mut spawner = FakeSpawner {
        refuse: vec![(3, "claimed")],
        ..FakeSpawner::default()
    };
    let mut cap = RepoCap::new(None, vec![0]);
    let tick =
        run_with(vec![workspace(0, "acme/one", &forge())], limits(10), &mut cap, &mut spawner);
    let asked: BTreeSet<(u32, StepRole)> = spawner.asked.iter().map(|a| (a.1, a.2)).collect();
    assert_eq!(
        asked,
        BTreeSet::from([
            (1, StepRole::Curator),
            (2, StepRole::ChampionPromote),
            (3, StepRole::Builder),
            (40, StepRole::Judge),
            (50, StepRole::Doctor),
            (60, StepRole::ChampionMerge),
        ])
    );
    // 3's claim was refused: a pass, counted, and its row says so.
    assert_eq!(tick.report.dispatched, 5);
    assert_eq!(tick.passes.get("claimed"), Some(&1));
    let row3 = tick
        .report
        .queue
        .iter()
        .find(|r| r.key.number == 3)
        .unwrap();
    assert_eq!(row3.disposition, Some(Qd::PeerClaim));
    // Claimed (7, 100), parked (8) and operator-held (9) items never reach
    // the spawner and are counted by reason; every pooled row names its step.
    assert!(tick.report.skipped_in_flight >= 2);
    assert!(tick.report.skipped_labeled >= 2);
    assert!(tick.report.queue.iter().all(|r| r.step.is_some()));
    // The record names the repo it seeded with, and replays.
    let record = tick.report.item_draw.as_ref().unwrap();
    assert!(record.draws.iter().all(|d| d.repo == "acme/one"));
    let replayed = replay_record(record).unwrap();
    assert_eq!(replayed.len(), 6);
}

#[test]
fn a_refused_claim_redraws_within_the_slot() {
    let rows = vec![
        row(1, &["loom:triage"], "", false),
        row(2, &["loom:triage"], "", false),
        row(3, &["loom:triage"], "", false),
    ];
    let mut spawner = FakeSpawner::default();
    let mut cap = RepoCap::new(None, vec![0]);
    // Find the draw order, then refuse the first two picks.
    run_with(vec![workspace(0, "acme/one", &rows)], limits(3), &mut cap, &mut spawner);
    let order: Vec<u32> = spawner.asked.iter().map(|a| a.1).collect();
    let mut spawner = FakeSpawner {
        refuse: vec![(order[0], "claimed"), (order[1], "claim_lost")],
        ..FakeSpawner::default()
    };
    let mut cap = RepoCap::new(None, vec![0]);
    let tick = run_with(vec![workspace(0, "acme/one", &rows)], limits(1), &mut cap, &mut spawner);
    let attempts: Vec<(u32, u32)> = spawner.asked.iter().map(|a| (a.1, a.3)).collect();
    assert_eq!(attempts, vec![(order[0], 1), (order[1], 2), (order[2], 3)]);
    assert_eq!(tick.report.dispatched, 1);
    assert_eq!(tick.passes.values().sum::<usize>(), 2);
}

#[test]
fn a_slot_gives_up_after_eight_draws_and_a_tick_after_thirty_two_passes() {
    let rows: Vec<RestIssue> = (1..=60)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let mut spawner = FakeSpawner {
        refuse_all: Some("pool_exhausted"),
        ..FakeSpawner::default()
    };
    let mut cap = RepoCap::new(None, vec![0]);
    let tick = run_with(vec![workspace(0, "acme/one", &rows)], limits(1), &mut cap, &mut spawner);
    assert_eq!(spawner.asked.len(), MAX_DRAWS_PER_SLOT as usize);
    assert_eq!(tick.report.dispatched, 0);

    let mut spawner = FakeSpawner {
        refuse_all: Some("claimed"),
        ..FakeSpawner::default()
    };
    let mut cap = RepoCap::new(None, vec![0]);
    run_with(vec![workspace(0, "acme/one", &rows)], limits(10), &mut cap, &mut spawner);
    assert_eq!(spawner.asked.len(), MAX_PASSES_PER_TICK as usize);
}

#[test]
fn saturation_holds_every_row_and_a_full_repo_is_drawn_no_more() {
    let mut spawner = FakeSpawner::default();
    let mut cap = RepoCap::new(None, vec![0]);
    let tick = run_with(
        vec![workspace(0, "acme/one", &forge())],
        StepTickLimits {
            saturation_held: true,
            ..limits(4)
        },
        &mut cap,
        &mut spawner,
    );
    assert!(spawner.asked.is_empty());
    assert!(tick
        .report
        .queue
        .iter()
        .all(|r| r.disposition == Some(Qd::DeferredSaturation)));

    // Repo `one` is at its cap of 2; `two` still gets the slots.
    let mut spawner = FakeSpawner::default();
    let mut cap = RepoCap::new(Some(2), vec![2, 0]);
    let two = vec![
        row(1, &["loom:triage"], "", false),
        row(2, &["loom:triage"], "", false),
    ];
    let tick = run_with(
        vec![
            workspace(0, "acme/one", &forge()),
            workspace(1, "acme/two", &two),
        ],
        limits(2),
        &mut cap,
        &mut spawner,
    );
    assert!(spawner.asked.iter().all(|a| a.0 == 1), "{:?}", spawner.asked);
    assert_eq!(tick.report.dispatched, 2);
    assert!(tick.passes.get(PASS_REPO_CAP).copied().unwrap_or(0) <= 1);
}

#[test]
fn a_halted_workspace_is_never_drawn() {
    let mut spawner = FakeSpawner::default();
    let mut cap = RepoCap::new(None, vec![0]);
    let mut ws = workspace(0, "acme/one", &forge());
    ws.halted = true;
    let tick = run_with(vec![ws], limits(4), &mut cap, &mut spawner);
    assert!(spawner.asked.is_empty());
    assert!(tick
        .report
        .queue
        .iter()
        .all(|r| r.disposition == Some(Qd::WorkspaceHalted)));
}

#[test]
fn live_claims_and_a_peer_merge_leave_the_pool_as_claimed_passes() {
    let mut ws = workspace(0, "acme/one", &forge());
    let before = ws.weighing.pool.len();
    let claims = DrawClaims {
        items: std::collections::HashSet::from([1, 40]),
        merge_in_flight: Some((61, "peer".to_string())),
    };
    let claimed_before = ws
        .weighing
        .excluded
        .get(&crate::step_dispatch::item_draw::Exclusion::Pass(PassReason::Claimed))
        .copied()
        .unwrap_or(0);
    exclude_claimed(&mut ws.weighing, &claims);
    let left: BTreeSet<u32> = ws.weighing.pool.iter().map(Candidate::target).collect();
    // 1 (this host or a peer runs it), 40 (likewise) and the merge of 60
    // (a peer merges #61 in this repo) are out; the rest stay.
    assert_eq!(left, BTreeSet::from([2, 3, 50]));
    assert_eq!(ws.weighing.pool.len(), before - 3);
    let claimed_after = ws.weighing.excluded
        [&crate::step_dispatch::item_draw::Exclusion::Pass(PassReason::Claimed)];
    assert_eq!(claimed_after, claimed_before + 3);
}

#[test]
fn a_refused_draw_is_counted_on_the_record() {
    let mut spawner = FakeSpawner {
        refuse: vec![(3, "claim_lost")],
        ..FakeSpawner::default()
    };
    let mut cap = RepoCap::new(None, vec![0]);
    let tick =
        run_with(vec![workspace(0, "acme/one", &forge())], limits(10), &mut cap, &mut spawner);
    let record = tick.report.item_draw.unwrap();
    let refused = record
        .excluded
        .iter()
        .find(|e| e.reason == "refused_claim_lost")
        .unwrap();
    assert_eq!((refused.repo.as_str(), refused.count), ("acme/one", 1));
    assert_eq!(refused.skip_reason, PickSkipReason::InFlight);
}

/// A failed open-PR listing, or a stale fallback listing, draws no Builder
/// step: the Builder claim is a blind label flip (#11909).
#[test]
fn no_builder_step_is_drawn_without_a_fresh_pr_listing() {
    let mut ws = workspace(0, "acme/one", &forge());
    assert!(ws
        .weighing
        .pool
        .iter()
        .any(|c| c.step == crate::work_finder::step_item::Step::Builder));
    let before = ws.weighing.pool.len();
    exclude_builders(&mut ws.weighing);
    assert!(ws
        .weighing
        .pool
        .iter()
        .all(|c| c.step != crate::work_finder::step_item::Step::Builder));
    assert_eq!(ws.weighing.pool.len(), before - 1, "only #3 was a Builder step");
    assert_eq!(
        ws.weighing.excluded
            [&crate::step_dispatch::item_draw::Exclusion::Pass(PassReason::SharedPr)],
        1
    );
}
