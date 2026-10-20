//! The OTLP `Resource` describing the emitting daemon host — one constructor
//! shared by the logs and metrics request builders here, the traces builder,
//! and the relay's re-attachment path.

use opentelemetry_proto::tonic::resource::v1::Resource;

use crate::fleet_sync;
use crate::host_shape;
use crate::telemetry::trace::provenance;

use super::{kv_int, kv_string};

/// A `Resource` describing the emitting daemon host. `service.version` is
/// always present: a record-supplied `daemon_version` (from a `host.health`
/// record, threaded in by `build_metrics_request`) wins; otherwise — traces,
/// logs, and metrics batches without `host.health` — it falls back to the
/// exporting build's own `CARGO_PKG_VERSION` (Issue #9028).
///
/// `host.name` (Issue #10977) has one source: the envelope's `host_id`, the
/// same string as `host.id` and `service.instance.id`. For a daemon that is
/// [`host_identity()`](crate::sweep_registry::host_identity) — fleet-gitops
/// `fleet.hostId`, else `$LOOM_HOST_ID`, else the persisted `~/.loom/host-id`
/// (#11530; never the OS hostname). It is set here, on every signal,
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
///   with "not known yet" — see [`fleet_sync::applied::applied_config_sha`].
///
/// Both describe the **exporting** daemon at export time: a record queued
/// before a roll and exported after it carries the new SHA, the same accepted
/// behaviour as the `service.version` fallback. "Applied" also means "on
/// disk", not "live in this process" — restart-only keys
/// take effect on the next start.
///
/// # Host shape (Issue #12874)
///
/// After those seven, the host's static shape from [`host_shape`]:
/// `host.cpu.count` and `host.arch` always, `loom.host.mem_total_bytes` when
/// RAM was measured (absent, never `0`), and — where readable —
/// `host.type` (the EC2 instance type, or a Mac's `hw.model`) plus
/// `cloud.provider` / `cloud.region` once the one-shot IMDS probe has
/// succeeded. The cloud cell is read without blocking, so a batch exported
/// before the probe finishes lacks those keys and the next one has them.
pub(in crate::observability::otlp) fn resource_for_host(
    host_id: &str,
    daemon_version: Option<&str>,
) -> Resource {
    let version = daemon_version
        .filter(|v| !v.is_empty())
        .unwrap_or(env!("CARGO_PKG_VERSION"));
    let mut attributes = vec![
        kv_string("service.name", "loom-daemon"),
        kv_string("service.instance.id", host_id),
        kv_string("host.id", host_id),
        kv_string("host.name", host_id),
        kv_string("service.version", version),
        kv_string(provenance::DAEMON_REVISION, provenance::daemon().revision),
        kv_string(
            fleet_sync::applied::FLEET_CONFIG_SHA,
            fleet_sync::applied::applied_config_sha().as_resource_value(),
        ),
    ];
    let shape = host_shape::static_shape();
    attributes.push(kv_int(
        host_shape::HOST_CPU_COUNT,
        i64::try_from(shape.cpu_count).unwrap_or(i64::MAX),
    ));
    if let Some(bytes) = shape.mem_total_bytes.and_then(|b| i64::try_from(b).ok()) {
        attributes.push(kv_int(host_shape::HOST_MEM_TOTAL_BYTES, bytes));
    }
    attributes.push(kv_string(host_shape::HOST_ARCH, shape.arch));
    // One read of the cloud cell, so `host.type` and `cloud.*` always agree.
    let cloud = host_shape::cloud_shape();
    let host_type = cloud
        .as_ref()
        .map(|c| c.instance_type.clone())
        .or_else(|| shape.model.clone());
    if let Some(host_type) = host_type {
        attributes.push(kv_string(host_shape::HOST_TYPE, host_type));
    }
    if let Some(cloud) = cloud {
        attributes.push(kv_string(host_shape::CLOUD_PROVIDER, cloud.provider));
        attributes.push(kv_string(host_shape::CLOUD_REGION, cloud.region));
    }
    Resource {
        attributes,
        ..Default::default()
    }
}
