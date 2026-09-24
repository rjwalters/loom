//! `daemon.event` bus-subscriber collector (Issue #8760, G4 of epic #8714).
//!
//! Mirrors [`super::collector`]'s "subscribe to the existing bus, add no new
//! call sites" design, scoped to the four named event-bus topics that had no
//! telemetry record kind at all before this issue: `daemon.drain.*`,
//! `daemon.capacity.advisory`, `daemon.preflight.advisory`, and
//! `epic.issue.*`. A separate subscriber rather than folding into
//! [`super::collector`] — that module's own doc names its scope as exactly
//! the `sweep.global.dispatch` / `sweep.issue.*` topics plus its periodic
//! host-level samples; these four topics are host/daemon-level operational
//! advisories with no sweep/issue correlation state to track, so they need
//! none of that module's `DispatchState`/repo-slug-resolution machinery.
//!
//! # No repo attribution
//!
//! Unlike [`super::collector`]'s sweep-scoped records, none of these four
//! topics carry a `repo`/`owner-repo` slug on the wire today (`epic.issue.*`
//! is the one that conceptually belongs to a repo, but `Event::EpicAction`
//! carries no `repo` field to resolve one from — a pre-existing gap in that
//! event's own shape, out of this issue's scope to fix). `DaemonEventRecord`
//! therefore carries no [`RepoVisibility`](crate::telemetry::RepoVisibility)
//! tag at all, the same "host-level, references no repository" contract
//! `tokens.snapshot`/`host.health` already establish — see the record's own
//! doc.

use std::sync::Arc;

use crate::event_bus::{EventBus, RecvError};
use crate::telemetry::{DaemonEventRecord, TelemetryEnvelope, TelemetryRecord};
use crate::types::Event;

use super::queue::QueueSink;

/// Subscribe to the four named topic families and spawn the collector loop
/// on the shared daemon runtime.
pub fn spawn_task(
    bus: &EventBus,
    queue: Arc<dyn QueueSink>,
    host_id: String,
) -> tokio::task::JoinHandle<()> {
    let subscription = bus.subscribe([
        "daemon.drain",
        "daemon.capacity.advisory",
        "daemon.preflight.advisory",
        "epic.issue",
    ]);
    tokio::spawn(run(subscription, queue, host_id))
}

async fn run(
    mut subscription: crate::event_bus::Subscription,
    queue: Arc<dyn QueueSink>,
    host_id: String,
) {
    loop {
        match subscription.recv().await {
            Ok(event) => {
                if let Some(record) = map_event_to_daemon_event_record(&event) {
                    queue.offer(TelemetryEnvelope::new(host_id.clone(), record));
                }
            }
            Err(RecvError::Closed) => {
                log::debug!("observability: event bus closed; daemon-event collector stopping");
                break;
            }
            // `TopicLag` and any other transient recv error: nothing to
            // translate, keep listening (matches `collector.rs`'s own
            // handling).
            Err(_) => {}
        }
    }
}

/// Pure event -> `daemon.event` mapping (Issue #8760), no I/O.
///
/// `None` for every event this collector does not translate — including
/// `Event::TopicLag` and any `Generic` event whose topic does not fall under
/// `daemon.drain.` (the subscription's own prefix filter already excludes
/// everything else in practice; this match stays exhaustive-by-construction
/// so a future `Event` variant added to one of this collector's subscribed
/// prefixes is a compile error here, not a silently-dropped record).
///
/// `pub(crate)` so the collector's own emit-site tests can drive genuine
/// [`Event`] fixtures through this mapping end-to-end, the same pattern
/// `collector::map_event_to_records`'s tests use.
pub(crate) fn map_event_to_daemon_event_record(event: &Event) -> Option<TelemetryRecord> {
    let topic = event.topic();
    let payload = match event {
        // The three typed variants: serialize the whole event, then drop the
        // `#[serde(tag = "type")]` discriminant — `topic` already names the
        // event unambiguously, so keeping both would be a redundant field
        // rather than added information.
        Event::CapacityAdvisory { .. }
        | Event::PreflightAdvisory { .. }
        | Event::EpicAction { .. } => {
            let mut value = serde_json::to_value(event).ok()?;
            if let serde_json::Value::Object(map) = &mut value {
                map.remove("type");
                reduce_workspace_root(map);
            }
            value
        }
        // `daemon.drain.*` rides `Event::Generic` (published via
        // `EventBus::publish_generic` from `ipc.rs`) — its payload is
        // already the bare, tag-free shape.
        Event::Generic { payload, .. } if topic.starts_with("daemon.drain.") => payload.clone(),
        _ => return None,
    };
    Some(TelemetryRecord::DaemonEvent(DaemonEventRecord { topic, payload }))
}

