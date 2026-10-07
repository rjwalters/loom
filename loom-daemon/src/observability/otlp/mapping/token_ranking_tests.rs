use chrono::{TimeZone, Utc};
use opentelemetry_proto::tonic::common::v1::any_value::Value;
use opentelemetry_proto::tonic::logs::v1::{LogRecord, SeverityNumber};

use super::super::log_record_for;
use crate::eta::Provenance;
use crate::telemetry::kinds::token_ranking_refresh::{
    AccountOutcome, CredentialKind, RankingSource, RoundOutcome, TokenRankingAccount,
    TokenRankingRefreshRecord, TOKEN_RANKING_LOG_ATTRIBUTE_KEYS,
};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

fn account(
    name: &str,
    outcome: AccountOutcome,
    kind: CredentialKind,
    probed: bool,
) -> TokenRankingAccount {
    TokenRankingAccount {
        account: name.to_string(),
        provider: "claude".to_string(),
        status: "available".to_string(),
        outcome,
        credential_kind: kind,
        probed,
    }
}

/// One account per outcome value, one of them an API-key probe.
fn record() -> TokenRankingRefreshRecord {
    TokenRankingRefreshRecord {
        round_id: "0123456789abcdef0123456789abcdef".to_string(),
        started_at: Utc.with_ymd_and_hms(2026, 10, 7, 13, 0, 0).unwrap(),
        workspace: "/home/ubuntu/GitHub/loom".to_string(),
        outcome: RoundOutcome::Failure,
        failure_class: Some("nonzero_exit".to_string()),
        source: RankingSource::Probe,
        probed_count: 4,
        api_key_probe_count: 1,
        accounts: vec![
            account("acct-ok", AccountOutcome::Ok, CredentialKind::Oauth, true),
            account("acct-key", AccountOutcome::RateLimited, CredentialKind::ApiKey, true),
            account("acct-dead", AccountOutcome::AuthDead, CredentialKind::Oauth, true),
            account("acct-fresh", AccountOutcome::SkippedFresh, CredentialKind::Oauth, false),
            account("acct-err", AccountOutcome::Error, CredentialKind::Oauth, true),
            account("acct-codex", AccountOutcome::Unsupported, CredentialKind::Unknown, false),
        ],
        duration_ms: 5300,
        loom: Provenance {
            version: "0.19.853".to_string(),
            revision: "2889d5ed50123456789abcdef0123456789abcde".to_string(),
            tree_state: "clean".to_string(),
            complete: true,
        },
    }
}

fn attr(log: &LogRecord, key: &str) -> Option<Value> {
    log.attributes
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| kv.value.as_ref())
        .and_then(|v| v.value.clone())
}

fn log_for(r: TokenRankingRefreshRecord) -> LogRecord {
    let envelope = TelemetryEnvelope::new("host", TelemetryRecord::TokenRankingRefresh(r));
    log_record_for(&envelope).unwrap()
}

#[test]
fn a_full_round_emits_every_key_and_only_allowlisted_ones() {
    let round = record();
    let log = log_for(round.clone());
    assert_eq!(log.event_name, "token_ranking.refresh");
    assert_eq!(log.time_unix_nano, super::nanos(round.started_at));
    for kv in &log.attributes {
        assert!(
            TOKEN_RANKING_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                || kv.key == "loom.record_id",
            "{} is not allowlisted",
            kv.key
        );
    }
    for key in TOKEN_RANKING_LOG_ATTRIBUTE_KEYS {
        assert!(attr(&log, key).is_some(), "{key} is emitted");
    }
    let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
        panic!("string body");
    };
    let parsed: TokenRankingRefreshRecord = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed, round);
}

