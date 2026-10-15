//! The step tick (#11909; design #11894 §4.2): the work finder's only item
//! dispatch path.
//!
//! Each tick, for every free slot:
//!
//! 1. **Draw a workspace** among those with items left in their pool, with
//!    the workspace draw's weights and bonuses
//!    ([`crate::priority_pick::draw_workspace`], seeded `loom-draw/v1` once
//!    per tick, #11103).
//! 2. **Draw an item** from it ([`ItemDraw::try_next`], `loom-draw/v2`,
//!    seeded per draw with the workspace's forge slug, the name
//!    `pick.decision` gives it, so the logged draws replay). A
//!    [`DrawError::WeightOverflow`] ends that workspace's draws for the tick
//!    and is recorded on the record, never only logged.
//! 3. **Gate**: the per-repo RAM, disk and count cap
//!    ([`RepoCap::defer_admission`]). A repo that does not fit is out of the
//!    draw for the rest of the tick.
//! 4. **Claim and spawn** through the [`StepSpawner`] (the registry's step
//!    spawn, #11906): the role's resolvers and pool gate, then the claim.
//!
//! Slots are filled in **rounds** (#12298): each round draws one candidate
//! for each of up to `spawn_concurrency` open slots, never two in one repo,
//! runs the gates for all of them in draw order (the round's earlier
//! candidates count as admitted), and hands the batch to
//! [`StepSpawner::spawn_batch`], which may spawn them at once. The answers
//! are applied in draw order. So the claim and the step ledger stay per
//! item and ordered within a repo, a repo spawns at most one step per round,
//! and with a concurrency of 1 the tick is the serial one.
//!
//! A refused gate or claim is a **pass**: draw again, up to
//! [`MAX_DRAWS_PER_SLOT`] draws in one slot and [`MAX_PASSES_PER_TICK`]
//! passes in the tick. Every pass is counted by its reason. Saturation and
//! the ramp cap are host-wide, so they bound how many slots the tick fills
//! rather than refusing one draw.
//!
//! Pure over its inputs: the listing, the classification and the weighing
//! happen before ([`super::gather`]), and the spawn is the caller's. That
//! is what lets one test drive a whole tick with a fake forge and a fake
//! spawner.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use super::item_draw::{Candidate, DrawError, DrawRecord, ItemDraw, SeedKey, Weighing};
use crate::observability::pick_decision::{item_draw_record, ItemDrawWorkspace};
use crate::priority_pick::{self, Level, PickRng, WeightBonus, WorkspaceEntry};
use crate::step_role::StepRole;
use crate::sweep_registry::step_spawn::draw_claims::DrawClaims;
use crate::sweep_registry::step_spawn::StepSpawnRequest;
use crate::telemetry::kinds::pick_decision::{PickItemExclusion, PickSkipReason};
use crate::types::QueueDisposition as Qd;
use crate::work_finder::ready_queue::TickQueueRow;
use crate::work_finder::repo_cap::RepoCap;
use crate::work_finder::step_item::{Lane, PassReason};
use crate::work_finder::workspace_draw::{self, DrawLog, DrawStep, SeedInputs};
use crate::work_finder::{PriorityCandidate, TickReport};

/// At most this many item draws fill one slot (design §4.2 step 4).
pub const MAX_DRAWS_PER_SLOT: u32 = 8;
/// At most this many passes in one tick (design §4.2 step 4).
pub const MAX_PASSES_PER_TICK: u32 = 32;

/// The pass reason of a draw the per-repo cap refused.
pub const PASS_REPO_CAP: &str = "repo_cap";
/// The pass reason of a second Champion merge in one repo in one tick.
pub const PASS_MERGE: &str = "merge_in_flight";

