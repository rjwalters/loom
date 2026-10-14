//! OTLP mapping for `token_ranking.refresh` (#10744): one log record per
//! workspace per refresh round, stamped at the round's start.
//!
//! The body is the record's JSON (it carries the per-account entries). The
//! round-level scalars, including a count per account outcome, ride as
//! `loom.token_ranking.*` attributes, so "how many metered probes did this host
//! send today" is a plain sum over `loom.token_ranking.api_key_probe_count`.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv, kv_int, kv_string, nanos};
use crate::telemetry::kinds::token_ranking_refresh::{
    AccountOutcome, RoundOutcome, TokenRankingRefreshRecord,
};
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

/// A failed round, or a round that spent money on an API-key probe, is a
/// warning. Everything else is info.
fn severity(r: &TokenRankingRefreshRecord) -> SeverityNumber {
    if r.outcome == RoundOutcome::Failure || r.api_key_probe_count > 0 {
        SeverityNumber::Warn
    } else {
        SeverityNumber::Info
    }
}

/// `(event_name, severity, record time, attributes, body)` for a
/// `token_ranking.refresh` record; `None` for every other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::TokenRankingRefresh(r) = record else {
        return None;
    };
    let count = |o: AccountOutcome| i64::try_from(r.count(o)).unwrap_or(i64::MAX);
    let mut attributes = vec![
        kv_string("loom.token_ranking.round_id", r.round_id.clone()),
        kv_string("loom.token_ranking.workspace", r.workspace.clone()),
        kv_string("loom.token_ranking.outcome", r.outcome.as_str()),
        kv_string("loom.token_ranking.source", r.source.as_str()),
        kv_bool("loom.token_ranking.skipped_fresh", r.skipped_fresh()),
        kv_int(
            "loom.token_ranking.account_count",
            i64::try_from(r.accounts.len()).unwrap_or(i64::MAX),
        ),
        kv_int("loom.token_ranking.probed_count", i64::from(r.probed_count)),
        kv_int("loom.token_ranking.api_key_probe_count", i64::from(r.api_key_probe_count)),
        kv_int("loom.token_ranking.ok_count", count(AccountOutcome::Ok)),
        kv_int("loom.token_ranking.rate_limited_count", count(AccountOutcome::RateLimited)),
        kv_int("loom.token_ranking.auth_dead_count", count(AccountOutcome::AuthDead)),
        kv_int("loom.token_ranking.skipped_fresh_count", count(AccountOutcome::SkippedFresh)),
        kv_int("loom.token_ranking.error_count", count(AccountOutcome::Error)),
        kv_int("loom.token_ranking.unsupported_count", count(AccountOutcome::Unsupported)),
        kv_int(
            "loom.token_ranking.duration_ms",
            i64::try_from(r.duration_ms).unwrap_or(i64::MAX),
        ),
        kv_string("loom.token_ranking.version", r.loom.version.clone()),
        kv_string("loom.token_ranking.revision", r.loom.revision.clone()),
        kv_string("loom.token_ranking.tree_state", r.loom.tree_state.clone()),
        kv_bool("loom.token_ranking.provenance_complete", r.loom.complete),
    ];
    if let Some(class) = &r.failure_class {
        attributes.push(kv_string("loom.token_ranking.failure_class", class.clone()));
    }
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("token_ranking.refresh", severity(r), nanos(r.started_at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "token_ranking_tests.rs"]
mod tests;