/// The one documented exception to this collector's carry-the-payload-verbatim
/// rule (Issue #8760): reduce `Event::PreflightAdvisory`'s `workspace_root` to
/// its final path component in place.
///
/// That field is the only **absolute host filesystem path** anywhere in the
/// four subscribed topic families, and no pre-existing telemetry record kind
/// ships one — an absolute path routinely embeds the operating user's name
/// (`/home/alice/repos/loom`), which is host-identifying in a way nothing else
/// on this wire is. The final component ("loom") keeps the whole operational
/// point of the field — *which* workspace in a multi-workspace fleet is dying
/// preflight — while matching the repo-slug granularity `loom.repo` already
/// establishes everywhere else.
///
/// Narrow by construction: keyed on the exact field name, a no-op for every
/// other topic and for a path with no separator.
fn reduce_workspace_root(map: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(serde_json::Value::String(root)) = map.get("workspace_root") else {
        return;
    };
    let basename = std::path::Path::new(root)
        .file_name()
        .map_or_else(|| root.clone(), |name| name.to_string_lossy().into_owned());
    map.insert("workspace_root".to_string(), serde_json::Value::String(basename));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::types::EpicActionClass;

    #[test]
    fn capacity_advisory_maps_to_a_daemon_event_record() {
        let event = Event::CapacityAdvisory {
            pressured: true,
            queued: 3,
            healthy_accounts: 1,
            exhausted_accounts: 2,
            total_accounts: 3,
            estimated_drain_minutes: Some(15),
            message: "add accounts".to_string(),
        };
        let record = map_event_to_daemon_event_record(&event).expect("mapped");
        let TelemetryRecord::DaemonEvent(inner) = record else {
            panic!("expected DaemonEvent");
        };
        assert_eq!(inner.topic, "daemon.capacity.advisory");
        assert_eq!(inner.payload["pressured"], true);
        assert_eq!(inner.payload["queued"], 3);
        assert_eq!(inner.payload["estimated_drain_minutes"], 15);
        assert!(inner.payload.get("type").is_none(), "the tag must be stripped");
    }

    #[test]
    fn preflight_advisory_maps_to_a_daemon_event_record() {
        let event = Event::PreflightAdvisory {
            workspace_root: "/repos/loom".to_string(),
            consecutive_deaths: 4,
            marker: "preflight-mcp-failed".to_string(),
            message: "check .mcp.json".to_string(),
        };
        let record = map_event_to_daemon_event_record(&event).expect("mapped");
        let TelemetryRecord::DaemonEvent(inner) = record else {
            panic!("expected DaemonEvent");
        };
        assert_eq!(inner.topic, "daemon.preflight.advisory");
        // Reduced to its final component — see `reduce_workspace_root`.
        assert_eq!(inner.payload["workspace_root"], "loom");
        assert_eq!(inner.payload["consecutive_deaths"], 4);
    }

    /// The one field in the four subscribed topic families that is an
    /// absolute host path never leaves the host whole — the home-directory
    /// prefix (which embeds the operating user's name) is dropped, while the
    /// workspace-discriminating final component survives.
    #[test]
    fn a_preflight_advisorys_absolute_workspace_path_never_ships_whole() {
        let event = Event::PreflightAdvisory {
            workspace_root: "/home/alice/repos/loom".to_string(),
            consecutive_deaths: 4,
            marker: "preflight-mcp-failed".to_string(),
            message: "check .mcp.json".to_string(),
        };
        let record = map_event_to_daemon_event_record(&event).expect("mapped");
        let TelemetryRecord::DaemonEvent(inner) = record else {
            panic!("expected DaemonEvent");
        };
        let json = serde_json::to_string(&inner.payload).unwrap();
        assert!(!json.contains("alice"), "the operating user's name leaked: {json}");
        assert!(!json.contains("/home/"), "an absolute host path leaked: {json}");
        assert_eq!(inner.payload["workspace_root"], "loom");
        // Every other field of the advisory is still carried verbatim.
        assert_eq!(inner.payload["marker"], "preflight-mcp-failed");
        assert_eq!(inner.payload["consecutive_deaths"], 4);
    }

    /// The reduction is keyed on the field name alone, so it is a no-op for
    /// the three topic families that carry no `workspace_root` at all.
    #[test]
    fn the_workspace_root_reduction_never_touches_another_topics_payload() {
        let event = Event::EpicAction {
            epic: 7,
            action: EpicActionClass::Join,
            state: "epic:phase-2".to_string(),
        };
        let record = map_event_to_daemon_event_record(&event).expect("mapped");
        let TelemetryRecord::DaemonEvent(inner) = record else {
            panic!("expected DaemonEvent");
        };
        assert!(inner.payload.get("workspace_root").is_none());
        assert_eq!(inner.payload["epic"], 7);
        assert_eq!(inner.payload["state"], "epic:phase-2");
    }

    #[test]
    fn epic_action_maps_to_a_daemon_event_record() {
        let event = Event::EpicAction {
            epic: 123,
            action: EpicActionClass::Decompose,
            state: "epic:needs_decomp".to_string(),
        };
        let record = map_event_to_daemon_event_record(&event).expect("mapped");
        let TelemetryRecord::DaemonEvent(inner) = record else {
            panic!("expected DaemonEvent");
        };
        assert_eq!(inner.topic, "epic.issue.123.decompose");
        assert_eq!(inner.payload["epic"], 123);
        assert_eq!(inner.payload["state"], "epic:needs_decomp");
    }

    #[test]
    fn a_drain_generic_event_maps_with_its_payload_verbatim() {
        let event = Event::Generic {
            topic: "daemon.drain.started".to_string(),
            payload: serde_json::json!({"in_flight": 2, "timeout_secs": 300}),
        };
        let record = map_event_to_daemon_event_record(&event).expect("mapped");
        let TelemetryRecord::DaemonEvent(inner) = record else {
            panic!("expected DaemonEvent");
        };
        assert_eq!(inner.topic, "daemon.drain.started");
        assert_eq!(inner.payload["in_flight"], 2);
        assert_eq!(inner.payload["timeout_secs"], 300);
    }

    #[test]
    fn a_non_drain_generic_event_is_not_translated() {
        let event = Event::Generic {
            topic: "some.other.topic".to_string(),
            payload: serde_json::json!({}),
        };
        assert!(map_event_to_daemon_event_record(&event).is_none());
    }

    #[test]
    fn topic_lag_is_not_translated() {
        assert!(map_event_to_daemon_event_record(&Event::TopicLag { skipped: 5 }).is_none());
    }

    #[test]
    fn a_sweep_scoped_event_is_not_translated_here() {
        // `collector.rs` owns sweep/issue-scoped events; this collector must
        // never double-emit them under a second record kind.
        let event = Event::SweepPhase {
            issue: 42,
            phase: "builder".to_string(),
            pr_number: None,
            repo: None,
        };
        assert!(map_event_to_daemon_event_record(&event).is_none());
    }
}
