//! Live event logs retain the authoritative sweep context across retirement.
//! Legacy events identify a sweep, not an exact role/tool, so they link to its
//! root span. An event whose dispatch this process never saw is correlated from
//! registry adoption evidence when there is any, and only otherwise falls back
//! to a synthesized id; the durable outcome journal supplies the authoritative
//! terminal record separately.
use super::*;
use crate::sweep_registry::TrackedSweepIdentity;
use crate::telemetry::trace::{store::TraceStore, TraceContext};

fn load_context(root: &Path, execution: &str) -> Option<TraceContext> {
    let path = TraceStore::new(root).path(root, execution);
    let saved = TraceStore::load(&path).ok()?;
    if path.file_stem().and_then(|name| name.to_str()) != Some(saved.identity.as_str()) {
        return None;
    }
    Some(saved.context)
}

/// Whether `event` is one of the lifecycle events that carries an `issue` but
/// no `sweep_id` of its own, i.e. one whose record's identity comes entirely
/// from correlation state. A `SweepGlobalDispatch` is excluded on purpose: it
/// names its own sweep and is itself the authoritative correlation source.
fn needs_correlated_identity(event: &Event) -> bool {
    matches!(
        event,
        Event::SweepPhase { .. } | Event::SweepExited { .. } | Event::SweepCrashed { .. }
    )
}

/// Re-seed this process's correlation map for a sweep that was **adopted**
/// across a daemon restart, from the owning registry's own evidence
/// (Issue #8720).
///
/// Lock-based recovery (`sweep_registry::locks::reconstruct`) keeps the
/// pre-restart `owner.sweep_id` and calls
/// [`crate::observability::lifecycle::execution_adopted`], but that only
/// re-anchors the on-disk trace journal — no `sweep.global.dispatch` is
/// re-published, so nothing ever reaches this map and the sweep's next phase
/// or terminal event used to be reported under a synthesized
/// `unknown-issue-N`, diverging from the id the very same daemon reports for
/// that sweep in `host.health` and `sweep.identity`.
///
/// # What this deliberately does NOT do
///
/// - **No replayed start.** Nothing here emits a `sweep.started` record. The
///   sweep started before this process existed; re-announcing it now would
///   stamp a fabricated `started_at` on a real sweep and, downstream,
///   resurrect a row the dashboard had already retired.
/// - **No invented timestamps.** `started_at` is the registry's own admitted
///   value (`owner.acquired_at` on the lock path), so the elapsed time a
///   `SweepCrashed` record derives from it is measured, not zeroed.
/// - **No guessing.** `evidence` returns `None` unless the owning registry
///   holds exactly one non-terminal entry for this issue, so a hand-injected
///   phase event for an issue this host is not running still falls through to
///   [`unknown_sweep_id`] exactly as before.
fn recover_adopted_dispatch(
    event: &Event,
    key: &DispatchKey,
    root: &Path,
    dispatches: &mut HashMap<DispatchKey, DispatchState>,
    evidence: &dyn Fn() -> Option<TrackedSweepIdentity>,
) {
    if !needs_correlated_identity(event) || dispatches.contains_key(key) {
        return;
    }
    let Some(identity) = evidence() else {
        return;
    };
    // The adopted sweep's trace context is keyed by that same authoritative
    // id, so this is a lookup, not the issue-number guess
    // `restart_without_dispatch_does_not_guess_trace_identity_from_issue_number`
    // forbids — with no evidence there is still no context.
    let trace_context = super::super::tracing::enabled(root)
        .then(|| load_context(root, &identity.sweep_id))
        .flatten();
    log::debug!(
        "observability: correlating issue #{} to adopted sweep {} from registry evidence (#8720)",
        key.1,
        identity.sweep_id
    );
    dispatches.insert(
        key.clone(),
        DispatchState {
            sweep_id: identity.sweep_id,
            started_at: identity.started_at,
            trace_context,
        },
    );
}

// `evidence` is a distinct injected I/O boundary (the owning registry), kept
// out of the other seven positional inputs rather than bundled into an
// otherwise meaningless context struct.
#[allow(clippy::too_many_arguments)]
pub(super) fn map_envelopes(
    event: &Event,
    issue: u32,
    repo: &str,
    visibility: RepoVisibility,
    root: &Path,
    host: &str,
    dispatches: &mut HashMap<DispatchKey, DispatchState>,
    evidence: &dyn Fn() -> Option<TrackedSweepIdentity>,
) -> Vec<TelemetryEnvelope> {
    let key = (repo.to_owned(), issue);
    // Before anything reads the correlation map: an adopted sweep's identity
    // is authoritative and belongs in it, so the mapping below never sees the
    // difference between "dispatched here" and "adopted here".
    recover_adopted_dispatch(event, &key, root, dispatches, evidence);
    let context = if !super::super::tracing::enabled(root) {
        None
    } else if let Event::SweepGlobalDispatch { sweep_id, .. } = event {
        load_context(root, sweep_id)
    } else {
        dispatches.get(&key).and_then(|d| d.trace_context.clone())
    };
    // Capture before map_event_to_records removes terminal dispatch state.
    let records = map_event_to_records(event, issue, repo, visibility, dispatches);
    if let Some(dispatch) = dispatches.get_mut(&key) {
        dispatch.trace_context = context.clone();
    }
    records
        .into_iter()
        .map(|record| {
            let mut envelope = TelemetryEnvelope::new(host, record);
            envelope.trace_context = context.clone();
            envelope
        })
        .collect()
}

#[cfg(test)]
mod tests;
