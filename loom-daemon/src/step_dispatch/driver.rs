//! The production step tick (#11909): the work-finder loop's dispatch call.
//!
//! The loop keeps computing what it always did (the dynamic cap, holds,
//! saturation, the build back-off, the per-repo limits) and hands it here.
//! This module reads each workspace's items ([`super::gather`]), weighs them
//! with the registry's live state (step cooldowns, the Champion ladder, the
//! exhausted role pools), runs [`super::tick::run`] with the registries'
//! step spawn, and returns the [`TickReport`] every tick consumer (the tick
//! line, the summary, the dispatch plan, `pick.decision`) reads.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::Utc;

use super::item_draw::{weigh, ItemWeights, WeighContext};
use super::stage_entry::{default_cache_path, StageEntryProvider, DEFAULT_EVENTS_BUDGET};
use super::tick::{self, SeedBase, SpawnResult, StepSpawner, StepTickLimits, StepWorkspace};
use crate::sweep_registry::step_spawn::{StepRefusal, StepSpawnRequest};
use crate::sweep_registry::SweepRegistry;
use crate::work_finder::halt_cause::HaltCause;
use crate::work_finder::repo_cap::{RepoCap, RepoLimits};
use crate::work_finder::step_item::Lane;
use crate::work_finder::{workspace_draw, RedMainLane, RegistryDispatcher, TickReport};
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
    pub max_concurrent: usize,
    pub max_admissions: usize,
    pub saturation_held: bool,
    pub limits: RepoLimits,
}

/// The step tick's state across ticks: the stage-entry provider's cache.
pub struct StepDriver {
    provider: StageEntryProvider,
}

impl Default for StepDriver {
    fn default() -> Self {
        Self {
            provider: StageEntryProvider::load(default_cache_path(), DEFAULT_EVENTS_BUDGET),
        }
    }
}

/// The registries' step spawn: begin under the lock, poll the child's
/// account selection with it released (#6592), finish under it again.
struct RegistrySpawner {
    registries: Vec<Arc<Mutex<SweepRegistry>>>,
}

fn refusal_reason(e: &anyhow::Error) -> String {
    e.downcast_ref::<StepRefusal>()
        .map_or_else(|| "spawn_failed".to_string(), |r| r.reason.to_string())
}

impl StepSpawner for RegistrySpawner {
    fn spawn(&mut self, idx: usize, request: &StepSpawnRequest) -> SpawnResult {
        let Some(registry) = self.registries.get(idx) else {
            return SpawnResult::Refused("spawn_failed".to_string());
        };
        let begun = match registry.lock() {
            Ok(mut reg) => reg.begin_step_spawn(request),
            Err(_) => return SpawnResult::Refused("spawn_failed".to_string()),
        };
        let mut prepared = match begun {
            Ok(p) => p,
            Err(e) => return SpawnResult::Refused(refusal_reason(&e)),
        };
        let (token, runtime, death) = crate::sweep_registry::poll_and_classify_spawned_child(
            &mut prepared.child,
            &prepared.log_path,
            &prepared.header_anchor,
        );
        let finished = match registry.lock() {
            Ok(mut reg) => reg.finish_step_spawn(prepared, token, runtime, death),
            Err(_) => return SpawnResult::Refused("spawn_failed".to_string()),
        };
        match finished {
            Ok(_) => SpawnResult::Spawned,
            Err(e) => SpawnResult::Refused(refusal_reason(&e)),
        }
    }
}

fn occupancy(registry: &Arc<Mutex<SweepRegistry>>) -> usize {
    registry.lock().map_or(0, |mut reg| {
        reg.reap_liveness();
        reg.occupied_issues().len()
    })
}

impl StepDriver {
    /// Run one step tick (module docs).
    pub fn tick(&mut self, input: &TickInputs<'_>) -> TickReport {
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
        let mut listing_failed = Vec::new();
        for (idx, root) in input.roots.iter().enumerate() {
            let halted = input.halted.get(idx).copied().unwrap_or(false);
            let lane = input.lanes.get(idx).copied().unwrap_or_default();
            let fixes_only = halted && lane.admits_fixes_while_halted();
            let repo = crate::observability::pick_decision::repo_label(root);
            let weight = input.weights.get(idx).copied().unwrap_or(0);
            if halted && !fixes_only {
                workspaces.push(StepWorkspace {
                    idx,
                    repo,
                    weight,
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
            let registry = &registries[idx];
            let red = |fixes: Vec<crate::work_finder::WorkItem>| {
                let mut fixes = fixes;
                let mut dispatcher = RegistryDispatcher::new(registry.clone());
                crate::work_finder::main_red_fix::evaluate(lane, &mut fixes, &mut dispatcher)
            };
            let gathered = match super::gather::gather(
                root,
                registry,
                red,
                &held_capabilities,
                &mut self.provider,
                now,
            ) {
                Ok(Some(g)) => g,
                Ok(None) => continue,
                Err(e) => {
                    log::warn!("work_finder: {}: step listing failed: {e:#}", root.display());
                    listing_failed.push(idx);
                    continue;
                }
            };
            let (mut cooldowns, pool_exhausted_roles) = registry.lock().map_or_else(
                |_| (Default::default(), BTreeSet::new()),
                |reg| (reg.step_cooldowns(now), reg.pool_exhausted_roles(now)),
            );
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
            if fixes_only {
                weighing.pool.retain(|c| c.item.lane == Lane::RedMain);
            }
            workspaces.push(StepWorkspace {
                idx,
                repo,
                weight,
                weighing,
                halted: false,
                created_at: gathered.created_at,
            });
        }
        if let Err(e) = self.provider.flush() {
            log::debug!("work_finder: stage-entry cache not written: {e}");
        }
        let total: usize = occupied.iter().sum();
        let mut repo_cap = RepoCap::from_limits(input.limits.clone(), occupied);
        let mut spawner = RegistrySpawner { registries };
        let minute = u64::try_from(now.timestamp() / 60).unwrap_or(0);
        let seed = SeedBase {
            minute,
            host: crate::sweep_registry::host_identity(),
            tick_counter: tick::next_tick_counter(),
        };
        let result = tick::run(
            workspaces,
            StepTickLimits {
                free_slots: input.max_concurrent.saturating_sub(total),
                max_admissions: input.max_admissions,
                saturation_held: input.saturation_held,
            },
            &seed,
            workspace_draw::tick_seed(),
            &workspace_draw::weight_bonus(),
            &mut repo_cap,
            &mut spawner,
        );
        let mut report = result.report;
        report.listing_failed = listing_failed;
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
