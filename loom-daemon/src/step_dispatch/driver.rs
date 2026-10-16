//! The production step tick (#11909): the work-finder loop's dispatch call.
//!
//! The loop computes the cap (the operator's `maxConcurrent`, #12406), the
//! holds and the build back-off, and hands them here.
//! This module reads each workspace's items ([`super::gather`]), weighs them
//! with the registry's live state (step cooldowns, the Champion ladder, the
//! exhausted role pools), runs [`super::tick::run`] with the registries'
//! step spawn, and returns the [`TickReport`] every tick consumer (the tick
//! line, the summary, the dispatch plan, `pick.decision`) reads.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;

use super::gather_budget::{
    deadline_passed, millis, GatherWalk, SpawnParts, Starvation, TickPhases, GUARANTEED_FLOOR,
};
use super::item_draw::{weigh, ItemWeights, PoolKnobs, WeighContext};
use super::stage_entry::{default_cache_path, StageEntryProvider, DEFAULT_EVENTS_BUDGET};
use super::tick::{self, SeedBase, SpawnResult, StepSpawner, StepTickLimits, StepWorkspace};
use crate::sweep_registry::step_spawn::{StepRefusal, StepSpawnRequest, StepSpawnTimings};
use crate::sweep_registry::SweepRegistry;
use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::work_finder::halt_cause::HaltCause;
use crate::work_finder::step_item::{Lane, Step};
use crate::work_finder::{RedMainLane, RegistryDispatcher, TickReport};
use crate::workspace_pool::WorkspacePool;

/// What the loop computed for this tick, per root and for the host.
pub struct TickInputs<'a> {
    pub pool: &'a WorkspacePool,
    pub roots: &'a [PathBuf],
    /// Roster weights, parallel to `roots`.
    pub weights: &'a [u32],
    pub halted: &'a [bool],
    pub halt_causes: &'a [Option<HaltCause>],
    pub lanes: &'a [RedMainLane],
    /// The build back-off is engaged, per root.
    pub build_backoff: &'a [bool],
    /// The cap: the operator's `maxConcurrent` (#12406).
    pub max_concurrent: usize,
    /// The tick's wall-clock deadline (#12074): past it, no further repo is
    /// gathered. `None`: gather every repo.
    pub deadline: Option<Instant>,
    /// How many repos the gather lists at once (#12256,
    /// `autonomous.workFinder.gatherConcurrency`); `1` is serial.
    pub gather_concurrency: usize,
    /// How many spawns, each in a different repo, the draw runs at once
    /// (#12298, `autonomous.workFinder.spawnConcurrency`); `1` is serial.
    pub spawn_concurrency: usize,
    /// The discovery tier chain that builds the tick's snapshot (#12311,
    /// `forge.mirror.discovery`).
    pub discovery: crate::discovery::Chain,
}

/// What a workspace's listing said beyond its items.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ListingState {
    /// The open-PR listing answered.
    pub pulls_complete: bool,
    /// The items are the last good listing, not this tick's.
    pub stale_listing: bool,
    /// The repo is held red and admits only its red-main fixes.
    pub fixes_only: bool,
}

/// Shape one listed workspace's pool before the draw. Pure.
///
/// - This host's and the peers' live claims leave it (B1, M5).
/// - A Builder claim is a blind label flip: no Builder step when the open-PR
///   listing failed (a branch-only PR is invisible) or the items are the
///   last good listing, not this tick's.
/// - A red repo held for its fixes keeps its red-main fixes and every step
///   that adds no code to a red `main`: Builder and `ChampionMerge` run only
///   for a red-main fix, so a fix PR can still be reviewed and repaired.
///
/// Returns how many Builder steps the listing held (#12025): the step path
/// fails closed on a listing it cannot vouch for, and never falls back to a
/// per-item open-PR probe.
pub fn shape_pool(
    weighing: &mut super::item_draw::Weighing,
    claims: &crate::sweep_registry::step_spawn::draw_claims::DrawClaims,
    listing: ListingState,
) -> usize {
    tick::exclude_claimed(weighing, claims);
    let held = if !listing.pulls_complete || listing.stale_listing {
        tick::exclude_builders(weighing)
    } else {
        0
    };
    if listing.fixes_only {
        weighing.pool.retain(|c| {
            c.item.lane == Lane::RedMain || !matches!(c.step, Step::Builder | Step::ChampionMerge)
        });
    }
    held
}

