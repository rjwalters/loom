//! Per-phase usage mirrored onto the execution's own `loom.role_attempt` spans
//! (Issue #9443), so the JSONL/D1 path (`sweep.outcome`'s `phase_durations`) and
//! the OTLP path report one set of per-phase costs and cannot disagree.

use chrono::{Duration, Utc};

use super::*;
use crate::telemetry::trace::{SpanStatus, TraceContext};

fn row(model: &str, input: i64, output: i64) -> ModelUsageTotals {
    ModelUsageTotals {
        model: model.into(),
        speed: "standard".into(),
        service_tier: "standard".into(),
        input,
        cache_read: 0,
        cache_write_5m: 0,
        cache_write_1h: 0,
        output,
    }
}

/// A traced execution carrying one completed `loom.role_attempt` per
/// `(role, attempt)`, in the order given — the shape #8525's lifecycle writes.
/// `labelled` controls whether each span carries its own `loom.attempt`, which
/// is the difference between [`match_attempt`]'s two matching strategies.
fn traced_attempts(
    root: &Path,
    execution: &str,
    attempts: &[(&str, u32)],
    labelled: bool,
) -> TraceContext {
    let store = TraceStore::new(root);
    let saved = store.load_or_create(root, execution).unwrap();
    let journal = Journal::for_context(&store.path(root, execution));
    let t0 = Utc::now() - Duration::minutes(30);
    for (index, (role, attempt)) in attempts.iter().enumerate() {
        let mut attributes = TraceAttributes::new();
        attributes.insert("loom.role".into(), (*role).to_string());
        if labelled {
            attributes.insert("loom.attempt".into(), attempt.to_string());
        }
        let at = t0 + Duration::seconds(i64::try_from(index).unwrap() * 60);
        let span = journal
            .start(
                saved
                    .context
                    .derived_child(&["attempt", role, &attempt.to_string()]),
                Some(&saved.context),
                SpanName::RoleAttempt,
                at,
                attributes,
            )
            .unwrap();
        journal
            .finish(&span, at + Duration::seconds(30), SpanStatus::Ok, TraceAttributes::new())
            .unwrap();
    }
    saved.context
}

fn phases<'a>(entries: &'a [(&'a str, u32, &'a [ModelUsageTotals])]) -> Vec<PhaseUsage<'a>> {
    let at = Utc::now();
    entries
        .iter()
        .map(|(role, attempt, rows)| PhaseUsage {
            role,
            attempt: *attempt,
            window: (at, at + Duration::seconds(60)),
            rows,
        })
        .collect()
}

fn usage_spans(root: &Path, execution: &str) -> Vec<SpanRecord> {
    let store = TraceStore::new(root);
    Journal::for_context(&store.path(root, execution))
        .completed()
        .unwrap()
        .into_iter()
        .filter(|span| span.name == SpanName::RuntimeUsage)
        .collect()
}

/// AC (#9443, third bullet): the five-phase `curator → builder → judge(fail) →
/// doctor → judge(pass)` lifecycle's per-phase numbers land on the matching
/// `loom.role_attempt` spans — including `judge` attempt 2 as its **own** parent
/// — and the per-model spans' totals sum to the same figure `sweep.outcome`'s
/// `phase_durations` carries.
#[test]
fn every_phase_attempt_gets_usage_under_its_own_role_attempt_span() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    let lifecycle = [
        ("curator", 1),
        ("builder", 1),
        ("judge", 1),
        ("doctor", 1),
        ("judge", 2),
    ];
    traced_attempts(ws, "sweep-1", &lifecycle, true);

    let curator = [row("claude-sonnet-5", 10, 1)];
    let builder = [row("claude-opus-5", 100, 10)];
    let judge1 = [row("claude-sonnet-5", 20, 2)];
    let doctor = [row("claude-opus-5", 200, 20)];
    let judge2 = [row("claude-sonnet-5", 30, 3)];
    let entries: Vec<(&str, u32, &[ModelUsageTotals])> = vec![
        ("curator", 1, &curator),
        ("builder", 1, &builder),
        ("judge", 1, &judge1),
        ("doctor", 1, &doctor),
        ("judge", 2, &judge2),
    ];
    let appended = journal_phase_usage(ws, "sweep-1", &phases(&entries), Some("claude")).unwrap();
    assert_eq!(appended.len(), 5, "one usage span per phase attempt: {appended:?}");

    let attempt_spans: Vec<SpanRecord> = {
        let store = TraceStore::new(ws);
        Journal::for_context(&store.path(ws, "sweep-1"))
            .completed()
            .unwrap()
            .into_iter()
            .filter(|s| s.name == SpanName::RoleAttempt)
            .collect()
    };
    let parent_of = |role: &str, attempt: u32| {
        attempt_spans
            .iter()
            .find(|s| {
                s.attributes["loom.role"] == role
                    && s.attributes["loom.attempt"] == attempt.to_string()
            })
            .unwrap()
            .context
            .span_id
            .clone()
    };
    let spans = usage_spans(ws, "sweep-1");
    let find = |role: &str, attempt: u32| {
        spans
            .iter()
            .find(|s| {
                s.attributes["loom.role"] == role
                    && s.attributes["loom.attempt"] == attempt.to_string()
            })
            .unwrap_or_else(|| panic!("a usage span for {role} attempt {attempt}"))
    };
    for (role, attempt, expected_in, expected_out) in [
        ("curator", 1u32, 10i64, 1i64),
        ("builder", 1, 100, 10),
        ("judge", 1, 20, 2),
        ("doctor", 1, 200, 20),
        ("judge", 2, 30, 3),
    ] {
        let span = find(role, attempt);
        assert_eq!(
            span.parent_span_id.as_ref(),
            Some(&parent_of(role, attempt)),
            "{role}#{attempt} hangs under its OWN role attempt span"
        );
        assert_eq!(span.attributes["loom.usage.scope"], "attempt");
        assert_eq!(span.attributes["loom.phase"], role);
        assert_eq!(span.attributes["loom.runtime"], "claude");
        assert_eq!(span.attributes["loom.sweep_id"], "sweep-1");
        assert_eq!(span.attributes["loom.tokens.input"], expected_in.to_string());
        assert_eq!(span.attributes["loom.tokens.output"], expected_out.to_string());
    }
    // The D1↔OTLP agreement: the spans' input/output totals equal the sum the
    // `sweep.outcome` record's per-phase entries report.
    let sum = |key: &str| -> i64 {
        spans
            .iter()
            .map(|s| s.attributes[key].parse::<i64>().unwrap())
            .sum()
    };
    assert_eq!((sum("loom.tokens.input"), sum("loom.tokens.output")), (360, 36));

    // Re-emitting is idempotent: the span ids derive from (parent, scope, model).
    assert!(journal_phase_usage(ws, "sweep-1", &phases(&entries), Some("claude"))
        .unwrap()
        .is_empty());
    assert_eq!(usage_spans(ws, "sweep-1").len(), 5);
}

