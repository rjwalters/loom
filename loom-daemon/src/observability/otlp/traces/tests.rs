#![allow(clippy::unwrap_used)]
use super::*;
use crate::telemetry::trace::{SpanName, SpanRecord, TraceAttributes, TraceContext};
use chrono::Utc;

fn span(context: TraceContext) -> SpanRecord {
    let now = Utc::now();
    SpanRecord {
        context,
        parent_span_id: None,
        name: SpanName::Sweep,
        started_at: now,
        ended_at: now,
        status: SpanStatus::Ok,
        attributes: TraceAttributes::new(),
        events: vec![],
        links: vec![],
    }
}

#[test]
fn trace_wire_has_hex_ids_and_preserves_parent_and_log_relationship() {
    let root = TraceContext::root(true);
    let child = root.child();
    let mut child_span = span(child.clone());
    child_span.name = SpanName::RoleAttempt;
    child_span.parent_span_id = Some(root.span_id.clone());
    let request = build_traces_request(&[
        TelemetryEnvelope::new("host", TelemetryRecord::Span(span(root.clone()))),
        TelemetryEnvelope::new("host", TelemetryRecord::Span(child_span)),
    ])
    .unwrap();
    let json = serde_json::to_value(&request).unwrap();
    let spans = &json["resourceSpans"][0]["scopeSpans"][0]["spans"];
    assert_eq!(spans[0]["traceId"], root.trace_id.as_str());
    assert_eq!(spans[1]["parentSpanId"], root.span_id.as_str());
    assert_eq!(spans[1]["spanId"], child.span_id.as_str());
    let mut log = TelemetryEnvelope::new(
        "host",
        TelemetryRecord::SweepStarted(crate::telemetry::SweepStartedRecord {
            story_points: None,
            repo: "test/fixture".into(),
            visibility: crate::telemetry::RepoVisibility::Private,
            issue: 18,
            sweep_id: "fixture".into(),
            started_at: Utc::now(),
            facts: Default::default(),
        }),
    );
    log.trace_context = Some(child.clone());
    let logs = super::super::mapping::build_logs_request(&[log]).unwrap();
    let log = &logs.resource_logs[0].scope_logs[0].log_records[0];
    assert_eq!(log.trace_id, root.trace_id.bytes());
    assert_eq!(log.span_id, child.span_id.bytes());
}

#[test]
fn unsampled_and_invalid_spans_are_not_fabricated() {
    let unsampled =
        TelemetryEnvelope::new("host", TelemetryRecord::Span(span(TraceContext::root(false))));
    assert!(build_traces_request(&[unsampled]).is_none());
    let mut invalid = span(TraceContext::root(true));
    invalid.parent_span_id = Some(invalid.context.span_id.clone());
    assert!(build_traces_request(&[TelemetryEnvelope::new(
        "host",
        TelemetryRecord::Span(invalid)
    )])
    .is_none());
}

#[test]
fn unrepresentable_timestamps_are_dropped_instead_of_rewritten_to_epoch() {
    for timestamp in ["1969-12-31T23:59:59Z", "9999-01-01T00:00:00Z"] {
        let mut invalid = span(TraceContext::root(true));
        invalid.started_at = timestamp.parse().unwrap();
        invalid.ended_at = invalid.started_at;
        assert!(invalid.validate().is_err());
        assert!(build_traces_request(&[TelemetryEnvelope::new(
            "host",
            TelemetryRecord::Span(invalid)
        )])
        .is_none());
    }
}

#[test]
fn traces_resource_carries_the_build_service_version() {
    let request = build_traces_request(&[TelemetryEnvelope::new(
        "host",
        TelemetryRecord::Span(span(TraceContext::root(true))),
    )])
    .unwrap();
    let json = serde_json::to_value(&request).unwrap();
    let attributes = json["resourceSpans"][0]["resource"]["attributes"]
        .as_array()
        .unwrap();
    let version = attributes
        .iter()
        .find(|kv| kv["key"] == "service.version")
        .map(|kv| kv["value"]["stringValue"].clone());
    assert_eq!(version, Some(serde_json::json!(env!("CARGO_PKG_VERSION"))));
}