/// Record on `report` what workspace `idx`'s listings cost this tick
/// (#12025): a failed item listing is a listing failure with its error
/// kind; items read from the last good listing are an incomplete listing;
/// Builder steps held because a listing could not vouch for them are one
/// `forge_listing` error, so `loom.dispatch.error_kind` names why the repo
/// drew no Builder step. Pure.
pub fn note_listing(
    report: &mut TickReport,
    idx: usize,
    failed: Option<&anyhow::Error>,
    listing: Option<ListingState>,
    builders_held: usize,
) {
    if let Some(e) = failed {
        report.note_listing_failure(idx, e);
    }
    if let Some(listing) = listing {
        report.note_listing(idx, !listing.stale_listing);
    }
    if builders_held > 0 {
        report.note_error(crate::work_finder::DispatchErrorKind::ForgeListing);
    }
}

/// The `loom.step.actionable{repo}` point of a workspace: its drawable
/// depth after [`shape_pool`]. `None` for a workspace this tick did not list
/// (`weighing` is `None`): absent, never a measured 0. Pure.
#[must_use]
pub fn actionable_point(
    repo: &str,
    weighing: Option<&super::item_draw::Weighing>,
) -> Option<MetricPoint> {
    let w = weighing?;
    Some(
        MetricPoint::int(
            MetricName::StepActionable,
            i64::try_from(w.pool.len()).unwrap_or(i64::MAX),
        )
        .label("repo", repo.to_string()),
    )
}

/// The step tick's state across ticks: the stage-entry provider's cache,
/// and where the next gather starts (#12074).
pub struct StepDriver {
    discovery: crate::discovery::Discovery,
    provider: StageEntryProvider,
    next_start: usize,
    starvation: Starvation,
}

impl Default for StepDriver {
    fn default() -> Self {
        Self {
            discovery: crate::discovery::Discovery::default(),
            provider: StageEntryProvider::load(default_cache_path(), DEFAULT_EVENTS_BUDGET),
            next_start: 0,
            starvation: Starvation::default(),
        }
    }
}

/// The registries' step spawn, with the lock released around containment,
/// the pre-claim and the account-selection poll
/// ([`SweepRegistry::spawn_step_unlocked`]). A round's spawns, one per repo,
/// run on up to `concurrency` threads (#12298).
struct RegistrySpawner {
    registries: Vec<Arc<Mutex<SweepRegistry>>>,
    roots: Vec<PathBuf>,
    /// Wall time spent inside spawns this tick (#12256).
    spent: Duration,
    attempts: usize,
    /// How many of a round's spawns run at once (#12298).
    concurrency: usize,
    /// Where the spawns' time went, summed over attempts (#12298).
    parts: StepSpawnTimings,
}

fn refusal_reason(e: &anyhow::Error) -> String {
    e.downcast_ref::<StepRefusal>()
        .map_or_else(|| "spawn_failed".to_string(), |r| r.reason.to_string())
}

