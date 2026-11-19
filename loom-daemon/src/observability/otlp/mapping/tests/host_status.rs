//! `host.status` gauges (#12112): names from the signal table, absent never
//! zero, per-volume series labelled `volume`.

use super::*;
use crate::telemetry::kinds::host_status::{HostStatusRecord, VolumeStatus};

fn record() -> HostStatusRecord {
    let mut r = HostStatusRecord {
        captured_at: ts(),
        interval_secs: 5,
        raw: BTreeMap::new(),
        filtered: BTreeMap::new(),
        volumes: Vec::new(),
    };
    r.raw.insert("cpu_utilisation".into(), 0.25);
    r.filtered.insert("cpu_utilisation".into(), 0.2);
    r.raw.insert("mem_used_bytes".into(), 4096.0);
    r.filtered.insert("mem_available_bytes".into(), 9000.0);
    r.volumes.push(VolumeStatus {
        volume: "worktree_root".into(),
        raw: BTreeMap::from([("vol_free_bytes".to_string(), 1e9)]),
        filtered: BTreeMap::from([("vol_free_bytes".to_string(), 1.1e9)]),
    });
    r
}

fn metrics_of(r: HostStatusRecord) -> Vec<opentelemetry_proto::tonic::metrics::v1::Metric> {
    let batch = vec![envelope("host-s", TelemetryRecord::HostStatus(r))];
    let request = build_metrics_request(&batch).expect("host.status is a gauge kind");
    request.resource_metrics[0].scope_metrics[0].metrics.clone()
}

#[test]
fn raw_and_filtered_values_become_named_gauges() {
    let metrics = metrics_of(record());
    let names: Vec<&str> = metrics.iter().map(|m| m.name.as_str()).collect();
    for expected in [
        "system.cpu.utilization",
        "loom.host.cpu_utilisation_ewma_30s",
        "system.memory.usage",
        "loom.host.mem_available_bytes_ewma_30s",
        "system.filesystem.usage",
        "loom.host.disk_free_bytes_ewma_300s",
    ] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }
    // Unread signals have no series: absent, never zero.
    for absent in [
        "loom.host.net_rx_bytes_per_sec",
        "system.paging.usage",
        "loom.host.disk_io_busy",
    ] {
        assert!(!names.contains(&absent), "{absent} must be absent: {names:?}");
    }
}

#[test]
fn volume_series_carry_the_volume_label_and_raw_state() {
    let metrics = metrics_of(record());
    let fs = metrics
        .iter()
        .find(|m| m.name == "system.filesystem.usage")
        .unwrap();
    let Some(metric::Data::Gauge(g)) = &fs.data else {
        panic!("gauge expected")
    };
    let attrs: Vec<(&str, String)> = g.data_points[0]
        .attributes
        .iter()
        .map(|kv| {
            let v = match kv.value.as_ref().and_then(|v| v.value.clone()) {
                Some(any_value::Value::StringValue(s)) => s,
                other => panic!("string attr expected, got {other:?}"),
            };
            (kv.key.as_str(), v)
        })
        .collect();
    assert!(attrs.contains(&("state", "free".to_string())), "{attrs:?}");
    assert!(attrs.contains(&("volume", "worktree_root".to_string())), "{attrs:?}");
}

#[test]
fn an_empty_record_produces_no_gauges() {
    let mut r = record();
    r.raw.clear();
    r.filtered.clear();
    r.volumes.clear();
    let batch = vec![envelope("host-s", TelemetryRecord::HostStatus(r))];
    let empty = build_metrics_request(&batch).is_none_or(|req| {
        req.resource_metrics
            .iter()
            .all(|rm| rm.scope_metrics.iter().all(|sm| sm.metrics.is_empty()))
    });
    assert!(empty);
}
