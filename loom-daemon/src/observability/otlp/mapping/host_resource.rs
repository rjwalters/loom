//! The OTLP `Resource` describing the emitting daemon host — one constructor
//! shared by the logs and metrics request builders here, the traces builder,
//! and the relay's re-attachment path.

use opentelemetry_proto::tonic::resource::v1::Resource;

use super::kv_string;

/// A `Resource` describing the emitting daemon host. `service.version` is
/// always present: a record-supplied `daemon_version` (from a `host.health`
/// record, threaded in by `build_metrics_request`) wins; otherwise — traces,
/// logs, and metrics batches without `host.health` — it falls back to the
/// exporting build's own `CARGO_PKG_VERSION` (Issue #9028).
///
/// `host.name` (Issue #10977) has one source: the envelope's `host_id`, the
/// same string as `host.id` and `service.instance.id`. For a daemon that is
/// [`host_identity()`](crate::sweep_registry::host_identity) — the
/// operator-assigned `$LOOM_HOST_ID` when set, else the OS hostname
/// (`$HOSTNAME`, then the `hostname` binary). It is set here, on every signal,
/// so a receiver reached without a collector still gets a readable host axis.
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
    ];
    Resource {
        attributes,
        ..Default::default()
    }
}
