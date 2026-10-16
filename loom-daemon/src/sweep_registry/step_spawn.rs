//! Spawning one step (Issue #11906; design #11894 §4.4 and §5).
//!
//! The step engine draws an item and the role its state calls for; this is
//! the launch. One step is one `/loom:<role> <N>` session
//! ([`StepRole::prompt`]) through the workspace's `spawn-worker.sh`, tracked
//! in the sweep registry as `SweepKind::Step` so the reaper, roll pause,
//! occupancy and restore all see it (#11898).
//!
//! # Resolvers
//!
//! A step runs with its **role's** resolvers, never the sweep's: the role
//! runner's runtime admission and pool gate for that role
//! (`runtime_admission::resolve_and_admit` then
//! `role_runner::runtime_preflight::check`, preference walk included), and
//! the role's model and effort (`roleModels` / `roleEfforts`, the role draw's
//! model when one picked the tap, #7894's reconciliation, #5028's mismatch
//! refusal). Only a Builder step keeps the per-issue experiment arm
//! (`model_dispatch.rs`). A binding whose only unmet requirement is
//! repository isolation is admitted on private-clone containment (#8787),
//! as a role run is. A refused gate refuses the step before any claim.
//!
//! # Claims (B1)
//!
//! - **Builder:** today's pre-claim: the `issue-<N>` lock, `loom:issue` →
//!   `loom:building`, the issue lease naming `step=builder`, and the
//!   dispatch-started renewal loop. `LOOM_SWEEP_CLAIM_OWNED` tells the
//!   prompt the claim is its invoker's.
//! - **Every other role:** the item's lock, then **only** the role-keyed
//!   lease (`lease pr claim --role <role> --step <step>`, under the step's
//!   sweep id). The prompt applies the claim label itself: Judge, Doctor,
//!   Curator and Champion all read a fresh claim label as another agent's.
//!   The session's own `lease pr claim` (no `--step`) resolves the same
//!   sweep id from `LOOM_TERMINAL_ID`; a lease is owned by role, host and
//!   sweep, the step being metadata (#11950), so it re-attaches to the
//!   pre-taken lease instead of standing down.
//!
//! The `(item, step)` claim is advertised to peers at spawn ([`lease`]), not
//! only on the reaper's heartbeat, so two hosts cannot draw the same item in
//! between.
//!
//! # Landed work (#12251)
//!
//! Before a Builder step claims an issue, the forge is asked once whether
//! its work already landed: a merged PR that closes it, or one built on its
//! own `feature/issue-N` branch into the default branch
//! (`work_finder::landed_prefilter::landed_at_spawn`). A landed issue is
//! routed to Curator, the step is refused `already_landed`, and the Builder
//! is held off it fleet-wide ([`SweepRegistry::hold_landed_builder`]).
//!
//! # After the spawn
//!
//! Every step's lease is renewed while it runs ([`lease`]); every step has
//! its role's timeout, and the reaper kills a step past it ([`timeout`]);
//! its end emits the role tick's capture ([`capture`]).

use super::*;
use crate::observability::ops::dispatch_step::{self, StepDispatch};
use crate::step_role::StepRole;
use crate::tokens_pool::private_workspace::{containment::Preparer, JobKind};

pub(crate) mod capture;
pub mod draw_claims;
pub mod ended;
pub mod launch_gate;
pub(crate) mod lease;
mod pool_state;
pub(crate) mod span;
pub(crate) mod timeout;
mod unlocked;
pub(crate) use unlocked::blocking;

pub use timeout::step_timeout;
pub use unlocked::StepSpawnTimings;

/// One step the engine asks to spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepSpawnRequest {
    pub item: u32,
    pub role: StepRole,
    /// 1-based draw attempt within the slot (design §4.2 step 4).
    pub draw_attempt: u32,
    /// Which stage-entry source answered the item's age (design §4.3).
    pub age_source: String,
    /// Whether the step resumes a paused session.
    pub resumed: bool,
}