impl StepSpawner for RegistrySpawner {
    fn held(&mut self) -> Option<&'static str> {
        let now = Utc::now();
        crate::rate_limit_breaker::global()
            .is_some_and(|rl| rl.skip_if_suppressed("work_finder", now))
            .then_some("rate_limit_breaker")
    }

    fn spawn(&mut self, idx: usize, request: &StepSpawnRequest) -> SpawnResult {
        let started = Instant::now();
        let (result, parts) = self.spawn_one(idx, request, self.concurrency > 1);
        self.spent += started.elapsed();
        self.attempts += 1;
        self.parts.add(&parts);
        result
    }

    fn spawn_batch(&mut self, batch: &[(usize, StepSpawnRequest)]) -> Vec<SpawnResult> {
        if self.concurrency <= 1 || batch.len() <= 1 {
            return batch
                .iter()
                .map(|(idx, request)| self.spawn(*idx, request))
                .collect();
        }
        // Each worker carries the trace parent and the runtime context, as
        // the gather's do; the draw's reads run outside the tick deadline,
        // so the (suspended) read scope goes along as it is.
        let started = Instant::now();
        let read_scope = crate::gh_invocation::read_deadline::snapshot();
        let parent = crate::gh_invocation::telemetry::thread_parent();
        let runtime = tokio::runtime::Handle::try_current().ok();
        let this = &*self;
        let answered = super::gather_budget::run_ordered(
            batch.to_vec(),
            self.concurrency,
            || {
                (
                    crate::gh_invocation::read_deadline::install(read_scope),
                    crate::gh_invocation::telemetry::enter_parent(parent.clone()),
                    runtime.as_ref().map(tokio::runtime::Handle::enter),
                )
            },
            |_, (idx, request)| Some(this.spawn_one(idx, &request, true)),
        );
        self.spent += started.elapsed();
        self.attempts += batch.len();
        // `run_ordered` answers in batch order; a missing answer (a worker
        // that panicked) is a failed spawn.
        let mut results = vec![None; batch.len()];
        for (pos, (result, parts)) in answered {
            self.parts.add(&parts);
            if let Some(slot) = results.get_mut(pos) {
                *slot = Some(result);
            }
        }
        results
            .into_iter()
            .map(|r| r.unwrap_or_else(|| SpawnResult::Refused("spawn_failed".to_string())))
            .collect()
    }
}