/// Attempt spans that carry no `loom.attempt` of their own are matched
/// positionally — the Nth span of a role is attempt N — so a lifecycle whose
/// spans predate attempt labelling still gets per-phase usage.
#[test]
fn unlabelled_attempt_spans_match_positionally_in_start_order() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    traced_attempts(ws, "sweep-2", &[("judge", 1), ("doctor", 1), ("judge", 2)], false);

    let judge1 = [row("m", 11, 1)];
    let judge2 = [row("m", 22, 2)];
    let entries: Vec<(&str, u32, &[ModelUsageTotals])> =
        vec![("judge", 1, &judge1), ("judge", 2, &judge2)];
    assert_eq!(
        journal_phase_usage(ws, "sweep-2", &phases(&entries), None)
            .unwrap()
            .len(),
        2
    );

    let attempt_ids: Vec<String> = {
        let store = TraceStore::new(ws);
        Journal::for_context(&store.path(ws, "sweep-2"))
            .completed()
            .unwrap()
            .into_iter()
            .filter(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.role"] == "judge")
            .map(|s| s.context.span_id.as_str().to_owned())
            .collect()
    };
    assert_eq!(attempt_ids.len(), 2);
    let spans = usage_spans(ws, "sweep-2");
    for (index, tokens) in [(0usize, "11"), (1, "22")] {
        let span = spans
            .iter()
            .find(|s| s.attributes["loom.tokens.input"] == tokens)
            .unwrap();
        assert_eq!(
            span.parent_span_id.as_ref().map(|id| id.as_str()),
            Some(attempt_ids[index].as_str()),
            "judge #{} maps to the {index}-th judge span in start order",
            index + 1
        );
    }
}

/// A phase with **no** matching attempt span in the trace is skipped rather than
/// given a fabricated parent: `sweep.outcome` still reports its usage, but an
/// invented span would claim a role attempt the trace never observed. Same for a
/// phase with no measured usage, and for an untraced execution.
#[test]
fn an_unmatched_phase_or_unmeasured_usage_journals_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path();
    traced_attempts(ws, "sweep-3", &[("builder", 1)], true);

    let none: [ModelUsageTotals; 0] = [];
    let some = [row("m", 5, 5)];
    // `judge` has no attempt span at all; `builder` attempt 2 is past the end of
    // the role's spans; `builder` attempt 1 is measured but empty.
    let entries: Vec<(&str, u32, &[ModelUsageTotals])> = vec![
        ("judge", 1, &some),
        ("builder", 2, &some),
        ("builder", 1, &none),
    ];
    assert!(journal_phase_usage(ws, "sweep-3", &phases(&entries), None)
        .unwrap()
        .is_empty());
    assert!(usage_spans(ws, "sweep-3").is_empty());

    // No persisted trace context ⇒ nothing, not an error.
    let builder = [row("m", 1, 1)];
    let one: Vec<(&str, u32, &[ModelUsageTotals])> = vec![("builder", 1, &builder)];
    assert!(journal_phase_usage(ws, "never-traced", &phases(&one), None)
        .unwrap()
        .is_empty());
    assert!(journal_phase_usage(ws, "sweep-3", &[], None)
        .unwrap()
        .is_empty());
}

/// `TokenUsage::split` is `sweep.outcome`'s `tokens_in`/`tokens_out` definition:
/// every input-side counter (uncached input, cache reads, cache writes) against
/// `output` alone. It has to agree with
/// `transcript_tokens::sum_sweep_tokens_split`, or Σ phases would not reconcile
/// against the sweep totals.
#[test]
fn the_token_split_puts_every_cache_counter_on_the_input_axis() {
    let usage = TokenUsage {
        input: 100,
        output: 7,
        cache_read: 1_000,
        cache_write: 20,
    };
    assert_eq!(usage.split(), (1_120, 7));
    assert_eq!(TokenUsage::default().split(), (0, 0));
    let rows = [ModelUsageTotals {
        model: "m".into(),
        speed: "standard".into(),
        service_tier: "standard".into(),
        input: 100,
        cache_read: 1_000,
        cache_write_5m: 5,
        cache_write_1h: 15,
        output: 7,
    }];
    assert_eq!(
        TokenUsage::from_models(&rows).split(),
        (1_120, 7),
        "both cache-write tiers count as input, exactly as the flat split does"
    );
}
