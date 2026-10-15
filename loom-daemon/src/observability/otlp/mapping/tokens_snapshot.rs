//! OTLP gauges for `tokens.snapshot`: one series per opaque pool account
//! (`account` + `provider` attributes, never credential material). Every
//! optional field follows the absent-not-zero rule — no source, no series.

use opentelemetry_proto::tonic::metrics::v1::number_data_point;

use super::{kv_int, kv_string, MetricSample};
use crate::telemetry::TokenSnapshotRecord;

/// The per-account gauge samples for one `tokens.snapshot` record.
pub(super) fn samples(r: &TokenSnapshotRecord, time_unix_nano: u64) -> Vec<MetricSample> {
    let mut samples = Vec::new();
    for account in &r.accounts {
        let mut attributes = vec![
            kv_string("account", account.account.clone()),
            kv_string("provider", account.provider.clone()),
        ];
        if let Some(rank) = account.rank {
            attributes.push(kv_int("rank", i64::from(rank)));
        }
        if let Some(usage_fraction) = account.usage_fraction {
            samples.push(MetricSample {
                name: "loom.tokens.usage_fraction",
                description: "Fraction of the current limit window consumed (0..1).",
                unit: "1",
                attributes: attributes.clone(),
                value: number_data_point::Value::AsDouble(usage_fraction),
                time_unix_nano,
            });
        }
        // Issue #9005: the rolling 7-day axis beside the 5h one. Same
        // absent-not-zero rule — no source, no series.
        if let Some(weekly) = account.usage_fraction_weekly {
            samples.push(MetricSample {
                name: "loom.tokens.usage_fraction_weekly",
                description: "Fraction of the rolling 7-day limit window consumed (0..1).",
                unit: "1",
                attributes: attributes.clone(),
                value: number_data_point::Value::AsDouble(weekly),
                time_unix_nano,
            });
        }
        // Issue #11992: the instant the gating window lifts, so a
        // capacity-blocked ready_wait reads as a countdown. Absent
        // means unknown — never "resets now".
        if let Some(reset_at) = account.limit_window_reset_at {
            samples.push(MetricSample {
                name: "loom.tokens.limit_window_reset_at",
                description: "Unix time (s) the window gating this account resets (7d if exhausted, else 5h).",
                unit: "s",
                attributes: attributes.clone(),
                value: number_data_point::Value::AsInt(reset_at.timestamp()),
                time_unix_nano,
            });
            // Signed on purpose: a negative value is a stale reset
            // the producer has not yet rolled over, not "free now".
            samples.push(MetricSample {
                name: "loom.tokens.seconds_until_reset",
                description:
                    "Seconds from the snapshot to limit_window_reset_at (negative = stale).",
                unit: "s",
                attributes: attributes.clone(),
                value: number_data_point::Value::AsInt((reset_at - r.captured_at).num_seconds()),
                time_unix_nano,
            });
        }
        samples.push(MetricSample {
            name: "loom.tokens.exhausted",
            description:
                "Whether the account is currently excluded from the usable pool (1 = exhausted).",
            unit: "1",
            attributes,
            value: number_data_point::Value::AsInt(i64::from(account.exhausted)),
            time_unix_nano,
        });
    }
    samples
}