impl RegistrySpawner {
    /// One spawn. `host_gate`: the tick runs spawns concurrently, so the
    /// launch keeps the host-wide stagger gap (#3887, #12298); a serial
    /// tick (`spawnConcurrency` 1) launches exactly as before.
    fn spawn_one(
        &self,
        idx: usize,
        request: &StepSpawnRequest,
        host_gate: bool,
    ) -> (SpawnResult, StepSpawnTimings) {
        let mut parts = StepSpawnTimings::default();
        let Some(registry) = self.registries.get(idx) else {
            return (SpawnResult::Refused("spawn_failed".to_string()), parts);
        };
        let result = match SweepRegistry::spawn_step_unlocked_timed(
            registry, request, &mut parts, host_gate,
        ) {
            Ok(_) => SpawnResult::Spawned,
            Err(e) => {
                let reason = refusal_reason(&e);
                if let Some(root) = self.roots.get(idx) {
                    super::step_end::record_refusal(
                        root,
                        registry,
                        request.item,
                        request.role,
                        &reason,
                    );
                }
                SpawnResult::Refused(reason)
            }
        };
        (result, parts)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One repo the tick lists (#12256), set aside by the serial first pass.
struct GatherJob {
    idx: usize,
    fixes_only: bool,
    repo: String,
    weight: u32,
    /// Starved for several ticks: this tick may use the writer (#12074).
    escalated: bool,
}

/// The forge half of one repo's gather, on whichever worker took it: the
/// repo's own read guards (the guaranteed floor, the starvation
/// escalation), then [`super::gather::list`].
fn list_one(
    input: &TickInputs<'_>,
    registry: &Arc<Mutex<SweepRegistry>>,
    idx: usize,
    guaranteed: bool,
    escalated: bool,
    snapshot: &crate::discovery::TickSnapshot,
    held_capabilities: &BTreeSet<String>,
) -> anyhow::Result<Option<super::gather::Listed>> {
    let root = &input.roots[idx];
    // The repo a tick always gathers gets a real read budget, even past the
    // deadline; a repo starved for several ticks gets one tick on the
    // writer (#12074).
    let _floor = guaranteed
        .then(|| crate::gh_invocation::read_deadline::at_least(Instant::now() + GUARANTEED_FLOOR));
    let _writer = escalated.then(|| {
        log::info!(
            "work_finder: {}: reads shed for several ticks; this tick may use the writer \
             (#12074)",
            root.display()
        );
        crate::gh_invocation::read_deadline::allow_writer()
    });
    let lane = input.lanes.get(idx).copied().unwrap_or_default();
    let red = |fixes: Vec<crate::work_finder::WorkItem>| {
        let mut fixes = fixes;
        let mut dispatcher = RegistryDispatcher::new(registry.clone());
        crate::work_finder::main_red_fix::evaluate(lane, &mut fixes, &mut dispatcher)
    };
    super::gather::list(root, registry, snapshot, red, held_capabilities)
}

pub(crate) fn occupancy(registry: &Arc<Mutex<SweepRegistry>>) -> usize {
    // A poisoned lock still holds the real claims; recover rather than report 0.
    let mut reg = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    reg.reap_liveness();
    reg.occupied_issues().len()
}

impl StepDriver {
    /// Run one step tick (module docs). Its forge reads and spawns block, so
    /// on the daemon's multi-thread runtime it runs under `block_in_place`
    /// rather than stalling a worker.
    pub fn tick(&mut self, input: &TickInputs<'_>) -> TickReport {
        crate::sweep_registry::step_spawn::blocking(|| self.tick_blocking(input))
    }

    fn tick_blocking(&mut self, input: &TickInputs<'_>) -> TickReport {
        let now = Utc::now();
        self.provider.begin_tick();
        let registries: Vec<_> = input
            .roots
            .iter()
            .map(|r| input.pool.get_or_provision(r))
            .collect();
        let occupied: Vec<usize> = registries.iter().map(occupancy).collect();
        let held_capabilities = crate::capability::held_capabilities();
        let mut workspaces = Vec::new();
        // Per workspace: (idx, failed listing, listing state, Builders held).
        let mut notes: Vec<(usize, Option<anyhow::Error>, Option<ListingState>, usize)> =
            Vec::new();
        // `loom.step.actionable{repo}`: the drawable depth the stalled and
        // idle alerts key on. Absent, never 0, for a repo not listed.
        let mut actionable = Vec::new();
        // #12074: walk from where the last truncated tick stopped; past the
        // deadline, gather no further repo.
        let walk = Mutex::new(GatherWalk::new(input.roots.len(), self.next_start, Instant::now()));
        let order: Vec<usize> = lock(&walk).order().collect();
        // Pass 1, serial and local: settle each repo's ended steps (so the
        // cooldowns they arm hold for this draw) and set the held repos aside.
        let mut jobs: Vec<GatherJob> = Vec::new();
        for idx in order {
            let root = &input.roots[idx];
            super::step_end::settle(root, &registries[idx]);
            let halted = input.halted.get(idx).copied().unwrap_or(false);
            let lane = input.lanes.get(idx).copied().unwrap_or_default();
            let fixes_only = halted && lane.admits_fixes_while_halted();
            let repo = crate::observability::pick_decision::repo_label(root);
            let weight = input.weights.get(idx).copied().unwrap_or(0);
            if halted && !fixes_only {
                // Not listed: no `loom.step.actionable` point, never a 0.
                actionable.extend(actionable_point(&repo, None));
                workspaces.push(StepWorkspace {
                    idx,
                    repo,
                    weight,
                    in_flight: 0,
                    weighing: super::item_draw::Weighing {
                        pool: Vec::new(),
                        excluded: Default::default(),
                        demotions: Vec::new(),
                    },
                    halted: true,
                    created_at: Default::default(),
                });
                continue;
            }
            jobs.push(GatherJob {
                idx,
                fixes_only,
                repo,
                weight,
                escalated: self.starvation.escalate(root),
            });
        }
        // Pass 2, the forge half, `gather_concurrency` repos at once
        // (#12256). Each worker carries the tick's read deadline, its trace
        // parent and the runtime context; a repo past the deadline is
        // skipped as before. Results come back in walk order.
        let listing_started = Instant::now();
        // One snapshot per tick (#12306): the Mirror backend makes its one
        // worklist request here; Direct lists each repo as the gather asks.
        let snapshot = self.discovery.begin_tick(input.discovery, now);
        let read_scope = crate::gh_invocation::read_deadline::snapshot();
        let parent = crate::gh_invocation::telemetry::thread_parent();
        let runtime = tokio::runtime::Handle::try_current().ok();
        let concurrency = input.gather_concurrency.max(1);
        let job_meta: Vec<(usize, bool)> = jobs.iter().map(|j| (j.idx, j.escalated)).collect();
        let listed = super::gather_budget::run_ordered(
            job_meta,
            concurrency,
            || {
                (
                    crate::gh_invocation::read_deadline::install(read_scope),
                    crate::gh_invocation::telemetry::enter_parent(parent.clone()),
                    runtime.as_ref().map(tokio::runtime::Handle::enter),
                )
            },
            |_, (idx, escalated)| {
                let guaranteed = {
                    let mut walk = lock(&walk);
                    if !walk.admit(idx, deadline_passed(input.deadline, Instant::now())) {
                        return None;
                    }
                    walk.is_guaranteed()
                };
                Some(list_one(
                    input,
                    &registries[idx],
                    idx,
                    guaranteed,
                    escalated,
                    &snapshot,
                    &held_capabilities,
                ))
            },
        );
        let listing_ms = millis(listing_started.elapsed());
        // Pass 3, serial and in walk order: stage entry, weighing, shaping.
        let filter_started = Instant::now();
        let mut jobs: Vec<Option<GatherJob>> = jobs.into_iter().map(Some).collect();
        for (pos, result) in listed {
            let Some(job) = jobs.get_mut(pos).and_then(Option::take) else {
                continue;
            };
            let GatherJob {
                idx,
                fixes_only,
                repo,
                weight,
                escalated,
            } = job;
            let root = &input.roots[idx];
            let registry = &registries[idx];
            let listed = match result {
                Ok(Some(l)) => l,
                Ok(None) => continue,
                Err(e) => {
                    self.starvation.record(root, true, escalated);
                    log::warn!("work_finder: {}: step listing failed: {e:#}", root.display());
                    notes.push((idx, Some(e), None, 0));
                    continue;
                }
            };
            let gathered = super::gather::finish(listed, root, &mut self.provider, now);
            self.starvation.record(
                root,
                gathered.stale_listing || !gathered.pulls_complete,
                escalated,
            );
            let (mut cooldowns, pool_exhausted_roles, claims) = {
                let reg = registry
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                (reg.step_cooldowns(now), reg.pool_exhausted_roles(now), reg.draw_claims())
            };
            for (item, role, until) in &gathered.ladder {
                cooldowns.insert(*item, *role, *until);
            }
            let ctx = WeighContext {
                now,
                build_backoff_engaged: input.build_backoff.get(idx).copied().unwrap_or(false),
                pool_exhausted_roles,
                workspace_idx: idx,
                cooldowns,
            };
            let mut weighing = weigh(&gathered.inputs, &ctx, &ItemWeights::for_root(root));
            let listing = ListingState {
                pulls_complete: gathered.pulls_complete,
                stale_listing: gathered.stale_listing,
                fixes_only,
            };
            // `in_flight_repo` (#12185): the repo's items this host runs plus
            // the live claims peers advertise, counted before the claimed
            // items leave the pool.
            let in_flight = claims.items.len();
            let held = shape_pool(&mut weighing, &claims, listing);
            if held > 0 {
                log::warn!(
                    "work_finder: {repo}: {held} Builder step(s) held this tick: the {} cannot \
                     vouch for them (fail closed, #12025)",
                    if gathered.stale_listing {
                        "item listing is the last good one"
                    } else {
                        "open-PR listing could not be read"
                    }
                );
            }
            notes.push((idx, None, Some(listing), held));
            actionable.extend(actionable_point(&repo, Some(&weighing)));
            workspaces.push(StepWorkspace {
                idx,
                repo,
                weight,
                in_flight,
                weighing,
                halted: false,
                created_at: gathered.created_at,
            });
        }
        let filter_ms = millis(filter_started.elapsed());
        let walk = walk
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let gather = walk.stats(Instant::now());
        self.next_start = walk.next_start();
        if gather.truncated {
            log::warn!(
                "work_finder: step gather truncated by the tick deadline after {}ms: {} repo(s) \
                 gathered, {} skipped; the next tick starts at the first skipped (#12074)",
                gather.gather_ms,
                gather.repos_gathered,
                gather.repos_skipped
            );
        }
        // The draw keeps its workspace order whatever the walk's rotation.
        workspaces.sort_by_key(|w| w.idx);
        crate::observability::ops::emit_metrics(actionable);
        if let Err(e) = self.provider.flush() {
            log::debug!("work_finder: stage-entry cache not written: {e}");
        }
        let total: usize = occupied.iter().sum();
        let mut spawner = RegistrySpawner {
            registries,
            roots: input.roots.to_vec(),
            spent: Duration::ZERO,
            attempts: 0,
            concurrency: input.spawn_concurrency.max(1),
            parts: StepSpawnTimings::default(),
        };
        let minute = u64::try_from(now.timestamp() / 60).unwrap_or(0);
        let seed = SeedBase {
            minute,
            host: crate::sweep_registry::host_identity(),
            tick_counter: tick::next_tick_counter(),
        };
        // The draw spawns from what was gathered: its pre-claim reads run
        // normally, outside the gather's read deadline (#12074).
        let _spawning = crate::gh_invocation::read_deadline::suspend();
        let draw_started = Instant::now();
        let result = tick::run(
            workspaces,
            StepTickLimits {
                free_slots: input.max_concurrent.saturating_sub(total),
                spawn_concurrency: input.spawn_concurrency.max(1),
            },
            &seed,
            // Pool-wide, so read once, from the primary workspace like the
            // work-finder's own hot-applied config.
            &input
                .roots
                .first()
                .map_or_else(PoolKnobs::default, |root| PoolKnobs::for_root(root)),
            &mut spawner,
        );
        let draw_total = draw_started.elapsed();
        let mut report = result.report;
        report.gather = Some(gather);
        report.discovery = Some(snapshot.stats());
        report.phases = Some(TickPhases {
            listing_ms,
            filter_ms,
            draw_ms: millis(draw_total.saturating_sub(spawner.spent)),
            spawn_ms: millis(spawner.spent),
            spawn_attempts: spawner.attempts,
            gather_concurrency: concurrency,
            spawn_concurrency: spawner.concurrency,
            spawn_parts: SpawnParts {
                resolve_ms: millis(spawner.parts.resolve),
                claim_ms: millis(spawner.parts.claim),
                launch_ms: millis(spawner.parts.launch),
                account_ms: millis(spawner.parts.account),
            },
        });
        for (idx, failed, listing, held) in &notes {
            note_listing(&mut report, *idx, failed.as_ref(), *listing, *held);
        }
        report.occupancy = Some(total + report.dispatched);
        report.note_halts(input.halted, Some(input.halt_causes), input.roots.len());
        if !result.passes.is_empty() {
            log::info!(
                "work_finder: step tick: {} draw(s), {} spawned, passes {:?}",
                result.draws,
                report.dispatched,
                result.passes
            );
        }
        report
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "driver_tests.rs"]
mod tests;
