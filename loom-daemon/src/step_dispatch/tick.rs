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
use crate::sweep_registry::step_spawn::StepSpawnRequest;
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
        "claimed" | "claim_lost" => Qd::PeerClaim,
        "pool_exhausted"
        | "no_token_pool"
        | "token_selection_failed"
        | "runtime_rejected"
        | "model_runtime_mismatch"
        | "admission_refused" => Qd::HostConstraint,
        "roll_paused" => Qd::DeferredCapacity,
        PASS_REPO_CAP => Qd::DeferredRepoCap,
        _ => Qd::DispatchError,
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
            E::CoolingDown => &mut report.skipped_noop_cooldown,
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

/// One workspace's draw state.
struct Drawing {
    ws: StepWorkspace,
    draw: ItemDraw,
    draws: Vec<DrawRecord>,
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
    let mut report = TickReport::default();
    report.saturation_held = limits.saturation_held;
    report.max_admissions_per_tick = Some(limits.max_admissions);
    report.repo_cap = Some(repo_cap.snapshot());

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
    for _slot in 0..budget {
        let mut attempts = 0u32;
        while attempts < MAX_DRAWS_PER_SLOT && passes < MAX_PASSES_PER_TICK {
            let entries: Vec<WorkspaceEntry> = drawing
                .iter()
                .filter(|d| !d.closed && !d.draw.remaining().is_empty())
                .map(Drawing::entry)
                .collect();
            let Some(ws_draw) = priority_pick::draw_workspace(&entries, bonus, &mut rng) else {
                exhausted = true;
                break;
            };
            let Some(d) = workspace_draw::workspace_idx(&ws_draw.picked)
                .and_then(|idx| drawing.iter_mut().find(|d| d.ws.idx == idx))
            else {
                exhausted = true;
                break;
            };
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
            attempts += 1;
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
            if repo_cap.defer_admission(&key, &mut report) {
                d.closed = true;
                passes += 1;
                *pass_counts.entry(PASS_REPO_CAP.to_string()).or_default() += 1;
                continue;
            }
            let request = StepSpawnRequest {
                item: key.number,
                role: StepRole::from(picked.step),
                draw_attempt: attempts,
                age_source: picked.terms.age_source.as_str().to_string(),
                resumed: false,
            };
            match spawner.spawn(d.ws.idx, &request) {
                SpawnResult::Spawned => {
                    resolve(&mut report.queue, &key, Qd::Dispatched, None);
                    report.dispatched += 1;
                    repo_cap.admit_candidate(&key);
                    break;
                }
                SpawnResult::Refused(reason) => {
                    let disposition = disposition_of(&reason);
                    if disposition == Qd::DispatchError {
                        report.errors += 1;
                    }
                    resolve(&mut report.queue, &key, disposition, Some(reason.clone()));
                    passes += 1;
                    *pass_counts.entry(reason).or_default() += 1;
                }
            }
        }
        if exhausted || passes >= MAX_PASSES_PER_TICK {
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
    report.workspace_draw = (draws_total > 0).then(|| DrawLog {
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
mod tests;
