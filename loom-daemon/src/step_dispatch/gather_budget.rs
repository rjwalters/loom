//! Which repos one step tick gathers before its deadline (#12074).
//!
//! The gather walks every managed repo, and each costs forge reads. A tick
//! has a wall-clock deadline (`autonomous.workFinder.tickDeadlineSecs`); once
//! it passes, the tick stops gathering, draws from what it gathered, and
//! reports the rest as skipped. The walk starts where the last truncated
//! tick stopped ([`GatherWalk::next_start`]), so a run of slow ticks still
//! covers every repo in turn instead of re-reading the first few forever.
//!
//! At least one repo is gathered every tick, deadline or not, so a tick whose
//! pre-dispatch section used the whole budget still makes progress (its reads
//! then fail fast and fall back to the last good listing).

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The cost and coverage of one tick's gather, for the tick report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GatherStats {
    /// Wall-clock time spent gathering.
    pub gather_ms: u64,
    /// Repos whose items were read (or attempted) this tick.
    pub repos_gathered: usize,
    /// Repos not read because the deadline passed.
    pub repos_skipped: usize,
    /// The deadline cut the gather short.
    pub truncated: bool,
}

/// Where one step tick's wall time went (#12256), for the tick span: the
/// listings (the forge half of the gather, run concurrently), the filter
/// (stage entry, weighing and pool shaping, serial), and the draw loop split
/// into the time spent inside spawns (claims, containment, the
/// account-selection poll) and the rest (the draw itself).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickPhases {
    pub listing_ms: u64,
    pub filter_ms: u64,
    pub draw_ms: u64,
    pub spawn_ms: u64,
    /// Spawn attempts the draw made (spawned or refused).
    pub spawn_attempts: usize,
    /// How many repos the listings read at once.
    pub gather_concurrency: usize,
    /// How many spawns ran at once (#12298).
    pub spawn_concurrency: usize,
    /// Inside spawns, by sub-phase (#12298), **summed over attempts**: with
    /// spawns running at once they can add up to more than `spawn_ms`.
    pub spawn_parts: SpawnParts,
}

/// A tick's draws and passes (#12410): why a tick with free slots made few
/// spawn attempts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TickPasses {
    /// Item draws taken, across workspaces.
    pub draws: usize,
    /// Draws that never reached the spawner: a repo at its cap or a second
    /// merge in a repo.
    pub draws_unspawned: usize,
    /// Passes by reason (`claim_lost`, `repo_cap`, ...).
    pub by_reason: std::collections::BTreeMap<String, usize>,
    /// The slots the tick could fill (free slots, bounded by the ramp cap).
    pub budget: usize,
    /// Why the draw loop stopped ([`super::tick::TickStop::as_str`]).
    pub stop: Option<&'static str>,
}

