//! The `build_gate.run` span (#11690): one per host-side run of
//! `buildGate.command`.
//!
//! Two runners remain on a host: the Builder's in-session pre-flight (the fast
//! tier, [`crate::preflight`]) and the opt-in daemon main-health gate
//! ([`crate::main_health_gate`]). Each run that executes the command is one
//! span naming the stage, the tier, the outcome, the wall-clock duration and
//! the attempt within its episode — never the command, a path or the output.
//! That is enough to see how long host-side gating costs and how often it
//! fails, per tier, without a log trawl.
//!
//! Delivery follows `cargo_target_guard::publish` and the `gh` facade: the
//! daemon's ops sink when one is registered (the main-health gate), else the
//! ambient execution's trace journal (`LOOM_TRACEPARENT` +
//! `LOOM_TRACE_CONTEXT_FILE`, a pre-flight inside a traced sweep), else
//! nothing. An attended session with no `LOOM_TRACEPARENT` emits no span.

use chrono::{DateTime, Utc};

use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// `stage` of the Builder's in-session pre-flight.
pub const STAGE_PREFLIGHT: &str = "preflight";
/// `stage` of the daemon's main-health gate.
pub const STAGE_MAIN_HEALTH: &str = "main_health";

/// One gate run, as the span reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRun<'a> {
    /// [`STAGE_PREFLIGHT`] / [`STAGE_MAIN_HEALTH`].
    pub stage: &'a str,
    /// `fast` / `full`.
    pub tier: &'a str,
    /// `pass` / `pass_flaky` / `fail` / `unresolved` / `timeout` /
    /// `unevaluated`.
    pub outcome: &'a str,
    /// 1-based run number within the stage's episode.
    pub attempt: u32,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
}

