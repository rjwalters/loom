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
//!   by the existing `RoleAdmission::InProgress` check (#4364).
//! - **Different roles in one repository run at once.** The guard key is
//!   `(root, role)`. No role declares repository exclusivity.
//! - **Budgets.** The role runner's ceiling is its share of the one host
//!   budget ([`crate::host_budget`], #11911): the work finder's
//!   `maxConcurrent` less the steps in flight. The per-role budget
//!   (`autonomous.roleRunner.roleMaxConcurrent`, default
//!   [`default_role_max_concurrent`]) stops one role from taking every slot.
//!   Both are checked under one lock in
//!   [`RoleRunGuard::admit_with_role_budget`].
//! - **One refusal line per tick.** Once a tick hits the ceiling or the
//!   budget it stops admitting and logs a single summary line naming the
//!   limit and how many roots were deferred, instead of a WARN per root.
//! - **Round-robin fairness.** Because a tick stops at the first refusal and
//!   no run can finish mid-walk, a walk that always began at the head of the
//!   registry would serve only the first `budget` roots forever. Each tick
//!   therefore starts just after the last root admitted (wrapping), so every
//!   root gets a turn within `ceil(roots / budget)` ticks.
//! - **Skipped runs do not spend the tick.** An event-triggered role whose
//!   trigger is absent ends [`RoleTickOutcome::QueueEmpty`] (no agent was
//!   spent), and the walk resumes over the roots the tick deferred in the
//!   same interval ([`RoleDispatcher::resume_after_queue_empty`]).
//!
//! The item roles (Curator, Judge, Doctor) and Champion's item work are
//! steps the work finder draws (#11909), so the Judge/Doctor queue gate, the
//! demand width and Champion-first reservation (#9392) and the per-repository
//! Doctor lanes (#10630, #10632) that sized them are gone (#11911).
//!
//! Finished runs are reaped on the loop's own task ([`RoleDispatcher::handle_joined`]),
//! so the #4349 fail/recover dedup maps and the #7607 pool-exhausted feed stay
//! single-owner, exactly as in the serial loop.

use super::*;
use tokio::task::{Id, JoinError, JoinSet};

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
/// half the role runner's ceiling, at least 1.
///
/// A default of 1 would change nothing (the old serial loop was one run per
/// role); a default equal to the ceiling would let one role take every slot.
#[must_use]
pub fn default_role_max_concurrent(ceiling: usize) -> usize {
    (ceiling / 2).max(1)
}

/// `root`'s own resolved `autonomous.roleRunner` block (`Null` when absent),
/// read once per admission for the knobs kept out of [`RoleRunnerConfig`].
#[must_use]
pub fn role_runner_block(root: &Path) -> serde_json::Value {
    let effective = crate::config_resolver::resolve_effective_config(root);
    crate::config_resolver::get_path(&effective, "autonomous.roleRunner")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

/// Read `root`'s own `autonomous.roleRunner.roleMaxConcurrent` (resolved per
/// root and hot-applied every tick, like `maxConcurrent`). Kept out of
/// [`RoleRunnerConfig`] so its many exhaustive test literals stay untouched.
#[must_use]
pub fn read_role_max_concurrent(root: &Path) -> BTreeMap<String, usize> {
    parse_role_max_concurrent(&role_runner_block(root))
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
    /// The role runner's share of the host budget, all roles together.
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

impl LimitRefusal {
    /// The stable wire name of the limit (`role_tick.outcome`'s
    /// `deferred_by`, #12084).
    #[must_use]
    pub const fn kind(self) -> &'static str {
        match self {
            Self::Ceiling { .. } => "ceiling",
            Self::RoleBudget { .. } => "role_budget",
        }
    }
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

/// Admit `(root, role)` against the role runner's share of the host budget
/// and `role`'s budget. The tail of `decide_root_tick_detailed`, shared so
/// tests can drive admission without the enablement/sharding prefix.
#[must_use]
pub fn admit_root_tick(
    root: &Path,
    role: &'static str,
    prompt: String,
    in_progress: &InProgressGuard,
    _config: &RoleRunnerConfig,
) -> RootTickDecision {
    let ceiling = resolve_ceiling(root);
    let budgets = read_role_max_concurrent(root);
    let budget = resolve_role_max_concurrent(&budgets, role, ceiling);
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
             flight at the role runner's share of the host budget, {ceiling} \
             (autonomous.workFinder.maxConcurrent less the steps in flight, #6102, #11911); \
             {deferred} root(s) deferred to the next tick"
        ),
        LimitRefusal::RoleBudget { active, budget } => format!(
            "role_runner: {role} tick stopped admitting — {active} {role} run(s) already in \
             flight at its budget of {budget} (autonomous.roleRunner.roleMaxConcurrent, #9391); \
             {deferred} root(s) deferred to the next tick"
        ),
    }
}

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