#[test]
fn ratification_counts_reconcile_on_the_otlp_wire_without_exporting_detail() {
    use crate::role_runner::ratification_lane::{
        tick_span, Key, KeyPost, TickOutcome, WithheldClass,
    };
    use prost::Message;

    let results = [
        vec![(
            731,
            vec![
                (
                    Key::Ee,
                    KeyPost::Withheld(
                        WithheldClass::VerdictInvalid,
                        "fixture private review".into(),
                    ),
                ),
                (Key::Market, KeyPost::Posted),
            ],
        )],
        vec![(
            732,
            vec![
                (Key::Ee, KeyPost::Posted),
                (
                    Key::Market,
                    KeyPost::Withheld(WithheldClass::PostFailed, "fixture raw error".into()),
                ),
            ],
        )],
    ];
    let root = std::path::Path::new("/fixture/private-prompt-path");
    let now = Utc::now();
    let times = [now, now + chrono::TimeDelta::seconds(1)];
    let envelopes: Vec<_> = results
        .iter()
        .zip(times)
        .map(|(results, at)| {
            let tick = TickOutcome::from_run(results, 0);
            let record = tick_span(root, "step_dispatch", &tick, at);
            TelemetryEnvelope::new("host", TelemetryRecord::Span(record))
        })
        .collect();
    let request = build_traces_request(&envelopes).unwrap();
    let wire = request.encode_to_vec();
    let json =
        serde_json::to_value(ExportTraceServiceRequest::decode(wire.as_slice()).unwrap()).unwrap();
    let spans = json["resourceSpans"][0]["scopeSpans"][0]["spans"]
        .as_array()
        .unwrap();
    assert_eq!(spans.len(), 2);
    let mut nonzero = BTreeMap::new();
    for span in spans {
        assert_eq!(span["name"], "ratification_lane.tick");
        let attrs = span["attributes"].as_array().unwrap();
        let count = |key: &str| -> usize {
            attrs.iter().find(|kv| kv["key"] == key).unwrap()["value"]["stringValue"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        };
        let mut posted = 0;
        let mut withheld = 0;
        for key in ["ee", "market"] {
            for class in [
                "posted",
                "pre_launch_failed",
                "invocation_failed",
                "output_missing",
                "verdict_invalid",
                "post_failed",
            ] {
                let count = count(&format!("loom.ratification.{key}.{class}"));
                if class == "posted" {
                    posted += count;
                } else {
                    withheld += count;
                }
                if count > 0 {
                    *nonzero.entry((key, class)).or_insert(0) += count;
                }
            }
        }
        assert_eq!(posted, count("loom.ratification.posted"));
        assert_eq!(withheld, count("loom.ratification.withheld"));
    }
    assert_eq!(
        nonzero,
        BTreeMap::from([
            (("ee", "verdict_invalid"), 1),
            (("ee", "posted"), 1),
            (("market", "post_failed"), 1),
            (("market", "posted"), 1),
        ])
    );
    let wire = json.to_string();
    for private in [
        "fixture private review",
        "fixture raw error",
        "/fixture/private-prompt-path",
    ] {
        assert!(!wire.contains(private));
    }
}

/// #9985: an `invoke github` span reaches the OTLP wire with its fixed name,
/// derived IDs, every `github.*` attribute and `service.name=loom-daemon`.
#[test]
fn invoke_github_spans_export_with_deterministic_ids_and_service_name() {
    use crate::gh_invocation::telemetry::{InvocationSpan, Outcome, SPAN_ATTRIBUTE_KEYS};
    use crate::gh_invocation::ParentContext;
    use crate::gh_invocation::{AccessIntent, GhBinSource, GhInvocation, GhTarget, Operation};
    let parent = TraceContext::derived("sweep", &["acme/widgets", "sweep-issue-9-1"]);
    // #10752: `github.number` only on a write whose argv names an issue/PR.
    let inv = GhInvocation::new(
        Operation::new("api.graphql"),
        AccessIntent::Write,
        GhTarget::repo("acme/widgets").unwrap(),
        std::time::Duration::from_secs(5),
    )
    .args(["api", "repos/acme/widgets/issues/9/labels"])
    .parent(ParentContext::Parent(parent.clone()));
    let at = Utc::now();
    let open = InvocationSpan::open_at(&inv, at, "42.0".into());
    let cred = crate::gh_invocation::accounting::cred_of_with(None, false);
    let billing = crate::gh_invocation::billing::Billing::sent(
        Some(200),
        "",
        Some(1),
        crate::gh_invocation::billing::BillingClass::Ok,
        "graphql",
        &cred,
        "writer",
    )
    .with_repo(Some("acme/widgets"));
    let record = open.record(&inv, GhBinSource::Path, Outcome::Ok, Some(0), at, &billing);
    let again = InvocationSpan::open_at(&inv, at, "42.0".into());
    assert_eq!(open.context, again.context, "IDs recompute from the span's facts");

    let request = build_traces_request(&[TelemetryEnvelope::new(
        "host",
        TelemetryRecord::Span(record),
    )])
    .unwrap();
    let json = serde_json::to_value(&request).unwrap();
    let resource = json["resourceSpans"][0]["resource"]["attributes"]
        .as_array()
        .unwrap();
    assert!(resource
        .iter()
        .any(|kv| kv["key"] == "service.name" && kv["value"]["stringValue"] == "loom-daemon"));
    let span = &json["resourceSpans"][0]["scopeSpans"][0]["spans"][0];
    assert_eq!(span["name"], "invoke github");
    assert_eq!(span["traceId"], parent.trace_id.as_str());
    assert_eq!(span["parentSpanId"], parent.span_id.as_str());
    assert_eq!(span["spanId"], open.context.span_id.as_str());
    let keys: Vec<_> = span["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kv| kv["key"].as_str().unwrap().to_string())
        .collect();
    for key in SPAN_ATTRIBUTE_KEYS {
        if crate::gh_invocation::telemetry::OPTIONAL_SPAN_ATTRIBUTE_KEYS.contains(key) {
            continue;
        }
        assert!(keys.iter().any(|k| k == key), "exported span lacks {key}");
    }
}

/// #10640: a failed span's one-line reason is the OTLP status message, not
/// an exported attribute; an `Ok`/`Unset` span exports no message even when
/// the attribute is present.
#[test]
fn the_status_message_attribute_becomes_the_error_status_message() {
    let mut failed = span(TraceContext::root(true));
    failed.status = SpanStatus::Error;
    failed
        .attributes
        .insert("loom.failure_class".into(), "exit-1".into());
    failed
        .attributes
        .insert(STATUS_MESSAGE.into(), "role child exited with code 1".into());
    let mut ok = failed.clone();
    ok.context = TraceContext::root(true);
    ok.status = SpanStatus::Ok;
    let request = build_traces_request(&[
        TelemetryEnvelope::new("host", TelemetryRecord::Span(failed)),
        TelemetryEnvelope::new("host", TelemetryRecord::Span(ok)),
    ])
    .unwrap();
    let spans = &request.resource_spans[0].scope_spans[0].spans;
    let status = spans[0].status.as_ref().unwrap();
    assert_eq!(status.code, 2);
    assert_eq!(status.message, "role child exited with code 1");
    assert!(spans[0]
        .attributes
        .iter()
        .any(|kv| kv.key == "loom.failure_class"));
    for span in spans {
        assert!(
            span.attributes.iter().all(|kv| kv.key != STATUS_MESSAGE),
            "the description is not also exported as an attribute"
        );
    }
    assert_eq!(spans[1].status.as_ref().unwrap().message, "", "no description off Error");
}

#[cfg(unix)]
#[test]
fn worker_preflight_fixture_process() {
    let Ok(profile) = std::env::var("LOOM_TEST_REJECTION_PROFILE") else {
        return;
    };
    crate::worker_spawn::cli(crate::worker_spawn::WorkerArgs {
        scripts_dir: None,
        args: ["-p", "fixture", "--profile", &profile]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect(),
    })
    .unwrap();
}

#[cfg(unix)]
#[test]
fn launch_preflight_report_survives_the_journal_and_export_without_trusting_the_log() {
    use crate::observability::lifecycle;
    use crate::telemetry::trace::{journal::Journal, store::TraceStore};
    use crate::worker_spawn::rejection::Receiver;
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::PermissionsExt,
        process::{Command, Stdio},
    };

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let pool = root.join(".loom/api-keys/fixture");
    fs::create_dir_all(&pool).unwrap();
    fs::write(pool.join("disabled.env"), "LOOM_FIXTURE_PREFLIGHT_KEY=inert-fixture-key\n").unwrap();
    fs::write(pool.join(".disabled"), "disabled\n").unwrap();
    fs::write(
        root.join(".loom/config.json"),
        r#"{
            "observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"},
            "runtimes":{"modelProfiles":{
                "blocked":{"model":"fixture-model","providers":{"opencode":"fixture"},
                    "credentialEnv":"LOOM_FIXTURE_PREFLIGHT_KEY","credentialPool":"fixture",
                    "credentialTargets":{"opencode":"LOOM_FIXTURE_HARNESS_KEY"}},
                "started":{"model":"fixture-model","providers":{"opencode":"fixture"}}
            }}
        }"#,
    )
    .unwrap();
    let harness = root.join("fake-opencode");
    fs::write(
        &harness,
        r#"#!/bin/sh
if [ "${1:-}" = --version ]; then
    echo 1.18.31
    exit 0
fi
test -z "${LOOM_LAUNCH_REJECTION_FD+x}" || exit 1
echo 'provider mentions sweep_id=older in an ordinary message'
echo '# LOOM_LAUNCH_REJECTED code=78 stage=credential'
exit 78
"#,
    )
    .unwrap();
    fs::set_permissions(&harness, fs::Permissions::from_mode(0o700)).unwrap();

    let log_path = root.join("shared.log");
    let attempt = |id: &str, profile: &str| {
        let span = lifecycle::begin(root, id, SpanName::Sweep, TraceAttributes::new()).unwrap();
        let mut log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap();
        writeln!(log, "==== loom-daemon dispatch: t sweep_id={id} item=7 step=builder ====")
            .unwrap();
        let mut worker = Command::new(std::env::current_exe().unwrap());
        worker
            .args([
                "--exact",
                "observability::otlp::traces::tests::worker_preflight_fixture_process",
                "--nocapture",
            ])
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root)
            .env("LOOM_WORKSPACE", root)
            .env("LOOM_RUNTIME", "opencode")
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .env("LOOM_SHARED_API_KEYS_DIR", "")
            .env("LOOM_OPENCODE_BIN", &harness)
            .env("LOOM_TEST_REJECTION_PROFILE", profile)
            .current_dir(root)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        span.command(&mut worker);
        let receiver = Receiver::prepare(&mut worker, true).unwrap();
        assert_eq!(
            worker.status().unwrap().code(),
            Some(78),
            "{}",
            fs::read_to_string(&log_path).unwrap()
        );
        receiver
    };

    let older = attempt("older", "started");
    let current = attempt("current", "blocked");
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("# LOOM_CLI_START runtime=opencode"));
    assert!(log.contains("provider mentions sweep_id=older"));
    assert_eq!(
        log.matches("# LOOM_LAUNCH_REJECTED code=78 stage=credential")
            .count(),
        2
    );
    assert_eq!(older.reason(), None, "the started child and reused log have no authority");
    assert_eq!(current.reason(), Some("preflight_credential"));

    let journal = Journal::for_context(&TraceStore::new(root).path(root, "current"));
    let completed = journal.completed().unwrap();
    let record = completed
        .into_iter()
        .find(|s| s.name == SpanName::RuntimePreflight)
        .unwrap();
    assert_eq!(record.status, SpanStatus::Error);
    let expected = [
        ("loom.admission.stage", "credential"),
        ("loom.launch.runtime", "opencode"),
        ("loom.launch.source", "global-environment"),
    ];
    let payload = serde_json::to_value(
        build_traces_request(&[TelemetryEnvelope::new(
            "fixture-host",
            TelemetryRecord::Span(record.clone()),
        )])
        .unwrap(),
    )
    .unwrap();
    let exported = payload["resourceSpans"][0]["scopeSpans"][0]["spans"][0]["attributes"]
        .as_array()
        .unwrap();
    let gateway = include_str!("../../../../../defaults/observability/collector/config.yaml");
    let span_allowlist = gateway
        .split("context: span\n")
        .nth(1)
        .unwrap()
        .lines()
        .find(|line| line.contains("keep_keys(attributes"))
        .unwrap();
    for (key, value) in expected {
        assert_eq!(record.attributes[key], value, "durable journal lost {key}");
        assert!(
            exported
                .iter()
                .any(|pair| pair["key"] == key && pair["value"]["stringValue"] == value),
            "OTLP lost {key}"
        );
        assert!(span_allowlist.contains(&format!("\"{key}\"")), "gateway drops {key}");
    }
    assert!(!payload.to_string().contains("inert-fixture-key"));
    assert!(!payload.to_string().contains("provider mentions"));
}
