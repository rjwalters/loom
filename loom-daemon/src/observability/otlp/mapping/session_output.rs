//! OTLP mapping for `session.output` (#9764).
//!
//! One log record per source event. The **body** is the record's
//! already-redacted readable text for a content record, and the category name
//! for a status record — never the record's JSON, because the whole point of
//! the kind is that a consumer can read the body directly.
//!
//! Two timestamps are carried, deliberately unmerged:
//!
//! - `time_unix_nano` = the **source** event time. A backfilled or late
//!   record therefore sorts where it happened, not where it arrived.
//! - `observed_time_unix_nano` = when this producer **read** it. The delta is
//!   also exported as `loom.session.output.producer_lag_ms` so the
//!   source-to-queryable measurement can be split into its producer and
//!   pipeline halves without joining two rows.
//!
//! Ordering and de-duplication key on `loom.session.output.stream_id` +
//! `loom.session.output.sequence`; `loom.session.output.event_id` is the pure
//! function of the two that survives a producer restart unchanged.

use opentelemetry_proto::tonic::common::v1::{any_value, AnyValue, KeyValue};
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv, kv_int, kv_string, nanos};
use crate::telemetry::kinds::session_output::{Coverage, OutputCategory, SessionOutputRecord};
use crate::telemetry::TelemetryRecord;

fn kv_bool(key: &str, value: bool) -> KeyValue {
    kv(
        key,
        AnyValue {
            value: Some(any_value::Value::BoolValue(value)),
        },
    )
}

/// A record a consumer must not present as healthy live output is raised above
/// `Info` so an alert can key on severity alone: a lost-events gap is an
/// `Error`, and a coverage statement that is anything other than "live" (an
/// unsupported runtime, a source we cannot read) is a `Warn`.
fn severity(record: &SessionOutputRecord) -> SeverityNumber {
    match record.category {
        OutputCategory::Gap => SeverityNumber::Error,
        OutputCategory::Coverage if record.coverage != Coverage::Live => SeverityNumber::Warn,
        _ => SeverityNumber::Info,
    }
}

/// Everything [`log_record_for`](super::log_record_for) needs to build one
/// `session.output` log record.
///
/// A named struct rather than a tuple: `source_at` and `observed_at` are
/// adjacent `u64` nanosecond stamps, and transposing them at the call site
/// would compile cleanly while silently inverting every consumer's latency
/// measurement. (It also keeps `clippy::type_complexity` happy, but that is the
/// lesser reason.)
pub(super) struct LogParts {
    pub event_name: &'static str,
    pub severity: SeverityNumber,
    /// Source event time, nanoseconds — becomes `time_unix_nano`.
    pub source_at: u64,
    /// Producer read time, nanoseconds — becomes `observed_time_unix_nano`.
    pub observed_at: u64,
    pub attributes: Vec<KeyValue>,
    pub body: String,
}