static TICK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The next value of this process's tick counter, the `loom-draw/v2` seed's
/// third input.
#[must_use]
pub fn next_tick_counter() -> u64 {
    TICK_COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// One workspace as the tick draws from it.
#[derive(Debug, Clone)]
pub struct StepWorkspace {
    /// Its index among the tick's roots.
    pub idx: usize,
    /// The forge slug: the `repo` `pick.decision` names and the item seed's
    /// `workspace` input.
    pub repo: String,
    /// Its roster weight (0: never drawn).
    pub weight: u32,
    /// Its items, weighed.
    pub weighing: Weighing,
    /// A hold keeps it out of the tick (red main, drain, preflight, ...).
    pub halted: bool,
    /// Creation times by target number, for the queue rows' age key.
    pub created_at: HashMap<u32, String>,
}

/// The host-wide limits of one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepTickLimits {
    /// `max_concurrent - occupancy`, saturating.
    pub free_slots: usize,
    /// The ramp cap, `max_admissions_per_tick`.
    pub max_admissions: usize,
    /// The saturation brake holds new admissions.
    pub saturation_held: bool,
    /// How many spawns, each in a different repo, run at once (#12298,
    /// `autonomous.workFinder.spawnConcurrency`); `1` is the serial tick.
    pub spawn_concurrency: usize,
}

/// What the item draws are seeded from, besides the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedBase {
    pub minute: u64,
    pub host: String,
    pub tick_counter: u64,
}

/// What a spawn attempt came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnResult {
    /// The step runs.
    Spawned,
    /// A gate or the claim refused it: a pass, with its closed reason token
    /// (`loom.dispatch.step`'s `loom.dispatch.reason`).
    Refused(String),
}

/// Spawns one step in workspace `idx`.
pub trait StepSpawner {
    fn spawn(&mut self, idx: usize, request: &StepSpawnRequest) -> SpawnResult;

    /// Spawn one round's steps (#12298), each in a different workspace, and
    /// answer **in `batch` order**. The tick applies the answers in that
    /// order, so a spawner may run them concurrently. The default runs them
    /// one after another.
    /// Whether spawning must stop for the rest of the tick (#12298): the
    /// shared GitHub rate-limit breaker tripped since the tick started, so
    /// every pre-claim would be a failed forge call. Asked before each
    /// round's batch. The default never stops.
    fn held(&mut self) -> Option<&'static str> {
        None
    }

    fn spawn_batch(&mut self, batch: &[(usize, StepSpawnRequest)]) -> Vec<SpawnResult> {
        batch
            .iter()
            .map(|(idx, request)| self.spawn(*idx, request))
            .collect()
    }
}

/// One tick's outcome: the report every tick consumer reads, plus the passes
/// by reason.
#[derive(Debug, Clone, Default)]
pub struct StepTick {
    pub report: TickReport,
    /// Passes by reason (`claimed`, `pool_exhausted`, `repo_cap`, ...).
    pub passes: BTreeMap<String, usize>,
    /// Draws taken, across workspaces.
    pub draws: usize,
}

/// The queue disposition a pass reason reads as.
#[must_use]
pub fn disposition_of(reason: &str) -> Qd {
    match reason {
        // #12105: a Doctor standing down on another actor's work
        // (`peer_active`), or a step held by the Judge no-verdict backstop
        // (`backstop_hold`), is someone else's claim, not a dispatch error.
        "claimed" | "claim_lost" | "peer_active" | "backstop_hold" => Qd::PeerClaim,
        "pool_exhausted"
        | "no_token_pool"
        | "token_selection_failed"
        | "runtime_rejected"
        | "model_runtime_mismatch"
        | "admission_refused" => Qd::HostConstraint,
        "roll_paused" => Qd::DeferredCapacity,
        "workspace_commands_missing" => Qd::WorkspaceCommandsMissing,
        PASS_REPO_CAP => Qd::DeferredRepoCap,
        PASS_MERGE => Qd::PeerClaim,
        _ => Qd::DispatchError,
    }
}

/// Take out of `w`'s pool every item `claims` names (this host's live runs
/// and the peers' live claims), and every Champion merge but the one a peer
/// is merging while a peer merge is in flight in this repo. Each counts as
/// a `claimed` pass. Pure.
pub fn exclude_claimed(w: &mut Weighing, claims: &DrawClaims) {
    use super::item_draw::Exclusion;
    let before = w.pool.len();
    w.pool.retain(|c| {
        let target = c.target();
        let merging_elsewhere = c.step == crate::work_finder::step_item::Step::ChampionMerge
            || c.step == crate::work_finder::step_item::Step::ChampionHoldReevaluate;
        !(claims.items.contains(&target)
            || (merging_elsewhere
                && claims
                    .merge_in_flight
                    .as_ref()
                    .is_some_and(|(pr, _)| *pr != target)))
    });
    let removed = before - w.pool.len();
    if removed > 0 {
        *w.excluded
            .entry(Exclusion::Pass(PassReason::Claimed))
            .or_default() += removed;
    }
}

