//! Start the daemon's **read-only observer side channels**.
//!
//! Two subsystems share one call site because they share one contract: each
//! is opt-in and off by default, each only ever *observes* (one pushes
//! telemetry out, one pulls event prompts in), and **neither may change what
//! any dispatch, claim, or merge path does**. Grouping them makes that
//! shared property reviewable in one place instead of implied by two
//! adjacent blocks in `daemon_service.rs`.
//!
//! A sibling module rather than more lines in `daemon_service.rs` (an
//! over-threshold ledger entry under `.loom/docs/file-size-policy.md`), which
//! is exactly the "new code goes in a new sibling module, leave a small
//! dispatch behind" shape the policy prescribes.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use loom_daemon::event_bus::EventBus;
use loom_daemon::workspace_pool::WorkspacePool;
use loom_daemon::{forge_events, observability};

/// Join handles for whatever started. Held by the caller for the process's
/// lifetime; dropping them does not stop the tasks (tokio detaches on drop),
/// it only gives up the ability to await them — which the daemon never does,
/// because these loops end with the process.
///
/// `dead_code` is allowed deliberately: the fields are never *read*, and that
/// is the point. They exist so the two spawn results are named rather than
/// discarded into `let _ = …`, which is what makes "did this observer start?"
/// answerable at the call site and gives a future shutdown path something to
/// await without changing this signature.
#[allow(dead_code)]
pub struct ObserverHandles {
    /// Collector + sender tasks, `None` when telemetry export is off or
    /// under-configured (#4705).
    pub observability: Option<Vec<tokio::task::JoinHandle<()>>>,
    /// Feed poll loop, `None` when the forge event feed is off or
    /// unprovisioned (ADR-0021, #8765).
    pub forge_events: Option<tokio::task::JoinHandle<()>>,
}

/// Spawn both observers against `workspace_root`'s resolved config.
///
/// `Instant::now()` approximates daemon uptime for the exporter's periodic
/// `host.health` sample — see `observability::spawn_task`'s doc comment for
/// why an exact daemon-start timestamp is not threaded through.
pub fn spawn(
    workspace_root: &Path,
    bus: &EventBus,
    workspace_pool: Arc<WorkspacePool>,
) -> ObserverHandles {
    // Pluggable telemetry exporter (#4705, epic #4702 Phase 1). Off by
    // default (FLAGS-OFF, `observability.enabled=true` to opt in) — a bus
    // subscriber plus a queue-drain sender.
    let observability_config = observability::read_config(workspace_root);
    let observability = observability::spawn_task(
        &observability_config,
        workspace_root.to_path_buf(),
        bus,
        Instant::now(),
        workspace_pool,
    );

    // Forge event-feed consumer (ADR-0021, epic #8764 Phase 1 — #8765). Off
    // by default. Observe-only in this phase: it publishes one `forge.event`
    // bus prompt per non-empty page and NOTHING subscribes yet, so a daemon
    // with the feed on behaves identically to one with it off apart from the
    // journal it writes and the status it reports.
    let forge_events_config = forge_events::read_config(workspace_root);
    let forge_events = forge_events::spawn_task(&forge_events_config, bus);

    ObserverHandles {
        observability,
        forge_events,
    }
}
