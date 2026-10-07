//! Per-repo demand-driven balance across builder / judge / doctor / champion
//! — **Slice 1: pipeline state + allocator core, shadow mode** (issue #10630).
//!
//! Today the host's effort is split by separate, mostly host-wide knobs: the
//! build back-off (#9410/#10624) only ever *stops* building, and judge /
//! doctor / champion width (#9392/#9414) is sized from the **host** debt
//! total. Nothing says where a free slot is worth the most. This module is
//! the allocator that will: one pure function that, given every repo's
//! pipeline state, distributes the host's agent budget across `(repo, role)`
//! pairs by marginal value.
//!
//! **Slice 1 changes no admission.** With `autonomous.balance.enabled: true`
//! the work finder computes the allocation once per tick and logs it as one
//! line ([`shadow_tick`]); nothing reads the result. Admission, the demand
//! width/reservation ([`super::demand::decide`]), doctor lanes
//! ([`super::demand::repo_lanes`]) and the build back-off behave identically
//! with the flag on or off. Wiring the allocation into admission is #10815;
//! event triggers (#10816) and idle generators (#10817) come later.
//!
//! - **Pipeline state** ([`RepoPipeline`]). Review / changes / merge are each
//!   repo's **fresh** demand-ledger totals ([`DemandLedger::repo_debt`]);
//!   ready / building come from the work finder's own tick queue (rows it has
//!   already listed — [`queue_counts`]; a halted repo reads ready as a known
//!   zero). No forge call is added. `None` is an
//!   unobserved axis and **fails open**: the role gets a one-slot floor, never
//!   zero for lack of data.
//! - **Allocation** ([`allocate`]). Champion first: every repo with merge
//!   debt gets its one champion slot before anything else, up to
//!   `budget − nonPrFloor` (the demand module's reservation cap) — skipped
//!   entirely when `demandWidth.reserve` is `false`. Every other
//!   slot goes, one at a time, to the `(repo, role)` with the highest
//!   marginal value `demand / (slots + 1)`, where judge demand is review debt
//!   × `reviewWeight`, doctor demand is changes debt × `reviewWeight`, and
//!   builder demand is ready issues minus `ceil(reviewWeight × (review +
//!   changes))` — a repo deep in debt pauses its own builds, a debt-free repo
//!   with ready work gets builders. Judge / doctor are capped per repo at
//!   `clamp(ceil(debt / perRun), 1, max)` (the demand width formula, applied
//!   to the repo's own debt). Ties break by repo path, then role (champion,
//!   judge, doctor, builder), so the result is deterministic.
//! - **Bias** (`reviewWeight`, default `1.0`). Above 1 tilts the host toward
//!   review (judge / doctor demand up, builds pause sooner); below 1 toward
//!   building.

use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::demand::{self, DebtAxis, DemandConfig, DemandLedger, HostDebt};
use crate::types::QueueDisposition;
use crate::work_finder::ready_queue::TickQueueRow;

/// Env override for `autonomous.balance.enabled` (env > config > default).
pub const ENABLED_ENV: &str = "LOOM_BALANCE_ENABLED";
/// Env override for `autonomous.balance.reviewWeight`.
pub const REVIEW_WEIGHT_ENV: &str = "LOOM_BALANCE_REVIEW_WEIGHT";
/// Default `reviewWeight`: review and build weighed evenly.
pub const DEFAULT_REVIEW_WEIGHT: f64 = 1.0;

/// The roles the allocator distributes slots across, in tie-break order
/// (within one repo; repos tie-break by path first).
pub const ROLES: [&str; 4] = ["champion", "judge", "doctor", "builder"];

/// `autonomous.balance`, resolved.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BalanceConfig {
    /// `enabled` — `false` (the default) computes nothing and logs nothing.
    pub enabled: bool,
    /// `reviewWeight` — finite and `> 0`; anything else is the default.
    pub review_weight: f64,
}

impl Default for BalanceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            review_weight: DEFAULT_REVIEW_WEIGHT,
        }
    }
}