/// Take every Builder step out of `w`'s pool, as a `shared_pr` pass: the
/// repo's open-PR listing failed, so a PR linked only by its branch is
/// invisible and a Builder step could duplicate it. Pure.
pub fn exclude_builders(w: &mut Weighing) -> usize {
    use super::item_draw::Exclusion;
    let before = w.pool.len();
    w.pool
        .retain(|c| c.step != crate::work_finder::step_item::Step::Builder);
    let removed = before - w.pool.len();
    if removed > 0 {
        *w.excluded
            .entry(Exclusion::Pass(PassReason::SharedPr))
            .or_default() += removed;
    }
    removed
}

/// The closed-set skip reason a spawn refusal reads as on `pick.decision`.
fn refusal_skip(reason: &str) -> PickSkipReason {
    match disposition_of(reason) {
        Qd::PeerClaim => PickSkipReason::InFlight,
        Qd::HostConstraint => PickSkipReason::Quota,
        Qd::DeferredCapacity | Qd::DeferredRepoCap => PickSkipReason::Cap,
        _ => PickSkipReason::Error,
    }
}

/// Fold the weight-0 counts onto the tick report's skip counters, so the
/// tick line and the summary keep reading.
fn count_exclusions(report: &mut TickReport, w: &Weighing) {
    use super::item_draw::Exclusion as E;
    for (reason, &n) in &w.excluded {
        let slot = match reason {
            E::Pass(PassReason::Claimed | PassReason::Queued) => &mut report.skipped_in_flight,
            E::Pass(PassReason::SharedPr) => &mut report.skipped_pr_open,
            E::Pass(PassReason::CapabilityMissing) | E::PoolExhausted => {
                &mut report.skipped_host_constraint
            }
            E::BuildBackoff => &mut report.deferred_build_backoff,
            E::CoolingDown | E::Pass(PassReason::CiPending) => &mut report.skipped_noop_cooldown,
            E::Pass(PassReason::HardExclusion) => &mut report.skipped_declined,
            E::Pass(
                PassReason::Frozen
                | PassReason::Operator
                | PassReason::Sequenced
                | PassReason::Blocked
                | PassReason::Epic
                | PassReason::Proposal
                | PassReason::NoStage,
            ) => &mut report.skipped_labeled,
        };
        *slot += n;
    }
}

fn key_of(ws: &StepWorkspace, c: &Candidate) -> PriorityCandidate {
    PriorityCandidate {
        workspace_idx: ws.idx,
        workspace_weight: ws.weight,
        created_at: ws.created_at.get(&c.target()).cloned(),
        number: c.target(),
        level: c.terms.level,
        main_red_fix: c.item.lane == Lane::RedMain,
    }
}

/// A candidate a round will spawn (#12298), in draw order.
struct Pending {
    /// Its open slot's position in the round.
    slot: usize,
    /// Its workspace's position in the tick's drawing state.
    pos: usize,
    ws_idx: usize,
    key: PriorityCandidate,
    merge: bool,
    request: StepSpawnRequest,
}

/// One workspace's draw state.
struct Drawing {
    ws: StepWorkspace,
    draw: ItemDraw,
    draws: Vec<DrawRecord>,
    /// A Champion merge was spawned in this repo this tick.
    merging: bool,
    /// Draws the spawn or the per-repo cap refused, by reason.
    refused: BTreeMap<String, usize>,
    /// Out of the draw for the rest of the tick (empty, capped, overflowed).
    closed: bool,
    overflow: bool,
}

impl Drawing {
    fn entry(&self) -> WorkspaceEntry {
        let left = self.draw.remaining();
        WorkspaceEntry {
            workspace: workspace_draw::workspace_key(self.ws.idx),
            weight: self.ws.weight,
            has_red_main_fix: left.iter().any(|c| c.item.lane == Lane::RedMain),
            has_very_important: left.iter().any(|c| c.terms.level == Level::VeryImportant),
            has_important: left.iter().any(|c| c.terms.level == Level::Important),
        }
    }
}