/// The [`LogParts`] for a `session.output` record; `None` for every other kind.
pub(super) fn log_parts(record: &TelemetryRecord) -> Option<LogParts> {
    let TelemetryRecord::SessionOutput(r) = record else {
        return None;
    };
    let mut attributes = vec![
        kv_int("loom.session.output.schema", i64::from(r.schema)),
        kv_string("loom.session.output.category", r.category.as_str()),
        kv_string("loom.session.output.stream", r.stream.as_str()),
        kv_string("loom.session.output.stream_id", r.stream_id.clone()),
        // i64: a sequence cannot realistically exceed i64::MAX, and a
        // saturating cast keeps a corrupt value from wrapping negative.
        kv_int("loom.session.output.sequence", i64::try_from(r.sequence).unwrap_or(i64::MAX)),
        kv_string("loom.session.output.event_id", r.event_id.clone()),
        kv_string("loom.session.output.coverage", r.coverage.as_str()),
        kv_string("loom.session.output.state", r.state.as_str()),
        kv_string("loom.session.output.redaction", r.redaction.clone()),
        kv_int("loom.session.output.producer_lag_ms", r.producer_lag_ms()),
        kv_string("loom.runtime", r.identity.runtime.clone()),
    ];
    // Identity: emitted only when genuinely known. An unattributed
    // interactive session omits `loom.repo` / `loom.issue` entirely rather
    // than exporting a placeholder a dashboard would then filter on.
    if let Some(repo) = &r.identity.repo {
        attributes.push(kv_string("loom.repo", repo.clone()));
        // Only meaningful alongside a repo, and only ever the fail-closed
        // `private` here — this producer makes no forge call to ask.
        attributes.push(kv_string(
            "loom.repo.visibility",
            match r.identity.visibility {
                crate::telemetry::RepoVisibility::Public => "public",
                crate::telemetry::RepoVisibility::Private => "private",
            },
        ));
    }
    if let Some(issue) = r.identity.issue {
        attributes.push(kv_int("loom.issue", i64::from(issue)));
    }
    // Why `loom.issue` is absent, when it is: `interactive` means deliberately
    // unscoped, a `sweep`/`role` row without an issue means attribution was
    // lost. A null `loom.issue` alone cannot distinguish those.
    if let Some(kind) = r.identity.session_kind {
        attributes.push(kv_string("loom.session_kind", kind.as_str()));
    }
    if let Some(sweep_id) = &r.identity.sweep_id {
        attributes.push(kv_string("loom.sweep_id", sweep_id.clone()));
    }
    if let Some(session_id) = &r.identity.session_id {
        attributes.push(kv_string("loom.session_id", session_id.clone()));
    }
    if let Some(attempt) = r.identity.attempt {
        attributes.push(kv_int("loom.attempt", i64::from(attempt)));
    }
    if let Some(role) = &r.identity.role {
        attributes.push(kv_string("loom.role", role.clone()));
    }
    if let Some(tool) = &r.tool {
        attributes.push(kv_string("loom.session.output.tool", tool.clone()));
    }
    if let Some(ok) = r.tool_ok {
        attributes.push(kv_bool("loom.session.output.tool_ok", ok));
    }
    if r.truncated_bytes > 0 {
        attributes.push(kv_int(
            "loom.session.output.truncated_bytes",
            i64::try_from(r.truncated_bytes).unwrap_or(i64::MAX),
        ));
    }
    if r.dropped_events > 0 {
        attributes.push(kv_int(
            "loom.session.output.dropped_events",
            i64::try_from(r.dropped_events).unwrap_or(i64::MAX),
        ));
    }
    if let Some(reason) = &r.gap_reason {
        attributes.push(kv_string("loom.session.output.gap_reason", reason.clone()));
    }
    // Producer-lag percentiles, on status records only. Absent — rather than
    // zero — when the run has observed no fresh source event: a zero would
    // assert a latency nothing measured. `lag_historical_excluded` rides along
    // so a consumer can see that backlog samples were kept out of the
    // distribution instead of reading the gap as missing data (#9764).
    if let Some(lag) = &r.lag {
        attributes.push(kv_int(
            "loom.session.output.lag_samples",
            i64::try_from(lag.samples).unwrap_or(i64::MAX),
        ));
        attributes.push(kv_int("loom.session.output.lag_p50_ms", lag.p50_ms));
        attributes.push(kv_int("loom.session.output.lag_p95_ms", lag.p95_ms));
        attributes.push(kv_int("loom.session.output.lag_max_ms", lag.max_ms));
        attributes.push(kv_int(
            "loom.session.output.lag_historical_excluded",
            i64::try_from(lag.historical_excluded).unwrap_or(i64::MAX),
        ));
    }
    let body = r
        .text
        .clone()
        .unwrap_or_else(|| r.category.as_str().to_string());
    Some(LogParts {
        event_name: "session.output",
        severity: severity(r),
        source_at: nanos(r.source_at),
        observed_at: nanos(r.observed_at),
        attributes,
        body,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::log_record_for;
    use crate::telemetry::kinds::session_output::{
        Coverage, LagStats, OutputCategory, RunIdentity, RunState, SessionOutputRecord,
        SESSION_OUTPUT_LOG_ATTRIBUTE_KEYS,
    };
    use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};
    use chrono::{TimeZone, Utc};
    use opentelemetry_proto::tonic::common::v1::any_value::Value;

    fn identity(issue: u32, attempt: u32, session: &str) -> RunIdentity {
        RunIdentity {
            repo: Some("rjwalters/loom".to_string()),
            visibility: crate::telemetry::RepoVisibility::Private,
            session_kind: Some(crate::telemetry::SessionKind::Sweep),
            issue: Some(issue),
            sweep_id: Some(format!("sweep-issue-{issue}-{attempt}")),
            session_id: Some(session.to_string()),
            attempt: Some(attempt),
            runtime: "claude".to_string(),
            role: Some("builder".to_string()),
        }
    }

    fn at(second: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, second).unwrap()
    }

    fn log(record: SessionOutputRecord) -> opentelemetry_proto::tonic::logs::v1::LogRecord {
        log_record_for(&TelemetryEnvelope::new("host", TelemetryRecord::SessionOutput(record)))
            .unwrap()
    }

    fn attr(log: &opentelemetry_proto::tonic::logs::v1::LogRecord, key: &str) -> Option<Value> {
        log.attributes
            .iter()
            .find(|kv| kv.key == key)
            .and_then(|kv| kv.value.as_ref())
            .and_then(|v| v.value.clone())
    }

    #[test]
    fn the_body_is_the_redacted_text_and_both_times_survive() {
        let record = SessionOutputRecord::new_output(
            identity(9764, 1, "0f8b"),
            "0f8b",
            5,
            at(0),
            at(2),
            "running cargo check",
        );
        let log = log(record);
        assert_eq!(log.event_name, "session.output");
        let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
            panic!("string body");
        };
        assert_eq!(body, "running cargo check");
        assert_eq!(log.time_unix_nano, super::nanos(at(0)));
        assert_eq!(log.observed_time_unix_nano, super::nanos(at(2)));
        assert_eq!(attr(&log, "loom.session.output.producer_lag_ms"), Some(Value::IntValue(2_000)));
    }

    #[test]
    fn a_secret_in_source_text_never_reaches_the_body() {
        let log = log(SessionOutputRecord::new_output(
            identity(9764, 1, "0f8b"),
            "0f8b",
            0,
            at(0),
            at(0),
            "export GH_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123",
        ));
        let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
            panic!("string body");
        };
        assert!(!body.contains("ghp_"), "{body}");
    }

    #[test]
    fn two_concurrent_issues_and_two_attempts_are_separable_by_attribute() {
        let a = log(SessionOutputRecord::new_output(
            identity(9764, 1, "aaaa"),
            "aaaa",
            0,
            at(0),
            at(0),
            "x",
        ));
        let b = log(SessionOutputRecord::new_output(
            identity(9765, 1, "bbbb"),
            "bbbb",
            0,
            at(0),
            at(0),
            "x",
        ));
        let retry = log(SessionOutputRecord::new_output(
            identity(9764, 2, "cccc"),
            "cccc",
            0,
            at(0),
            at(0),
            "x",
        ));
        assert_ne!(attr(&a, "loom.issue"), attr(&b, "loom.issue"));
        assert_eq!(attr(&a, "loom.issue"), attr(&retry, "loom.issue"));
        assert_ne!(attr(&a, "loom.attempt"), attr(&retry, "loom.attempt"));
        assert_ne!(attr(&a, "loom.sweep_id"), attr(&retry, "loom.sweep_id"));
        assert_ne!(
            attr(&a, "loom.session.output.event_id"),
            attr(&retry, "loom.session.output.event_id")
        );
        // The repo is the canonical forge slug on every one of them, never a
        // worktree directory name.
        for record in [&a, &b, &retry] {
            assert_eq!(
                attr(record, "loom.repo"),
                Some(Value::StringValue("rjwalters/loom".to_string()))
            );
        }
    }

    #[test]
    fn an_unscoped_session_exports_no_repo_or_issue_attribute() {
        let log = log(SessionOutputRecord::new_output(
            RunIdentity {
                runtime: "claude".to_string(),
                ..RunIdentity::default()
            },
            "solo",
            0,
            at(0),
            at(0),
            "hello",
        ));
        assert_eq!(attr(&log, "loom.repo"), None);
        assert_eq!(attr(&log, "loom.issue"), None);
        assert_eq!(attr(&log, "loom.runtime"), Some(Value::StringValue("claude".to_string())));
    }

    #[test]
    fn a_gap_is_an_error_and_an_unsupported_runtime_is_a_warning() {
        let gap = log(SessionOutputRecord::status(
            identity(9764, 1, "0f8b"),
            OutputCategory::Gap,
            "0f8b",
            9,
            at(0),
            Coverage::Degraded,
            RunState::Running,
        )
        .with_gap("queue_overflow", 7));
        assert_eq!(
            gap.severity_number,
            opentelemetry_proto::tonic::logs::v1::SeverityNumber::Error as i32
        );
        assert_eq!(
            attr(&gap, "loom.session.output.gap_reason"),
            Some(Value::StringValue("queue_overflow".to_string()))
        );
        assert_eq!(attr(&gap, "loom.session.output.dropped_events"), Some(Value::IntValue(7)));

        let mut unsupported = identity(9764, 1, "0f8b");
        unsupported.runtime = "codex".to_string();
        let coverage = log(SessionOutputRecord::status(
            unsupported,
            OutputCategory::Coverage,
            "sweep-issue-9764-1",
            0,
            at(0),
            Coverage::Unsupported,
            RunState::Running,
        ));
        assert_eq!(
            coverage.severity_number,
            opentelemetry_proto::tonic::logs::v1::SeverityNumber::Warn as i32
        );
        assert_eq!(
            attr(&coverage, "loom.session.output.coverage"),
            Some(Value::StringValue("unsupported".to_string()))
        );

        let live = log(SessionOutputRecord::status(
            identity(9764, 1, "0f8b"),
            OutputCategory::Heartbeat,
            "0f8b",
            1,
            at(0),
            Coverage::Live,
            RunState::Idle,
        ));
        assert_eq!(
            live.severity_number,
            opentelemetry_proto::tonic::logs::v1::SeverityNumber::Info as i32
        );
    }

    #[test]
    fn a_tool_record_exports_its_name_and_outcome_and_no_body_text() {
        let log = log(SessionOutputRecord::tool(
            identity(9764, 1, "0f8b"),
            "0f8b",
            3,
            at(0),
            at(0),
            "Bash",
            Some(true),
        ));
        assert_eq!(
            attr(&log, "loom.session.output.tool"),
            Some(Value::StringValue("Bash".to_string()))
        );
        assert_eq!(attr(&log, "loom.session.output.tool_ok"), Some(Value::BoolValue(true)));
        let Some(Value::StringValue(body)) = log.body.as_ref().and_then(|b| b.value.clone()) else {
            panic!("string body");
        };
        assert_eq!(body, "tool_finish", "a tool record's body is its category");
    }

    #[test]
    fn lag_percentiles_ride_on_status_records_and_are_absent_when_unmeasured() {
        let measured = log(SessionOutputRecord::status(
            identity(9764, 1, "0f8b"),
            OutputCategory::Heartbeat,
            "0f8b",
            1,
            at(0),
            Coverage::Live,
            RunState::Idle,
        )
        .with_lag(Some(LagStats {
            samples: 10,
            p50_ms: 150,
            p95_ms: 1_800,
            max_ms: 2_400,
            historical_excluded: 3,
        })));
        assert_eq!(attr(&measured, "loom.session.output.lag_p50_ms"), Some(Value::IntValue(150)));
        assert_eq!(attr(&measured, "loom.session.output.lag_p95_ms"), Some(Value::IntValue(1_800)));
        assert_eq!(attr(&measured, "loom.session.output.lag_max_ms"), Some(Value::IntValue(2_400)));
        assert_eq!(attr(&measured, "loom.session.output.lag_samples"), Some(Value::IntValue(10)));
        // The exclusion count is exported even though it is not a latency, so
        // a reader can tell backlog samples were withheld from the percentiles
        // rather than that no samples existed.
        assert_eq!(
            attr(&measured, "loom.session.output.lag_historical_excluded"),
            Some(Value::IntValue(3))
        );

        // An unmeasured run omits the keys entirely. A zero p95 would claim a
        // latency the producer never observed.
        let unmeasured = log(SessionOutputRecord::status(
            identity(9764, 1, "0f8b"),
            OutputCategory::Coverage,
            "0f8b",
            0,
            at(0),
            Coverage::Degraded,
            RunState::Running,
        ));
        assert_eq!(attr(&unmeasured, "loom.session.output.lag_p95_ms"), None);
        assert_eq!(attr(&unmeasured, "loom.session.output.lag_samples"), None);
    }

    #[test]
    fn a_content_record_never_carries_a_window_summary() {
        // One source event reports its own lag, not the run's distribution:
        // `with_lag` is a no-op off the status stream, so a per-row join
        // against the percentiles can never double-count.
        let content = SessionOutputRecord::new_output(
            identity(9764, 1, "0f8b"),
            "0f8b",
            0,
            at(0),
            at(1),
            "hello",
        )
        .with_lag(Some(LagStats {
            samples: 9,
            p50_ms: 1,
            p95_ms: 2,
            max_ms: 3,
            historical_excluded: 0,
        }));
        assert_eq!(content.lag, None, "a content record took a window summary");
        let log = log(content);
        assert_eq!(attr(&log, "loom.session.output.lag_p95_ms"), None);
        // Its own single-event lag is still there.
        assert_eq!(attr(&log, "loom.session.output.producer_lag_ms"), Some(Value::IntValue(1_000)));
    }

    #[test]
    fn visibility_rides_with_a_repo_and_never_claims_public() {
        let scoped = log(SessionOutputRecord::new_output(
            identity(9764, 1, "0f8b"),
            "0f8b",
            0,
            at(0),
            at(0),
            "x",
        ));
        assert_eq!(
            attr(&scoped, "loom.repo.visibility"),
            Some(Value::StringValue("private".to_string())),
            "this producer makes no forge call, so it must never assert public"
        );

        // No repo, no visibility claim: tagging the visibility of a repo we
        // could not identify would be meaningless.
        let unscoped = log(SessionOutputRecord::new_output(
            RunIdentity {
                runtime: "claude".to_string(),
                ..RunIdentity::default()
            },
            "solo",
            0,
            at(0),
            at(0),
            "x",
        ));
        assert_eq!(attr(&unscoped, "loom.repo"), None);
        assert_eq!(attr(&unscoped, "loom.repo.visibility"), None);
    }

    #[test]
    fn session_kind_distinguishes_a_deliberately_unscoped_session_from_a_lost_one() {
        let interactive = log(SessionOutputRecord::new_output(
            RunIdentity {
                runtime: "claude".to_string(),
                session_kind: Some(crate::telemetry::SessionKind::Interactive),
                ..RunIdentity::default()
            },
            "solo",
            0,
            at(0),
            at(0),
            "x",
        ));
        assert_eq!(attr(&interactive, "loom.issue"), None);
        assert_eq!(
            attr(&interactive, "loom.session_kind"),
            Some(Value::StringValue("interactive".to_string()))
        );

        // Same absent issue, different reason — and the wire says which.
        let lost = log(SessionOutputRecord::new_output(
            RunIdentity {
                runtime: "claude".to_string(),
                session_kind: Some(crate::telemetry::SessionKind::Sweep),
                ..RunIdentity::default()
            },
            "solo",
            0,
            at(0),
            at(0),
            "x",
        ));
        assert_eq!(attr(&lost, "loom.issue"), None);
        assert_eq!(attr(&lost, "loom.session_kind"), Some(Value::StringValue("sweep".to_string())));
    }

    #[test]
    fn every_exported_attribute_is_allowlisted() {
        let records = [
            SessionOutputRecord::new_output(
                identity(9764, 1, "0f8b"),
                "0f8b",
                0,
                at(0),
                at(1),
                &"x".repeat(5_000),
            ),
            SessionOutputRecord::tool(
                identity(9764, 1, "0f8b"),
                "0f8b",
                1,
                at(0),
                at(0),
                "Bash",
                Some(false),
            ),
            SessionOutputRecord::status(
                identity(9764, 1, "0f8b"),
                OutputCategory::Gap,
                "0f8b",
                2,
                at(0),
                Coverage::Degraded,
                RunState::Running,
            )
            .with_gap("producer_restart", 3),
            SessionOutputRecord::status(
                identity(9764, 1, "0f8b"),
                OutputCategory::Heartbeat,
                "0f8b",
                3,
                at(0),
                Coverage::Live,
                RunState::Idle,
            )
            .with_lag(Some(LagStats {
                samples: 4,
                p50_ms: 120,
                p95_ms: 480,
                max_ms: 900,
                historical_excluded: 2,
            })),
        ];
        let shared = [
            "loom.repo",
            "loom.repo.visibility",
            "loom.session_kind",
            "loom.issue",
            "loom.sweep_id",
            "loom.session_id",
            "loom.attempt",
            "loom.runtime",
            "loom.role",
        ];
        let mut seen = std::collections::BTreeSet::new();
        for record in records {
            let log = log(record);
            for kv in &log.attributes {
                assert!(
                    SESSION_OUTPUT_LOG_ATTRIBUTE_KEYS.contains(&kv.key.as_str())
                        || shared.contains(&kv.key.as_str()),
                    "{} is not allowlisted",
                    kv.key
                );
                seen.insert(kv.key.clone());
            }
        }
        // Every declared key must actually be reachable — a key in the
        // allowlist that nothing emits is drift the collector contract test
        // would not catch.
        for key in SESSION_OUTPUT_LOG_ATTRIBUTE_KEYS {
            assert!(seen.contains(*key), "{key} is declared but never emitted");
        }
    }
}
