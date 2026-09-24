//! Runtime admission and traced invocation for scheduled roles.
use super::*;

impl RoleInvocationRunner for ScriptRoleInvocationRunner {
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
        let trace_root = self.workspace_root.clone();
        let (outcome, context) =
            crate::observability::lifecycle::role_invocation(&trace_root, role, || {
                let script = match self.resolve_spawn_bin() {
                    Ok(p) => p,
                    Err(e) => {
                        note_pre_spawn_skip(
                            &self.logs_dir(),
                            role,
                            &format!("spawn-bin unresolved: {e}"),
                        );
                        return RoleTickOutcome::Failure(e);
                    }
                };
                // Resolve the runtime BEFORE the credential-pool preflight (#8408): the
                // gate reads the pool the admitted runtime draws from, never Claude's
                // by default. Claude keeps its old ordering (pool gate ahead of an
                // admission rejection), including on incomplete installations.
                // #8787: a Codex binding whose only unmet requirement is
                // repository isolation (scheduled Doctor) may be admitted on
                // verified private-clone containment. Preparation is Docker
                // work, done here on the blocking launch path; the selection
                // it holds is the one the launch uses, never a second pick.
                let model_hint = match &self.model {
                    Some(m) => (m.clone(), "override".to_string()),
                    None => resolve_role_runner_model(&self.workspace_root, role),
                };
                let mut containment =
                    crate::tokens_pool::private_workspace::containment::Preparer::new(
                        &self.workspace_root,
                        crate::tokens_pool::private_workspace::JobKind::Role,
                        None,
                        format!("role-{role}-{}", uuid::Uuid::new_v4()),
                        Box::new(move |runtime: &str| {
                            let (model, _) = reconcile_unpinned_model_with_runtime(
                                runtime,
                                model_hint.0.clone(),
                                model_hint.1.clone(),
                            );
                            (!model.is_empty()).then_some(model)
                        }),
                    );
                let mut admission_result = self.spawn_bin.is_none().then(|| {
                    match crate::runtime_admission::resolve_and_admit(
                        &self.workspace_root,
                        role,
                        None,
                    ) {
                        Err(rejection) if rejection.containment_eligible() => {
                            use crate::runtime_preference::ContainmentPreparer;
                            containment.contain(role, None, rejection)
                        }
                        other => other,
                    }
                });
                // #8555: the metered backstop slot this tick's resolution took,
                // if any. It is carried by value from here to the spawn site —
                // never parked in shared state — so the several pre-spawn
                // bail-outs below release it simply by dropping it, and no
                // concurrent role tick or sweep dispatch can collide with it.
                let backstop;
                match runtime_preflight::check(
                    &self.workspace_root,
                    &self.logs_dir(),
                    role,
                    admission_result.as_ref(),
                    &mut containment,
                ) {
                    // #8554: a configured `runtimes.preference` /
                    // `rolePreference.<role>` list decided this tick's tap —
                    // launch with THAT runtime, not the one static admission
                    // named. Re-pointed here (before model resolution) so
                    // #7894's reconciliation and #5028's mismatch refusal
                    // both judge the model against the runtime that will
                    // actually run. An absent `admitted` means no list applied:
                    // proceed with the admission already resolved, unchanged.
                    Ok(chosen) => {
                        if let Some(runtime) = chosen.admitted {
                            admission_result = Some(Ok(runtime));
                        }
                        backstop = chosen.backstop;
                    }
                    Err(outcome) => return outcome,
                }
                // Issue #5028 (follow-up to #5001 AC2/AC3): runtime admission now
                // resolves BEFORE the model, because the runtime is a per-role INPUT
                // to the model/runtime mismatch check just below — a Claude-shaped
                // model can only be judged wrong once the admitted runtime is known.
                let admission = if let Some(result) = admission_result {
                    match result {
                        Ok(value) => Some(value),
                        Err(e) => {
                            note_pre_spawn_skip(
                                &self.logs_dir(),
                                role,
                                &format!("runtime admission rejected: {e}"),
                            );
                            return RoleTickOutcome::RuntimeRejected(e);
                        }
                    }
                } else {
                    None
                };
                // Issue #4501: pin the child's model instead of inheriting the account's
                // interactive CLI default (`fable` on the host that filed the issue,
                // where every role child burned the most constrained quota tier and then
                // died on "You've reached your Fable 5 limit").
                let (model, model_source) = match &self.model {
                    Some(m) => (m.clone(), "override".to_string()),
                    None => resolve_role_runner_model(&self.workspace_root, role),
                };
                // Issue #7894: an UNPINNED model that conflicts with the admitted
                // runtime degrades to the runtime CLI's own default rather than being
                // refused below — see `reconcile_unpinned_model_with_runtime`. Only the
                // shipped-default tier is touched, so an explicit pin still reaches
                // #5028's refusal unchanged.
                let (model, model_source) = match &admission {
                    Some(admitted) => reconcile_unpinned_model_with_runtime(
                        &admitted.runtime,
                        model,
                        model_source,
                    ),
                    None => (model, model_source),
                };
                // Issue #5028: refuse a launch whose resolved model is a provable
                // conflict with the just-admitted runtime — e.g.
                // `runtimes.roles.judge = "codex"` with
                // `autonomous.roleRunner.roleModels.judge = "sonnet"`, a Claude-shaped
                // pin the Codex adapter rejects with an HTTP 400. Detected here, before
                // any spawn, so the role runner skips the doomed launch instead of
                // burning a tick (and a token draw) on a guaranteed failure every time
                // (#5001 AC2/AC3). Since #7894 this only ever fires on a model that
                // came from a tier an operator actually configured: an unpinned
                // conflict was already degraded to the CLI-default pass-through just
                // above, so the refusal is now exclusively about a wrong *stated
                // intent*, never about a default nobody chose.
                // Gated on `admission` being `Some` — tests that opt out of admission
                // via `spawn_bin` have no resolved runtime to check against, and are
                // unaffected (mirrors the token-pool preflight's `spawn_bin.is_none()`
                // gate above).
                if let Some(admitted) = &admission {
                    if let Some(reason) =
                        crate::sweep_registry::model_runtime_mismatch(&admitted.runtime, &model)
                    {
                        MODEL_RUNTIME_MISMATCH_SKIP_COUNT.fetch_add(1, Ordering::Relaxed);
                        let mismatch = ModelRuntimeMismatch {
                            role: role.to_string(),
                            runtime: admitted.runtime.clone(),
                            model,
                            model_source,
                            reason,
                        };
                        note_pre_spawn_skip(&self.logs_dir(), role, &mismatch.detail());
                        return RoleTickOutcome::ModelRuntimeMismatch(mismatch);
                    }
                }
                // Issue #8054: the reasoning-effort axis, resolved independently of the
                // model (no shipped default — unconfigured resolves to the empty string,
                // which the emission site renders as no `--effort` argument at all).
                // Resolved AFTER the mismatch preflight above so a refused launch does
                // not pay for a second config read.
                let (effort, effort_source) =
                    resolve_role_runner_effort(&self.workspace_root, role);
                // Issue #8056: the launched values, captured for the durable
                // `role_tick.outcome` record. Set here — after every pre-spawn bail-out
                // above has already returned — so "resolved" never claims a model for a
                // tick that skipped before resolving one.
                self.resolved_model_effort = Some((model.clone(), effort.clone()));
                run_role_with_timeout(
                    &script,
                    &self.workspace_root,
                    role,
                    prompt,
                    self.logs_dir(),
                    self.timeout,
                    &model,
                    &model_source,
                    &effort,
                    &effort_source,
                    admission.as_ref(),
                    self.load_per_core_override,
                    backstop,
                    containment.take().map(|(selection, _)| selection),
                )
            });
        self.trace_context = context;
        outcome
    }

    fn resolved_model_effort(&self) -> Option<(String, String)> {
        self.resolved_model_effort.clone()
    }
}
