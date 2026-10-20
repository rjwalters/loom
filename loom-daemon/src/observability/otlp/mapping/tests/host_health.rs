//! #12811: the `host.health` fields that rode only `/ingest` now have OTLP
//! gauges (`mapping/host_health.rs`): in-flight sweep ids, the managed-repo
//! roster, protection, build time and the persistent role failures.

use super::*;
use crate::observability::otlp::mapping::host_health::{
    HOST_HEALTH_DATAPOINT_KEYS, MAX_ACTIVE_SWEEP_IDS,
};
use crate::telemetry::{HostProtectionSummary, ManagedRepoEntry};

fn metrics_of(e: TelemetryEnvelope) -> Vec<Metric> {
    let request = build_metrics_request(&[e]).expect("host.health is a gauge kind");
    request.resource_metrics[0].scope_metrics[0].metrics.clone()
}

fn with(edit: impl FnOnce(&mut HostHealthRecord)) -> TelemetryEnvelope {
    let mut e = host_health_envelope();
    if let TelemetryRecord::HostHealth(r) = &mut e.record {
        edit(r);
    }
    e
}

/// `(value, attributes)` of every point of `name`.
fn points(metrics: &[Metric], name: &str) -> Vec<(i64, BTreeMap<String, String>)> {
    let Some(metric) = metrics.iter().find(|m| m.name == name) else {
        return Vec::new();
    };
    let Some(metric::Data::Gauge(gauge)) = &metric.data else {
        panic!("{name} must be a gauge");
    };
    gauge
        .data_points
        .iter()
        .map(|p| {
            let value = match p.value {
                Some(number_data_point::Value::AsInt(v)) => v,
                ref other => panic!("{name}: expected AsInt, got {other:?}"),
            };
            let attrs = p
                .attributes
                .iter()
                .map(|kv| {
                    let v = match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
                        Some(any_value::Value::StringValue(s)) => s.clone(),
                        other => panic!("{}: expected a string, got {other:?}", kv.key),
                    };
                    (kv.key.clone(), v)
                })
                .collect();
            (value, attrs)
        })
        .collect()
}

fn full() -> TelemetryEnvelope {
    with(|r| {
        r.active_sweep_ids = vec!["sweep-b".into(), "sweep-a".into()];
        r.managed_repos = vec![
            ManagedRepoEntry {
                slug: "rjwalters/loom".into(),
                visibility: RepoVisibility::Public,
                weight: Some(3),
            },
            ManagedRepoEntry {
                slug: "acme/secret".into(),
                visibility: RepoVisibility::Private,
                weight: None,
            },
        ];
        r.protection = Some(HostProtectionSummary {
            state: "watchdog-not-provisioned".into(),
            watchdog_provisioned: Some(false),
        });
    })
}

