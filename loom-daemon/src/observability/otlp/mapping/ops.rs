//! OTLP mapping for `metric.points` (Issue #8860): each point becomes one
//! `NumberDataPoint` of a `Gauge` or a monotonic delta `Sum`, as fixed by its
//! [`MetricName`]. The name/label vocabulary is owned by
//! [`crate::telemetry::ops`]; this module only renders it, re-applying the
//! label policy so a restored queue record cannot bypass it.

use std::collections::BTreeMap;

use opentelemetry_proto::tonic::metrics::v1::{
    metric, number_data_point, AggregationTemporality, Gauge, Metric, NumberDataPoint, Sum,
};

use super::{kv_string, nanos};
use crate::telemetry::ops::{MetricKind, MetricName, MetricValue};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

/// Data points for one host, keyed by metric name.
pub(super) type OpsMetricsForHost = BTreeMap<MetricName, Vec<NumberDataPoint>>;

/// The bounded data points of a `metric.points` envelope, or `None` for any
/// other kind.
pub(super) fn points_for(
    envelope: &TelemetryEnvelope,
) -> Option<Vec<(MetricName, NumberDataPoint)>> {
    let TelemetryRecord::MetricPoints(record) = &envelope.record else {
        return None;
    };
    let time_unix_nano = nanos(record.captured_at);
    // Delta sums need their interval start (OTLP data model); gauges leave it
    // unset. Clamped so a skewed start never lands after the point time.
    let delta_start =
        nanos(record.interval_start.unwrap_or(record.captured_at)).min(time_unix_nano);
    Some(
        record
            .bounded_points()
            .into_iter()
            .map(|point| {
                let value = match point.value {
                    MetricValue::Int(v) => number_data_point::Value::AsInt(v),
                    MetricValue::Double(v) => number_data_point::Value::AsDouble(v),
                };
                let data_point = NumberDataPoint {
                    attributes: point
                        .labels
                        .iter()
                        .map(|(key, value)| kv_string(key, value.clone()))
                        .collect(),
                    time_unix_nano,
                    start_time_unix_nano: match point.name.kind() {
                        MetricKind::DeltaCounter => delta_start,
                        MetricKind::Gauge => 0,
                    },
                    value: Some(value),
                    ..Default::default()
                };
                (point.name, data_point)
            })
            .collect(),
    )
}

/// Render one host's grouped points as OTLP metrics.
pub(super) fn metrics(points: OpsMetricsForHost) -> impl Iterator<Item = Metric> {
    points.into_iter().map(|(name, data_points)| Metric {
        name: name.as_str().to_string(),
        description: name.description().to_string(),
        unit: name.unit().to_string(),
        data: Some(match name.kind() {
            MetricKind::Gauge => metric::Data::Gauge(Gauge { data_points }),
            MetricKind::DeltaCounter => metric::Data::Sum(Sum {
                data_points,
                aggregation_temporality: AggregationTemporality::Delta as i32,
                is_monotonic: true,
            }),
        }),
        ..Default::default()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