/// The `gh` binary the trigger listings use: `LOOM_GH_BIN` when set, else
/// the resolved default.
#[must_use]
pub fn gate_gh_bin() -> PathBuf {
    std::env::var("LOOM_GH_BIN")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map_or_else(|| PathBuf::from(crate::gh_invocation::gh_bin()), PathBuf::from)
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
    observer: Option<Arc<dyn PoolExhaustedObserver>>,
    decide: DecideFn,
    decide_state: DecideState,
    /// The auditor/guide event-trigger probe and ledger (#10816).
    trigger_probe: triggers::TriggerProbe,
    trigger_ledger: &'static triggers::TriggerLedger,
    in_flight: JoinSet<FinishedRun>,
    /// Which root each task runs, so a panicked task still names its root.
    running: HashMap<Id, PathBuf>,
    /// Per-root fail edge (#4349), no-token-pool (#4642), pool-exhausted
    /// (#7607) and model-mismatch (#5028) dedup state, each independent.
    failing_roots: HashMap<PathBuf, bool>,
    no_token_pool_roots: HashMap<PathBuf, bool>,
    pool_exhausted_roots: HashMap<PathBuf, bool>,
    model_mismatch_roots: HashMap<PathBuf, bool>,
    /// Round-robin cursor: the last root a walk admitted. The next tick
    /// starts just after it.
    last_admitted: Option<PathBuf>,
    /// `last_admitted`'s registry index. When that root has left the list,
    /// the root that followed it has slid into this index, so the next tick
    /// starts here.
    last_admitted_index: usize,
    /// Roots the current tick has not decided yet (deferred by a limit), in
    /// walk order with their registry index.
    pending: VecDeque<(usize, PathBuf)>,
    /// A run returned `QueueEmpty` since the last walk, so the `pending`
    /// roots may be resumed.
    resume_requested: bool,
}

impl RoleDispatcher {
    /// A dispatcher using the production decision for `spec`.
    #[must_use]
    pub fn new(
        spec: RoleSpec,
        interval: Duration,
        runner_factory: RunnerFactory,
        observer: Option<Arc<dyn PoolExhaustedObserver>>,
    ) -> Self {
        Self::with_decide(spec, interval, runner_factory, observer, production_decide(spec))
            .with_triggers(triggers::forge_trigger_probe(), triggers::global())
    }

    /// Use `probe` and `ledger` for the event triggers (#10816).
    /// [`Self::with_decide`] defaults to [`triggers::no_trigger_probe`], which
    /// never reads git or the forge (an enabled gate then fails open).
    #[must_use]
    pub fn with_triggers(
        mut self,
        probe: triggers::TriggerProbe,
        ledger: &'static triggers::TriggerLedger,
    ) -> Self {
        self.trigger_probe = probe;
        self.trigger_ledger = ledger;
        self
    }

