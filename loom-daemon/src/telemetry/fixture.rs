//! Versioned deterministic synthetic telemetry for independent backend queries.
//! No clock, filesystem, provider, daemon state, or forge is consulted here.
use super::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};
use super::{
    RepoVisibility, SweepPhaseRecord, TelemetryEnvelope, TelemetryRecord, TokenAccountState,
    TokenSnapshotRecord,
};
use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const PRIVACY_SENTINEL: &str = "LOOM_SYNTHETIC_PRIVATE_PROMPT_SENTINEL_8529";

pub struct FixtureBundle {
    pub envelopes: Vec<TelemetryEnvelope>,
    pub manifest: Value,
}

struct Scenario {
    name: &'static str,
    repo: &'static str,
    phases: &'static [(&'static str, &'static str)],
    terminal: Option<&'static str>,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "success",
        repo: "synthetic/alpha",
        phases: &[
            ("builder", "success"),
            ("judge", "approved"),
            ("merge", "success"),
        ],
        terminal: Some("success"),
    },
    Scenario {
        name: "repair",
        repo: "synthetic/alpha",
        phases: &[
            ("builder", "success"),
            ("judge", "rejected"),
            ("doctor", "success"),
            ("judge", "approved"),
            ("merge", "success"),
        ],
        terminal: Some("success"),
    },
    Scenario {
        name: "preflight_rejection",
        repo: "synthetic/alpha",
        phases: &[("preflight", "runtime_rejected")],
        terminal: Some("runtime_rejected"),
    },
    Scenario {
        name: "cancelled",
        repo: "synthetic/alpha",
        phases: &[("builder", "cancelled")],
        terminal: Some("cancelled"),
    },
    Scenario {
        name: "crash_incomplete",
        repo: "synthetic/alpha",
        phases: &[("preflight", "success")],
        terminal: None,
    },
    Scenario {
        name: "timeout",
        repo: "synthetic/alpha",
        phases: &[("builder", "timeout")],
        terminal: Some("timeout"),
    },
    Scenario {
        name: "concurrent_alpha",
        repo: "synthetic/alpha",
        phases: &[("builder", "success")],
        terminal: Some("success"),
    },
    Scenario {
        name: "concurrent_beta",
        repo: "synthetic/beta",
        phases: &[("builder", "success")],
        terminal: Some("success"),
    },
];

fn context(run: &str, scenario: &str, operation: &str) -> Result<TraceContext> {
    let trace = hex::encode(Sha256::digest(format!("loom-fixture-v1\0{run}\0{scenario}")));
    let span =
        hex::encode(Sha256::digest(format!("loom-fixture-v1\0{run}\0{scenario}\0{operation}")));
    TraceContext::parse(&format!("00-{}-{}-01", &trace[..32], &span[..16]))
        .map_err(anyhow::Error::msg)
}

fn envelope(host: &str, at: DateTime<Utc>, record: TelemetryRecord) -> TelemetryEnvelope {
    TelemetryEnvelope {
        // The kind's own declared gate (`telemetry/kinds.rs`, #8921) — the same
        // value `TelemetryEnvelope::new` stamps, so a fixture envelope is never
        // a second, hand-maintained copy of the version ladder. Byte-identical
        // for the three kinds this bundle emits (`trace.span` 3,
        // `sweep.phase` / `tokens.snapshot` 2).
        schema_version: record.schema_version(),
        emitted_at: at,
        host_id: host.to_owned(),
        record,
        trace_context: None,
    }
}

fn attributes(scenario: &Scenario, phase: &str, result: &str, attempt: usize) -> TraceAttributes {
    let mut attrs: TraceAttributes = [
        ("loom.repo", scenario.repo.to_owned()),
        ("loom.repo.visibility", "private".to_owned()),
        ("loom.issue", "18".to_owned()),
        ("loom.phase", phase.to_owned()),
        ("loom.result", result.to_owned()),
        ("loom.attempt", attempt.to_string()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v))
    .collect();
    if phase != "preflight" && phase != "merge" && phase != "sweep" {
        attrs.insert("loom.role".into(), phase.into());
        attrs.insert("loom.runtime".into(), "synthetic-runtime".into());
        attrs.insert("loom.provider".into(), "synthetic-provider".into());
        attrs.insert("loom.model".into(), "synthetic-model-no-inference".into());
    }
    attrs
}

