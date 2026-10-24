//! OTLP gauges for `host.status` (#12112): the raw and EWMA-filtered host
//! signals. The names, units and time constants come from the one
//! [`SIGNALS`] table; a signal missing from a record's map has no data point
//! (absent, never zero).

use std::collections::BTreeMap;

use opentelemetry_proto::tonic::metrics::v1::number_data_point;

use super::{kv_string, MetricSample};
use crate::telemetry::kinds::host_status::{HostStatusRecord, SignalScope, SignalSpec, SIGNALS};

fn gauge(
    spec: &SignalSpec,
    name: &'static str,
    state: Option<&'static str>,
    volume: Option<&str>,
    value: f64,
    time_unix_nano: u64,
) -> MetricSample {
    let mut attributes = Vec::new();
    if let Some(state) = state {
        attributes.push(kv_string("state", state));
    }
    if let Some(volume) = volume {
        attributes.push(kv_string("volume", volume));
    }
    MetricSample {
        name,
        description: spec.description,
        unit: spec.unit,
        attributes,
        value: number_data_point::Value::AsDouble(value),
        time_unix_nano,
    }
}

fn push_scope(
    out: &mut Vec<MetricSample>,
    scope: SignalScope,
    raw: &BTreeMap<String, f64>,
    filtered: &BTreeMap<String, f64>,
    volume: Option<&str>,
    time_unix_nano: u64,
) {
    for spec in SIGNALS.iter().filter(|s| s.scope == scope) {
        if let (Some(name), Some(v)) = (spec.raw_name, raw.get(spec.key)) {
            out.push(gauge(spec, name, spec.raw_state, volume, *v, time_unix_nano));
        }
        if let Some(v) = filtered.get(spec.key) {
            out.push(gauge(spec, spec.filtered_name, None, volume, *v, time_unix_nano));
        }
    }
}

/// The gauge samples for one `host.status` record.
pub(super) fn samples(r: &HostStatusRecord, time_unix_nano: u64) -> Vec<MetricSample> {
    let mut out = Vec::new();
    push_scope(&mut out, SignalScope::Host, &r.raw, &r.filtered, None, time_unix_nano);
    for v in &r.volumes {
        push_scope(
            &mut out,
            SignalScope::Volume,
            &v.raw,
            &v.filtered,
            Some(&v.volume),
            time_unix_nano,
        );
    }
    out
}
