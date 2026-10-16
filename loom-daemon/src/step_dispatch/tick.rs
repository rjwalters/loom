//! The step tick (#11909; design #11894 §4.2): the work finder's only item
//! dispatch path.
//!
//! Each tick, for every free slot:
//!
//! 1. **Draw an item** from one pool of every actionable item across the
//!    host's workspaces ([`ItemDraw::try_next`], `loom-draw/v3`, seeded per
//!    draw, #12185). An item's weight is its priority and age scaled by its
//!    repo's roster weight, backlog and in-flight count
//!    ([`super::item_draw::pool_weights`]); there is no separate workspace
//!    draw. A [`DrawError::WeightOverflow`] ends the tick's draws and is
//!    recorded on the record, never only logged.
//! 2. **Gate**: the per-repo RAM, disk and count cap
//!    ([`RepoCap::defer_admission`]). A repo that does not fit is out of the
//!    draw for the rest of the tick.
//! 3. **Claim and spawn** through the [`StepSpawner`] (the registry's step
//!    spawn, #11906): the role's resolvers and pool gate, then the claim.
//!
//! Slots are filled in **rounds** (#12298): each round draws one candidate
//! for each of up to `spawn_concurrency` open slots, never two in one repo,
//! runs the gates for all of them in draw order (the round's earlier
//! candidates count as admitted), and hands the batch to
//! [`StepSpawner::spawn_batch`], which may spawn them at once. The answers
//! are applied in draw order. So the claim and the step ledger stay per
//! item and ordered within a repo, a repo spawns at most one step per round,
//! and with a concurrency of 1 the tick is the serial one. The draws are
//! the pool's draws in order: one that lands in a repo the round already
//! holds starts the next round instead of being put back.
//!
//! A refused gate or claim is a **pass**: draw again, up to
//! [`MAX_DRAWS_PER_SLOT`] draws in one slot and [`MAX_PASSES_PER_TICK`]
//! passes in the tick. Every pass is counted by its reason. Saturation and
//! the ramp cap are host-wide, so they bound how many slots the tick fills
//! rather than refusing one draw.
//!
//! A **lost claim** ([`REFUSAL_CLAIM_LOST`]: a peer claimed the item first)
//! is a cheap pass (#12410): the item has already left the tick's pool (draws
//! are without replacement), and the slot draws again at once, but the loss
//! spends neither the tick's [`MAX_PASSES_PER_TICK`] nor the slot's
//! [`MAX_DRAWS_PER_SLOT`]. Losing is a fact about a peer, not about this
//! host's gates, so on a fleet where every host draws the same heavy items it
//! must not starve the tick of attempts. It has its own bound,
//! [`MAX_FREE_CLAIM_LOST_PER_TICK`], past which a loss is an ordinary pass.
//!
//! Every tick also says why its draw loop stopped ([`TickStop`], the tick
//! span's `loom.dispatch.stop`) and how many slots it could fill
//! (`loom.dispatch.budget`), so a tick with free slots and few attempts names
//! its limit.
//!
//! An **unverifiable claim** ([`REFUSAL_CLAIM_UNVERIFIED`]) is the exception
//! to "draw again" (#12260): the pre-claim's lease reads and post run on the
//! forge owner's writer installation, so when one cannot be verified (in
//! practice, that installation is rate-limited) every other draw of the
//! same owner fails the same way. Every workspace of that owner leaves the
//! draw for the rest of the tick; other owners keep drawing.
//!
//! Pure over its inputs: the listing, the classification and the weighing
//! happen before ([`super::gather`]), and the spawn is the caller's. That
//! is what lets one test drive a whole tick with a fake forge and a fake
//! spawner.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};

use super::item_draw::{
    Candidate, DrawError, DrawRecord, ItemDraw, PoolKnobs, RepoTerms, SeedKey, Weighing,
};
use crate::observability::pick_decision::{item_draw_record, ItemDrawPool, ItemDrawWorkspace};
use crate::step_role::StepRole;
use crate::sweep_registry::step_spawn::draw_claims::DrawClaims;
use crate::sweep_registry::step_spawn::{StepDrawTerms, StepSpawnRequest};
use crate::telemetry::kinds::pick_decision::{PickItemExclusion, PickSkipReason};
use crate::types::QueueDisposition as Qd;
use crate::work_finder::ready_queue::TickQueueRow;
use crate::work_finder::repo_cap::RepoCap;
use crate::work_finder::step_item::{Lane, PassReason};
use crate::work_finder::{PriorityCandidate, TickReport};

