//! Concurrent per-`(repository, role)` dispatch for the multi-workspace role
//! loop (issue #9391).
//!
//! Before #9391 each role loop awaited every registered workspace in turn, so
//! a role was effectively one session per daemon: a host with 62 workspaces
//! gave judge one PR pass per workspace rotation, however deep the queue.
//! The repository is the parallelism boundary instead:
//!
//! - **One instance per `(repository, role)`.** Each admitted run is spawned
//!   into a [`JoinSet`] and holds its [`RoleRunGuard`] until it finishes. The
//!   tick does not wait for it; a run still going at the next tick is refused
//!   by the existing `RoleAdmission::InProgress` check (#4364), which is what
//!   keeps a repository to one instance of a role.
//! - **Different roles in one repository run at once.** The guard key is
//!   `(root, role)`, and the label state machine (`loom:reviewing`,
//!   `loom:treating`, ...) arbitrates between roles, as it already does
//!   between hosts. No role declares repository exclusivity.
//! - **Budgets.** The host ceiling (`autonomous.roleRunner.maxConcurrent`)
//!   still counts every role together; the per-role budget
//!   (`autonomous.roleRunner.roleMaxConcurrent`, default
//!   [`default_role_max_concurrent`]) stops one role from taking every slot.
//!   Both are checked under one lock in
//!   [`RoleRunGuard::admit_with_role_budget`].
//! - **Queue gating.** A role with a label-defined work queue
//!   ([`work_queue_labels`]) is not spawned for a repository whose queue is
//!   empty; the tick records [`RoleTickOutcome::QueueEmpty`]. A listing error
//!   fails open.
//! - **One refusal line per tick.** Once a tick hits the ceiling or the
//!   budget it stops admitting and logs a single summary line naming the
//!   limit and how many roots were deferred, instead of a WARN per root.
//!
//! Finished runs are reaped on the loop's own task ([`RoleDispatcher::handle_joined`]),
//! so the #4349 fail/recover dedup maps and the #7607 pool-exhausted feed stay
//! single-owner, exactly as in the serial loop.

use super::*;
use tokio::task::{Id, JoinError, JoinSet};

/// `forge_call_stats` caller name for the queue-gate listing.
pub const QUEUE_GATE_CALLER: &str = "role_queue_gate";

/// The work-queue labels a role is gated on, or `None` for an ungated role
/// (dispatched every tick, as before #9391).
///
/// Only judge and doctor are gated in Phase 1: each has exactly one queue
/// label. Champion also promotes `loom:curated` issues and curator works
/// unlabeled issues, so a single-label gate would starve part of their work
/// (`role_collision::probe_target_for_role` is deliberately not reused — its
/// curator target is the in-flight marker, not the queue).
#[must_use]
pub fn work_queue_labels(role: &str) -> Option<&'static [&'static str]> {
    match role {
        "judge" => Some(&["loom:review-requested"]),
        "doctor" => Some(&["loom:changes-requested"]),
        _ => None,
    }
}

