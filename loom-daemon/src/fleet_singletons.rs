//! Start the daemon's **fleet-captain singleton tasks that own their loop**.
//!
//! A singleton job ([`crate::fleet_captain`], #8848) watches one shared thing
//! and must run on exactly one fleet host. Most of them ride an existing loop
//! (the ETA fleet refresh, the collector's captain gauges); the ones here have
//! a loop of their own, and share one call site because they share one
//! contract: each re-evaluates its captain gate on every tick, so a
//! `fleet.captain` edit takes effect without a restart, and each is inert on a
//! host that is not the producer.
//!
//! A sibling module rather than more lines in `daemon_service.rs`, an
//! over-threshold entry of the file-size ratchet.

use std::path::PathBuf;

use crate::event_bus::EventBus;

/// Join handles for whatever started. Held by the caller for the process's
/// lifetime; dropping them does not stop the tasks (tokio detaches on drop).
pub struct SingletonHandles {
    /// The CI telemetry poller, `None` when `autonomous.ciTelemetry.enabled`
    /// is off (#8824).
    pub ci_telemetry: Option<tokio::task::JoinHandle<()>>,
    /// The intake reconcile singleton (W7). Always running; it makes a forge
    /// call only on the declared captain with
    /// `fleet.intakeReconcile.singleton` set.
    pub intake_reconcile: tokio::task::JoinHandle<()>,
    /// The `loom:blocked` release task (#10763). Not a captain singleton: it
    /// is sharded per workspace by the role-runner shard, and owns its loop so
    /// the pass runs on the shard owner whether or not its work finder does.
    pub stale_blocked_release: tokio::task::JoinHandle<()>,
}

/// Spawn every loop-owning singleton task against `workspace_root`'s config.
/// `loops` is which daemon loops actually started; the release task serves a
/// workspace only through one of them.
pub fn spawn(
    workspace_root: PathBuf,
    bus: &EventBus,
    loops: crate::stale_blocked::release_task::Loops,
) -> SingletonHandles {
    SingletonHandles {
        ci_telemetry: crate::ci_telemetry::spawn_task_on(workspace_root.clone(), bus),
        intake_reconcile: crate::intake_reconcile::singleton::spawn_task(workspace_root.clone()),
        stale_blocked_release: crate::stale_blocked::release_task::spawn_task(
            workspace_root,
            loops,
        ),
    }
}
