//! The step tick end to end: forge rows through the gather's builders, the
//! classifier and the weighing, into the tick with a fake spawner.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use super::*;
use crate::forge_listing::RestIssue;
use crate::step_dispatch::gather::{build_inputs, draw_inputs, pull_from_row, IssueFacts};
use crate::step_dispatch::item_draw::{replay_record, weigh, ItemWeights, PoolKnobs, WeighContext};
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
pub(crate) fn forge() -> Vec<RestIssue> {
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

pub(crate) fn workspace(idx: usize, repo: &str, rows: &[RestIssue]) -> StepWorkspace {
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
        in_flight: 0,
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
    terms: Vec<Option<StepDrawTerms>>,
    /// Each round's batch, as `(workspace, item)` (#12298).
    batches: Vec<Vec<(usize, u32)>>,
    /// Hold the spawns once this many batches ran (#12298).
    hold_after: Option<usize>,
    /// Refuse the first `.0` spawn calls, whatever their item, with `.1`.
    refuse_first: Option<(usize, &'static str)>,
}

impl StepSpawner for FakeSpawner {
    fn spawn(&mut self, idx: usize, request: &StepSpawnRequest) -> SpawnResult {
        self.asked
            .push((idx, request.item, request.role, request.draw_attempt));
        self.terms.push(request.draw);
        if let Some(reason) = self.refuse_all {
            return SpawnResult::Refused(reason.to_string());
        }
        if let Some((n, reason)) = self.refuse_first {
            if self.asked.len() <= n {
                return SpawnResult::Refused(reason.to_string());
            }
        }
        if let Some(i) = self.refuse.iter().position(|(n, _)| *n == request.item) {
            let (_, reason) = self.refuse.remove(i);
            return SpawnResult::Refused(reason.to_string());
        }
        SpawnResult::Spawned
    }

    fn held(&mut self) -> Option<&'static str> {
        self.hold_after
            .is_some_and(|n| self.batches.len() >= n)
            .then_some("rate_limit_breaker")
    }

    fn spawn_batch(&mut self, batch: &[(usize, StepSpawnRequest)]) -> Vec<SpawnResult> {
        self.batches
            .push(batch.iter().map(|(idx, r)| (*idx, r.item)).collect());
        // Answer in batch order, as the trait requires, after "running"
        // them back to front: the order the work finished in is not the
        // order the tick sees.
        let mut answers: Vec<(usize, SpawnResult)> = batch
            .iter()
            .enumerate()
            .rev()
            .map(|(pos, (idx, r))| (pos, self.spawn(*idx, r)))
            .collect();
        answers.sort_by_key(|(pos, _)| *pos);
        answers.into_iter().map(|(_, r)| r).collect()
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
        spawn_concurrency: 1,
        fallback: false,
    }
}

fn run_with(
    workspaces: Vec<StepWorkspace>,
    limits: StepTickLimits,
    spawner: &mut FakeSpawner,
) -> StepTick {
    run_knobs(workspaces, limits, PoolKnobs::default(), spawner)
}

fn run_knobs(
    workspaces: Vec<StepWorkspace>,
    limits: StepTickLimits,
    knobs: PoolKnobs,
    spawner: &mut FakeSpawner,
) -> StepTick {
    run(workspaces, limits, &seed(), &knobs, spawner)
}

#[test]
fn every_stage_gets_its_step_and_claimed_and_parked_items_are_skipped() {
    let mut spawner = FakeSpawner {
        refuse: vec![(3, "claimed")],
        ..FakeSpawner::default()
    };
    let tick = run_with(vec![workspace(0, "acme/one", &forge())], limits(10), &mut spawner);
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
    assert_eq!(record.repos.len(), 1);
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
    // Find the draw order, then refuse the first two picks.
    run_with(vec![workspace(0, "acme/one", &rows)], limits(3), &mut spawner);
    let order: Vec<u32> = spawner.asked.iter().map(|a| a.1).collect();
    let mut spawner = FakeSpawner {
        refuse: vec![(order[0], "claimed"), (order[1], "claim_lost")],
        ..FakeSpawner::default()
    };
    let tick = run_with(vec![workspace(0, "acme/one", &rows)], limits(1), &mut spawner);
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
    let tick = run_with(vec![workspace(0, "acme/one", &rows)], limits(1), &mut spawner);
    assert_eq!(spawner.asked.len(), MAX_DRAWS_PER_SLOT as usize);
    assert_eq!(tick.report.dispatched, 0);

    let mut spawner = FakeSpawner {
        refuse_all: Some("claimed"),
        ..FakeSpawner::default()
    };
    run_with(vec![workspace(0, "acme/one", &rows)], limits(10), &mut spawner);
    assert_eq!(spawner.asked.len(), MAX_PASSES_PER_TICK as usize);
}

#[test]
fn a_halted_workspace_is_never_drawn() {
    let mut spawner = FakeSpawner::default();
    let mut ws = workspace(0, "acme/one", &forge());
    ws.halted = true;
    let tick = run_with(vec![ws], limits(4), &mut spawner);
    assert!(spawner.asked.is_empty());
    assert!(tick
        .report
        .queue
        .iter()
        .all(|r| r.disposition == Some(Qd::WorkspaceHalted)));
}

#[test]
fn live_claims_leave_the_pool_as_claimed_and_a_peer_merge_as_merge_in_flight() {
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
    assert_eq!(claimed_after, claimed_before + 2);
    // #12494: the merge is counted on its own, so the tick can report it.
    assert_eq!(
        ws.weighing.excluded[&crate::step_dispatch::item_draw::Exclusion::MergeInFlight],
        1
    );
}

/// `n` approved PRs (Champion merges) and one triage issue.
fn merges_and_a_curator(n: u32) -> Vec<RestIssue> {
    let mut rows = vec![row(1, &["loom:triage"], "", false)];
    for i in 0..n {
        let issue = 10 + i;
        rows.push(row(issue, &["loom:building"], "", false));
        rows.push(row(100 + i, &["loom:pr"], &format!("Closes #{issue}"), true));
    }
    rows
}

fn merge_steps(w: &StepWorkspace) -> usize {
    w.weighing
        .pool
        .iter()
        .filter(|c| is_merge_step(c.step))
        .count()
}

#[test]
fn a_repo_with_a_merge_in_flight_spends_no_draw_or_pass_on_its_merges() {
    // #12494: repo 0 has a merge in flight (this host's or a peer's) and six
    // more approved PRs; repo 1 has none. Repo 0's merges leave the pool
    // before the draw: no draw, no pass, counted as deferred. Repo 1's
    // items, and repo 0's other steps, are drawn as usual.
    let mut busy = workspace(0, "acme/busy", &merges_and_a_curator(6));
    assert_eq!(merge_steps(&busy), 6);
    let claims = DrawClaims {
        items: std::collections::HashSet::new(),
        merge_in_flight: Some((999, "peer".to_string())),
    };
    exclude_claimed(&mut busy.weighing, &claims);
    assert_eq!(merge_steps(&busy), 0);
    let idle = workspace(1, "acme/idle", &merges_and_a_curator(2));

    let mut spawner = FakeSpawner::default();
    let tick = run_with(vec![busy, idle], limits(10), &mut spawner);
    assert!(
        !spawner
            .asked
            .iter()
            .any(|a| a.0 == 0 && a.2 == StepRole::ChampionMerge),
        "a merge-blocked item reached the spawner: {:?}",
        spawner.asked
    );
    // Repo 0's curator, and one of repo 1's merges and its curator: repo 1
    // spawned a merge, so its second merge is deferred too.
    assert_eq!(tick.report.dispatched, 3, "{:?}", spawner.asked);
    assert!(spawner
        .asked
        .iter()
        .any(|a| a.0 == 1 && a.2 == StepRole::ChampionMerge));
    assert!(spawner
        .asked
        .iter()
        .any(|a| a.0 == 1 && a.2 == StepRole::Curator));
    assert!(spawner
        .asked
        .iter()
        .any(|a| a.0 == 0 && a.2 == StepRole::Curator));
    // No pass at all, and no draw beyond the spawns.
    assert!(tick.passes.is_empty(), "{:?}", tick.passes);
    assert_eq!(tick.draws, 3);
    assert_eq!(tick.merge_deferred, 6 + 1);
    assert_eq!(tick.draws_unspawned, 0);
    assert_eq!(tick.stop, TickStop::PoolDrained);
    // Every merge row of repo 0 reads as someone else's work, not capacity.
    let record = tick.report.item_draw.as_ref().unwrap();
    let excluded = |reason: &str| {
        record
            .excluded
            .iter()
            .find(|e| e.repo == "acme/busy" && e.reason == reason)
            .map(|e| e.count)
    };
    assert_eq!(excluded("merge_in_flight"), Some(6));
    assert!(record
        .excluded
        .iter()
        .all(|e| e.reason != format!("refused_{PASS_MERGE}")));
    replay_record(record).unwrap();
}

#[test]
fn a_merge_spawned_mid_tick_defers_the_repos_other_merges() {
    // #12494: a serial tick over one repo with five approved PRs and a
    // curator item. The first merge spawned takes the other four out of the
    // pool at once: they are never drawn, spend no pass, and are reported.
    let mut spawner = FakeSpawner::default();
    let tick = run_with(
        vec![workspace(0, "acme/one", &merges_and_a_curator(5))],
        limits(10),
        &mut spawner,
    );
    let merges = spawner
        .asked
        .iter()
        .filter(|a| a.2 == StepRole::ChampionMerge)
        .count();
    assert_eq!(merges, 1, "{:?}", spawner.asked);
    assert_eq!(tick.report.dispatched, 2);
    assert_eq!(tick.draws, 2);
    assert!(tick.passes.is_empty(), "{:?}", tick.passes);
    assert_eq!(tick.merge_deferred, 4);
    assert_eq!(tick.stop, TickStop::PoolDrained);
    // The deferred rows say why, rather than waiting on capacity.
    let deferred_rows = tick
        .report
        .queue
        .iter()
        .filter(|r| r.detail.as_deref() == Some(PASS_MERGE))
        .count();
    assert_eq!(deferred_rows, 4);
    assert!(tick
        .report
        .queue
        .iter()
        .filter(|r| r.detail.as_deref() == Some(PASS_MERGE))
        .all(|r| r.disposition == Some(Qd::PeerClaim)));
    // The record names the partial closure and still replays.
    let record = tick.report.item_draw.as_ref().unwrap();
    assert_eq!(record.closures.len(), 1);
    assert_eq!(
        record.closures[0].steps,
        vec![
            "champion_merge".to_string(),
            "champion_hold_reevaluate".to_string()
        ]
    );
    let deferred = record
        .excluded
        .iter()
        .find(|e| e.reason == format!("deferred_{PASS_MERGE}"))
        .unwrap();
    assert_eq!(deferred.count, 4);
    assert_eq!(replay_record(record).unwrap().len(), 2);
}

#[test]
fn a_hold_reevaluation_stays_serialised_behind_the_repos_merge() {
    // #12494 asked whether `champion_hold_reevaluate` needs the repo's merge
    // lock. It does: it runs the Champion merge role and can merge the PR
    // (see `MERGE_STEPS`). So a merge in flight defers it, before the draw
    // and after a merge this tick spawned, like any other merge step.
    use crate::work_finder::step_item::Step;
    assert!(is_merge_step(Step::ChampionHoldReevaluate));
    assert_eq!(StepRole::from(Step::ChampionHoldReevaluate), StepRole::ChampionMerge);
    let as_hold = |mut w: StepWorkspace, pr: u32| {
        for c in &mut w.weighing.pool {
            if c.target() == pr {
                c.step = Step::ChampionHoldReevaluate;
            }
        }
        w
    };
    // Before the draw: a peer's merge in flight takes it out.
    let mut ws = as_hold(workspace(0, "acme/one", &merges_and_a_curator(2)), 100);
    exclude_claimed(
        &mut ws.weighing,
        &DrawClaims {
            items: std::collections::HashSet::new(),
            merge_in_flight: Some((101, "peer".to_string())),
        },
    );
    assert!(ws
        .weighing
        .pool
        .iter()
        .all(|c| c.step != Step::ChampionHoldReevaluate));
    // Mid-tick: whichever of the two is drawn first, the other is deferred.
    let mut spawner = FakeSpawner::default();
    let tick = run_with(
        vec![as_hold(
            workspace(0, "acme/one", &merges_and_a_curator(2)),
            100,
        )],
        limits(10),
        &mut spawner,
    );
    let merges = spawner
        .asked
        .iter()
        .filter(|a| a.2 == StepRole::ChampionMerge)
        .count();
    assert_eq!(merges, 1, "{:?}", spawner.asked);
    assert_eq!(tick.merge_deferred, 1);
    assert!(tick.passes.is_empty());
}

#[test]
fn a_refused_draw_is_counted_on_the_record() {
    let mut spawner = FakeSpawner {
        refuse: vec![(3, "claim_lost")],
        ..FakeSpawner::default()
    };
    let tick = run_with(vec![workspace(0, "acme/one", &forge())], limits(10), &mut spawner);
    let record = tick.report.item_draw.unwrap();
    let refused = record
        .excluded
        .iter()
        .find(|e| e.reason == "refused_claim_lost")
        .unwrap();
    assert_eq!((refused.repo.as_str(), refused.count), ("acme/one", 1));
    assert_eq!(refused.skip_reason, PickSkipReason::InFlight);
}

/// #12410: a `claim_lost` refusal is a counted pass, and the draw behind it
/// is a draw taken: the tick span reads both from the tick.
#[test]
fn claim_lost_refusals_are_counted_as_passes_and_draws() {
    let mut spawner = FakeSpawner {
        refuse: vec![(3, "claim_lost")],
        ..FakeSpawner::default()
    };
    let tick = run_with(vec![workspace(0, "acme/one", &forge())], limits(10), &mut spawner);
    assert_eq!(tick.passes.get("claim_lost"), Some(&1));
    assert!(tick.draws > 1, "{} draws", tick.draws);
}

/// #12410: a lost claim is cheap. Forty lost claims in a row spend neither the
/// tick's passes nor (for the first sixteen) a slot's draws, so the tick
/// still spawns; forty real refusals (`claimed`) spend the pass budget and
/// spawn nothing.
#[test]
fn claim_lost_refusals_do_not_exhaust_the_tick_passes() {
    let rows: Vec<RestIssue> = (1..=60)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let mut spawner = FakeSpawner {
        refuse_first: Some((40, "claim_lost")),
        ..FakeSpawner::default()
    };
    let lost = run_with(vec![workspace(0, "acme/one", &rows)], limits(10), &mut spawner);
    // Slot 1: 16 free losses + 8 charged; slots 2 and 3: 8 charged each (the
    // free ones are spent); slots 4..10 spawn.
    assert_eq!(lost.passes.get("claim_lost"), Some(&40), "every loss is still counted");
    assert_eq!(lost.report.dispatched, 7);
    assert_eq!(lost.stop, TickStop::SlotDraws);
    assert_eq!(lost.draws, 47);

    let mut spawner = FakeSpawner {
        refuse_first: Some((40, "claimed")),
        ..FakeSpawner::default()
    };
    let claimed = run_with(vec![workspace(0, "acme/one", &rows)], limits(10), &mut spawner);
    assert_eq!(spawner.asked.len(), MAX_PASSES_PER_TICK as usize);
    assert_eq!(claimed.report.dispatched, 0);
    assert_eq!(claimed.stop, TickStop::PassLimit);
}

/// #12410: a few lost claims cost the slot nothing: it redraws at once and
/// keeps its numbering, and the lost item never comes back in the tick.
#[test]
fn a_lost_claim_redraws_without_spending_the_slot() {
    let rows: Vec<RestIssue> = (1..=20)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let mut spawner = FakeSpawner {
        refuse_first: Some((10, "claim_lost")),
        ..FakeSpawner::default()
    };
    let tick = run_with(vec![workspace(0, "acme/one", &rows)], limits(1), &mut spawner);
    // Ten losses exceed the slot's eight draws, but none is charged.
    assert_eq!(tick.report.dispatched, 1);
    assert_eq!(tick.stop, TickStop::Filled);
    let attempts: Vec<u32> = spawner.asked.iter().map(|a| a.3).collect();
    assert_eq!(attempts, (1..=11).collect::<Vec<u32>>());
    let items: BTreeSet<u32> = spawner.asked.iter().map(|a| a.1).collect();
    assert_eq!(items.len(), 11, "a lost item is never redrawn in the tick");
}

/// #12410: the tick names why its draw loop stopped and how many slots it
/// could fill.
#[test]
fn the_tick_names_its_stop_and_budget() {
    let rows: Vec<RestIssue> = (1..=12)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let ws = || vec![workspace(0, "acme/one", &rows)];
    let tick_of = |limits: StepTickLimits| run_with(ws(), limits, &mut FakeSpawner::default());
    let filled = tick_of(limits(4));
    assert_eq!((filled.stop, filled.budget, filled.report.dispatched), (TickStop::Filled, 4, 4));
    let drained = tick_of(limits(30));
    assert_eq!((drained.stop, drained.budget), (TickStop::PoolDrained, 30));
    assert_eq!(drained.report.dispatched, 12);
    assert_eq!(tick_of(limits(0)).stop, TickStop::NoSlots);
    assert_eq!(TickStop::PassLimit.as_str(), "pass_limit");
}

/// #12048: a workspace missing the role's command reads as blocked in the
/// pick decision, not as a dispatch error.
#[test]
fn a_missing_role_command_reads_as_workspace_commands_missing() {
    assert_eq!(disposition_of("workspace_commands_missing"), Qd::WorkspaceCommandsMissing);
}

fn many(n: u32) -> Vec<RestIssue> {
    (1..=n)
        .map(|i| row(i, &["loom:triage"], "", false))
        .collect()
}

/// #12185: one pool across the workspaces. Each draw names its item's repo,
/// carries the repo terms (`n_items`, `in_flight_repo`) to the spawn request,
/// and the whole record replays.
#[test]
fn one_pool_spans_the_workspaces_and_the_record_replays() {
    let mut spawner = FakeSpawner::default();
    let mut busy = workspace(1, "acme/two", &many(3));
    busy.in_flight = 4;
    busy.weight = 16;
    let tick = run_knobs(
        vec![workspace(0, "acme/one", &many(30)), busy],
        limits(12),
        PoolKnobs {
            backlog_exponent: 0.5,
            busy_exponent: 1.0,
        },
        &mut spawner,
    );
    let repos: BTreeSet<usize> = spawner.asked.iter().map(|a| a.0).collect();
    assert_eq!(repos, BTreeSet::from([0, 1]), "both repos drew from the one pool");
    let record = tick.report.item_draw.as_ref().unwrap();
    assert_eq!(record.seed_scheme, "loom-draw/v3");
    assert_eq!(
        record
            .repos
            .iter()
            .map(|r| (r.repo.as_str(), r.weight, r.in_flight, r.items))
            .collect::<Vec<_>>(),
        vec![("acme/one", 1, 0, 30), ("acme/two", 16, 4, 3)]
    );
    // The spawn request, and so the step span, carries the weight terms.
    for (asked, terms) in spawner.asked.iter().zip(&spawner.terms) {
        let terms = terms.expect("a drawn step carries its terms");
        let (items, in_flight) = if asked.0 == 0 { (30, 0) } else { (3, 4) };
        assert!(terms.weight > 0 && terms.weight <= terms.total_weight);
        assert_eq!(terms.in_flight_repo, in_flight);
        assert!(terms.repo_items <= items, "the repo's pool shrinks as it is drawn");
    }
    assert_eq!(replay_record(record).unwrap().len(), tick.draws);
    // The old workspace draw is gone.
    assert!(tick.report.workspace_draw.is_none());
}

#[test]
fn a_zero_weight_workspace_is_never_drawn() {
    let mut spawner = FakeSpawner::default();
    let mut off = workspace(0, "acme/off", &many(5));
    off.weight = 0;
    let tick = run_with(vec![off, workspace(1, "acme/on", &many(5))], limits(8), &mut spawner);
    assert!(spawner.asked.iter().all(|a| a.0 == 1), "{:?}", spawner.asked);
    assert_eq!(tick.report.dispatched, 5, "the repo that is on drains, no more");
}

/// #12251: a Builder step refused on a landed issue reads as
/// `already_landed`, not as a dispatch error.
#[test]
fn a_landed_refusal_reads_as_already_landed() {
    assert_eq!(disposition_of("already_landed"), Qd::AlreadyLanded);
    assert_eq!(refusal_skip("already_landed"), PickSkipReason::AlreadyLanded);
}

/// #12105: a Doctor that stands down because another actor is working the PR
/// (`peer_active`), and a step the Judge backstop holds (`backstop_hold`),
/// read as a peer's claim: not a dispatch error, an in-flight skip.
#[test]
fn peer_active_and_backstop_hold_read_as_a_peer_claim() {
    for reason in ["peer_active", "backstop_hold"] {
        assert_eq!(disposition_of(reason), Qd::PeerClaim, "{reason}");
    }
    let mut spawner = FakeSpawner {
        refuse: vec![(3, "peer_active")],
        ..FakeSpawner::default()
    };
    let tick = run_with(vec![workspace(0, "acme/one", &forge())], limits(10), &mut spawner);
    assert_eq!(tick.report.errors, 0, "a stand-down is not a dispatch error");
    let record = tick.report.item_draw.unwrap();
    let refused = record
        .excluded
        .iter()
        .find(|e| e.reason == "refused_peer_active")
        .unwrap();
    assert_eq!(refused.skip_reason, PickSkipReason::InFlight);
}

// ---- #12298: concurrent spawns -------------------------------------------

fn concurrent(free_slots: usize, spawn_concurrency: usize) -> StepTickLimits {
    StepTickLimits {
        spawn_concurrency,
        ..limits(free_slots)
    }
}

fn repos(n: usize) -> Vec<StepWorkspace> {
    (0..n)
        .map(|i| workspace(i, &format!("acme/r{i}"), &forge()))
        .collect()
}

/// The items the spawner was asked for and did not refuse, by workspace.
fn spawned(spawner: &FakeSpawner, refused: &[u32]) -> Vec<(usize, u32, StepRole)> {
    spawner
        .asked
        .iter()
        .filter(|a| !refused.contains(&a.1))
        .map(|a| (a.0, a.1, a.2))
        .collect()
}

#[test]
fn a_round_spawns_at_most_one_step_per_repo_and_never_an_item_twice() {
    let mut spawner = FakeSpawner::default();
    let tick = run_with(repos(4), concurrent(12, 4), &mut spawner);
    assert!(spawner.batches.iter().any(|b| b.len() > 1), "{:?}", spawner.batches);
    for batch in &spawner.batches {
        assert!(batch.len() <= 4);
        let repos: BTreeSet<usize> = batch.iter().map(|(ws, _)| *ws).collect();
        assert_eq!(repos.len(), batch.len(), "two spawns in one repo: {batch:?}");
    }
    let asked: Vec<(usize, u32)> = spawner.asked.iter().map(|a| (a.0, a.1)).collect();
    let unique: BTreeSet<(usize, u32)> = asked.iter().copied().collect();
    assert_eq!(unique.len(), asked.len(), "an item was asked twice: {asked:?}");
    assert_eq!(tick.report.dispatched, 12);
    assert_eq!(
        tick.report
            .queue
            .iter()
            .filter(|r| r.disposition == Some(Qd::Dispatched))
            .count(),
        12
    );
}

#[test]
fn a_concurrent_tick_fills_every_free_slot() {
    for free in [3, 10, 5] {
        let mut spawner = FakeSpawner::default();
        let tick = run_with(repos(4), concurrent(free, 4), &mut spawner);
        // Four repos of six actionable items each: more than any `free`.
        let want = free;
        assert_eq!(tick.report.dispatched, want, "free {free}");
        assert_eq!(spawner.asked.len(), want, "free {free}");
    }
}

#[test]
fn one_champion_merge_per_repo_holds_across_rounds() {
    let rows = vec![
        row(6, &["loom:building"], "", false),
        row(60, &["loom:pr"], "Closes #6", true),
        row(7, &["loom:building"], "", false),
        row(70, &["loom:pr"], "Closes #7", true),
        row(8, &["loom:building"], "", false),
        row(80, &["loom:pr"], "Closes #8", true),
    ];
    let mut spawner = FakeSpawner::default();
    let tick = run_with(
        vec![
            workspace(0, "acme/one", &rows),
            workspace(1, "acme/two", &rows),
        ],
        concurrent(10, 4),
        &mut spawner,
    );
    for ws in 0..2 {
        let merges = spawned(&spawner, &[])
            .iter()
            .filter(|a| a.0 == ws && a.2 == StepRole::ChampionMerge)
            .count();
        assert_eq!(merges, 1, "repo {ws}: {:?}", spawner.asked);
    }
    assert_eq!(tick.report.dispatched, 2);
    // #12494: the other four merges are deferred, not passed. One drawn in
    // a round before its repo's merge spawned can still be met, carried
    // into the next round; it is deferred like the rest, never spawned.
    assert_eq!(tick.passes.get(PASS_MERGE), None);
    assert_eq!(tick.merge_deferred, 4);
    assert_eq!(tick.draws, 2 + tick.draws_unspawned);
    replay_record(tick.report.item_draw.as_ref().unwrap()).unwrap();
}

#[test]
fn one_merge_per_repo_holds_whatever_the_concurrency_and_seed() {
    // The invariant of design §6 under the #12494 deferral: across seeds,
    // concurrencies and repos, no repo spawns two merge steps in one tick,
    // no merge step is ever a pass, and every one not spawned is deferred.
    for counter in 0..24 {
        for conc in [1, 2, 4] {
            let mut spawner = FakeSpawner::default();
            let ws: Vec<StepWorkspace> = (0..3)
                .map(|i| workspace(i, &format!("acme/r{i}"), &merges_and_a_curator(4)))
                .collect();
            let tick = run(
                ws,
                concurrent(16, conc),
                &SeedBase {
                    tick_counter: counter,
                    ..seed()
                },
                &PoolKnobs::default(),
                &mut spawner,
            );
            for repo in 0..3 {
                let merges = spawner
                    .asked
                    .iter()
                    .filter(|a| a.0 == repo && a.2 == StepRole::ChampionMerge)
                    .count();
                assert_eq!(merges, 1, "seed {counter} conc {conc}: {:?}", spawner.asked);
            }
            assert_eq!(tick.passes.get(PASS_MERGE), None);
            assert_eq!(tick.merge_deferred, 3 * 3, "seed {counter} conc {conc}");
            assert_eq!(tick.report.dispatched, 6);
            replay_record(tick.report.item_draw.as_ref().unwrap()).unwrap();
        }
    }
}

#[test]
fn refusals_in_a_round_are_counted_in_draw_order() {
    // Every spawn refused: each attempt is one pass, the tick stops at the
    // pass cap, and nothing is dispatched.
    let mut spawner = FakeSpawner {
        refuse_all: Some("claimed"),
        ..FakeSpawner::default()
    };
    let rows: Vec<RestIssue> = (1..=40)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let ws: Vec<StepWorkspace> = (0..4)
        .map(|i| workspace(i, &format!("acme/r{i}"), &rows))
        .collect();
    let tick = run_with(ws, concurrent(4, 4), &mut spawner);
    assert_eq!(tick.report.dispatched, 0);
    assert_eq!(tick.passes.get("claimed"), Some(&spawner.asked.len()));
    assert!(spawner.asked.len() <= MAX_PASSES_PER_TICK as usize);
    assert!(tick
        .report
        .queue
        .iter()
        .filter(|r| r.disposition == Some(Qd::PeerClaim))
        .all(|r| r.detail.as_deref() == Some("claimed")));

    // Some refused: the dispatched count, the passes and the rows agree with
    // what the spawner answered.
    let mut spawner = FakeSpawner {
        refuse: vec![(1, "claimed"), (3, "pool_exhausted"), (40, "claim_lost")],
        ..FakeSpawner::default()
    };
    let tick = run_with(repos(4), concurrent(12, 4), &mut spawner);
    let refused = spawner.asked.len() - tick.report.dispatched;
    assert_eq!(tick.passes.values().sum::<usize>(), refused);
    let dispatched_rows = tick
        .report
        .queue
        .iter()
        .filter(|r| r.disposition == Some(Qd::Dispatched))
        .count();
    assert_eq!(dispatched_rows, tick.report.dispatched);
}

#[test]
fn a_concurrency_of_one_or_one_repo_is_the_serial_tick() {
    let order = |conc: usize, n: usize| {
        let mut spawner = FakeSpawner {
            refuse: vec![(3, "claimed")],
            ..FakeSpawner::default()
        };
        let tick = run_with(repos(n), concurrent(6, conc), &mut spawner);
        (spawner.asked, tick.report.dispatched, tick.passes)
    };
    // One repo: every round is one draw, whatever the concurrency.
    assert_eq!(order(4, 1), order(1, 1));
    // Several repos at a concurrency of 1: one spawn per round.
    let mut spawner = FakeSpawner::default();
    run_with(repos(3), concurrent(6, 1), &mut spawner);
    assert!(spawner.batches.iter().all(|b| b.len() == 1));
}

#[test]
fn a_breaker_tripped_mid_tick_stops_the_next_round() {
    let mut spawner = FakeSpawner {
        hold_after: Some(1),
        ..FakeSpawner::default()
    };
    let tick = run_with(repos(4), concurrent(12, 4), &mut spawner);
    // One round ran; the second was drawn but never spawned.
    assert_eq!(spawner.batches.len(), 1);
    assert_eq!(tick.report.dispatched, spawner.batches[0].len());
    assert_eq!(tick.report.spawn_hold.as_deref(), Some("rate_limit_breaker"));
    let held = tick
        .report
        .queue
        .iter()
        .filter(|r| r.detail.as_deref() == Some("rate_limit_breaker"))
        .count();
    assert!(held > 0);
    assert!(tick
        .report
        .queue
        .iter()
        .filter(|r| r.detail.as_deref() == Some("rate_limit_breaker"))
        .all(|r| r.disposition == Some(Qd::DeferredCapacity)));
    // Held before the first round: nothing is spawned at all.
    let mut spawner = FakeSpawner {
        hold_after: Some(0),
        ..FakeSpawner::default()
    };
    let tick = run_with(repos(4), concurrent(12, 4), &mut spawner);
    assert!(spawner.asked.is_empty());
    assert_eq!(tick.report.dispatched, 0);
}

/// #12260: an unverifiable claim means the owner's writer cannot claim
/// anything this tick, so no other workspace of that owner is drawn; another
/// owner's workspaces still are.
#[test]
fn an_unverified_claim_ends_the_owners_draws_for_the_tick() {
    let rows: Vec<RestIssue> = (1..=10)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let mut spawner = FakeSpawner {
        refuse_all: Some(REFUSAL_CLAIM_UNVERIFIED),
        ..FakeSpawner::default()
    };
    let tick = run_with(
        vec![
            workspace(0, "acme/one", &rows),
            workspace(1, "ACME/two", &rows),
        ],
        limits(10),
        &mut spawner,
    );
    // One draw, not MAX_PASSES_PER_TICK: both repos share the owner.
    assert_eq!(spawner.asked.len(), 1, "{:?}", spawner.asked);
    assert_eq!(tick.passes.get(REFUSAL_CLAIM_UNVERIFIED), Some(&1));
    assert_eq!(tick.report.dispatched, 0);

    // A second owner keeps drawing after the first owner's refusal.
    // Find the first acme draw, then refuse it.
    let mut spawner = FakeSpawner::default();
    run_with(
        vec![
            workspace(0, "acme/one", &rows),
            workspace(1, "acme/two", &rows),
            workspace(2, "other/three", &rows),
        ],
        limits(4),
        &mut spawner,
    );
    let first_acme = spawner.asked.iter().find(|a| a.0 != 2).copied().unwrap();
    let mut spawner = FakeSpawner {
        refuse: vec![(first_acme.1, REFUSAL_CLAIM_UNVERIFIED)],
        ..FakeSpawner::default()
    };
    let tick = run_with(
        vec![
            workspace(0, "acme/one", &rows),
            workspace(1, "acme/two", &rows),
            workspace(2, "other/three", &rows),
        ],
        limits(4),
        &mut spawner,
    );
    // The refused acme draw is the only acme draw that follows it.
    let refused_at = spawner
        .asked
        .iter()
        .position(|a| a.0 == first_acme.0 && a.1 == first_acme.1)
        .unwrap();
    assert!(spawner.asked[refused_at + 1..].iter().all(|a| a.0 == 2), "{:?}", spawner.asked);
    // `other/three` fills every slot the refusal left.
    assert_eq!(tick.report.dispatched, 4);
}

#[test]
fn other_refusals_still_redraw_in_the_same_owner() {
    let rows: Vec<RestIssue> = (1..=10)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let mut spawner = FakeSpawner {
        refuse_all: Some("claim_lost"),
        ..FakeSpawner::default()
    };
    run_with(
        vec![
            workspace(0, "acme/one", &rows),
            workspace(1, "acme/two", &rows),
        ],
        limits(1),
        &mut spawner,
    );
    // Neither owner's repo leaves the draw, and (#12410) a lost claim spends
    // no slot draw while the free ones last: all 20 items are tried.
    let repos: BTreeSet<usize> = spawner.asked.iter().map(|a| a.0).collect();
    assert_eq!(repos.len(), 2);
    assert_eq!(spawner.asked.len(), 20);
}

#[test]
fn forge_owner_is_the_lowercased_slug_prefix() {
    assert_eq!(forge_owner("AcMe/one"), "acme");
    assert_eq!(forge_owner("bare"), "bare");
}

// ---- #12450: fleet dispatch ------------------------------------------------

/// A `captain_held` refusal is a cheap pass like a lost claim; in fallback
/// neither is free, so the same refusals spend the slot's draws.
#[test]
fn a_captain_held_refusal_is_free_and_fallback_makes_no_loss_free() {
    let rows: Vec<RestIssue> = (1..=20)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let mut spawner = FakeSpawner {
        refuse_first: Some((10, "captain_held")),
        ..FakeSpawner::default()
    };
    let tick = run_with(vec![workspace(0, "acme/one", &rows)], limits(1), &mut spawner);
    assert_eq!(tick.report.dispatched, 1, "ten holds cost the slot nothing");
    assert_eq!(tick.passes.get("captain_held"), Some(&10));
    let row = tick
        .report
        .queue
        .iter()
        .find(|r| r.disposition == Some(Qd::PeerClaim));
    assert!(row.is_some(), "a held item reads as a peer's claim");

    for reason in ["claim_lost", "captain_held"] {
        let mut spawner = FakeSpawner {
            refuse_first: Some((10, reason)),
            ..FakeSpawner::default()
        };
        let fallback = StepTickLimits {
            fallback: true,
            ..limits(1)
        };
        let tick = run_with(vec![workspace(0, "acme/one", &rows)], fallback, &mut spawner);
        assert_eq!(tick.report.dispatched, 0, "{reason}: the slot gives up after its draws");
        assert_eq!(tick.stop, TickStop::SlotDraws, "{reason}");
        assert_eq!(spawner.asked.len(), MAX_DRAWS_PER_SLOT as usize, "{reason}");
    }
}

/// `captain_cap` ends every repo's draws for the tick: the captain would
/// refuse each later claim the same way.
#[test]
fn a_captain_cap_refusal_ends_the_ticks_draws() {
    let rows: Vec<RestIssue> = (1..=10)
        .map(|n| row(n, &["loom:triage"], "", false))
        .collect();
    let mut spawner = FakeSpawner {
        refuse_all: Some("captain_cap"),
        ..FakeSpawner::default()
    };
    let tick = run_with(
        vec![
            workspace(0, "acme/one", &rows),
            workspace(1, "other/two", &rows),
        ],
        limits(4),
        &mut spawner,
    );
    assert_eq!(spawner.asked.len(), 1, "{:?}", spawner.asked);
    assert_eq!(tick.report.dispatched, 0);
    assert_eq!(tick.stop, TickStop::CaptainCap, "its own stop, not pool_drained");
    assert_eq!(TickStop::CaptainCap.as_str(), "captain_cap");
    assert_eq!(disposition_of("captain_cap"), Qd::DeferredCapacity);
    assert_eq!(disposition_of("captain_not_eligible"), Qd::HostConstraint);
}
