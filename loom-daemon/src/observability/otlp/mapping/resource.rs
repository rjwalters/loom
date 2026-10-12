//! The OTLP `Resource` that describes the emitting daemon host (#10977,
//! #11197). Extracted from `mapping.rs` when it grew past the file-size
//! ratchet — every signal's resource is built here, and here only.

use opentelemetry_proto::tonic::resource::v1::Resource;

use crate::fleet_sync;
use crate::telemetry::trace::provenance;

use super::kv_string;

/// A `Resource` describing the emitting daemon host. `service.version` is
/// always present: a record-supplied `daemon_version` (from a `host.health`
/// record, threaded in by [`build_metrics_request`](super::build_metrics_request))
/// wins; otherwise — traces, logs, and metrics batches without `host.health`
/// — it falls back to the exporting build's own `CARGO_PKG_VERSION`
/// (Issue #9028).
///
/// `host.name` (Issue #10977) has one source: the envelope's `host_id`, the
/// same string as `host.id` and `service.instance.id`. For a daemon that is
/// [`host_identity()`](crate::sweep_registry::host_identity) — the
/// operator-assigned `$LOOM_HOST_ID` when set, else the OS hostname
/// (`$HOSTNAME`, then the `hostname` binary). It is set here, on every signal,
/// so a receiver reached without a collector still gets a readable host axis.
///
/// # Fleet provenance (Issue #11197)
///
/// Two more keys ride every signal, read **at call time** — the resource is
/// rebuilt per export batch and must never be memoized, so a fleet-sync timer
/// pass that applies a new commit shows up on the next batch with no restart:
///
/// - `loom.daemon.revision` — the full git SHA of the exporting build
///   (`unknown` for a tarball build), the same value
///   [`provenance::stamp`] puts on every span.
/// - `loom.fleet.config_sha` — the fleet-gitops commit whose rendered config
///   tiers are on disk on this host right now: a 40-hex SHA, `unknown` (a
///   fleet host where no applied commit is established yet — e.g. during a
///   startup-pass timeout) or `none` (no fleet store). Never empty and never
///   omitted, so a backend `resources_string` read cannot confuse "missing"
///   with "not known yet" — see [`fleet_sync::applied_config_sha`].
///
/// Both describe the **exporting** daemon at export time: a record queued
/// before a roll and exported after it carries the new SHA, the same accepted
/// behaviour as the `service.version` fallback. "Applied" also means "on
/// disk", not "live in this process" — restart-only keys
/// (`config_journal::restart_keys`) take effect on the next start.
pub(in crate::observability::otlp) fn resource_for_host(
    host_id: &str,
    daemon_version: Option<&str>,
) -> Resource {
    let version = daemon_version
        .filter(|v| !v.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    let attributes = vec![
        kv_string("service.name", "loom-daemon"),
        kv_string("service.instance.id", host_id),
        kv_string("host.id", host_id),
        kv_string("host.name", host_id),
        kv_string("service.version", version),
        kv_string(provenance::DAEMON_REVISION, provenance::daemon().revision),
        kv_string(
            fleet_sync::FLEET_CONFIG_SHA,
            fleet_sync::applied_config_sha().as_resource_value(),
        ),
    ];
    Resource {
        attributes,
        ..Default::default()
    }
}