fn valid_weight(w: f64) -> Option<f64> {
    (w.is_finite() && w > 0.0).then_some(w)
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

impl BalanceConfig {
    /// Resolve from an `autonomous.balance` block plus the raw env values
    /// (pure, so tests never touch the process env). Each key falls back on
    /// its own: an unparseable env value drops to config, an invalid config
    /// value (non-bool flag; zero, negative, non-finite or non-number weight)
    /// drops to the default — the `parse_demand_config` discipline.
    #[must_use]
    pub fn resolve(block: &Value, env_enabled: Option<&str>, env_weight: Option<&str>) -> Self {
        let d = Self::default();
        let enabled = env_enabled
            .and_then(parse_bool)
            .or_else(|| block.get("enabled").and_then(Value::as_bool))
            .unwrap_or(d.enabled);
        let review_weight = env_weight
            .and_then(|w| w.trim().parse::<f64>().ok())
            .and_then(valid_weight)
            .or_else(|| {
                block
                    .get("reviewWeight")
                    .and_then(Value::as_f64)
                    .and_then(valid_weight)
            })
            .unwrap_or(d.review_weight);
        Self {
            enabled,
            review_weight,
        }
    }

    /// Read `root`'s effective `autonomous.balance` with the env tier.
    #[must_use]
    pub fn read(root: &Path) -> Self {
        let effective = crate::config_resolver::resolve_effective_config(root);
        let block = crate::config_resolver::get_path(&effective, "autonomous.balance")
            .cloned()
            .unwrap_or(Value::Null);
        let env = |k: &str| std::env::var(k).ok();
        Self::resolve(&block, env(ENABLED_ENV).as_deref(), env(REVIEW_WEIGHT_ENV).as_deref())
    }
}

/// The allocator's tunables: the bias plus the demand module's own knobs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AllocatorConfig {
    /// `autonomous.balance.reviewWeight`.
    pub review_weight: f64,
    /// `demandWidth.perRun` — debt items per judge / doctor slot.
    pub per_run: usize,
    /// `demandWidth.max` — most judge / doctor slots one repo may get.
    pub max_per_repo: usize,
    /// `demandWidth.nonPrFloor` — slots the champion-first pass leaves free.
    pub non_pr_floor: usize,
    /// `demandWidth.reserve` — `false` skips the champion-first pass
    /// ("keeps the width but reserves nothing"); champions then compete in
    /// the marginal pass like every other role.
    pub reserve: bool,
}

impl AllocatorConfig {
    /// Combine the balance bias with the demand module's knobs.
    #[must_use]
    pub fn new(balance: &BalanceConfig, demand: &DemandConfig) -> Self {
        Self {
            review_weight: valid_weight(balance.review_weight).unwrap_or(DEFAULT_REVIEW_WEIGHT),
            per_run: demand.per_run.max(1),
            max_per_repo: demand.max.max(1),
            non_pr_floor: demand.non_pr_floor,
            reserve: demand.reserve,
        }
    }
}

impl Default for AllocatorConfig {
    fn default() -> Self {
        Self::new(&BalanceConfig::default(), &DemandConfig::default())
    }
}

/// One repo's pipeline, every count `None` when unobserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RepoPipeline {
    /// Open `loom:review-requested` PRs (fresh ledger entry).
    pub review: Option<usize>,
    /// Open `loom:changes-requested` PRs, parked ones excluded.
    pub changes: Option<usize>,
    /// Open `loom:pr` PRs, operator-held ones excluded.
    pub merge: Option<usize>,
    /// Ready issues the work finder could admit this tick.
    pub ready: Option<usize>,
    /// Issues with a sweep in flight.
    pub building: Option<usize>,
    /// Untriaged issues — not observed anywhere yet (always `None` in
    /// Slice 1; curator triggers are #10816).
    pub untriaged: Option<usize>,
}

impl RepoPipeline {
    /// The debt axes of a [`DemandLedger::repo_debt`] reading (fresh totals).
    #[must_use]
    pub fn from_debt(debt: &HostDebt) -> Self {
        Self {
            review: debt.review.map(|a| a.total),
            changes: debt.changes.map(|a| a.total),
            merge: debt.merge.map(|a| a.total),
            ..Self::default()
        }
    }