#[test]
fn every_formerly_ingest_only_field_has_a_gauge() {
    let m = metrics_of(full());

    let sweeps = points(&m, "loom.host.active_sweeps");
    assert_eq!(sweeps.len(), 1);
    assert_eq!(sweeps[0].0, 2);
    assert_eq!(sweeps[0].1["sweep_ids"], "sweep-a,sweep-b", "sorted, comma-joined");

    let repos = points(&m, "loom.host.managed_repo");
    assert_eq!(repos.len(), 2);
    assert!(repos.iter().all(|(v, _)| *v == 1));
    assert_eq!(repos[0].1["repo"], "rjwalters/loom");
    assert_eq!(repos[0].1["visibility"], "public");
    assert_eq!(repos[1].1["repo"], "acme/secret");
    assert_eq!(repos[1].1["visibility"], "private");
    // Weight only where known: the unweighted repo has no weight point.
    let weights = points(&m, "loom.host.managed_repo_weight");
    assert_eq!(weights.len(), 1);
    assert_eq!((weights[0].0, weights[0].1["repo"].as_str()), (3, "rjwalters/loom"));

    let protection = points(&m, "loom.host.protection");
    assert_eq!(protection.len(), 1);
    assert_eq!(protection[0].1["state"], "watchdog-not-provisioned");

    let built = points(&m, "loom.host.built_at_seconds");
    assert_eq!(built, vec![(ts().timestamp(), BTreeMap::new())]);

    // The fixture's one persistent failure: judge @ /repos/loom, 2 ticks.
    let failures = points(&m, "loom.host.role_persistent_failure");
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, 2);
    assert_eq!(failures[0].1["role"], "judge");
    assert_eq!(failures[0].1["workspace"], "loom", "the directory name, never the path");
    let last_at = points(&m, "loom.host.role_persistent_failure_last_at_seconds");
    assert_eq!(last_at.len(), 1);
    assert_eq!(last_at[0].0, ts().timestamp());
    assert_eq!(last_at[0].1, failures[0].1);
    // The free-text detail ("no-token-pool") rides no attribute.
    assert!(m.iter().all(|metric| {
        let Some(metric::Data::Gauge(g)) = &metric.data else { return true };
        g.data_points.iter().flat_map(|p| &p.attributes).all(|kv| {
            !matches!(kv.value.as_ref().and_then(|v| v.value.as_ref()),
                Some(any_value::Value::StringValue(s)) if s.contains("no-token-pool") || s.contains("/repos/"))
        })
    }));
}

#[test]
fn empty_and_unmeasured_fields_stay_absent_but_zero_sweeps_is_a_point() {
    let m = metrics_of(with(|r| {
        r.active_sweep_ids.clear();
        r.managed_repos.clear();
        r.protection = None;
        r.built_at = None;
        r.roles.persistent.clear();
    }));
    // Zero in-flight sweeps is a real reading: value 0, no id list.
    assert_eq!(points(&m, "loom.host.active_sweeps"), vec![(0, BTreeMap::new())]);
    for absent in [
        "loom.host.managed_repo",
        "loom.host.managed_repo_weight",
        "loom.host.protection",
        "loom.host.built_at_seconds",
        "loom.host.role_persistent_failure",
        "loom.host.role_persistent_failure_last_at_seconds",
    ] {
        assert!(points(&m, absent).is_empty(), "{absent} must have no point");
    }
}

#[test]
fn a_long_sweep_list_is_capped_and_the_value_keeps_the_true_count() {
    let total = MAX_ACTIVE_SWEEP_IDS + 36;
    let m = metrics_of(with(|r| {
        r.active_sweep_ids = (0..total).rev().map(|i| format!("sweep-{i:04}")).collect();
    }));
    let sweeps = points(&m, "loom.host.active_sweeps");
    assert_eq!(sweeps[0].0, i64::try_from(total).unwrap());
    let ids: Vec<&str> = sweeps[0].1["sweep_ids"].split(',').collect();
    assert_eq!(ids.len(), MAX_ACTIVE_SWEEP_IDS, "the list is bounded");
    assert_eq!(ids[0], "sweep-0000", "the kept ids are the sorted prefix");
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn datapoint_keys_are_declared_and_the_collector_keeps_them() {
    const CONFIG: &str =
        include_str!("../../../../../../defaults/observability/collector/config.yaml");
    let m = metrics_of(full());
    for metric in m.iter().filter(|m| {
        m.name.starts_with("loom.host.active_sweeps")
            || m.name.starts_with("loom.host.managed_repo")
            || m.name == "loom.host.protection"
            || m.name.starts_with("loom.host.role_persistent_failure")
    }) {
        for (_, attrs) in points(&m, &metric.name) {
            for key in attrs.keys() {
                assert!(HOST_HEALTH_DATAPOINT_KEYS.contains(&key.as_str()), "{key}");
            }
        }
    }
    let datapoint_keep = CONFIG
        .lines()
        .find(|l| l.contains("keep_keys(attributes, [") && l.contains("\"volume\""))
        .expect("the transform/privacy datapoint keep_keys line");
    for key in HOST_HEALTH_DATAPOINT_KEYS {
        assert!(datapoint_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
}