impl TickPasses {
    /// The tick span's `loom.dispatch.passes` value: `reason=count` pairs,
    /// comma-joined in reason order. Refusal reasons are an open set, so
    /// they ride one attribute value rather than a key each.
    #[must_use]
    pub fn render(&self) -> String {
        self.by_reason
            .iter()
            .map(|(reason, n)| format!("{reason}={n}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Where the time inside a tick's spawns went (#12298), in milliseconds
/// summed over its attempts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpawnParts {
    /// The role's resolvers: runtime admission, the pool preflight,
    /// containment.
    pub resolve_ms: u64,
    /// The claim: the item's lock, the pre-claim subprocess (`lease pr
    /// claim`) or a Builder's label flip and lease comment.
    pub claim_ms: u64,
    /// The launch: the dispatch stagger and the child's spawn.
    pub launch_ms: u64,
    /// The account-selection poll (up to 5 s per spawn) and the record.
    pub account_ms: u64,
}

/// Milliseconds in `d`, saturating. Pure.
#[must_use]
pub fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The default number of repos the step gather lists at once (#12256).
pub const DEFAULT_GATHER_CONCURRENCY: usize = 8;
/// The most repos the step gather lists at once, whatever the config says.
pub const MAX_GATHER_CONCURRENCY: usize = 32;

/// `autonomous.workFinder.gatherConcurrency` from an effective config: a
/// positive integer (at most [`MAX_GATHER_CONCURRENCY`]), else
/// [`DEFAULT_GATHER_CONCURRENCY`]. `1` restores the serial gather. Pure.
#[must_use]
pub fn gather_concurrency_from(effective: &serde_json::Value) -> usize {
    crate::config_resolver::get_path(effective, "autonomous.workFinder.gatherConcurrency")
        .and_then(serde_json::Value::as_u64)
        .filter(|n| *n > 0)
        .map_or(DEFAULT_GATHER_CONCURRENCY, |n| {
            usize::try_from(n).map_or(MAX_GATHER_CONCURRENCY, |n| n.min(MAX_GATHER_CONCURRENCY))
        })
}

/// The default number of spawns a step tick runs at once (#12298).
pub const DEFAULT_SPAWN_CONCURRENCY: usize = 4;
/// The most spawns a step tick runs at once, whatever the config says.
pub const MAX_SPAWN_CONCURRENCY: usize = 16;

/// `autonomous.workFinder.spawnConcurrency` from an effective config: a
/// positive integer (at most [`MAX_SPAWN_CONCURRENCY`]), else
/// [`DEFAULT_SPAWN_CONCURRENCY`]. `1` restores the serial spawns. Pure.
#[must_use]
pub fn spawn_concurrency_from(effective: &serde_json::Value) -> usize {
    crate::config_resolver::get_path(effective, "autonomous.workFinder.spawnConcurrency")
        .and_then(serde_json::Value::as_u64)
        .filter(|n| *n > 0)
        .map_or(DEFAULT_SPAWN_CONCURRENCY, |n| {
            usize::try_from(n).map_or(MAX_SPAWN_CONCURRENCY, |n| n.min(MAX_SPAWN_CONCURRENCY))
        })
}

/// The gather concurrency for the daemon rooted at `root`, read live. Only
/// the Direct backend lists per repo, so only it takes
/// `autonomous.workFinder.gatherConcurrency` (#12307).
#[must_use]
pub fn gather_concurrency(root: &std::path::Path) -> usize {
    gather_concurrency_for(&crate::config_resolver::resolve_effective_config(root))
}

/// [`gather_concurrency`] for an effective config. On the mirror's worklist
/// the gather reads one in-memory snapshot, so the key has nothing to
/// bound: it is ignored (the gather is serial) and, when set, a warning says
/// so once.
#[must_use]
pub fn gather_concurrency_for(effective: &serde_json::Value) -> usize {
    if crate::discovery::Chain::from_effective(effective).backend()
        != crate::discovery::BackendKind::Mirror
    {
        return gather_concurrency_from(effective);
    }
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let set =
        crate::config_resolver::get_path(effective, "autonomous.workFinder.gatherConcurrency")
            .is_some_and(|v| !v.is_null());
    if set && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        log::warn!(
            "autonomous.workFinder.gatherConcurrency is ignored while forge.mirror.discovery = \
             \"worklist\": the gather reads the tick's one snapshot, not one listing per repo \
             (#12307)"
        );
    }
    1
}

/// The spawn concurrency for the daemon rooted at `root`, read live
/// (#12298).
#[must_use]
pub fn spawn_concurrency(root: &std::path::Path) -> usize {
    spawn_concurrency_from(&crate::config_resolver::resolve_effective_config(root))
}

/// Run `work` over `jobs` on at most `workers` threads, each started with
/// `setup` (the caller's thread-local scopes carried over), and return the
/// results **in job order**, whatever order they finished in (#12256): the
/// tick's draw must not depend on which listing answered first. Jobs are
/// taken in order, one at a time, so `work` sees them start in job order
/// too. `work` returning `None` is a job not run (a deadline skip).
pub fn run_ordered<J, R, G>(
    jobs: Vec<J>,
    workers: usize,
    setup: impl Fn() -> G + Sync,
    work: impl Fn(usize, J) -> Option<R> + Sync,
) -> Vec<(usize, R)>
where
    J: Send,
    R: Send,
{
    let n = jobs.len();
    let workers = workers.clamp(1, n.max(1));
    let queue = Mutex::new(jobs.into_iter().enumerate());
    let results: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::with_capacity(n));
    let run = || {
        let _scope = setup();
        loop {
            let next = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .next();
            let Some((pos, job)) = next else { break };
            if let Some(r) = work(pos, job) {
                results
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push((pos, r));
            }
        }
    };
    if workers == 1 {
        run();
    } else {
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(run);
            }
        });
    }
    let mut out = results
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    out.sort_by_key(|(pos, _)| *pos);
    out
}

/// One tick's walk over `n` repos from `start`.
#[derive(Debug)]
pub struct GatherWalk {
    n: usize,
    start: usize,
    gathered: usize,
    skipped: Vec<usize>,
    began: Instant,
}

impl GatherWalk {
    /// A walk over `n` repos starting at `start` (taken modulo `n`).
    #[must_use]
    pub fn new(n: usize, start: usize, began: Instant) -> Self {
        Self {
            n,
            start: if n == 0 { 0 } else { start % n },
            gathered: 0,
            skipped: Vec::new(),
            began,
        }
    }