impl StepSpawnRequest {
    /// A first-draw, fresh step on `item` for `role`, its age source unknown.
    #[must_use]
    pub fn new(item: u32, role: StepRole) -> Self {
        Self {
            item,
            role,
            draw_attempt: 1,
            age_source: "unknown".to_string(),
            resumed: false,
        }
    }

    fn kind(&self) -> SweepKind {
        SweepKind::Step {
            item: self.item,
            role: self.role,
        }
    }
}

/// Why a step was not spawned. `reason` is a closed token: the
/// `loom.dispatch.step` span's `loom.dispatch.reason`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRefusal {
    pub reason: &'static str,
    pub detail: String,
}

impl StepRefusal {
    pub(crate) fn new(reason: &'static str, detail: impl std::fmt::Display) -> Self {
        Self {
            reason,
            detail: detail.to_string(),
        }
    }
}

impl std::fmt::Display for StepRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "step refused ({}): {}", self.reason, self.detail)
    }
}

impl std::error::Error for StepRefusal {}

/// What a step launches with: its role's resolvers' answer.
pub(crate) struct StepLaunch {
    pub(crate) admission: crate::runtime_preference::DispatchAdmission,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<String>,
    /// A private-clone containment selection (#8787), when admission needed
    /// one; the launch uses exactly it.
    pub(crate) selection: Option<crate::tokens_pool::private_workspace::dispatch::Selection>,
}

/// A spawned step awaiting its account-selection poll, which the caller runs
/// with the registry lock released ([`poll_and_classify_spawned_child`]),
/// then hands back to [`SweepRegistry::finish_step_spawn`] (#6592's split).
pub struct PreparedStep {
    pub child: Child,
    pub header_anchor: String,
    pub log_path: PathBuf,
    request: StepSpawnRequest,
    sweep_id: SweepId,
    launch: StepLaunch,
    _mid_spawn: roll_gate::MidSpawn,
}