/// The span for `r`: a child of `parent` when there is one, else its own root.
#[must_use]
pub fn run_span(r: &GateRun<'_>, parent: Option<&TraceContext>) -> SpanRecord {
    let name = SpanName::BuildGateRun;
    let started = crate::telemetry::trace::instant(r.started_at);
    let pid = std::process::id().to_string();
    let key = [name.as_str(), r.stage, started.as_str(), pid.as_str()];
    let (context, parent_span_id) = match parent {
        Some(p) => (p.derived_child(&key), Some(p.span_id.clone())),
        None => (TraceContext::derived(name.as_str(), &key[1..]), None),
    };
    let ended_at = r.ended_at.max(r.started_at);
    let duration_ms = (ended_at - r.started_at).num_milliseconds().max(0);
    let mut attributes = TraceAttributes::new();
    attributes.insert("loom.build_gate.stage".into(), r.stage.to_string());
    attributes.insert("loom.build_gate.tier".into(), r.tier.to_string());
    attributes.insert("loom.build_gate.outcome".into(), r.outcome.to_string());
    attributes.insert("loom.build_gate.duration_ms".into(), duration_ms.to_string());
    attributes.insert("loom.build_gate.attempt".into(), r.attempt.to_string());
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id,
        name,
        started_at: r.started_at,
        ended_at,
        status: if matches!(r.outcome, "pass" | "pass_flaky") {
            SpanStatus::Ok
        } else {
            SpanStatus::Error
        },
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Export one run through whatever path this process has. Never fails.
pub fn publish(r: &GateRun<'_>) {
    if super::spans_exported() {
        super::emit_span(run_span(r, None));
        return;
    }
    if let (Some(parent), Some(file)) = crate::gh_invocation::telemetry::ambient_journal() {
        let span = run_span(r, Some(&parent));
        if let Err(e) =
            crate::telemetry::trace::journal::Journal::for_context(&file).append_completed(span)
        {
            log::debug!("build_gate: span not journalled: {e}");
        }
    }
}

/// Run one main-health gate run (`run`) and export its span (daemon side,
/// so the ops sink).
pub fn main_health(
    tier: crate::main_health_gate::GateTier,
    run: impl FnOnce() -> crate::main_health_gate::GateOutcome,
) -> crate::main_health_gate::GateOutcome {
    let started_at = Utc::now();
    let outcome = run();
    publish(&GateRun {
        stage: STAGE_MAIN_HEALTH,
        tier: tier.label(),
        outcome: main_health_outcome(&outcome),
        attempt: 1,
        started_at,
        ended_at: Utc::now(),
    });
    outcome
}

/// The span `outcome` for a main-health [`GateOutcome`](crate::main_health_gate::GateOutcome).
fn main_health_outcome(outcome: &crate::main_health_gate::GateOutcome) -> &'static str {
    use crate::main_health_gate::{GateOutcome, UnevaluatedClass};
    match outcome {
        GateOutcome::Green { .. } => "pass",
        GateOutcome::Red { .. } => "fail",
        GateOutcome::Unevaluated {
            class: UnevaluatedClass::Timeout,
            ..
        } => "timeout",
        GateOutcome::Unevaluated { .. } => "unevaluated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(outcome: &str) -> GateRun<'_> {
        let at = DateTime::parse_from_rfc3339("2026-10-12T20:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        GateRun {
            stage: STAGE_MAIN_HEALTH,
            tier: "full",
            outcome,
            attempt: 2,
            started_at: at,
            ended_at: at + chrono::Duration::milliseconds(1500),
        }
    }

    #[test]
    fn the_span_carries_every_attribute_and_keeps_them_through_export() {
        let span = run_span(&run("fail"), None);
        assert_eq!(span.name.as_str(), "build_gate.run");
        assert_eq!(span.status, SpanStatus::Error);
        assert!(span.parent_span_id.is_none());
        assert!(span.validate().is_ok());
        let bounded = span.clone().bounded();
        for (key, want) in [
            ("loom.build_gate.stage", "main_health"),
            ("loom.build_gate.tier", "full"),
            ("loom.build_gate.outcome", "fail"),
            ("loom.build_gate.duration_ms", "1500"),
            ("loom.build_gate.attempt", "2"),
        ] {
            assert_eq!(bounded.attributes.get(key).map(String::as_str), Some(want), "{key}");
            assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&key), "{key}");
        }
        assert_eq!(run_span(&run("pass"), None).status, SpanStatus::Ok);
        assert_eq!(run_span(&run("pass_flaky"), None).status, SpanStatus::Ok);
        // Deterministic IDs: the same run derives the same context.
        assert_eq!(span.context, run_span(&run("fail"), None).context);
    }

    #[test]
    fn with_an_ambient_execution_the_span_is_its_child() {
        let parent = TraceContext::derived("test", &["parent"]);
        let span = run_span(&run("timeout"), Some(&parent));
        assert_eq!(span.parent_span_id.as_ref(), Some(&parent.span_id));
        assert_eq!(span.context.trace_id, parent.trace_id);
    }

    #[test]
    fn main_health_outcomes_map_to_span_outcomes() {
        use crate::main_health_gate::{GateOutcome, UnevaluatedClass};
        let green = GateOutcome::Green {
            elapsed: std::time::Duration::from_secs(1),
        };
        let unevaluated = |class| GateOutcome::Unevaluated {
            class,
            reason: String::new(),
        };
        assert_eq!(main_health_outcome(&green), "pass");
        assert_eq!(main_health_outcome(&GateOutcome::red("x")), "fail");
        assert_eq!(main_health_outcome(&unevaluated(UnevaluatedClass::Timeout)), "timeout");
        assert_eq!(main_health_outcome(&unevaluated(UnevaluatedClass::DirtyTree)), "unevaluated");
    }

    #[test]
    fn a_main_health_run_returns_its_outcome_and_emits_one_span() {
        use crate::main_health_gate::{GateOutcome, GateTier};
        let (outcome, captured) = super::super::capture::capture(|| {
            main_health(GateTier::Fast, || GateOutcome::red("boom"))
        });
        assert!(matches!(outcome, GateOutcome::Red { .. }));
        assert_eq!(captured.spans.len(), 1);
        let attrs = &captured.spans[0].attributes;
        assert_eq!(attrs["loom.build_gate.stage"], "main_health");
        assert_eq!(attrs["loom.build_gate.tier"], "fast");
        assert_eq!(attrs["loom.build_gate.outcome"], "fail");
        assert!(!attrs.values().any(|v| v.contains("boom")), "no output in attributes");
    }

    #[test]
    fn publish_goes_to_the_ops_sink_when_one_is_registered() {
        let ((), captured) = super::super::capture::capture(|| publish(&run("pass")));
        assert_eq!(captured.spans.len(), 1);
        assert_eq!(captured.spans[0].name.as_str(), "build_gate.run");
    }
}
