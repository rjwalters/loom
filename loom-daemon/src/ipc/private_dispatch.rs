//! Dispatch preparation and polling outside the registry mutex.
use super::*;

pub(super) async fn dispatch_sweep_nonblocking(
    sweep_registry: &Arc<Mutex<SweepRegistry>>,
    workspace_pool: &Arc<WorkspacePool>,
    event_bus: &Arc<EventBus>,
    kind: crate::types::SweepKind,
    idempotency_key: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    depends_on: Option<u32>,
    workspace_root: Option<String>,
    force: bool,
    runtime: Option<String>,
    model_profile: Option<String>,
    origin: crate::sweep_registry::DispatchOrigin,
) -> Response {
    // GitHub rate-limit circuit breaker (#4429/#4440/#4666) — unchanged.
    if let Some(refusal) = rate_limit_dispatch_refusal(
        &kind,
        crate::rate_limit_breaker::global_snapshot().as_ref(),
        force,
    ) {
        return refusal;
    }
    // Dispatch-only resolution (Issue #4299) — unchanged.
    let target = match resolve_dispatch_registry(
        sweep_registry,
        workspace_pool,
        workspace_root.as_deref(),
    ) {
        Ok(target) => target,
        Err(response) => return response,
    };

    // Per-dispatch runtime/modelProfile (#11284): validate with NO side
    // effects; a refused tap never reaches the guards. The record is only
    // saved/cleared once the dispatch has been accepted (see commit below), so
    // a guard-rejected duplicate cannot disturb an in-flight sweep's record.
    let (tap_root, tap_plan) = {
        let repo_root = target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config()
            .workspace_root
            .clone();
        let (runtime, model_profile) = (runtime.clone(), model_profile.clone());
        let tap_kind = kind.clone();
        let validate_root = repo_root.clone();
        match tokio::task::spawn_blocking(move || {
            crate::dispatch_tap::validate_request(
                &validate_root,
                &tap_kind,
                runtime.as_deref(),
                model_profile.as_deref(),
            )
        })
        .await
        {
            Ok(Ok(plan)) => (repo_root, plan),
            Ok(Err(response)) => return *response,
            Err(error) => return dispatch_error_response(&kind, error.into()),
        }
    };

    let prepare_target = target.clone();
    let prepare_kind = kind.clone();
    let prepare_model = model.clone();
    let prepare_key = idempotency_key.clone();
    let prepared_launch = match tokio::task::spawn_blocking(move || {
        crate::sweep_registry::private_dispatch::prepare(
            &prepare_target,
            &prepare_kind,
            prepare_key.as_deref(),
            crate::sweep_registry::DispatchModel::Request(prepare_model.as_deref()),
        )
    })
    .await
    {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => return dispatch_error_response(&kind, error),
        Err(error) => return dispatch_error_response(&kind, error.into()),
    };
    // Phase 1: headroom advisory + model resolution (both unchanged from the
    // previous arm) under a short registry lock, then `begin_issue_dispatch`
    // — the guard chain, claim lock, label flip, dispatch stagger and
    // `Command::spawn()` — which takes the registry lock only for its
    // in-memory/local steps and releases it around every forge `gh` call and
    // sleep (#9573; lock-scope audit in `sweep_registry::dispatch::registry_access`).
    {
        let mut sr = target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let repo_root = sr.config().workspace_root.clone();

        let headroom = assess_dispatch_headroom(&mut sr, &repo_root);
        let low_headroom = dispatch_would_meet_or_exceed_headroom(&headroom);
        emit_dispatch_headroom_advisory_on_change(
            event_bus,
            &repo_root,
            low_headroom,
            &headroom,
            &kind,
        );

        log::info!(
            "dispatch_sweep: {:?}; \
             headroom occupancy={} dynamic_cap={} (tokens={} [informational \
             only, not capacity-limiting since #5270])",
            kind,
            headroom.occupancy,
            headroom.dynamic_cap,
            headroom.token_axis_limit
        );
    }
    let begin_outcome = {
        let begin_target = target.clone();
        let begin_kind = kind.clone();
        let begin_model = model.clone();
        let begin_effort = effort.clone();
        match tokio::task::spawn_blocking(move || {
            crate::sweep_registry::SweepRegistry::begin_prepared_issue_dispatch_shared(
                &begin_target,
                &begin_kind,
                idempotency_key,
                crate::sweep_registry::DispatchModel::Request(begin_model.as_deref()),
                begin_effort.as_deref(),
                depends_on,
                prepared_launch,
                origin,
            )
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(error) => return dispatch_error_response(&kind, error.into()),
        }
    };

    let prepared = match begin_outcome {
        Err(e) => return dispatch_error_response(&kind, e),
        Ok(BeginIssueDispatch::Done(result)) => {
            // An idempotency hit (`was_new == false`) is the in-flight sweep
            // itself: leave its record alone. A refusal or spawn failure is an
            // `Err`: nothing is recorded.
            if matches!(&result, Ok(outcome) if outcome.was_new) {
                if let Err(response) = tap_plan.commit(&tap_root) {
                    return *response;
                }
            }
            return dispatch_result_to_response(&kind, result);
        }
        Ok(BeginIssueDispatch::Spawned(prepared)) => prepared,
    };
    // Guards passed and the child is spawned: record (or clear) the tap now,
    // before the worker's phase runner can read it.
    let tap_rollback = match tap_plan.commit(&tap_root) {
        Ok(rollback) => rollback,
        Err(response) => return *response,
    };

    // Phase 2 (UNLOCKED): poll the child for its account-selection log line.
    // Run via `spawn_blocking` — `poll_and_classify_spawned_child` calls
    // `std::thread::sleep` internally (bounded by `TOKEN_NAME_CAPTURE_TIMEOUT`,
    // up to 5s) and must never run inline on a tokio async worker thread.
    let poll_result = tokio::task::spawn_blocking(move || {
        let mut prepared = prepared;
        let (token_name, runtime, immediate_preflight_death) = poll_and_classify_spawned_child(
            &mut prepared.child,
            &prepared.log_path,
            &prepared.header_anchor,
        );
        (prepared, token_name, runtime, immediate_preflight_death)
    })
    .await;
    let (prepared, token_name, runtime, immediate_preflight_death) = match poll_result {
        Ok(result) => result,
        Err(join_err) => {
            // Extremely unlikely (a panic inside the poll) — never silently
            // drop the ack. The spawned child is orphaned (no `self.children`
            // entry was ever recorded — `finish_issue_dispatch` never ran),
            // so the reaper's later journal/`/proc` scan is the recovery
            // path (Issue #3953), matching how a `spawn_child` panic would
            // have been handled pre-split.
            log::error!("dispatch_sweep: poll task for {kind:?} panicked: {join_err}");
            return Response::Error {
                message: format!(
                    "dispatch_sweep failed: account-selection poll panicked: {join_err}"
                ),
            };
        }
    };

    // Phase 3 (lock-scoped): record the outcome.
    let result = {
        let mut sr = target
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sr.finish_issue_dispatch(*prepared, token_name, runtime, immediate_preflight_death)
    };
    if result.is_err() {
        if let Some(rollback) = tap_rollback {
            rollback.restore();
        }
    }
    dispatch_result_to_response(&kind, result)
}