    /// The count on a debt axis.
    #[must_use]
    pub fn axis(&self, axis: DebtAxis) -> Option<usize> {
        match axis {
            DebtAxis::Review => self.review,
            DebtAxis::Changes => self.changes,
            DebtAxis::Merge => self.merge,
        }
    }
}

/// What decided a `(repo, role)` allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The repo's debt on this axis.
    Debt(DebtAxis),
    /// The repo's ready-issue queue (builders).
    Ready,
    /// An event (merge on `main`, new untriaged issue) — #10816.
    Event,
    /// The repo's pipeline is empty — #10817.
    Idle,
    /// The deciding input is unobserved: a fail-open one-slot floor.
    Floor,
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Debt(DebtAxis::Review) => f.write_str("debt:review"),
            Self::Debt(DebtAxis::Changes) => f.write_str("debt:changes"),
            Self::Debt(DebtAxis::Merge) => f.write_str("debt:merge"),
            Self::Ready => f.write_str("ready"),
            Self::Event => f.write_str("event"),
            Self::Idle => f.write_str("idle"),
            Self::Floor => f.write_str("floor"),
        }
    }
}

/// The slots one `(repo, role)` gets, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Allocation {
    /// The repo.
    pub root: PathBuf,
    /// One of [`ROLES`].
    pub role: &'static str,
    /// Slots allocated (may be 0).
    pub slots: usize,
    /// What decided it.
    pub trigger: Trigger,
    /// Human-readable reason.
    pub reason: String,
}

/// One `(repo, role)`'s demand before slots are handed out.
struct Want {
    /// Weighted demand; marginal value of the next slot is `demand / (slots + 1)`.
    demand: f64,
    /// Most slots it can use.
    cap: usize,
    trigger: Trigger,
    why: String,
}

fn pr_want(role: &str, axis: DebtAxis, p: &RepoPipeline, cfg: &AllocatorConfig) -> Want {
    let w = cfg.review_weight;
    let weighted = if axis == DebtAxis::Merge { 1.0 } else { w };
    match p.axis(axis) {
        None => Want {
            demand: weighted,
            cap: 1,
            trigger: Trigger::Floor,
            why: format!("{} unobserved — fail-open floor", axis.label()),
        },
        Some(0) => Want {
            demand: 0.0,
            cap: 0,
            trigger: Trigger::Debt(axis),
            why: format!("no {} debt", axis.label()),
        },
        Some(d) => {
            let cap = if role == "champion" {
                1
            } else {
                d.div_ceil(cfg.per_run).clamp(1, cfg.max_per_repo)
            };
            Want {
                demand: d as f64 * weighted,
                cap,
                trigger: Trigger::Debt(axis),
                why: format!("{} debt {d} → wants {cap}", axis.label()),
            }
        }
    }
}

fn builder_want(p: &RepoPipeline, cfg: &AllocatorConfig) -> Want {
    let pr_debt = p.review.unwrap_or(0) + p.changes.unwrap_or(0);
    let penalty = (cfg.review_weight * pr_debt as f64).ceil() as usize;
    let (ready, base_trigger) = match p.ready {
        Some(n) => (n, Trigger::Ready),
        None => (1, Trigger::Floor),
    };
    let demand = ready.saturating_sub(penalty);
    if ready > 0 && demand == 0 {
        let axis = if p.changes.unwrap_or(0) >= p.review.unwrap_or(0) {
            DebtAxis::Changes
        } else {
            DebtAxis::Review
        };
        return Want {
            demand: 0.0,
            cap: 0,
            trigger: Trigger::Debt(axis),
            why: match p.ready {
                Some(n) => format!(
                    "builds paused — review+changes debt {pr_debt} × weight {:.2} ≥ ready {n}",
                    cfg.review_weight
                ),
                None => format!(
                    "builds paused — ready queue unobserved, review+changes debt {pr_debt} × \
                     weight {:.2} outweighs the one-slot floor",
                    cfg.review_weight
                ),
            },
        };
    }
    let why = match p.ready {
        None => "ready queue unobserved — fail-open floor".to_string(),
        Some(0) => "no ready issues".to_string(),
        Some(n) => format!("ready {n} − debt penalty {penalty} → wants {demand}"),
    };
    Want {
        demand: demand as f64,
        cap: demand,
        trigger: base_trigger,
        why,
    }
}