fn push_span(bundle: &mut FixtureBundle, host: &str, span: SpanRecord) -> Result<()> {
    span.validate().map_err(anyhow::Error::msg)?;
    let mut expected_attributes = span.attributes.clone();
    expected_attributes.remove("prompt.content");
    let expected = json!({
        "trace_id":span.context.trace_id,"span_id":span.context.span_id,
        "parent_span_id":span.parent_span_id,"name":span.name.as_str(),
        "started_at":span.started_at,"ended_at":span.ended_at,"status":span.status,
        "otlp_status_code":match span.status { SpanStatus::Unset => 0, SpanStatus::Ok => 1, SpanStatus::Error => 2 },
        "attributes":expected_attributes,
    });
    bundle.manifest["spans"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid span manifest"))?
        .push(expected);
    bundle
        .envelopes
        .push(envelope(host, span.ended_at, TelemetryRecord::Span(span)));
    Ok(())
}

pub fn build(run_id: &str, start: DateTime<Utc>) -> Result<FixtureBundle> {
    anyhow::ensure!(
        !run_id.is_empty()
            && run_id.len() <= 64
            && run_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
        "run-id must contain 1..=64 ASCII letters, digits, underscores or hyphens"
    );
    anyhow::ensure!(
        start.timestamp() >= 0
            && start.timestamp_nanos_opt().is_some()
            && start
                .checked_add_signed(Duration::hours(1))
                .and_then(|t| t.timestamp_nanos_opt())
                .is_some(),
        "start-time is outside supported nanosecond timestamp bounds"
    );
    let host = format!("loom-synthetic-{run_id}");
    let mut bundle = FixtureBundle {
        envelopes: vec![],
        manifest: json!({
            "schema_version":1,"synthetic":true,"run_id":run_id,"host_id":host,
            "resource_attributes":{"service.name":"loom-daemon","service.instance.id":host,"host.id":host},
            "start_time":start,"sampling":"all","backend_verification":"not_performed",
            "identity_policy":"same run-id and start-time replay identical envelopes; change run-id for a distinct trial",
            "log_identity_fields":["host.id","trace_id","span_id","time_unix_nano","event_name"],
            "spans":[],"logs":[],"metrics":[],"scenarios":[],
            "must_not_appear":[PRIVACY_SENTINEL],
            "unproven":["real lifecycle instrumentation","daemon crash/adoption recovery","backend indexing","UI navigation","backend authorization","retention","fan-out outage isolation","collector restart","buffer saturation","paid inference cost"]
        }),
    };
    for scenario in SCENARIOS {
        // All scenarios intentionally overlap in wall time, including issue18
        // in two repositories. Parentage must follow IDs, never issue number.
        let root = context(run_id, scenario.name, "root")?;
        let sweep_id = format!("synthetic-{run_id}-{}", scenario.name);
        let mut phases = vec![];
        let mut attempts = std::collections::BTreeMap::new();
        for (index, (phase, result)) in scenario.phases.iter().enumerate() {
            let attempt = attempts.entry(*phase).or_insert(0_usize);
            *attempt += 1;
            let attempt = *attempt;
            let at = start + Duration::seconds(10 * i64::try_from(index)?);
            let end = at + Duration::seconds(8);
            let ctx = context(run_id, scenario.name, &format!("attempt-{index}"))?;
            let phase_ctx = context(run_id, scenario.name, &format!("phase-{index}"))?;
            push_span(
                &mut bundle,
                &host,
                SpanRecord {
                    context: phase_ctx.clone(),
                    parent_span_id: Some(root.span_id.clone()),
                    name: SpanName::Phase,
                    started_at: at,
                    ended_at: end,
                    status: if matches!(*result, "success" | "approved") {
                        SpanStatus::Ok
                    } else {
                        SpanStatus::Error
                    },
                    attributes: attributes(scenario, phase, result, attempt),
                    events: vec![],
                    links: vec![],
                },
            )?;
            let mut attrs = attributes(scenario, phase, result, attempt);
            attrs.insert("loom.sweep_id".into(), sweep_id.clone());
            // A harmless synthetic privacy probe must be removed by the OTLP
            // attribute allowlist. It is deliberately visible only in input.
            attrs.insert("prompt.content".into(), PRIVACY_SENTINEL.into());
            push_span(
                &mut bundle,
                &host,
                SpanRecord {
                    context: ctx.clone(),
                    parent_span_id: Some(phase_ctx.span_id),
                    name: if *phase == "preflight" {
                        SpanName::RuntimePreflight
                    } else {
                        SpanName::RoleAttempt
                    },
                    started_at: at,
                    ended_at: end,
                    status: if matches!(*result, "success" | "approved") {
                        SpanStatus::Ok
                    } else {
                        SpanStatus::Error
                    },
                    attributes: attrs,
                    events: vec![],
                    links: vec![],
                },
            )?;
            if scenario.name == "success" && *phase == "builder" {
                let runtime = context(run_id, scenario.name, "runtime-builder")?;
                push_span(
                    &mut bundle,
                    &host,
                    SpanRecord {
                        context: runtime.clone(),
                        parent_span_id: Some(ctx.span_id.clone()),
                        name: SpanName::RuntimeRun,
                        started_at: at + Duration::seconds(1),
                        ended_at: at + Duration::seconds(7),
                        status: SpanStatus::Ok,
                        attributes: attributes(scenario, phase, result, attempt),
                        events: vec![],
                        links: vec![],
                    },
                )?;
                let mut tool_attrs = attributes(scenario, phase, result, attempt);
                tool_attrs.insert("loom.tool.name".into(), "synthetic-owned-tool".into());
                push_span(
                    &mut bundle,
                    &host,
                    SpanRecord {
                        context: context(run_id, scenario.name, "owned-tool-builder")?,
                        parent_span_id: Some(runtime.span_id),
                        name: SpanName::Tool,
                        started_at: at + Duration::seconds(2),
                        ended_at: at + Duration::seconds(3),
                        status: SpanStatus::Ok,
                        attributes: tool_attrs,
                        events: vec![],
                        links: vec![],
                    },
                )?;
            }
            let mut log = envelope(
                &host,
                at,
                TelemetryRecord::SweepPhase(SweepPhaseRecord {
                    repo: scenario.repo.into(),
                    visibility: RepoVisibility::Private,
                    issue: 18,
                    sweep_id: sweep_id.clone(),
                    phase: (*phase).into(),
                    entered_at: at,
                }),
            );
            log.trace_context = Some(ctx.clone());
            bundle.manifest["logs"]
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("invalid log manifest"))?
                .push(json!({
                    "trace_id":ctx.trace_id,"span_id":ctx.span_id,"emitted_at":at,
                    "event":"sweep.phase","phase":phase,"repo":scenario.repo,"issue":18,
                }));
            bundle.envelopes.push(log);
            phases.push(json!({"phase":phase,"result":result,"span_id":ctx.span_id}));
        }
        if let Some(result) = scenario.terminal {
            push_span(
                &mut bundle,
                &host,
                SpanRecord {
                    context: root.clone(),
                    parent_span_id: None,
                    name: SpanName::Sweep,
                    started_at: start,
                    ended_at: start + Duration::seconds(10 * i64::try_from(scenario.phases.len())?),
                    status: if result == "success" {
                        SpanStatus::Ok
                    } else {
                        SpanStatus::Error
                    },
                    attributes: attributes(scenario, "sweep", result, 0),
                    events: vec![],
                    links: vec![],
                },
            )?;
        }
        bundle.manifest["scenarios"].as_array_mut().ok_or_else(|| anyhow::anyhow!("invalid scenario manifest"))?.push(json!({
            "name":scenario.name,"repo":scenario.repo,"issue":18,"trace_id":root.trace_id,
            "root_span_id":root.span_id,"root_exported":scenario.terminal.is_some(),
            "terminal_result":scenario.terminal,"phases":phases,
            "model_launch_expected":if scenario.name == "crash_incomplete" { None } else { Some(scenario.name != "preflight_rejection") },
            "note":if scenario.terminal.is_none() {"Missing root ending is intentional; never infer success or fabricate its span."} else {"Synthetic expected graph, not an observed execution."},
        }));
    }
    bundle.envelopes.push(envelope(
        &host,
        start,
        TelemetryRecord::TokensSnapshot(TokenSnapshotRecord {
            captured_at: start,
            accounts: vec![
                TokenAccountState {
                    account: "synthetic-zero".into(),
                    provider: "claude".into(),
                    rank: None,
                    usage_fraction: Some(0.0),
                    limit_window_reset_at: None,
                    exhausted: false,
                },
                TokenAccountState {
                    account: "synthetic-unknown".into(),
                    provider: "claude".into(),
                    rank: None,
                    usage_fraction: None,
                    limit_window_reset_at: None,
                    exhausted: false,
                },
            ],
        }),
    ));
    bundle.manifest["metrics"] = json!([
        {"name":"loom.tokens.usage_fraction","account":"synthetic-zero","timestamp":start,"present":true,"value":0.0},
        {"name":"loom.tokens.usage_fraction","account":"synthetic-unknown","timestamp":start,"present":false},
        {"name":"loom.tokens.exhausted","account":"synthetic-zero","timestamp":start,"present":true,"value":0},
        {"name":"loom.tokens.exhausted","account":"synthetic-unknown","timestamp":start,"present":true,"value":0},
    ]);
    bundle.manifest["expected_distinct"] = json!({"spans":bundle.manifest["spans"].as_array().map_or(0,Vec::len),"logs":bundle.manifest["logs"].as_array().map_or(0,Vec::len),"metric_data_points":3});
    Ok(bundle)
}

#[cfg(test)]
mod tests;
