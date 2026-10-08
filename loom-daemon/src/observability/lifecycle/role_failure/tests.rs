//! #10640: a failed `loom.role_attempt` says why — class, exit code and a
//! one-line status message — and no longer repeats `failure` as its
//! admission reason.
#![allow(clippy::unwrap_used)]

use super::super::{admission_attributes, attributes, Journal, SpanName, SpanStatus};
use super::*;
use crate::telemetry::trace::TraceContext;
use chrono::Utc;

fn failure() -> RoleFailure {
    RoleFailure::new("exit-1", Some(1), "role child exited with code 1")
}

#[test]
fn a_failure_reports_its_note_or_the_explicit_fallback() {
    let outcome = RoleTickOutcome::Failure("free-form".into());
    assert_eq!(for_outcome(&outcome, Some(failure())), Some(failure()));
    let fallback = for_outcome(&outcome, None).unwrap();
    assert_eq!(fallback.class(), UNCLASSIFIED);
    assert_eq!(fallback.class(), "unclassified:after-launch");
    assert_eq!(fallback.exit_code(), None);
    assert!(!fallback.message().is_empty());
}

#[test]
fn only_a_failure_reports_one() {
    for outcome in [
        RoleTickOutcome::Success,
        RoleTickOutcome::QueueEmpty,
        RoleTickOutcome::NoTokenPool,
        RoleTickOutcome::LoadSkipped {
            load_per_core: 4.2,
            detail: "deferred".into(),
        },
    ] {
        assert_eq!(for_outcome(&outcome, Some(failure())), None, "{outcome:?}");
    }
}

#[test]
fn the_message_is_one_bounded_line() {
    let failure = RoleFailure::new("x", None, &format!("  a\nb\tc{}  ", "é".repeat(300)));
    assert!(failure.message().starts_with("a b c"));
    assert!(failure.message().len() <= 256);
    assert!(!failure.message().chars().any(char::is_control));
}

#[test]
fn the_attributes_carry_the_class_the_code_and_the_message() {
    let attrs = failure().attributes();
    assert_eq!(attrs[FAILURE_CLASS], "exit-1");
    assert_eq!(attrs[EXIT_CODE], "1");
    assert_eq!(attrs[STATUS_MESSAGE], "role child exited with code 1");
    let no_code = RoleFailure::new("timeout-ceiling", None, "ran past the timeout").attributes();
    assert!(!no_code.contains_key(EXIT_CODE), "no exit code is absent, never 0");
}

/// The pre-#10640 `failure` literal only repeated `loom.result`; the other
/// reasons, which do say something, are unchanged.
#[test]
fn a_failed_attempt_no_longer_repeats_failure_as_its_admission_reason() {
    let failed = admission_attributes(&RoleTickOutcome::Failure("exit 1".into()));
    assert!(!failed.contains_key("loom.admission.reason"), "{failed:?}");
    let deferred = admission_attributes(&RoleTickOutcome::LoadSkipped {
        load_per_core: 4.2,
        detail: "deferred".into(),
    });
    assert_eq!(deferred["loom.admission.reason"], "load-ceiling");
}

#[test]
fn failure_attributes_survive_the_span_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    let active = journal
        .start(
            root.child(),
            Some(&root),
            SpanName::RoleAttempt,
            Utc::now(),
            attributes(&[("loom.role", "judge")]),
        )
        .unwrap();
    journal
        .finish(&active, Utc::now(), SpanStatus::Error, failure().attributes())
        .unwrap();
    let mut spans = Vec::new();
    journal
        .drain(|record| {
            spans.push(record);
            Ok(())
        })
        .unwrap();
    assert_eq!(spans.len(), 1);
    for (key, value) in failure().attributes() {
        assert_eq!(spans[0].attributes.get(&key), Some(&value), "allowlist dropped {key}");
    }
}

const COLLECTOR_CONFIG: &str =
    include_str!("../../../../../defaults/observability/collector/config.yaml");

/// The gateway Collector's span `keep_keys` (the same parse the ops contract
/// test uses).
fn span_keep_keys() -> std::collections::BTreeSet<String> {
    let mut current = String::new();
    let mut keys = std::collections::BTreeSet::new();
    for line in COLLECTOR_CONFIG.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- context:") {
            current = rest.trim().to_string();
        }
        if current == "span" && trimmed.contains("keep_keys(") {
            keys.extend(trimmed.split('"').skip(1).step_by(2).map(str::to_string));
        }
    }
    keys
}