fn non_empty(value: String) -> Option<String> {
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Whether issue `item`'s work already landed, asked of the forge right
/// before a Builder step claims it (#12251): the merged PR when it did.
/// `target` is the registry's `(gh, root, slug)`.
pub(crate) fn landed_at_spawn(target: &(PathBuf, PathBuf, String), item: u32) -> Option<u32> {
    let (gh_bin, root, slug) = target;
    crate::work_finder::landed_prefilter::landed_at_spawn(gh_bin, root, slug, item)
}

/// The refusal of a Builder step on an issue whose work landed by `pr`.
pub(crate) fn landed_refusal(item: u32, pr: u32) -> StepRefusal {
    StepRefusal::new(
        ALREADY_LANDED_REASON,
        format!("issue #{item} is still open but PR #{pr}, which lands it, has merged"),
    )
}

/// Map a role tick's pre-spawn refusal to a step refusal. Pure.
pub(crate) fn refusal_of(outcome: &crate::role_runner::RoleTickOutcome) -> StepRefusal {
    use crate::role_runner::RoleTickOutcome as O;
    match outcome {
        O::NoTokenPool => StepRefusal::new("no_token_pool", "no credential pool for the runtime"),
        O::PoolExhausted { .. } => StepRefusal::new("pool_exhausted", format!("{outcome:?}")),
        O::RuntimeRejected(rejection) => StepRefusal::new("runtime_rejected", &rejection.reason),
        O::ModelRuntimeMismatch(mismatch) => {
            StepRefusal::new("model_runtime_mismatch", mismatch.detail())
        }
        other => StepRefusal::new("admission_refused", format!("{other:?}")),
    }
}

impl SweepRegistry {
    /// Spawn one step: resolve, claim, launch, poll and record. Holds `&mut
    /// self` across the account-selection poll; a caller that must not do
    /// that runs [`Self::begin_step_spawn`], the poll and
    /// [`Self::finish_step_spawn`] itself.
    ///
    /// # Errors
    /// A [`StepRefusal`] (downcastable) naming why nothing ran.
    pub fn spawn_step(&mut self, request: &StepSpawnRequest) -> Result<DispatchOutcome> {
        let mut prepared = self.begin_step_spawn(request)?;
        let (token_name, runtime, death) = poll_and_classify_spawned_child(
            &mut prepared.child,
            &prepared.log_path,
            &prepared.header_anchor,
        );
        self.finish_step_spawn(prepared, token_name, runtime, death)
    }

    /// Resolve, claim and launch a step, up to a live child.
    ///
    /// # Errors
    /// A [`StepRefusal`]: every side effect taken so far is undone.
    pub fn begin_step_spawn(&mut self, request: &StepSpawnRequest) -> Result<PreparedStep> {
        self.begin_step_spawn_inner(request).map_err(|refusal| {
            self.record_step_dispatch(request, dispatch_step::RESULT_REFUSED, refusal.reason, None);
            log::info!(
                "sweep_registry: {} step on #{} not spawned — {refusal}",
                request.role,
                request.item
            );
            anyhow::Error::new(refusal)
        })
    }

    fn begin_step_spawn_inner(
        &mut self,
        request: &StepSpawnRequest,
    ) -> std::result::Result<PreparedStep, StepRefusal> {
        let (item, role, kind) = (request.item, request.role, request.kind());
        self.check_role_command(role)?;
        self.roll_gate
            .admit(&kind)
            .map_err(|e| StepRefusal::new("roll_paused", e))?;
        if role == StepRole::Builder {
            if let Some(pr) = self
                .open_pr_listing_target()
                .and_then(|t| landed_at_spawn(&t, item))
            {
                self.hold_landed_builder(item);
                return Err(landed_refusal(item, pr));
            }
        }
        let launch = Self::resolve_step_launch(&self.config, role, &kind)?;
        let sweep_id = generate_sweep_id(&kind);
        let locked = if role.acts_on_pr() {
            self.acquire_pr_lock(item, &sweep_id)
        } else {
            self.acquire_lock(item, &sweep_id)
        };
        locked.map_err(|e| StepRefusal::new("claimed", format!("{e:#}")))?;
        // Advertise at spawn, before any forge write (#11906 review item 4).
        self.publish_step_claim(peer_claims::ClaimKind::Advertise, item, role);
        if let Err(refusal) = self.pre_claim_step(item, role, &sweep_id) {
            self.unwind_step_claim(item, role, &sweep_id, false);
            return Err(refusal);
        }
        self.spawn_prepared_step(request, &sweep_id, launch, false)
    }

    /// Refuse a step whose `/loom:<role>` command is not installed in the
    /// workspace (#12048, the step-dispatch counterpart of #4027/#6440): the
    /// child would die on `Unknown command` and cool the item down as a failed
    /// spawn. Runs before any lock or forge write; WARNs once per role until the
    /// file appears. Hermetic fixtures (`skip_label_flip`) are exempt.
    fn check_role_command(&mut self, role: StepRole) -> std::result::Result<(), StepRefusal> {
        if self.config.skip_label_flip {
            return Ok(());
        }
        let name = role.role_name();
        if !self.config.role_command_missing(role) {
            self.missing_command_warned.remove(name);
            return Ok(());
        }
        let path = self.config.role_command_path(role);
        let detail = format!(
            "workspace {} is missing {} (run `loom-daemon init`)",
            self.config.workspace_root.display(),
            path.display()
        );
        if self.missing_command_warned.insert(name) {
            log::warn!("sweep_registry: step refused — {detail}");
        }
        Err(StepRefusal::new("workspace_commands_missing", detail))
    }

    /// The claimed step's launch: the stagger and the child. A failed spawn
    /// undoes the claim. `host_gate` (a concurrent tick's spawn, #12298)
    /// also launches through the host-wide gap
    /// ([`launch_gate::HOST_LAUNCH_GATE`]), so children in different repos
    /// launch at least the stagger apart too.
    fn spawn_prepared_step(
        &mut self,
        request: &StepSpawnRequest,
        sweep_id: &str,
        launch: StepLaunch,
        host_gate: bool,
    ) -> std::result::Result<PreparedStep, StepRefusal> {
        let (item, role, kind) = (request.item, request.role, request.kind());
        let sweep_id = sweep_id.to_string();
        self.apply_dispatch_stagger();
        let log_path = self.compute_step_log_path(item, role);
        let spawn = |reg: &Self| {
            reg.spawn_child_process(
                &kind,
                &log_path,
                &sweep_id,
                launch.model.as_deref(),
                launch.effort.as_deref(),
                None,
                launch.admission.admitted.as_ref(),
                launch.selection.as_ref(),
            )
        };
        let spawned = if host_gate {
            let gap = self.dispatch_stagger;
            let this = &*self;
            launch_gate::HOST_LAUNCH_GATE.launch(gap, || spawn(this))
        } else {
            spawn(self)
        };
        match spawned {
            Ok((child, header_anchor)) => Ok(PreparedStep {
                child,
                header_anchor,
                log_path,
                request: request.clone(),
                sweep_id,
                launch,
                _mid_spawn: self.roll_gate.enter(),
            }),
            Err(e) => {
                self.unwind_step_claim(item, role, &sweep_id, true);
                Err(StepRefusal::new("spawn_failed", format!("{e:#}")))
            }
        }
    }

    /// Take the step's claim on the forge (B1, module docs).
    fn pre_claim_step(
        &mut self,
        item: u32,
        role: StepRole,
        sweep_id: &str,
    ) -> std::result::Result<(), StepRefusal> {
        if self.config.skip_label_flip {
            return Ok(());
        }
        match role {
            StepRole::Builder => self.pre_claim_builder_step(item, sweep_id),
            StepRole::Curator
            | StepRole::ChampionPromote
            | StepRole::Judge
            | StepRole::Doctor
            | StepRole::ChampionMerge => self.pre_claim_step_lease(item, role, sweep_id),
        }
    }

    /// A Builder step's claim, claim-then-verify and FAIL CLOSED (#12133). The
    /// label flip is idempotent, so every host "wins" it; the lease comment's
    /// forge order is the only cross-host mutex. So after the flip and the
    /// lease write, the earliest live lease comment must be this step's own:
    /// a peer's earlier one is `claim_lost`; a lease that could not be written,
    /// or an order that could not be read, is `claim_unverified`. (An issue
    /// sweep falls open on an unreadable forge; a step must not.) Either
    /// refusal leaves the label as it is: it may be the winner's claim.
    fn pre_claim_builder_step(
        &mut self,
        item: u32,
        sweep_id: &str,
    ) -> std::result::Result<(), StepRefusal> {
        let episode_start = Utc::now();
        self.flip_label_to_building(item)
            .map_err(|e| StepRefusal::new("label_flip_failed", format!("{e:#}")))?;
        let wrote = self.write_lease_comment_for(item, sweep_id, Some(StepRole::Builder));
        self.note_label_flip(item);
        if !wrote {
            return Err(StepRefusal::new(
                "claim_unverified",
                "the Builder lease comment could not be written",
            ));
        }
        match self.resolve_lease_order_strict(item, sweep_id, episode_start) {
            LeaseOrderDecision::Proceed => Ok(()),
            LeaseOrderDecision::Unverified => {
                Err(StepRefusal::new("claim_unverified", "the lease order could not be read back"))
            }
            LeaseOrderDecision::Yield {
                earliest_host,
                earliest_sweep_id,
            } => Err(StepRefusal::new(
                "claim_lost",
                format!("a peer's earlier lease holds it ({earliest_host} {earliest_sweep_id})"),
            )),
            // Not produced in the strict mode; a refusal is the safe reading.
            LeaseOrderDecision::YieldToLeaselessClaim { .. } => {
                Err(StepRefusal::new("claim_lost", "a leaseless claim holds it"))
            }
        }
    }

    /// Undo a step's claim: the peer advertisement, the forge claim when one
    /// was taken (`claimed`), and the lock.
    fn unwind_step_claim(&mut self, item: u32, role: StepRole, sweep_id: &str, claimed: bool) {
        self.publish_step_claim(peer_claims::ClaimKind::Retract, item, role);
        if claimed && !self.config.skip_label_flip {
            match role {
                StepRole::Builder => {
                    let _ = self.restore_label_to_ready(item);
                    self.note_label_flip(item);
                }
                _ => self.reap_step_leases(sweep_id, "spawn_failed"),
            }
        }
        let _ = if role.acts_on_pr() {
            self.release_pr_lock_owned(item, sweep_id)
        } else {
            self.release_lock_owned(item, sweep_id)
        };
    }

    /// Record a step whose child was spawned and polled.
    ///
    /// # Errors
    /// A [`StepRefusal`] when the child died in its token-selection preflight;
    /// its claim is undone.
    pub fn finish_step_spawn(
        &mut self,
        prepared: PreparedStep,
        token_name: String,
        runtime: String,
        immediate_preflight_death: Option<&'static str>,
    ) -> Result<DispatchOutcome> {
        let PreparedStep {
            child,
            log_path,
            request,
            sweep_id,
            mut launch,
            ..
        } = prepared;
        let (item, role) = (request.item, request.role);
        if immediate_preflight_death == Some("preflight-token-selection-failed") {
            self.unwind_step_claim(item, role, &sweep_id, true);
            self.record_role_tick_pool_exhausted(role.role_name());
            let refusal = StepRefusal::new(
                "token_selection_failed",
                format!("no account could be selected; see {}", log_path.display()),
            );
            self.record_step_dispatch(
                &request,
                dispatch_step::RESULT_REFUSED,
                refusal.reason,
                None,
            );
            return Err(refusal.into());
        }
        let pid = child.id();
        crate::runtime_preference::handoff::attach(launch.admission.backstop.take(), pid);
        let pgid = super::dispatch::spawned_leader_pgid(pid);
        let recorded = if role.acts_on_pr() {
            self.record_child_pid_in_pr_lock(item, pid, pgid)
        } else {
            self.record_child_pid_in_lock(
                item,
                pid,
                pgid,
                launch.model.as_deref(),
                launch.effort.as_deref(),
            )
        };
        if let Err(e) = recorded {
            log::warn!(
                "sweep_registry: {role} step {sweep_id}: child pid not recorded in its lock: {e}"
            );
        }
        self.children.insert(sweep_id.clone(), child);
        let admitted = launch.admission.admitted.as_ref();
        self.entries.insert(
            sweep_id.clone(),
            SweepInfo {
                sweep_id: sweep_id.clone(),
                kind: request.kind(),
                pid,
                pgid,
                token_name: token_name.clone(),
                runtime,
                runtime_source: admitted.map(|a| a.source.clone()),
                log_path: log_path.clone(),
                idempotency_key: None,
                started_at: Utc::now(),
                state: SweepState::Running,
                latest_phase: None,
                pr_number: None,
                model: launch.model.clone(),
                effort: launch.effort.clone(),
                depends_on: None,
                repo: Some(self.config.workspace_root.display().to_string()),
                overflow: false,
            },
        );
        self.start_step_lease_renewal(item, role, &sweep_id, pid, &log_path);
        self.publish_step_slots();
        self.emit_dispatch(&sweep_id, &launch.admission, None, None);
        // A named account proves the role's pool works again (#11909).
        if token_name != crate::sweep_registry::UNKNOWN_TOKEN_NAME {
            self.clear_role_tick_pool_exhausted(role.role_name());
        }
        self.record_step_dispatch(&request, dispatch_step::RESULT_SPAWNED, "ok", Some(&sweep_id));
        log::info!(
            "sweep_registry: {role} step {sweep_id} on #{item} spawned (pid {pid}, model {}, \
             timeout {}s, #11906)",
            launch.model.as_deref().unwrap_or("default"),
            step_timeout(&self.config.workspace_root, role).as_secs()
        );
        Ok(DispatchOutcome {
            sweep_id,
            pid,
            token_name,
            log_path,
            was_new: true,
        })
    }

    /// The `loom.dispatch.step` span for one attempt.
    fn record_step_dispatch(
        &self,
        request: &StepSpawnRequest,
        result: &str,
        reason: &str,
        sweep_id: Option<&str>,
    ) {
        dispatch_step::record(
            &self.config.workspace_root,
            &StepDispatch {
                item: request.item,
                role: request.role,
                result,
                reason,
                draw_attempt: request.draw_attempt,
                age_source: &request.age_source,
                resumed: request.resumed,
                sweep_id,
            },
        );
    }

    /// Publish this registry's live steps by role to `busy_slots` (#11906).
    pub(crate) fn publish_step_slots(&self) {
        let mut running: std::collections::BTreeMap<&'static str, usize> = Default::default();
        for info in self.entries.values() {
            if let (SweepKind::Step { role, .. }, SweepState::Running | SweepState::Pending) =
                (&info.kind, &info.state)
            {
                *running.entry(role.role_name()).or_insert(0) += 1;
            }
        }
        crate::observability::ops::busy_slots::set_steps(&self.config.workspace_root, running);
    }
}

impl SweepRegistry {
    /// The role's resolvers' answer for a step of `role` (module docs). Reads
    /// only the configuration, so it runs with the registry lock released
    /// (the containment preparation, #8787, can call Docker).
    pub(crate) fn resolve_step_launch(
        config: &SweepRegistryConfig,
        role: StepRole,
        kind: &SweepKind,
    ) -> std::result::Result<StepLaunch, StepRefusal> {
        use crate::role_runner::model_resolution as models;
        let root = &config.workspace_root;
        let name = role.role_name();
        let effort = non_empty(models::resolve_role_runner_effort(root, name).0);
        if config.skip_label_flip {
            // Hermetic fixtures install no runtime manifests.
            let model = match role {
                StepRole::Builder => DispatchModel::Autonomous.resolve(config, kind, None),
                _ => non_empty(models::resolve_role_runner_model(root, name).0),
            };
            return Ok(StepLaunch {
                admission: crate::runtime_preference::DispatchAdmission::none(),
                model,
                effort,
                selection: None,
            });
        }
        // #8787: a binding whose only unmet requirement is repository
        // isolation (a Codex Doctor, say) may be admitted on verified
        // private-clone containment, exactly as the role runner admits a role
        // run. The selection the preparer holds is the one the launch uses.
        let hint = match role {
            StepRole::Builder => None,
            _ => Some(models::resolve_role_runner_model(root, name)),
        };
        let (job, issue) = match role {
            StepRole::Builder => (JobKind::Sweep, Some(u64::from(kind.item_number().unwrap_or(0)))),
            _ => (JobKind::Role, None),
        };
        let mut containment = Preparer::new(
            root,
            job,
            issue,
            format!("step-{role}-{}", uuid::Uuid::new_v4()),
            Box::new(move |runtime: &str| match &hint {
                None => DispatchModel::Autonomous.resolve_for_runtime(config, kind, Some(runtime)),
                Some((model, source)) => {
                    let (model, _) = models::reconcile_unpinned_model_with_runtime(
                        runtime,
                        model.clone(),
                        source.clone(),
                    );
                    non_empty(model)
                }
            }),
        );
        let statically = match crate::runtime_admission::resolve_and_admit(root, name, None) {
            Err(rejection) if rejection.containment_eligible() => {
                use crate::runtime_preference::ContainmentPreparer;
                containment.contain(name, None, rejection)
            }
            other => other,
        };
        let logs = root.join(".loom").join("logs");
        let chosen = crate::role_runner::runtime_preflight::check(
            root,
            &logs,
            name,
            Some(&statically),
            &mut containment,
        )
        .map_err(|outcome| {
            if matches!(outcome, crate::role_runner::RoleTickOutcome::PoolExhausted { .. }) {
                log::info!("sweep_registry: the {name} pool is exhausted; steps refused (#11906)");
            }
            refusal_of(&outcome)
        })?;
        let selection = containment.take().map(|(selection, _)| selection);
        let admitted = match chosen.admitted.map_or(statically, Ok) {
            Ok(admitted) => admitted,
            Err(rejection) => return Err(StepRefusal::new("runtime_rejected", rejection.reason)),
        };
        let model = match role {
            StepRole::Builder => DispatchModel::Autonomous.resolve(config, kind, Some(&admitted)),
            _ => {
                let drawn = admitted.preference.as_ref().and_then(|p| p.model.clone());
                let (model, source) = match drawn {
                    Some(drawn) => (drawn, "role-draw".to_string()),
                    None => models::resolve_role_runner_model(root, name),
                };
                let (model, _) =
                    models::reconcile_unpinned_model_with_runtime(&admitted.runtime, model, source);
                if let Some(why) = model_runtime_mismatch(&admitted.runtime, &model) {
                    return Err(StepRefusal::new("model_runtime_mismatch", why));
                }
                non_empty(model)
            }
        };
        Ok(StepLaunch {
            admission: crate::runtime_preference::DispatchAdmission {
                admitted: Some(admitted),
                backstop: chosen.backstop,
            },
            model,
            effort,
            selection,
        })
    }
}

/// The step-only environment of a step child (called from
/// `spawn_child_process_for`): the lease file its won PR leases are listed
/// in, so a timed-out step's leases are reaped (#11931); the daemon's lease
/// host id (`LOOM_LEASE_HOST_ID`), so its pre-taken lease is the session's own
/// wherever the session runs; its pick journal
/// (#11877's capture); and, for a Curator, the curation provenance the role
/// runner gives its Curator (`role_runner/launch/curation.rs`).
pub(crate) fn apply_step_env(
    cmd: &mut Command,
    root: &Path,
    sweep_id: &str,
    role: StepRole,
    model: Option<&str>,
    runtime: Option<&str>,
) {
    cmd.env(
        crate::pr_lease_reap::LEASE_FILE_ENV,
        crate::pr_lease_reap::path_for(root, sweep_id),
    );
    // The lease's owner is role, host and sweep. A session in a container
    // (Codex) has its own hostname, so without the daemon's host id pinned
    // it would read the daemon's pre-taken lease as a peer's (#11906). The pin
    // is lease-only: `LOOM_HOST_ID` is also the telemetry `host.id` tier, and
    // setting it here would split the host's identity (#11530).
    cmd.env(
        crate::host_identity::LEASE_HOST_ID_ENV,
        crate::host_identity::lease_host_identity(),
    );
    crate::observability::pick_journal::attach_step(cmd, sweep_id);
    if role == StepRole::Curator {
        cmd.env("LOOM_CURATION_MODEL", model.filter(|m| !m.is_empty()).unwrap_or("unknown"))
            .env("LOOM_CURATION_RUNTIME", runtime.unwrap_or("unknown"));
    } else {
        cmd.env_remove("LOOM_CURATION_MODEL")
            .env_remove("LOOM_CURATION_RUNTIME");
    }
}

#[cfg(test)]
#[path = "step_spawn/tests.rs"]
mod tests;