/// At most this many item draws fill one slot (design §4.2 step 4).
pub const MAX_DRAWS_PER_SLOT: u32 = 8;
/// At most this many passes in one tick (design §4.2 step 4).
pub const MAX_PASSES_PER_TICK: u32 = 32;

/// At most this many `claim_lost` refusals in one tick are free (#12410):
/// they spend no pass and no slot draw. Past it a loss is an ordinary pass,
/// so a tick's draws stay bounded (`MAX_PASSES_PER_TICK` +
/// `MAX_FREE_CLAIM_LOST_PER_TICK` refusals) while each loss still costs a
/// claim subprocess.
pub const MAX_FREE_CLAIM_LOST_PER_TICK: u32 = 16;

/// The pass reason of a draw the per-repo cap refused.
pub const PASS_REPO_CAP: &str = "repo_cap";
/// The pass reason of a second Champion merge in one repo in one tick.
pub const PASS_MERGE: &str = "merge_in_flight";
/// The spawn refusal of a pre-claim that could not be verified
/// (`sweep_registry::step_spawn::lease`): it ends the owner's draws for the
/// tick (module docs, #12260).
pub const REFUSAL_CLAIM_UNVERIFIED: &str = "claim_unverified";
/// The spawn refusal of a claim a peer won first: a cheap pass (module docs,
/// #12410).
pub const REFUSAL_CLAIM_LOST: &str = "claim_lost";

/// Why a tick's draw loop stopped (#12410): the tick span's
/// `loom.dispatch.stop`, a closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TickStop {
    /// Every slot the tick could fill was filled.
    #[default]
    Filled,
    /// Every slot was filled, but the ramp cap (`maxAdmissionsPerTick`)
    /// allowed fewer slots than were free.
    RampCap,
    /// The saturation brake held every admission.
    Saturation,
    /// No slot was free.
    NoSlots,
    /// The pool had nothing left to draw.
    PoolDrained,
    /// The tick spent its [`MAX_PASSES_PER_TICK`] passes.
    PassLimit,
    /// Every open slot gave up after [`MAX_DRAWS_PER_SLOT`] draws.
    SlotDraws,
    /// The spawner held the spawns (the shared rate-limit breaker, #12298).
    SpawnHold,
    /// The pool's total weight overflowed ([`DrawError::WeightOverflow`]).
    WeightOverflow,
}

impl TickStop {
    /// The attribute value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Filled => "filled",
            Self::RampCap => "ramp_cap",
            Self::Saturation => "saturation",
            Self::NoSlots => "no_slots",
            Self::PoolDrained => "pool_drained",
            Self::PassLimit => "pass_limit",
            Self::SlotDraws => "slot_draws",
            Self::SpawnHold => "spawn_hold",
            Self::WeightOverflow => "weight_overflow",
        }
    }
}

/// The forge owner of a workspace's `owner/repo` slug, lowercased (GitHub
/// owners are case-insensitive). Pure.
#[must_use]
pub fn forge_owner(repo: &str) -> String {
    repo.split_once('/')
        .map_or(repo, |(owner, _)| owner)
        .to_ascii_lowercase()
}

static TICK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The next value of this process's tick counter, the `loom-draw/v3` seed's
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
    /// The forge slug: the `repo` `pick.decision` names.
    pub repo: String,
    /// Its roster weight `w_repo` (0: never drawn).
    pub weight: u32,
    /// `in_flight_repo`: its items this host runs or a peer advertises a
    /// live claim on, counted before the draw takes them out of the pool.
    pub in_flight: usize,
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

/// What the item draws are seeded from.
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
    /// The slots the tick could fill: the free slots, bounded by the ramp
    /// cap, `0` under the saturation brake.
    pub budget: usize,
    /// Why the draw loop stopped.
    pub stop: TickStop,
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
        crate::sweep_registry::ALREADY_LANDED_REASON => Qd::AlreadyLanded,
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
        Qd::AlreadyLanded => PickSkipReason::AlreadyLanded,
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
    ws_idx: usize,
    key: PriorityCandidate,
    merge: bool,
    request: StepSpawnRequest,
}

