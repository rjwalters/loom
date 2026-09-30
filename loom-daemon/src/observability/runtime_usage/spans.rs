//! One `loom.runtime.usage` span per model, scoped (Issue #9303, #9204).
//!
//! The export allowlist is a static key set, so a per-model breakdown cannot
//! be per-model attribute keys: it is one span per model instead. Each carries
//! that model's counters (5-minute and 1-hour cache writes kept apart), the
//! OTel GenAI aliases, a USD estimate from the daemon's rate card, and
//! `loom.usage.scope`:
//!
//! - `execution` — a whole execution's usage (a daemon sweep's terminal
//!   transition, a role-runner tick);
//! - `attempt` — one role attempt's usage (an in-session subagent via
//!   `loom-daemon usage-record`, a single-target role tick's story span).
//!
//! A daemon-dispatched sweep can carry both in one trace (its `claude -p`
//! child records attempts too), so a consumer totals a `loom.sweep_id` from
//! its `execution` spans when present, else from the sum of its `attempt`
//! spans — never both.
//!
//! Span ids are `parent.derived_child(["loom.runtime.usage", scope, model])`,
//! so a re-emit yields the same ids and [`append_new`] skips it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use super::cost::Pricing;
use super::TokenUsage;
use crate::script_helpers::sweep_experiment::ModelUsageTotals;
use crate::telemetry::trace::journal::Journal;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// `loom.usage.scope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageScope {
    Execution,
    Attempt,
}

impl UsageScope {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Execution => "execution",
            Self::Attempt => "attempt",
        }
    }
}

/// `rows` folded to one row per model (over `speed`/`service_tier`), sorted by
/// model.
#[must_use]
pub fn per_model(rows: &[ModelUsageTotals]) -> Vec<ModelUsageTotals> {
    let mut by_model: BTreeMap<&str, ModelUsageTotals> = BTreeMap::new();
    for row in rows {
        let entry = by_model
            .entry(row.model.as_str())
            .or_insert_with(|| ModelUsageTotals {
                model: row.model.clone(),
                ..ModelUsageTotals::default()
            });
        entry.input = entry.input.saturating_add(row.input);
        entry.output = entry.output.saturating_add(row.output);
        entry.cache_read = entry.cache_read.saturating_add(row.cache_read);
        entry.cache_write_5m = entry.cache_write_5m.saturating_add(row.cache_write_5m);
        entry.cache_write_1h = entry.cache_write_1h.saturating_add(row.cache_write_1h);
    }
    by_model.into_values().collect()
}

/// The counter attributes of one model row: `loom.tokens.*` (with the
/// cache-write split) and the `gen_ai.usage.*` aliases. The aliases use
/// Anthropic's disjoint vocabulary, exactly like `loom.tokens.*`:
/// `input_tokens` is **uncached** input.
#[must_use]
pub fn counter_attributes(row: &ModelUsageTotals) -> TraceAttributes {
    let usage = TokenUsage::from_models(std::slice::from_ref(row));
    let mut attributes = usage.attributes();
    for (key, value) in [
        ("loom.tokens.cache_write_5m", row.cache_write_5m),
        ("loom.tokens.cache_write_1h", row.cache_write_1h),
        ("gen_ai.usage.input_tokens", usage.input),
        ("gen_ai.usage.output_tokens", usage.output),
        ("gen_ai.usage.cache_read_input_tokens", usage.cache_read),
        ("gen_ai.usage.cache_creation_input_tokens", usage.cache_write),
    ] {
        attributes.insert(key.to_string(), value.to_string());
    }
    attributes
}

/// One `loom.runtime.usage` span per model in `rows`, children of `parent`
/// over `window`. `common` (e.g. `loom.runtime`, `loom.role`, `loom.attempt`,
/// `loom.sweep_id`, `loom.issue`) goes on every span. Empty `rows` → no spans.
#[must_use]
pub fn model_usage_spans(
    parent: &TraceContext,
    window: (DateTime<Utc>, DateTime<Utc>),
    rows: &[ModelUsageTotals],
    scope: UsageScope,
    common: &TraceAttributes,
    pricing: &Pricing<'_>,
) -> Vec<SpanRecord> {
    let (started_at, ended_at) = window;
    per_model(rows)
        .iter()
        .map(|row| {
            let mut attributes = common.clone();
            attributes.retain(|_, v| !v.is_empty());
            attributes.extend(counter_attributes(row));
            attributes.extend(pricing.attributes(row));
            attributes.insert("loom.model".into(), row.model.clone());
            attributes.insert("loom.usage.scope".into(), scope.as_str().into());
            crate::telemetry::trace::provenance::stamp(&mut attributes);
            SpanRecord {
                context: parent.derived_child(&[
                    SpanName::RuntimeUsage.as_str(),
                    scope.as_str(),
                    &row.model,
                ]),
                parent_span_id: Some(parent.span_id.clone()),
                name: SpanName::RuntimeUsage,
                started_at,
                ended_at: ended_at.max(started_at),
                status: SpanStatus::Ok,
                attributes,
                events: Vec::new(),
                links: Vec::new(),
            }
            .bounded()
        })
        .collect()
}

/// Append `spans` to `journal` as completed spans, skipping any span id the
/// journal already holds (a re-emit). Returns the spans appended.
pub fn append_new(journal: &Journal, spans: Vec<SpanRecord>) -> anyhow::Result<Vec<SpanRecord>> {
    if spans.is_empty() {
        return Ok(Vec::new());
    }
    let existing: BTreeSet<String> = if journal.path().exists() {
        journal
            .completed()?
            .into_iter()
            .map(|span| span.context.span_id.as_str().to_owned())
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut appended = Vec::new();
    for span in spans {
        if existing.contains(span.context.span_id.as_str()) {
            continue;
        }
        journal.append_completed(span.clone())?;
        appended.push(span);
    }
    Ok(appended)
}