#[test]
fn gateway_collector_keeps_every_role_failure_attribute() {
    let keep = span_keep_keys();
    for key in ROLE_FAILURE_ATTRIBUTE_KEYS {
        assert!(keep.contains(*key), "collector span keep_keys lacks {key}");
    }
}

#[cfg(feature = "otlp")]
mod role_invocation_close {
    use super::super::super::{
        role_child_exited, role_child_spawned, role_command, role_invocation, RoleTrace,
    };
    use super::*;
    use crate::telemetry::trace::{store::TraceStore, SpanRecord};
    use std::path::Path;
    use std::process::Command;

    fn traced_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}"#,
        )
        .unwrap();
        dir
    }

    /// Stand-in for `role_runner::launch`: open the span at the launch and
    /// spawn a real child.
    fn launch(result: &str) {
        let mut command = Command::new("true");
        role_command(&mut command);
        let mut child = command.spawn().unwrap();
        role_child_spawned(child.id());
        child.wait().unwrap();
        role_child_exited(result);
    }

    fn root_span(root: &Path, trace: &RoleTrace) -> SpanRecord {
        let store = TraceStore::new(root);
        let journal = Journal::for_context(&store.path(root, &trace.execution));
        let mut spans = Vec::new();
        journal
            .drain(|s| {
                spans.push(s);
                Ok(())
            })
            .unwrap();
        spans
            .into_iter()
            .find(|s| s.context == trace.context)
            .unwrap()
    }

    #[test]
    #[serial_test::serial] // `loom.repo` resolution reads the process-global `LOOM_REPO`
    fn a_launched_tick_that_failed_closes_with_its_noted_cause() {
        let dir = traced_root();
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            launch("failure");
            note_role_failure(failure());
            RoleTickOutcome::Failure("`spawn-codex.sh` exited with exit status: 1: tail".into())
        });
        let trace = trace.unwrap();
        assert_eq!(trace.failure, Some(failure()), "the story copies get the same cause");
        let span = root_span(dir.path(), &trace);
        assert_eq!(span.status, SpanStatus::Error);
        assert_eq!(span.attributes["loom.result"], "failure");
        assert_eq!(span.attributes[FAILURE_CLASS], "exit-1");
        assert_eq!(span.attributes[EXIT_CODE], "1");
        assert_eq!(span.attributes[STATUS_MESSAGE], "role child exited with code 1");
        assert!(!span.attributes.contains_key("loom.admission.reason"));
        assert!(
            span.attributes.values().all(|v| !v.contains("spawn-codex")),
            "the outcome's free-form text stays off the span"
        );
    }

    #[test]
    #[serial_test::serial]
    fn an_unnoted_failure_closes_as_explicitly_unclassified() {
        let dir = traced_root();
        let (_, trace) = role_invocation(dir.path(), "champion", || {
            launch("failure");
            RoleTickOutcome::Failure("boom".into())
        });
        let span = root_span(dir.path(), &trace.unwrap());
        assert_eq!(span.attributes[FAILURE_CLASS], UNCLASSIFIED);
        assert!(!span.attributes.contains_key(EXIT_CODE));
    }

    /// A note left on the thread by an earlier tick never describes this one.
    #[test]
    #[serial_test::serial]
    fn a_stale_note_never_reaches_a_later_tick() {
        let dir = traced_root();
        note_role_failure(failure());
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            launch("failure");
            RoleTickOutcome::Failure("boom".into())
        });
        let trace = trace.unwrap();
        assert_eq!(trace.failure.as_ref().map(RoleFailure::class), Some(UNCLASSIFIED));
        note_role_failure(failure());
        let (_, trace) = role_invocation(dir.path(), "judge", || {
            launch("success");
            RoleTickOutcome::Success
        });
        let trace = trace.unwrap();
        assert_eq!(trace.failure, None);
        let span = root_span(dir.path(), &trace);
        assert!(!span.attributes.contains_key(FAILURE_CLASS));
        assert!(!span.attributes.contains_key(STATUS_MESSAGE));
    }
}
