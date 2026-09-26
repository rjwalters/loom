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
) -> Response {
    // Host-distress circuit breaker (#4235) — unchanged from the previous
    // synchronous arm; a pure, lock-free global snapshot read.
    if !force {
        if let Some(snap) = crate::host_breaker::global_snapshot() {
            if snap.suppressed {
                let releases = snap.releases_at.map_or_else(
                    || " (host still hot — cool-down not yet started)".to_string(),
                    |r| format!(" (cool-down releases at {r})"),
                );
                log::warn!(
                    "dispatch_sweep: refused {kind:?} — host circuit breaker is {} \
                     ({}){releases}; running work drains, new dispatch paused. \
                     Re-run with force to override.",
                    snap.phase.as_str(),
                    snap.reason.as_deref().unwrap_or("sustained host distress"),
                );
                return Response::Error {
                    message: format!(
                        "dispatch_sweep refused: host circuit breaker is {} ({}).{releases} \
                         Running work is draining and new dispatch is paused (#4235). \
                         Re-run with force to override.",
                        snap.phase.as_str(),
                        snap.reason.as_deref().unwrap_or("sustained host distress"),
                    ),
                };
            }
        }
    }
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
    // Phase 1 (lock-scoped): headroom advisory + model resolution (both
    // unchanged from the previous arm) + `begin_issue_dispatch` — the FULL
    // guard chain, claim lock, label flip, dispatch stagger, and
    // `Command::spawn()`. Everything here is either cheap in-memory/local-fs
    // work or (for `Issue` guards) the SAME `gh` round trips the previous
    // single-call `dispatch()` already made under this same lock — this
    // split changes WHEN the lock is released, not what runs under it up to
    // this point.
    let begin_outcome = {
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
             headroom occupancy={} dynamic_cap={} (disk={} ram={} tokens={} [informational \
             only, not capacity-limiting since #5270])",
            kind,
            headroom.occupancy,
            headroom.dynamic_cap,
            headroom.disk_headroom,
            headroom.ram_headroom,
            headroom.token_axis_limit
        );

        sr.begin_prepared_issue_dispatch(
            &kind,
            idempotency_key,
            crate::sweep_registry::DispatchModel::Request(model.as_deref()),
            effort.as_deref(),
            depends_on,
            None,
            prepared_launch,
        )
    };

    let prepared = match begin_outcome {
        Err(e) => return dispatch_error_response(&kind, e),
        Ok(BeginIssueDispatch::Done(result)) => return dispatch_result_to_response(&kind, result),
        Ok(BeginIssueDispatch::Spawned(prepared)) => prepared,
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
    dispatch_result_to_response(&kind, result)
}