/// Run one step tick over `workspaces` (module docs).
#[allow(clippy::too_many_lines)] // one loop, kept whole so the pass rules read in order.
pub fn run(
    workspaces: Vec<StepWorkspace>,
    limits: StepTickLimits,
    seed: &SeedBase,
    (workspace_seed, seed_inputs): (u64, Option<SeedInputs>),
    bonus: &WeightBonus,
    repo_cap: &mut RepoCap,
    spawner: &mut dyn StepSpawner,
) -> StepTick {
    let mut pass_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut draws_taken = 0usize;
    let mut report = TickReport {
        saturation_held: limits.saturation_held,
        max_admissions_per_tick: Some(limits.max_admissions),
        repo_cap: Some(repo_cap.snapshot()),
        ..TickReport::default()
    };

    let mut drawing: Vec<Drawing> = Vec::new();
    for ws in workspaces {
        count_exclusions(&mut report, &ws.weighing);
        report.seen += ws.weighing.pool.len() + ws.weighing.excluded.values().sum::<usize>();
        report
            .vi_demotions
            .extend(ws.weighing.demotions.iter().cloned());
        for c in &ws.weighing.pool {
            report.queue.push(TickQueueRow {
                key: key_of(&ws, c),
                disposition: ws.halted.then_some(Qd::WorkspaceHalted),
                detail: None,
                updated_at: None,
                held_until: None,
                story_points: None,
                step: Some(c.step),
            });
        }
        let key = SeedKey {
            minute: seed.minute,
            host: seed.host.clone(),
            tick_counter: seed.tick_counter,
            workspace: ws.repo.clone(),
        };
        let draw = ItemDraw::new(ws.weighing.pool.clone(), key);
        let closed = ws.halted || ws.weighing.pool.is_empty() || ws.weight == 0;
        drawing.push(Drawing {
            ws,
            draw,
            draws: Vec::new(),
            refused: BTreeMap::new(),
            merging: false,
            closed,
            overflow: false,
        });
    }

    let budget = if limits.saturation_held {
        0
    } else {
        limits.free_slots.min(limits.max_admissions)
    };
    let mut rng = PickRng::from_seed(workspace_seed);
    let mut steps: Vec<DrawStep> = Vec::new();
    let mut draws_total = 0usize;
    let mut passes = 0u32;
    let mut order: Vec<(usize, u32)> = Vec::new();
    let mut exhausted = false;
    // #12298: slots are filled in rounds. Each round draws one candidate
    // for up to `spawn_concurrency` open slots, each in a repo no other
    // candidate of the round is in, and spawns them as one batch; the
    // results are applied in draw order. A refused slot stays open for the
    // next round. With a concurrency of 1 every round is one draw and one
    // spawn, which is the serial tick exactly.
    let concurrency = limits.spawn_concurrency.max(1);
    // Draw attempts of each open slot, in the order the slots opened.
    let mut open: Vec<u32> = Vec::new();
    let mut opened = 0usize;
    loop {
        while open.len() < concurrency && opened < budget {
            open.push(0);
            opened += 1;
        }
        if open.is_empty() || passes >= MAX_PASSES_PER_TICK {
            break;
        }
        // The round's gates see the round's earlier candidates as admitted,
        // so a batch cannot overrun a cap the candidates share.
        let mut planned = repo_cap.clone();
        let mut batch: Vec<Pending> = Vec::new();
        let mut done: BTreeSet<usize> = BTreeSet::new();
        'slots: for (slot, attempts) in open.iter_mut().enumerate() {
            loop {
                if passes >= MAX_PASSES_PER_TICK {
                    break 'slots;
                }
                if *attempts >= MAX_DRAWS_PER_SLOT {
                    done.insert(slot);
                    break;
                }
                let entries: Vec<WorkspaceEntry> = drawing
                    .iter()
                    .filter(|d| {
                        !d.closed
                            && !d.draw.remaining().is_empty()
                            && !batch.iter().any(|p| p.ws_idx == d.ws.idx)
                    })
                    .map(Drawing::entry)
                    .collect();
                let Some(ws_draw) = priority_pick::draw_workspace(&entries, bonus, &mut rng) else {
                    // Nothing left to draw outside this round's repos: the
                    // tick is exhausted when the round is empty, else the
                    // slot waits for the next round.
                    exhausted = batch.is_empty();
                    break 'slots;
                };
                let Some(pos) = workspace_draw::workspace_idx(&ws_draw.picked)
                    .and_then(|idx| drawing.iter().position(|d| d.ws.idx == idx))
                else {
                    exhausted = batch.is_empty();
                    break 'slots;
                };
                let d = &mut drawing[pos];
                draws_total += 1;
                let record = match d.draw.try_next() {
                    Ok(Some(record)) => record,
                    Ok(None) => {
                        d.closed = true;
                        continue;
                    }
                    Err(DrawError::WeightOverflow) => {
                        log::error!(
                            "work_finder: {}: item draw stopped — {} ({} items pooled)",
                            d.ws.repo,
                            DrawError::WeightOverflow,
                            d.draw.remaining().len()
                        );
                        d.overflow = true;
                        d.closed = true;
                        continue;
                    }
                };
                *attempts += 1;
                let key = key_of(&d.ws, &record.picked);
                if steps.len() < workspace_draw::MAX_RECORDED_DRAWS {
                    steps.push(DrawStep {
                        draw: ws_draw,
                        workspace_idx: d.ws.idx,
                        issue: key.number,
                    });
                }
                order.push((d.ws.idx, key.number));
                let picked = record.picked.clone();
                d.draws.push(record);
                draws_taken += 1;
                if planned.defer_admission(&key, &mut report) {
                    d.closed = true;
                    passes += 1;
                    *pass_counts.entry(PASS_REPO_CAP.to_string()).or_default() += 1;
                    *d.refused.entry(PASS_REPO_CAP.to_string()).or_default() += 1;
                    continue;
                }
                // One Champion merge per repo at a time (design §6): a second
                // merge drawn in a repo that spawned one this tick passes.
                let merge = matches!(
                    picked.step,
                    crate::work_finder::step_item::Step::ChampionMerge
                        | crate::work_finder::step_item::Step::ChampionHoldReevaluate
                );
                if merge && d.merging {
                    passes += 1;
                    resolve(&mut report.queue, &key, Qd::PeerClaim, Some(PASS_MERGE.to_string()));
                    *d.refused.entry(PASS_MERGE.to_string()).or_default() += 1;
                    *pass_counts.entry(PASS_MERGE.to_string()).or_default() += 1;
                    continue;
                }
                planned.admit_candidate(&key);
                batch.push(Pending {
                    slot,
                    pos,
                    ws_idx: d.ws.idx,
                    merge,
                    request: StepSpawnRequest {
                        item: key.number,
                        role: StepRole::from(picked.step),
                        draw_attempt: *attempts,
                        age_source: picked.terms.age_source.as_str().to_string(),
                        resumed: false,
                    },
                    key,
                });
                break;
            }
        }
        if batch.is_empty() && exhausted {
            break;
        }
        let requests: Vec<(usize, StepSpawnRequest)> = batch
            .iter()
            .map(|p| (p.ws_idx, p.request.clone()))
            .collect();
        // A breaker that tripped mid-tick ends the spawns (#12298): the
        // drawn candidates wait for a later tick, named by the hold, and
        // no further round runs. Checked per round, so concurrent spawns
        // cannot burst failed calls into an exhausted budget.
        if !requests.is_empty() {
            if let Some(hold) = spawner.held() {
                log::info!(
                    "work_finder: step spawns stopped for this tick — {hold}; {} drawn \
                     candidate(s) wait",
                    batch.len()
                );
                for p in &batch {
                    resolve(
                        &mut report.queue,
                        &p.key,
                        Qd::DeferredCapacity,
                        Some(hold.to_string()),
                    );
                    report.deferred_capacity += 1;
                }
                report.spawn_hold = Some(hold.to_string());
                break;
            }
        }
        let results = if requests.is_empty() {
            Vec::new()
        } else {
            spawner.spawn_batch(&requests)
        };
        // Apply in draw order, whatever order the spawns finished in. A
        // spawner that answered short refused the rest.
        let results = results
            .into_iter()
            .chain(std::iter::repeat_with(|| SpawnResult::Refused("spawn_failed".to_string())));
        for (p, result) in batch.into_iter().zip(results) {
            let key = p.key;
            let d = &mut drawing[p.pos];
            match result {
                SpawnResult::Spawned => {
                    d.merging |= p.merge;
                    resolve(&mut report.queue, &key, Qd::Dispatched, None);
                    report.dispatched += 1;
                    repo_cap.admit_candidate(&key);
                    done.insert(p.slot);
                }
                SpawnResult::Refused(reason) => {
                    let disposition = disposition_of(&reason);
                    if disposition == Qd::DispatchError {
                        report.errors += 1;
                    }
                    resolve(&mut report.queue, &key, disposition, Some(reason.clone()));
                    passes += 1;
                    *d.refused.entry(reason.clone()).or_default() += 1;
                    *pass_counts.entry(reason).or_default() += 1;
                }
            }
        }
        // Filled and given-up slots close; the rest draw again.
        let mut slot = 0usize;
        open.retain(|_| {
            let keep = !done.contains(&slot);
            slot += 1;
            keep
        });
        if exhausted {
            break;
        }
    }

    // Rows nothing resolved wait for a later tick: name the limit that held
    // them.
    let held = if limits.saturation_held {
        Qd::DeferredSaturation
    } else if limits.max_admissions < limits.free_slots
        && report.dispatched >= limits.max_admissions
    {
        Qd::DeferredRampCap
    } else {
        Qd::DeferredCapacity
    };
    for row in report.queue.iter_mut().filter(|r| r.disposition.is_none()) {
        row.disposition = Some(held);
        match held {
            Qd::DeferredSaturation => report.deferred_saturation += 1,
            Qd::DeferredRampCap => report.deferred_ramp_cap += 1,
            _ => report.deferred_capacity += 1,
        }
    }
    // The plan order: the draws as taken, then every pooled item not drawn.
    let drawn: BTreeSet<(usize, u32)> = order.iter().copied().collect();
    for d in &drawing {
        for c in d.draw.remaining() {
            if !d.ws.halted && !drawn.contains(&(d.ws.idx, c.target())) {
                order.push((d.ws.idx, c.target()));
            }
        }
    }
    report.plan_order = order;
    repo_cap.finish(&mut report);
    report.workspace_draw = (draws_total > 0).then_some(DrawLog {
        seed: workspace_seed,
        draws_total,
        steps,
        seed_inputs,
    });
    let views: Vec<ItemDrawWorkspace<'_>> = drawing
        .iter()
        .filter(|d| !d.draws.is_empty() || d.overflow)
        .map(|d| ItemDrawWorkspace {
            repo: &d.ws.repo,
            excluded: &d.ws.weighing.excluded,
            pool: d.draw.starting_pool(),
            draws: &d.draws,
            overflow: d.overflow,
        })
        .collect();
    report.item_draw = item_draw_record(seed.minute, &seed.host, seed.tick_counter, &views);
    // The draws a gate or the claim refused, beside the weight-0 counts, so
    // a refused draw is visible on the record (the integrated review's M1).
    if let Some(record) = report.item_draw.as_mut() {
        for d in &drawing {
            for (reason, &count) in &d.refused {
                record.excluded.push(PickItemExclusion {
                    repo: d.ws.repo.clone(),
                    reason: format!("refused_{reason}"),
                    skip_reason: refusal_skip(reason),
                    count,
                });
            }
        }
    }
    StepTick {
        report,
        passes: pass_counts,
        draws: draws_taken,
    }
}

/// Resolve the still-open queue row of `key` (the first one: an item has
/// one row per tick).
fn resolve(rows: &mut [TickQueueRow], key: &PriorityCandidate, d: Qd, detail: Option<String>) {
    crate::work_finder::ready_queue::resolve(rows, key, d, detail);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
#[path = "tick_tests.rs"]
pub(crate) mod tests;
