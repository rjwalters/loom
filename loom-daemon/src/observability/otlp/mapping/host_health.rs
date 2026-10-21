//! OTLP carriers for the `host.health` fields that had none (#12811): the
//! in-flight sweep ids, the managed-repo roster, the crash-protection state,
//! the build time and the persistent role-tick failures. Before this they
//! rode only the native `/ingest` exporter, which is being retired (#12808).
//!
//! All are gauges on the same `host.health` sample, so a reader groups them
//! by `host.id` exactly like the existing `loom.host.*` gauges:
//!
//! | Field | Gauge | Value | Attributes |
//! | --- | --- | --- | --- |
//! | `active_sweep_ids` | `loom.host.active_sweeps` | count (uncapped) | `sweep_ids` |
//! | `managed_repos[]` | `loom.host.managed_repo` | 1 per repo | `repo`, `visibility` |
//! | `managed_repos[].weight` | `loom.host.managed_repo_weight` | weight | `repo` |
//! | `protection` | `loom.host.protection` | 1 | `state` |
//! | `built_at` | `loom.host.built_at_seconds` | Unix seconds | — |
//! | `roles.persistent[]` | `loom.host.role_persistent_failure` | failures | `role`, `workspace` |
//! | `roles.persistent[].last_at` | `loom.host.role_persistent_failure_last_at_seconds` | Unix seconds | `role`, `workspace` |
//! | `is_captain` | `loom.host.is_captain` | 1 captain, 0 not | — |
//!
//! Absent stays absent ("unknown != zero"): no `protection`, `built_at` or
//! `weight` or `is_captain` (no `fleet.captain` declared) means no point. `loom.host.active_sweeps` is always emitted, since
//! the producer always reads the registry (`collect_active_sweep_ids`), so 0
//! is a real "no sweeps in flight".
//!
//! `sweep_ids` is the sorted id list joined with `,`, at most
//! [`MAX_ACTIVE_SWEEP_IDS`] ids. The value is the true count, so a reader
//! detects a truncated list as fewer ids than the value and must not treat
//! the list as complete then. A role failure's `workspace` is the final path
//! component of its root, never the absolute path; its free-text `detail` is
//! not exported.

use opentelemetry_proto::tonic::metrics::v1::number_data_point;

use super::metadata::visibility_str;
use super::{kv_string, MetricSample};
use crate::telemetry::HostHealthRecord;

/// The most sweep ids `loom.host.active_sweeps` lists in `sweep_ids`.
pub(super) const MAX_ACTIVE_SWEEP_IDS: usize = 32;

/// The datapoint attribute keys these gauges set; each must be on the
/// collector's datapoint `keep_keys` list (asserted by the contract test).
#[cfg(test)]
pub(super) const HOST_HEALTH_DATAPOINT_KEYS: &[&str] = &[
    "sweep_ids",
    "repo",
    "visibility",
    "state",
    "role",
    "workspace",
];

fn int(value: i64) -> number_data_point::Value {
    number_data_point::Value::AsInt(value)
}

fn count(n: usize) -> number_data_point::Value {
    int(i64::try_from(n).unwrap_or(i64::MAX))
}

fn sample(
    name: &'static str,
    description: &'static str,
    unit: &'static str,
    attributes: Vec<opentelemetry_proto::tonic::common::v1::KeyValue>,
    value: number_data_point::Value,
    time_unix_nano: u64,
) -> MetricSample {
    MetricSample {
        name,
        description,
        unit,
        attributes,
        value,
        time_unix_nano,
    }
}

/// The `sweep_ids` attribute value: sorted, de-duplicated, capped.
fn sweep_ids_value(ids: &[String]) -> String {
    let mut sorted: Vec<&str> = ids.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();
    sorted.truncate(MAX_ACTIVE_SWEEP_IDS);
    sorted.join(",")
}

/// The final path component of a workspace root, never the absolute path.
fn workspace_name(root: &std::path::Path) -> String {
    root.file_name()
        .map_or_else(|| "-".to_string(), |name| name.to_string_lossy().into_owned())
}

/// The gauge samples carrying `r`'s fleet-state fields.
pub(super) fn samples(r: &HostHealthRecord, time_unix_nano: u64) -> Vec<MetricSample> {
    let mut out = Vec::new();

    let mut sweeps_attrs = Vec::new();
    if !r.active_sweep_ids.is_empty() {
        sweeps_attrs.push(kv_string("sweep_ids", sweep_ids_value(&r.active_sweep_ids)));
    }
    out.push(sample(
        "loom.host.active_sweeps",
        "In-flight (non-terminal) sweeps in this host's registry; sweep_ids lists them \
         (sorted, comma-joined, capped), fewer ids than the value means truncated.",
        "{sweep}",
        sweeps_attrs,
        count(r.active_sweep_ids.len()),
        time_unix_nano,
    ));

    for repo in &r.managed_repos {
        out.push(sample(
            "loom.host.managed_repo",
            "One point per repository this host's workspace registry manages.",
            "1",
            vec![
                kv_string("repo", repo.slug.clone()),
                kv_string("visibility", visibility_str(repo.visibility)),
            ],
            int(1),
            time_unix_nano,
        ));
        if let Some(weight) = repo.weight {
            out.push(sample(
                "loom.host.managed_repo_weight",
                "A managed repository's draw weight (higher draws more).",
                "1",
                vec![kv_string("repo", repo.slug.clone())],
                int(i64::from(weight)),
                time_unix_nano,
            ));
        }
    }

    if let Some(protection) = &r.protection {
        out.push(sample(
            "loom.host.protection",
            "Watchdog/crash-protection state (1 per current state; state label: \
             protected|no-marker|watchdog-not-provisioned|unknown).",
            "1",
            vec![kv_string("state", protection.state.clone())],
            int(1),
            time_unix_nano,
        ));
    }

    if let Some(built_at) = r.built_at {
        out.push(sample(
            "loom.host.built_at_seconds",
            "Unix time the running daemon binary was compiled.",
            "s",
            Vec::new(),
            int(built_at.timestamp()),
            time_unix_nano,
        ));
    }

    if let Some(is_captain) = r.is_captain {
        out.push(sample(
            "loom.host.is_captain",
            "1 when this host's daemon believes it is the fleet captain, 0 when a captain is \
             declared and it is not this host; absent when no captain is declared.",
            "1",
            Vec::new(),
            int(i64::from(is_captain)),
            time_unix_nano,
        ));
    }

    for failure in &r.roles.persistent {
        let attributes = || {
            vec![
                kv_string("role", failure.role.clone()),
                kv_string("workspace", workspace_name(&failure.root)),
            ]
        };
        out.push(sample(
            "loom.host.role_persistent_failure",
            "Failed ticks in the sampled window of a (workspace, role) pair whose latest \
             tick failed.",
            "{tick}",
            attributes(),
            count(failure.failures),
            time_unix_nano,
        ));
        out.push(sample(
            "loom.host.role_persistent_failure_last_at_seconds",
            "Unix time of the latest tick record of a persistently failing (workspace, role) pair.",
            "s",
            attributes(),
            int(failure.last_at.timestamp()),
            time_unix_nano,
        ));
    }

    out
}