/// Run one step tick over `workspaces` (module docs).
#[allow(clippy::too_many_lines)] // one loop, kept whole so the pass rules read in order.
pub fn run(
    mut workspaces: Vec<StepWorkspace>,
    limits: StepTickLimits,
    seed: &SeedBase,
    knobs: &PoolKnobs,
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

    // The pool is every repo's items, repos in workspace order.
    workspaces.sort_by_key(|w| w.idx);
    let mut repos: BTreeMap<usize, RepoTerms> = BTreeMap::new();
    let mut pool: Vec<Candidate> = Vec::new();
    for ws in &workspaces {
        count_exclusions(&mut report, &ws.weighing);
        report.seen += ws.weighing.pool.len() + ws.weighing.excluded.values().sum::<usize>();
        report
            .vi_demotions
            .extend(ws.weighing.demotions.iter().cloned());
        for c in &ws.weighing.pool {
            report.queue.push(TickQueueRow {
                key: key_of(ws, c),
                disposition: ws.halted.then_some(Qd::WorkspaceHalted),
                detail: None,
                updated_at: None,
                held_until: None,
                story_points: None,
                step: Some(c.step),
            });
        }
        if !ws.halted {
            repos.insert(
                ws.idx,
                RepoTerms {
                    weight: ws.weight,
                    in_flight: ws.in_flight,
                },
            );
            pool.extend(ws.weighing.pool.iter().cloned());
        }
    }
    let by_idx: BTreeMap<usize, &StepWorkspace> = workspaces.iter().map(|w| (w.idx, w)).collect();
    let mut draw = ItemDraw::new(
        pool,
        repos,
        *knobs,
        SeedKey {
            minute: seed.minute,
            host: seed.host.clone(),
            tick_counter: seed.tick_counter,
        },
    );
    let mut draws: Vec<DrawRecord> = Vec::new();
    // A Champion merge was spawned in the repo this tick.
    let mut merging: BTreeSet<usize> = BTreeSet::new();
    // Draws the spawn or the per-repo cap refused, by repo and reason.
    let mut refused: BTreeMap<usize, BTreeMap<String, usize>> = BTreeMap::new();
    let mut overflow = false;

    let budget = if limits.saturation_held {
        0
    } else {
        limits.free_slots.min(limits.max_admissions)
    };
    let mut passes = 0u32;
    let mut order: Vec<(usize, u32)> = Vec::new();
    let mut exhausted = false;
    // #12298: slots are filled in rounds. Each round draws one candidate
    // for up to `spawn_concurrency` open slots, never two in one repo, and
    // spawns them as one batch; the results are applied in draw order. A
    // refused slot stays open for the next round. With a concurrency of 1
    // every round is one draw and one spawn, which is the serial tick
    // exactly.
    //
    // The draws are the pool's draws in order, whatever the batching: a
    // draw that lands in a repo the round already holds is not put back
    // (that would change the pool, and its replay) but carried to the
    // head of the next round.
    let concurrency = limits.spawn_concurrency.max(1);
    // Each open slot's draws and, of those, the free `claim_lost` ones, in
    // the order the slots opened.
    let mut open: Vec<(u32, u32)> = Vec::new();
    let mut free_losses = 0u32;
    let mut gave_up = 0usize;
    let mut opened = 0usize;
    let mut carry: Option<DrawRecord> = None;
    loop {
        while open.len() < concurrency && opened < budget {
            open.push((0, 0));
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
        'slots: for (slot, (attempts, free)) in open.iter_mut().enumerate() {
            loop {
                if passes >= MAX_PASSES_PER_TICK {
                    break 'slots;
                }
                // A free `claim_lost` does not count toward the slot's draws.
                if attempts.saturating_sub(*free) >= MAX_DRAWS_PER_SLOT {
                    done.insert(slot);
                    gave_up += 1;
                    break;
                }
                let record = match carry.take() {
                    Some(record) => record,
                    None => match draw.try_next() {
                        Ok(Some(record)) => record,
                        Ok(None) => {
                            // The tick is exhausted when the round is empty,
                            // else the slot waits for the next round.
                            exhausted = batch.is_empty();
                            break 'slots;
                        }
                        Err(DrawError::WeightOverflow) => {
                            log::error!(
                                "work_finder: item draw stopped — {} ({} items pooled)",
                                DrawError::WeightOverflow,
                                draw.remaining().len()
                            );
                            overflow = true;
                            exhausted = batch.is_empty();
                            break 'slots;
                        }
                    },
                };
                let idx = record.picked.workspace_idx;
                if batch.iter().any(|p| p.ws_idx == idx) {
                    carry = Some(record);
                    break 'slots;
                }
                let Some(&ws) = by_idx.get(&idx) else {
                    exhausted = batch.is_empty();
                    break 'slots;
                };
                *attempts += 1;
                let key = key_of(ws, &record.picked);
                order.push((idx, key.number));
                let picked = record.picked.clone();
                let terms = record.terms;
                draws.push(record);
                draws_taken += 1;
                if planned.defer_admission(&key, &mut report) {
                    draw.close_repo(idx);
                    passes += 1;
                    *pass_counts.entry(PASS_REPO_CAP.to_string()).or_default() += 1;
                    *refused
                        .entry(idx)
                        .or_default()
                        .entry(PASS_REPO_CAP.to_string())
                        .or_default() += 1;
                    continue;
                }
                // One Champion merge per repo at a time (design §6): a second
                // merge drawn in a repo that spawned one this tick passes.
                let merge = matches!(
                    picked.step,
                    crate::work_finder::step_item::Step::ChampionMerge
                        | crate::work_finder::step_item::Step::ChampionHoldReevaluate
                );
                if merge && merging.contains(&idx) {
                    passes += 1;
                    resolve(&mut report.queue, &key, Qd::PeerClaim, Some(PASS_MERGE.to_string()));
                    *refused
                        .entry(idx)
                        .or_default()
                        .entry(PASS_MERGE.to_string())
                        .or_default() += 1;
                    *pass_counts.entry(PASS_MERGE.to_string()).or_default() += 1;
                    continue;
                }
                planned.admit_candidate(&key);
                batch.push(Pending {
                    slot,
                    ws_idx: idx,
                    merge,
                    request: StepSpawnRequest {
                        item: key.number,
                        role: StepRole::from(picked.step),
                        draw_attempt: *attempts,
                        age_source: picked.terms.age_source.as_str().to_string(),
                        resumed: false,
                        draw: Some(StepDrawTerms {
                            weight: terms.weight,
                            total_weight: terms.total_weight,
                            repo_items: terms.repo.items,
                            in_flight_repo: terms.repo.in_flight,
                        }),
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
            let idx = p.ws_idx;
            match result {
                SpawnResult::Spawned => {
                    if p.merge {
                        merging.insert(idx);
                    }
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
                    // A lost claim is cheap (module docs, #12410): the item
                    // already left the pool and the slot redraws next round
                    // without spending a pass or one of its draws.
                    if reason == REFUSAL_CLAIM_LOST && free_losses < MAX_FREE_CLAIM_LOST_PER_TICK {
                        free_losses += 1;
                        if let Some((_, free)) = open.get_mut(p.slot) {
                            *free += 1;
                        }
                    } else {
                        passes += 1;
                    }
                    *refused
                        .entry(idx)
                        .or_default()
                        .entry(reason.clone())
                        .or_default() += 1;
                    if reason == REFUSAL_CLAIM_UNVERIFIED {
                        // The owner's writer installation cannot verify a
                        // claim: every workspace of that owner leaves the
                        // draw for the rest of the tick (#12260).
                        let owner = match by_idx.get(&idx) {
                            Some(ws) => forge_owner(&ws.repo),
                            None => String::new(),
                        };
                        for other in workspaces.iter() {
                            if forge_owner(&other.repo) == owner {
                                draw.close_repo(other.idx);
                            }
                        }
                    }
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

    let stop = if limits.saturation_held {
        TickStop::Saturation
    } else if budget == 0 {
        TickStop::NoSlots
    } else if report.spawn_hold.is_some() {
        TickStop::SpawnHold
    } else if overflow {
        TickStop::WeightOverflow
    } else if exhausted {
        TickStop::PoolDrained
    } else if passes >= MAX_PASSES_PER_TICK {
        TickStop::PassLimit
    } else if gave_up > 0 {
        TickStop::SlotDraws
    } else if budget < limits.free_slots {
        TickStop::RampCap
    } else {
        TickStop::Filled
    };

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
    for c in draw.remaining() {
        if !drawn.contains(&(c.workspace_idx, c.target())) {
            order.push((c.workspace_idx, c.target()));
        }
    }
    report.plan_order = order;
    repo_cap.finish(&mut report);
    let views: Vec<ItemDrawWorkspace<'_>> = workspaces
        .iter()
        .map(|w| ItemDrawWorkspace {
            idx: w.idx,
            repo: &w.repo,
            excluded: &w.weighing.excluded,
        })
        .collect();
    report.item_draw = item_draw_record(
        seed.minute,
        &seed.host,
        seed.tick_counter,
        &views,
        &ItemDrawPool {
            draw: &draw,
            draws: &draws,
            overflow,
        },
    );
    // The draws a gate or the claim refused, beside the weight-0 counts, so
    // a refused draw is visible on the record (the integrated review's M1).
    if let Some(record) = report.item_draw.as_mut() {
        for (idx, reasons) in &refused {
            let repo = by_idx
                .get(idx)
                .map_or("repo_unresolved", |w| w.repo.as_str());
            for (reason, &count) in reasons {
                record.excluded.push(PickItemExclusion {
                    repo: repo.to_string(),
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
        budget,
        stop,
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