fn want_for(role: &str, p: &RepoPipeline, cfg: &AllocatorConfig) -> Want {
    match role {
        "champion" => pr_want(role, DebtAxis::Merge, p, cfg),
        "judge" => pr_want(role, DebtAxis::Review, p, cfg),
        "doctor" => pr_want(role, DebtAxis::Changes, p, cfg),
        _ => builder_want(p, cfg),
    }
}

/// Distribute `host_budget` slots across every `(repo, role)` pair (see the
/// module doc). Pure and deterministic: repos are processed in path order
/// whatever order they arrive in, and the total never exceeds `host_budget`.
/// Returns one [`Allocation`] per repo per [`ROLES`] entry, zero-slot ones
/// included, ordered by path then role.
#[must_use]
pub fn allocate(
    repos: &[(PathBuf, RepoPipeline)],
    host_budget: usize,
    cfg: &AllocatorConfig,
) -> Vec<Allocation> {
    let mut sorted: Vec<&(PathBuf, RepoPipeline)> = repos.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    sorted.dedup_by(|a, b| a.0 == b.0);
    // Candidates in (path, role) order — the tie-break order.
    let wants: Vec<(usize, usize, Want)> = sorted
        .iter()
        .enumerate()
        .flat_map(|(ri, (_, p))| {
            ROLES
                .iter()
                .enumerate()
                .map(move |(qi, role)| (ri, qi, want_for(role, p, cfg)))
        })
        .collect();
    let mut slots = vec![0usize; wants.len()];
    let mut left = host_budget;

    // Champion first: each repo with merge debt, deepest first, gets its one
    // slot before anything else — capped like the demand reservation.
    // `demandWidth.reserve: false` reserves nothing: no prepass at all.
    let mut champions: Vec<usize> = (0..wants.len())
        .filter(|_| cfg.reserve)
        .filter(|&i| wants[i].1 == 0 && wants[i].2.trigger == Trigger::Debt(DebtAxis::Merge))
        .filter(|&i| wants[i].2.cap > 0)
        .collect();
    champions.sort_by(|&a, &b| {
        wants[b]
            .2
            .demand
            .total_cmp(&wants[a].2.demand)
            .then(a.cmp(&b))
    });
    let mut reserve = host_budget.saturating_sub(cfg.non_pr_floor);
    for i in champions {
        if reserve == 0 || left == 0 {
            break;
        }
        slots[i] = 1;
        reserve -= 1;
        left -= 1;
    }

    // Then the rest, one slot at a time, by marginal value.
    while left > 0 {
        let mut best: Option<(usize, f64)> = None;
        for (i, (_, _, w)) in wants.iter().enumerate() {
            if slots[i] >= w.cap || w.demand <= 0.0 {
                continue;
            }
            let value = w.demand / (slots[i] + 1) as f64;
            if best.is_none_or(|(_, v)| value > v) {
                best = Some((i, value));
            }
        }
        let Some((i, _)) = best else { break };
        slots[i] += 1;
        left -= 1;
    }

    wants
        .into_iter()
        .zip(slots)
        .map(|((ri, qi, w), got)| {
            let short = if got < w.cap {
                format!("; got {got} (host budget {host_budget} spent)")
            } else {
                String::new()
            };
            Allocation {
                root: sorted[ri].0.clone(),
                role: ROLES[qi],
                slots: got,
                trigger: w.trigger,
                reason: format!("{}{short}", w.why),
            }
        })
        .collect()
}

