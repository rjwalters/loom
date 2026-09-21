//! Live event logs retain the authoritative sweep context across retirement.
//! Legacy events identify a sweep, not an exact role/tool, so they link to its
//! root span. Events missed at daemon restart remain uncorrelated; the durable
//! outcome journal supplies the authoritative terminal record separately.
use super::*;
use crate::telemetry::trace::{store::TraceStore, TraceContext};

fn load_context(root: &Path, execution: &str) -> Option<TraceContext> {
    let path = TraceStore::new(root).path(root, execution);
    let saved = TraceStore::load(&path).ok()?;
    if path.file_stem().and_then(|name| name.to_str()) != Some(saved.identity.as_str()) {
        return None;
    }
    Some(saved.context)
}

pub(super) fn map_envelopes(
    event: &Event,
    issue: u32,
    repo: &str,
    visibility: RepoVisibility,
    root: &Path,
    host: &str,
    dispatches: &mut HashMap<DispatchKey, DispatchState>,
) -> Vec<TelemetryEnvelope> {
    let key = (repo.to_owned(), issue);
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