/// Parse `autonomous.roleRunner.roleMaxConcurrent` (`{"<role>": N}`).
///
/// Keys are trimmed and lower-cased (like `roleModels`/`onIdleMaxWait`); a
/// blank key, or a zero, negative or non-integer value, is dropped **per
/// entry**, so that role falls back to the default budget. An absent or
/// non-object value is an empty map.
#[must_use]
pub fn parse_role_max_concurrent(block: &serde_json::Value) -> BTreeMap<String, usize> {
    block
        .get("roleMaxConcurrent")
        .and_then(serde_json::Value::as_object)
        .map(|obj| {
            obj.iter()
                .filter_map(|(k, v)| {
                    let key = k.trim().to_ascii_lowercase();
                    if key.is_empty() {
                        return None;
                    }
                    let n = v.as_u64().filter(|&n| n > 0)?;
                    Some((key, usize::try_from(n).ok()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The per-role budget when `roleMaxConcurrent` names no value for a role:
/// half the host ceiling, at least 1 (3 at the default ceiling of 7).
///
/// A default of 1 would change nothing (the old serial loop was one run per
/// role); a default equal to the ceiling would let one role take every slot.
#[must_use]
pub fn default_role_max_concurrent(ceiling: usize) -> usize {
    (ceiling / 2).max(1)
}

/// Read `root`'s own `autonomous.roleRunner.roleMaxConcurrent` (resolved per
/// root and hot-applied every tick, like `maxConcurrent`). Kept out of
/// [`RoleRunnerConfig`] so its many exhaustive test literals stay untouched.
#[must_use]
pub fn read_role_max_concurrent(root: &Path) -> BTreeMap<String, usize> {
    let effective = crate::config_resolver::resolve_effective_config(root);
    crate::config_resolver::get_path(&effective, "autonomous.roleRunner")
        .map(parse_role_max_concurrent)
        .unwrap_or_default()
}

/// Resolve `role`'s budget against `ceiling`: the configured value, else
/// [`default_role_max_concurrent`], clamped to the ceiling (a budget above
/// it could never bind).
#[must_use]
pub fn resolve_role_max_concurrent(
    budgets: &BTreeMap<String, usize>,
    role: &str,
    ceiling: usize,
) -> usize {
    let ceiling = ceiling.max(1);
    budgets
        .get(role)
        .copied()
        .unwrap_or_else(|| default_role_max_concurrent(ceiling))
        .min(ceiling)
}

/// A host-level limit that refused an admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitRefusal {
    /// The host ceiling (`maxConcurrent`, all roles together).
    Ceiling {
        /// Role runs in flight across every workspace.
        active: usize,
        /// The ceiling compared against.
        ceiling: usize,
    },
    /// This role's own budget (`roleMaxConcurrent`).
    RoleBudget {
        /// Runs of this role in flight across every workspace.
        active: usize,
        /// The budget compared against.
        budget: usize,
    },
}

/// The per-root decision of one tick.
#[derive(Debug)]
pub enum RootTickDecision {
    /// Admitted: invoke `prompt`, holding `guard` for the whole run.
    Admit {
        /// The `claude -p` prompt.
        prompt: String,
        /// The `(root, role)` in-progress entry.
        guard: RoleRunGuard,
    },
    /// Nothing to do for this root (disabled, sharded away, not configured,
    /// ...); the decision already logged why.
    Skip,
    /// A run of this `(root, role)` is still in flight (#4364).
    InProgress,
    /// Refused by the host ceiling or the role budget.
    Refused(LimitRefusal),
}

impl RootTickDecision {
    /// The admitted `(prompt, guard)`, discarding why anything else was not.
    #[must_use]
    pub fn into_admitted(self) -> Option<(String, RoleRunGuard)> {
        match self {
            Self::Admit { prompt, guard } => Some((prompt, guard)),
            Self::Skip | Self::InProgress | Self::Refused(_) => None,
        }
    }
}

/// Admit `(root, role)` against `config`'s host ceiling and `role`'s budget.
/// The tail of `decide_root_tick_detailed`, shared so tests can drive
/// admission without the enablement/sharding prefix.
#[must_use]
pub fn admit_root_tick(
    root: &Path,
    role: &'static str,
    prompt: String,
    in_progress: &InProgressGuard,
    config: &RoleRunnerConfig,
) -> RootTickDecision {
    let ceiling = resolve_max_concurrent(config);
    let budget = resolve_role_max_concurrent(&read_role_max_concurrent(root), role, ceiling);
    match RoleRunGuard::admit_with_role_budget(
        in_progress.clone(),
        root.to_path_buf(),
        role,
        ceiling,
        budget,
    ) {
        RoleAdmission::Admitted(guard) => RootTickDecision::Admit { prompt, guard },
        RoleAdmission::InProgress => {
            log::debug!(
                "role_runner: {role} tick for {} skipped — a run is already in progress (#4364)",
                root.display()
            );
            RootTickDecision::InProgress
        }
        RoleAdmission::CeilingReached { active, ceiling } => {
            RootTickDecision::Refused(LimitRefusal::Ceiling { active, ceiling })
        }
        RoleAdmission::RoleBudgetReached { active, budget, .. } => {
            RootTickDecision::Refused(LimitRefusal::RoleBudget { active, budget })
        }
    }
}

/// The single summary line for a tick that stopped admitting at a limit.
#[must_use]
pub fn refusal_summary_line(role: &str, refusal: LimitRefusal, deferred: usize) -> String {
    match refusal {
        LimitRefusal::Ceiling { active, ceiling } => format!(
            "role_runner: {role} tick stopped admitting — {active} role agent(s) already in \
             flight at the host ceiling of {ceiling} (autonomous.roleRunner.maxConcurrent / \
             {ROLE_RUNNER_MAX_CONCURRENT_ENV}, #6102); {deferred} root(s) deferred to the next \
             tick"
        ),
        LimitRefusal::RoleBudget { active, budget } => format!(
            "role_runner: {role} tick stopped admitting — {active} {role} run(s) already in \
             flight at its budget of {budget} (autonomous.roleRunner.roleMaxConcurrent, #9391); \
             {deferred} root(s) deferred to the next tick"
        ),
    }
}

/// Whether `root`'s queue for any of `labels` has an open item. `Err` means
/// the listing failed, and the caller dispatches anyway (fail open).
pub type QueueProbe = Arc<dyn Fn(&Path, &[&str]) -> Result<bool, String> + Send + Sync>;

/// Builds the invocation runner for one root.
pub type RunnerFactory = Arc<dyn Fn(PathBuf) -> Box<dyn RoleInvocationRunner + Send> + Send + Sync>;

/// Decides one root's tick. Production is `decide_root_tick_detailed`; tests
/// substitute a narrower decision.
pub type DecideFn =
    Box<dyn FnMut(&Path, &InProgressGuard, &mut DecideState) -> RootTickDecision + Send>;

/// The warn-once / log-once state `decide_root_tick_detailed` carries across
/// ticks (#4377, #5654, #6163).
#[derive(Debug, Default)]
pub struct DecideState {
    disabled_roots_warned: HashSet<PathBuf>,
    resolved_roles_logged: HashMap<PathBuf, String>,
    missing_defaults_logged: HashMap<PathBuf, Vec<&'static str>>,
}

/// The production decision for `spec`.
#[must_use]
pub fn production_decide(spec: RoleSpec) -> DecideFn {
    Box::new(move |root, in_progress, state| {
        decide_root_tick_detailed(
            root,
            &spec,
            in_progress,
            &mut state.disabled_roots_warned,
            &mut state.resolved_roles_logged,
            &mut state.missing_defaults_logged,
        )
    })
}

/// The production runner: `spawn-claude.sh` via [`ScriptRoleInvocationRunner`].
#[must_use]
pub fn script_runner_factory() -> RunnerFactory {
    Arc::new(|root| Box::new(ScriptRoleInvocationRunner::new(root)))
}

/// The production queue probe: the ETag-cached REST listing, so an unchanged
/// queue costs a free `304`. REST issue listings include pull requests, which
/// is what judge and doctor queues hold.
#[must_use]
pub fn forge_queue_probe() -> QueueProbe {
    Arc::new(|root, labels| {
        let gh_bin = std::env::var("LOOM_GH_BIN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| PathBuf::from("gh"), PathBuf::from);
        for label in labels {
            let rows = crate::forge_listing::list_issues_cached_as(
                QUEUE_GATE_CALLER,
                &gh_bin,
                Some(root),
                None,
                label,
                "open",
            )
            .map_err(|e| e.to_string())?;
            if !rows.is_empty() {
                return Ok(true);
            }
        }
        Ok(false)
    })
}

/// Run one admitted invocation on the blocking thread: the queue gate, then
/// the collision-probed invocation. Returns without spawning an agent when a
/// gated role's queue is empty.
fn run_gated<R: RoleInvocationRunner + ?Sized>(
    runner: &mut R,
    probe: &QueueProbe,
    root: &Path,
    role: &'static str,
    prompt: &str,
    interval: Duration,
) -> RoleTickOutcome {
    if let Some(labels) = work_queue_labels(role) {
        match probe(root, labels) {
            Ok(false) => return RoleTickOutcome::QueueEmpty,
            Ok(true) => {}
            Err(e) => log::debug!(
                "role_runner: {role} queue probe for {} failed ({e}) — dispatching anyway \
                 (fail open, #9391)",
                root.display()
            ),
        }
    }
    invoke_with_collision_probe(runner, root, role, prompt, interval)
}

/// One finished run, as returned by its task.
#[derive(Debug)]
pub struct FinishedRun {
    /// The workspace the run was for.
    pub root: PathBuf,
    /// What it did.
    pub outcome: RoleTickOutcome,
    /// How long it took, measured inside the task.
    pub elapsed: Duration,
}

/// What one [`RoleDispatcher::dispatch_tick`] did.
#[derive(Debug, Default)]
pub struct TickReport {
    /// Roots whose run was spawned this tick.
    pub spawned: Vec<PathBuf>,
    /// Roots refused because their run of this role was still in flight.
    pub in_progress: Vec<PathBuf>,
    /// The limit that stopped admission, if any.
    pub refusal: Option<LimitRefusal>,
    /// Roots not admitted because of `refusal` (the refused root included).
    pub deferred: usize,
}

/// The per-role dispatcher: owns the in-flight runs and the log-dedup state.
pub struct RoleDispatcher {
    spec: RoleSpec,
    interval: Duration,
    runner_factory: RunnerFactory,
    queue_probe: QueueProbe,
    observer: Option<Arc<dyn PoolExhaustedObserver>>,
    decide: DecideFn,
    decide_state: DecideState,
    in_flight: JoinSet<FinishedRun>,
    /// Which root each task runs, so a panicked task still names its root.
    running: HashMap<Id, PathBuf>,
    /// Per-root fail edge (#4349), no-token-pool (#4642), pool-exhausted
    /// (#7607) and model-mismatch (#5028) dedup state, each independent.
    failing_roots: HashMap<PathBuf, bool>,
    no_token_pool_roots: HashMap<PathBuf, bool>,
    pool_exhausted_roots: HashMap<PathBuf, bool>,
    model_mismatch_roots: HashMap<PathBuf, bool>,
}

impl RoleDispatcher {
    /// A dispatcher using the production decision for `spec`.
    #[must_use]
    pub fn new(
        spec: RoleSpec,
        interval: Duration,
        runner_factory: RunnerFactory,
        queue_probe: QueueProbe,
        observer: Option<Arc<dyn PoolExhaustedObserver>>,
    ) -> Self {
        Self::with_decide(
            spec,
            interval,
            runner_factory,
            queue_probe,
            observer,
            production_decide(spec),
        )
    }

    /// A dispatcher with an injected per-root decision (tests).
    #[must_use]
    pub fn with_decide(
        spec: RoleSpec,
        interval: Duration,
        runner_factory: RunnerFactory,
        queue_probe: QueueProbe,
        observer: Option<Arc<dyn PoolExhaustedObserver>>,
        decide: DecideFn,
    ) -> Self {
        Self {
            spec,
            interval,
            runner_factory,
            queue_probe,
            observer,
            decide,
            decide_state: DecideState::default(),
            in_flight: JoinSet::new(),
            running: HashMap::new(),
            failing_roots: HashMap::new(),
            no_token_pool_roots: HashMap::new(),
            pool_exhausted_roots: HashMap::new(),
            model_mismatch_roots: HashMap::new(),
        }
    }

    /// Runs spawned and not yet reaped.
    #[must_use]
    pub fn in_flight_len(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether `root` is currently marked failing by the #4349 dedup.
    #[must_use]
    pub fn is_failing(&self, root: &Path) -> bool {
        self.failing_roots.get(root).copied().unwrap_or(false)
    }

    /// Decide every root and spawn each admitted run without waiting for it.
    /// Must be called inside a tokio runtime.
    pub fn dispatch_tick(
        &mut self,
        roots: Vec<PathBuf>,
        in_progress: &InProgressGuard,
    ) -> TickReport {
        let mut report = TickReport::default();
        for root in roots {
            if report.refusal.is_some() {
                // The limit is host-wide: every later root would be refused
                // for the same reason, so stop deciding for this tick.
                report.deferred += 1;
                continue;
            }
            // #6201 AC2: a panic in the synchronous decision must skip only
            // this root, never end the loop. `AssertUnwindSafe` is sound: the
            // captured state is log-dedup bookkeeping, at worst one tick stale.
            let decision = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (self.decide)(&root, in_progress, &mut self.decide_state)
            })) {
                Ok(decision) => decision,
                Err(panic) => {
                    log::error!(
                        "role_runner: {} tick decision for {} panicked ({}) — skipping only this \
                         root's this tick; the loop continues on the next interval (#6201)",
                        self.spec.name,
                        root.display(),
                        describe_panic(&*panic)
                    );
                    continue;
                }
            };
            match decision {
                RootTickDecision::Admit { prompt, guard } => {
                    self.spawn(root.clone(), prompt, guard);
                    report.spawned.push(root);
                }
                RootTickDecision::Skip => {}
                RootTickDecision::InProgress => report.in_progress.push(root),
                RootTickDecision::Refused(refusal) => {
                    report.refusal = Some(refusal);
                    report.deferred += 1;
                }
            }
        }
        if let Some(refusal) = report.refusal {
            log::warn!("{}", refusal_summary_line(self.spec.name, refusal, report.deferred));
        }
        report
    }

    fn spawn(&mut self, root: PathBuf, prompt: String, guard: RoleRunGuard) {
        let name = self.spec.name;
        let interval = self.interval;
        let factory = Arc::clone(&self.runner_factory);
        let probe = Arc::clone(&self.queue_probe);
        let task_root = root.clone();
        let handle = self.in_flight.spawn_blocking(move || {
            // Held for the run's real lifetime; dropped on every exit path,
            // including a panic unwinding this closure.
            let _guard = guard;
            let tick_start = Instant::now();
            let started_at = chrono::Utc::now();
            let mut runner = factory(task_root.clone());
            let outcome = run_gated(&mut *runner, &probe, &task_root, name, &prompt, interval);
            // Durable `role_tick.outcome` record (#8056), best-effort.
            crate::role_tick_telemetry::emit_for_tick_correlated(
                &task_root,
                name,
                started_at,
                &outcome,
                runner.resolved_launch(),
                runner.trace_context(),
            );
            FinishedRun {
                root: task_root,
                outcome,
                elapsed: tick_start.elapsed(),
            }
        });
        self.running.insert(handle.id(), root);
    }

    /// Wait for the next run to finish (`None` when nothing is in flight).
    pub async fn join_next(&mut self) -> Option<Result<(Id, FinishedRun), JoinError>> {
        self.in_flight.join_next_with_id().await
    }

    /// Reap every run that has already finished, without waiting.
    pub fn reap_finished(&mut self) {
        while let Some(joined) = self.in_flight.try_join_next_with_id() {
            self.handle_joined(joined);
        }
    }

    /// Log and account one finished (or panicked) run.
    pub fn handle_joined(&mut self, joined: Result<(Id, FinishedRun), JoinError>) {
        match joined {
            Ok((id, run)) => {
                self.running.remove(&id);
                feed_pool_exhausted_observer(
                    self.observer.as_deref(),
                    &run.outcome,
                    &run.root,
                    self.spec.name,
                );
                log_outcome_for_root_deduped(
                    self.spec.name,
                    &run.root,
                    &run.outcome,
                    run.elapsed,
                    &mut self.failing_roots,
                    &mut self.no_token_pool_roots,
                    &mut self.pool_exhausted_roots,
                    &mut self.model_mismatch_roots,
                );
            }
            Err(e) => {
                let root = self
                    .running
                    .remove(&e.id())
                    .map_or_else(|| "<unknown root>".to_string(), |r| r.display().to_string());
                log::error!(
                    "role_runner: {} invocation task for {root} panicked ({e}); the loop keeps \
                     ticking",
                    self.spec.name
                );
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "tests/concurrent_dispatch.rs"]
mod tests;