/// `(ready, building)` per root from the work finder's tick queue — rows it
/// already listed, so no forge call. A halted root admits nothing, so its
/// ready count is a known zero (not a fail-open floor); its building count
/// is unobserved (`None`).
#[must_use]
pub fn queue_counts(
    queue: &[TickQueueRow],
    roots: usize,
    halted: &[bool],
) -> Vec<(Option<usize>, Option<usize>)> {
    let mut ready = vec![0usize; roots];
    let mut building = vec![0usize; roots];
    for row in queue {
        let Some(idx) = (row.key.workspace_idx < roots).then_some(row.key.workspace_idx) else {
            continue;
        };
        match row.disposition {
            None
            | Some(
                QueueDisposition::Dispatched
                | QueueDisposition::DeferredCapacity
                | QueueDisposition::DeferredRampCap
                | QueueDisposition::DeferredSaturation
                | QueueDisposition::DeferredBuildBackoff
                | QueueDisposition::DeferredOutOfSlice
                | QueueDisposition::DeferredRepoCap,
            ) => ready[idx] += 1,
            Some(QueueDisposition::InFlight) => building[idx] += 1,
            Some(_) => {}
        }
    }
    (0..roots)
        .map(|i| {
            if halted.get(i).copied().unwrap_or(false) {
                (Some(0), None)
            } else {
                (Some(ready[i]), Some(building[i]))
            }
        })
        .collect()
}

/// The shadow allocation for one tick, or `None` when `balance` is disabled —
/// in which case nothing is read (not even the ledger).
#[must_use]
pub fn shadow_allocation(
    balance: &BalanceConfig,
    demand_cfg: &DemandConfig,
    ledger: &DemandLedger,
    roots: &[PathBuf],
    queue: &[TickQueueRow],
    halted: &[bool],
    host_budget: usize,
) -> Option<Vec<Allocation>> {
    if !balance.enabled {
        return None;
    }
    let counts = queue_counts(queue, roots.len(), halted);
    let repos: Vec<(PathBuf, RepoPipeline)> = roots
        .iter()
        .zip(counts)
        .map(|(root, (ready, building))| {
            let debt = ledger.repo_debt(root, demand_cfg.stale());
            let pipeline = RepoPipeline {
                ready,
                building,
                ..RepoPipeline::from_debt(&debt)
            };
            (root.clone(), pipeline)
        })
        .collect();
    let cfg = AllocatorConfig::new(balance, demand_cfg);
    Some(allocate(&repos, host_budget, &cfg))
}

/// The one log line for a tick's allocation, carrying the build identity
/// (version + full SHA, `trace-identity.md`).
#[must_use]
pub fn log_line(allocs: &[Allocation], host_budget: usize, review_weight: f64) -> String {
    let build = crate::telemetry::trace::provenance::daemon();
    let assigned: usize = allocs.iter().map(|a| a.slots).sum();
    let entries: Vec<String> = allocs
        .iter()
        .map(|a| {
            format!("{}/{}={} [{}: {}]", a.root.display(), a.role, a.slots, a.trigger, a.reason)
        })
        .collect();
    format!(
        "role_runner::balance: shadow allocation (no admission change, #10630) — budget \
         {host_budget}, assigned {assigned}, reviewWeight {review_weight:.2}, loom {} ({}): {}",
        build.version,
        build.revision,
        entries.join("; ")
    )
}

/// The work finder's per-tick hook: when `autonomous.balance.enabled` (read
/// from `primary`), compute the allocation over `roots` and log it once.
/// `host_budget` is this tick's sweep cap plus the role-agent ceiling.
/// Disabled is a config read and nothing else.
pub fn shadow_tick(
    primary: &Path,
    roots: &[PathBuf],
    queue: &[TickQueueRow],
    halted: &[bool],
    sweep_cap: usize,
) {
    let balance = BalanceConfig::read(primary);
    if !balance.enabled {
        return;
    }
    let budget = sweep_cap + super::resolve_max_concurrent_for(primary);
    let demand_cfg = demand::read_demand_config(primary);
    let ledger = demand::global();
    if let Some(allocs) =
        shadow_allocation(&balance, &demand_cfg, ledger, roots, queue, halted, budget)
    {
        log::info!("{}", log_line(&allocs, budget, balance.review_weight));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/balance.rs"]
mod tests;