    /// The repo indices in walk order.
    pub fn order(&self) -> impl Iterator<Item = usize> {
        let (n, start) = (self.n, self.start);
        (0..n).map(move |k| (start + k) % n)
    }

    /// Whether to gather repo `idx` when the deadline has `passed`: yes for
    /// the first repo of the tick and for any repo before the deadline;
    /// otherwise it is recorded as skipped.
    pub fn admit(&mut self, idx: usize, passed: bool) -> bool {
        if passed && self.gathered > 0 {
            self.skipped.push(idx);
            return false;
        }
        self.gathered += 1;
        true
    }

    /// Whether the repo just admitted is the tick's first, the one it always
    /// gathers ([`GUARANTEED_FLOOR`]).
    #[must_use]
    pub fn is_guaranteed(&self) -> bool {
        self.gathered == 1
    }

    /// Where the next tick's walk starts: the first repo skipped this tick,
    /// else where this one started.
    #[must_use]
    pub fn next_start(&self) -> usize {
        self.skipped.first().copied().unwrap_or(self.start)
    }

    /// This walk's figures, measured at `now`.
    #[must_use]
    pub fn stats(&self, now: Instant) -> GatherStats {
        GatherStats {
            gather_ms: u64::try_from(now.saturating_duration_since(self.began).as_millis())
                .unwrap_or(u64::MAX),
            repos_gathered: self.gathered,
            repos_skipped: self.skipped.len(),
            truncated: !self.skipped.is_empty(),
        }
    }
}

/// Whether `deadline` (if any) has passed at `now`.
#[must_use]
pub fn deadline_passed(deadline: Option<Instant>, now: Instant) -> bool {
    deadline.is_some_and(|d| now >= d)
}

/// The default work-finder tick deadline.
pub const DEFAULT_TICK_DEADLINE: Duration = Duration::from_secs(90);

/// `autonomous.workFinder.tickDeadlineSecs` from an effective config: a
/// positive integer, else [`DEFAULT_TICK_DEADLINE`]. Pure.
#[must_use]
pub fn tick_deadline_from(effective: &serde_json::Value) -> Duration {
    crate::config_resolver::get_path(effective, "autonomous.workFinder.tickDeadlineSecs")
        .and_then(serde_json::Value::as_u64)
        .filter(|s| *s > 0)
        .map_or(DEFAULT_TICK_DEADLINE, Duration::from_secs)
}

/// The tick deadline for the daemon rooted at `root`, read live each tick.
#[must_use]
pub fn tick_deadline(root: &std::path::Path) -> Duration {
    tick_deadline_from(&crate::config_resolver::resolve_effective_config(root))
}

/// Start one work-finder tick body: log it at INFO, so a tick that never
/// reaches its dispatch line can be placed, and return its read budget
/// (#12074). The clock starts when the loop enters its read scope, after the
/// local pre-tick work (the eager reclaim, the CPU sample), so slow local work
/// never turns into forge shedding.
#[must_use]
pub fn begin_tick(root: &std::path::Path) -> Duration {
    static TICKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = TICKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let budget = tick_deadline(root);
    log::info!("work_finder: tick {n} start (deadline {}s, #12074)", budget.as_secs());
    budget
}

/// The read budget the one repo a tick always gathers gets, however late it
/// starts: its reads run until `max(tick deadline, start + this)`.
pub const GUARANTEED_FLOOR: Duration = Duration::from_secs(20);

/// Consecutive starved ticks after which a repo's reads may fall through to
/// the writer for one tick.
pub const STARVED_TICKS_BEFORE_WRITER: u32 = 3;

/// Which repos have been starved (#12074): their gather failed, or answered
/// only from a last good listing, or could not read the open-PR listing, tick
/// after tick. Inside a tick a read whose readers are out of budget is shed
/// rather than sent to the writer; this bounds that rule, so a long back-off
/// slows a repo down but never stops it.
#[derive(Debug, Default)]
pub struct Starvation {
    ticks: std::collections::HashMap<std::path::PathBuf, u32>,
}

impl Starvation {
    /// Whether `root`'s reads may use the writer this tick.
    #[must_use]
    pub fn escalate(&self, root: &std::path::Path) -> bool {
        self.ticks.get(root).copied().unwrap_or(0) >= STARVED_TICKS_BEFORE_WRITER
    }

    /// Record this tick's outcome for `root`. A healthy gather, or a tick
    /// that already escalated, starts the count again.
    pub fn record(&mut self, root: &std::path::Path, starved: bool, escalated: bool) {
        if starved && !escalated {
            *self.ticks.entry(root.to_path_buf()).or_default() += 1;
        } else {
            self.ticks.remove(root);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "gather_budget_tests.rs"]
mod tests;