#[test]
fn every_account_outcome_is_counted_once() {
    let log = log_for(record());
    for key in [
        "loom.token_ranking.ok_count",
        "loom.token_ranking.rate_limited_count",
        "loom.token_ranking.auth_dead_count",
        "loom.token_ranking.skipped_fresh_count",
        "loom.token_ranking.error_count",
        "loom.token_ranking.unsupported_count",
    ] {
        assert_eq!(attr(&log, key), Some(Value::IntValue(1)), "{key}");
    }
    assert_eq!(attr(&log, "loom.token_ranking.account_count"), Some(Value::IntValue(6)));
    assert_eq!(attr(&log, "loom.token_ranking.probed_count"), Some(Value::IntValue(4)));
    assert_eq!(attr(&log, "loom.token_ranking.api_key_probe_count"), Some(Value::IntValue(1)));
    assert_eq!(
        attr(&log, "loom.token_ranking.outcome"),
        Some(Value::StringValue("failure".into()))
    );
    assert_eq!(
        attr(&log, "loom.token_ranking.failure_class"),
        Some(Value::StringValue("nonzero_exit".into()))
    );
}

#[test]
fn outcome_and_credential_spellings_are_the_documented_wire_values() {
    let body = serde_json::to_value(record()).unwrap();
    let outcomes: Vec<&str> = body["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["outcome"].as_str().unwrap())
        .collect();
    assert_eq!(
        outcomes,
        [
            "ok",
            "rate_limited",
            "auth_dead",
            "skipped_fresh",
            "error",
            "unsupported"
        ]
    );
    assert_eq!(body["accounts"][0]["credential_kind"], "oauth");
    assert_eq!(body["accounts"][1]["credential_kind"], "api_key");
    for o in [
        AccountOutcome::Ok,
        AccountOutcome::RateLimited,
        AccountOutcome::AuthDead,
        AccountOutcome::SkippedFresh,
        AccountOutcome::Error,
        AccountOutcome::Unsupported,
    ] {
        assert_eq!(serde_json::to_value(o).unwrap(), o.as_str());
    }
}

#[test]
fn a_monitor_served_success_is_info_and_marks_skipped_fresh() {
    let mut round = record();
    round.outcome = RoundOutcome::Success;
    round.failure_class = None;
    round.source = RankingSource::Monitor;
    round.api_key_probe_count = 0;
    let log = log_for(round);
    assert_eq!(log.severity_number, SeverityNumber::Info as i32);
    assert_eq!(attr(&log, "loom.token_ranking.skipped_fresh"), Some(Value::BoolValue(true)));
    assert_eq!(attr(&log, "loom.token_ranking.failure_class"), None);
    assert_eq!(
        attr(&log, "loom.token_ranking.source"),
        Some(Value::StringValue("monitor".into()))
    );
}

#[test]
fn a_failure_or_an_api_key_probe_is_a_warning() {
    let mut round = record();
    round.outcome = RoundOutcome::Success;
    round.failure_class = None;
    assert_eq!(log_for(round.clone()).severity_number, SeverityNumber::Warn as i32);
    round.api_key_probe_count = 0;
    assert_eq!(log_for(round.clone()).severity_number, SeverityNumber::Info as i32);
    round.outcome = RoundOutcome::Failure;
    assert_eq!(log_for(round.clone()).severity_number, SeverityNumber::Warn as i32);
    round.outcome = RoundOutcome::Disabled;
    assert_eq!(log_for(round).severity_number, SeverityNumber::Info as i32);
}

#[test]
fn collector_keeps_every_token_ranking_log_attribute() {
    const CONFIG: &str =
        include_str!("../../../../../defaults/observability/collector/config.yaml");
    let log_keep = CONFIG
        .lines()
        .find(|l| {
            l.contains("keep_keys(attributes, [")
                && l.contains("loom.ci.chunk_index")
                && l.contains("loom.eta.estimate_id")
        })
        .expect("the transform/privacy log keep_keys line");
    for key in TOKEN_RANKING_LOG_ATTRIBUTE_KEYS {
        assert!(log_keep.contains(&format!("\"{key}\"")), "collector drops {key}");
    }
}
