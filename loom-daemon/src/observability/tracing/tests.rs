#![allow(clippy::unwrap_used)]
use super::*;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use crate::telemetry::{TelemetryEnvelope, TelemetryRecord};

#[test]
fn legacy_queue_envelope_stays_compatible_and_native_context_is_stripped() {
    let mut old = TelemetryEnvelope::new(
        "host",
        TelemetryRecord::SweepStarted(crate::telemetry::SweepStartedRecord {
            repo: "test/fixture".into(),
            visibility: crate::telemetry::RepoVisibility::Private,
            issue: 18,
            sweep_id: "fixture".into(),
            started_at: chrono::Utc::now(),
            model: None,
            effort: None,
            runtime: None,
        }),
    );
    let bytes = serde_json::to_vec(&old).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("trace_context"));
    let parsed: TelemetryEnvelope = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(parsed.schema_version, 2);
    assert!(parsed.trace_context.is_none());
    old.trace_context = Some(TraceContext::root(true));
    let now = chrono::Utc::now();
    let trace = TelemetryEnvelope::new(
        "host",
        TelemetryRecord::Span(SpanRecord {
            context: TraceContext::root(true),
            parent_span_id: None,
            name: SpanName::Sweep,
            started_at: now,
            ended_at: now,
            status: SpanStatus::Ok,
            attributes: TraceAttributes::new(),
            events: vec![],
            links: vec![],
        }),
    );
    let filtered = native_envelopes(&[trace, old]);
    assert_eq!(filtered, vec![parsed]);
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn actual_propagation_hook_handles_enabled_disabled_and_invalid_configuration() {
    // Match observability's global serial lock: process environment is global.
    let keys = [
        super::super::ENABLED_ENV,
        super::super::ENDPOINT_ENV,
        super::super::EXPORTER_ENV,
    ];
    let previous: Vec<_> = keys.iter().map(std::env::var_os).collect();
    struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Drop for Restore {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                match value {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
    let _restore = Restore(keys.into_iter().zip(previous).collect());
    for (enabled, endpoint, expect_context) in [
        ("true", "http://127.0.0.1:4318", cfg!(feature = "otlp")),
        ("false", "http://127.0.0.1:4318", false),
        ("true", "not a URL", false),
        ("true", "https://example.com", false),
        ("true", "http://user:secret@localhost:4318", false),
    ] {
        std::env::set_var(super::super::ENABLED_ENV, enabled);
        std::env::set_var(super::super::ENDPOINT_ENV, endpoint);
        std::env::set_var(super::super::EXPORTER_ENV, "otlp");
        let dir = tempfile::tempdir().unwrap();
        let mut command = Command::new("/usr/bin/printenv");
        command
            .arg(TRACEPARENT_ENV)
            .env_clear()
            .env(TRACEPARENT_ENV, "stale-context")
            .env(CONTEXT_FILE_ENV, "stale-file");
        prepare_child(&mut command, dir.path(), "child-boundary");
        let output = command.output().unwrap();
        let store = TraceStore::new(dir.path());
        if expect_context {
            assert!(output.status.success());
            let child_context =
                TraceContext::parse(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
            assert_eq!(
                child_context,
                TraceStore::load(&store.path(dir.path(), "child-boundary"))
                    .unwrap()
                    .context
            );
        } else {
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert!(!dir.path().join(".loom/logs/trace-context").exists());
            // With env_clear(), env_remove() may erase an explicit entry
            // instead of retaining a None tombstone. Verify the child boundary.
            let output = command.arg(CONTEXT_FILE_ENV).output().unwrap();
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
        }
    }
}