    /// A dispatcher with an injected per-root decision (tests).
    #[must_use]
    pub fn with_decide(
        spec: RoleSpec,
        interval: Duration,
        runner_factory: RunnerFactory,
        observer: Option<Arc<dyn PoolExhaustedObserver>>,
        decide: DecideFn,
    ) -> Self {
        Self {
            spec,
            interval,
            runner_factory,
            observer,
            decide,
            decide_state: DecideState::default(),
            trigger_probe: triggers::no_trigger_probe(),
            trigger_ledger: triggers::global(),
            in_flight: JoinSet::new(),
            running: HashMap::new(),
            failing_roots: HashMap::new(),
            no_token_pool_roots: HashMap::new(),
            pool_exhausted_roots: HashMap::new(),
            model_mismatch_roots: HashMap::new(),
            last_admitted: None,
            last_admitted_index: 0,
            pending: VecDeque::new(),
            resume_requested: false,
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

    /// Decide every root and spawn each admitted run without waiting for it,
    /// starting just after the last root admitted (round-robin). Must be
    /// called inside a tokio runtime.
    pub fn dispatch_tick(
        &mut self,
        roots: Vec<PathBuf>,
        in_progress: &InProgressGuard,
    ) -> TickReport {
        let start = self.rotation_start(&roots);
        let mut order: VecDeque<(usize, PathBuf)> = roots.into_iter().enumerate().collect();
        order.rotate_left(start);
        // A new tick replaces whatever the previous one left undecided.
        self.resume_requested = false;
        let report = self.walk(order, in_progress);
        if let Some(refusal) = report.refusal {
            log::warn!("{}", refusal_summary_line(self.spec.name, refusal, report.deferred));
            self.emit_deferred(refusal);
        }
        report
    }

    /// Record each root this tick left in `pending` as deferred (#12084). Only
    /// the tick calls it: a resume re-walks the same roots and would repeat
    /// the fact. Off the async loop, because it resolves each repo's slug and
    /// shard.
    fn emit_deferred(&self, refusal: LimitRefusal) {
        let roots: Vec<PathBuf> = self.pending.iter().map(|(_, root)| root.clone()).collect();
        if roots.is_empty() {
            return;
        }
        let (role, kind) = (self.spec.name, refusal.kind());
        tokio::task::spawn_blocking(move || {
            for root in &roots {
                crate::role_tick_telemetry::deferred::emit_deferred(root, role, kind);
            }
        });
    }

    /// Resume this tick's deferred roots after a run returned
    /// [`RoleTickOutcome::QueueEmpty`], so an empty queue does not use up the
    /// tick's budget. `None` when no resume is due. Must be called inside a
    /// tokio runtime.
    pub fn resume_after_queue_empty(
        &mut self,
        in_progress: &InProgressGuard,
    ) -> Option<TickReport> {
        if !std::mem::take(&mut self.resume_requested) || self.pending.is_empty() {
            return None;
        }
        let order = std::mem::take(&mut self.pending);
        let report = self.walk(order, in_progress);
        if let Some(refusal) = report.refusal {
            // The tick already logged its WARN summary; a resume that stops
            // again is the same condition, so keep it at DEBUG.
            log::debug!(
                "{} (resumed after an empty queue)",
                refusal_summary_line(self.spec.name, refusal, report.deferred)
            );
        }
        Some(report)
    }

    /// Whether a finished `QueueEmpty` run has made a resume due.
    #[must_use]
    pub fn resume_due(&self) -> bool {
        self.resume_requested && !self.pending.is_empty()
    }

    /// Index of the first root of this tick's walk: just after the last root
    /// admitted, or its old index if that root left the list.
    fn rotation_start(&self, roots: &[PathBuf]) -> usize {
        if roots.is_empty() {
            return 0;
        }
        self.last_admitted
            .as_ref()
            .and_then(|last| roots.iter().position(|r| r == last))
            .map_or(self.last_admitted_index, |i| i + 1)
            % roots.len()
    }

    /// Decide `order` front to back, stopping at the first limit refusal.
    /// The refused root and every root after it are kept in `pending`.
    fn walk(
        &mut self,
        mut order: VecDeque<(usize, PathBuf)>,
        in_progress: &InProgressGuard,
    ) -> TickReport {
        let mut report = TickReport::default();
        while let Some((index, root)) = order.pop_front() {
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
                    self.last_admitted = Some(root.clone());
                    self.last_admitted_index = index;
                    report.spawned.push(root);
                }
                RootTickDecision::Skip => {}
                RootTickDecision::InProgress => report.in_progress.push(root),
                RootTickDecision::Refused(refusal) => {
                    // The limit is host-wide: every later root would be
                    // refused for the same reason, so stop deciding and keep
                    // the rest for a resume or the next tick's rotation.
                    report.refusal = Some(refusal);
                    order.push_front((index, root));
                    break;
                }
            }
        }
        report.deferred = order.len();
        self.pending = order;
        report
    }

    fn spawn(&mut self, root: PathBuf, prompt: String, guard: RoleRunGuard) {
        let name = self.spec.name;
        let interval = self.interval;
        let factory = Arc::clone(&self.runner_factory);
        let (trigger_probe, trigger_ledger) =
            (Arc::clone(&self.trigger_probe), self.trigger_ledger);
        let task_root = root.clone();
        let handle = self.in_flight.spawn_blocking(move || {
            // Held for the run's real lifetime; dropped on every exit path,
            // including a panic unwinding this closure.
            let _guard = guard;
            let tick_start = Instant::now();
            let started_at = chrono::Utc::now();
            crate::observability::pick_decision::clear_role_tick_stash();
            let mut runner = factory(task_root.clone());
            // Auditor/guide event trigger (#10816); a no-op unless this root
            // enables `eventTriggers`.
            let triggers_cfg = if triggers::is_triggered_role(name) {
                triggers::read_event_trigger_config(&task_root)
            } else {
                triggers::EventTriggerConfig::default()
            };
            let outcome = triggers::run_with_trigger_gate(
                triggers_cfg,
                &trigger_probe,
                trigger_ledger,
                &task_root,
                name,
                || invoke_with_collision_probe(&mut *runner, &task_root, name, &prompt, interval),
            );
            // Durable `role_tick.outcome` record (#8056), best-effort.
            crate::role_tick_telemetry::emit_for_tick_correlated(
                &task_root,
                name,
                started_at,
                &outcome,
                runner.resolved_launch(),
                runner.trace_context(),
                runner.unlaunched_failure(),
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
                if matches!(run.outcome, RoleTickOutcome::QueueEmpty) {
                    // No agent was spent, so the slot it held may go to a
                    // root this tick deferred.
                    self.resume_requested = true;
                }
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
