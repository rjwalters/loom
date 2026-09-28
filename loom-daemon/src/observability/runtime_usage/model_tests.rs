//! Per-model usage spans, scope, deterministic ids and USD estimates
//! (Issues #9204, #9303).

use chrono::{Duration, Utc};

use super::cost::{Pricing, COMPILED_VERIFIED_ON};
use super::spans::{model_usage_spans, per_model, UsageScope};
use super::*;
use crate::activity::pricing_card::{shipped_asset_path, PricingCard};
use crate::telemetry::trace::{SpanStatus, TraceContext};

fn model(name: &str, input: i64, output: i64, read: i64, w5: i64, w1: i64) -> ModelUsageTotals {
    ModelUsageTotals {
        model: name.into(),
        speed: "standard".into(),
        service_tier: "standard".into(),
        input,
        cache_read: read,
        cache_write_5m: w5,
        cache_write_1h: w1,
        output,
    }
}

/// An Opus 5 parent with a Haiku 4.5 subagent — the mixed-price run #9204
/// is about.
fn two_models() -> Vec<ModelUsageTotals> {
    vec![
        model("claude-opus-5", 1000, 2000, 10_000, 4000, 3000),
        model("claude-haiku-4-5", 2000, 1000, 20_000, 1000, 5000),
    ]
}

fn spans_for(rows: &[ModelUsageTotals], pricing: &Pricing<'_>) -> Vec<SpanRecord> {
    let at = Utc::now();
    model_usage_spans(
        &TraceContext::root(true),
        (at, at),
        rows,
        UsageScope::Attempt,
        &TraceAttributes::new(),
        pricing,
    )
}

fn usd(span: &SpanRecord) -> f64 {
    span.attributes["loom.cost.usd_estimate"].parse().unwrap()
}

#[test]
fn each_model_is_priced_at_its_own_rates_with_both_cache_write_tiers() {
    let shipped = PricingCard::load(&shipped_asset_path()).unwrap();
    for pricing in [Pricing::with(None), Pricing::with(Some(&shipped))] {
        let spans = spans_for(&two_models(), &pricing);
        assert_eq!(spans.len(), 2, "one span per model");
        let by_model = |m: &str| {
            spans
                .iter()
                .find(|s| s.attributes["loom.model"] == m)
                .unwrap()
        };
        // Opus 5: $5/$25 per MTok; read 0.1x, 5m write 1.25x, 1h write 2x.
        // 1000*.005 + 2000*.025 + 10000*.0005 + 4000*.00625 + 3000*.01 (per 1k)
        let opus = by_model("claude-opus-5");
        assert!((usd(opus) - 0.115).abs() < 1e-9, "{}", usd(opus));
        // Haiku 4.5: $1/$5 per MTok.
        // 2000*.001 + 1000*.005 + 20000*.0001 + 1000*.00125 + 5000*.002
        let haiku = by_model("claude-haiku-4-5");
        assert!((usd(haiku) - 0.02025).abs() < 1e-9, "{}", usd(haiku));
        assert_eq!(opus.attributes["gen_ai.cost.usd_estimate"], "0.115000");
        assert_eq!(opus.attributes["loom.tokens.cache_write_5m"], "4000");
        assert_eq!(opus.attributes["loom.tokens.cache_write_1h"], "3000");
        assert_eq!(opus.attributes["loom.tokens.cache_write"], "7000");
        assert_eq!(opus.attributes["loom.pricing.source"], pricing.source());
        assert_eq!(opus.attributes["loom.pricing.verified_on"], "2026-09-18");
        assert_eq!(opus.attributes["loom.usage.scope"], "attempt");
    }
}

#[test]
fn the_compiled_cards_verified_on_matches_the_shipped_asset() {
    let shipped = PricingCard::load(&shipped_asset_path()).unwrap();
    assert_eq!(
        shipped.verified_on().format("%Y-%m-%d").to_string(),
        COMPILED_VERIFIED_ON,
        "bump COMPILED_VERIFIED_ON with the compiled card"
    );
}

#[test]
fn an_unknown_model_gets_its_tokens_but_no_fabricated_cost() {
    let spans = spans_for(
        &[
            model("mystery-model-9", 5, 6, 7, 8, 9),
            model(UNATTRIBUTED, 1, 1, 0, 0, 0),
        ],
        &Pricing::with(None),
    );
    assert_eq!(spans.len(), 2);
    for span in &spans {
        assert!(span.attributes.contains_key("loom.tokens.total"));
        for key in [
            "loom.cost.usd_estimate",
            "gen_ai.cost.usd_estimate",
            "loom.pricing.verified_on",
            "loom.pricing.source",
        ] {
            assert!(!span.attributes.contains_key(key), "{key} on {:?}", span.attributes);
        }
    }
}
const UNATTRIBUTED: &str = crate::script_helpers::sweep_experiment::UNATTRIBUTED_MODEL;

#[test]
fn per_model_spans_sum_to_the_execution_total_and_fold_speed_tiers() {
    let mut rows = two_models();
    let mut fast = model("claude-opus-5", 1, 1, 1, 1, 1);
    fast.speed = "fast".into();
    rows.push(fast);
    assert_eq!(per_model(&rows).len(), 2, "speed/tier rows fold per model");
    let spans = spans_for(&rows, &Pricing::with(None));
    let total: i64 = spans
        .iter()
        .map(|s| s.attributes["loom.tokens.total"].parse::<i64>().unwrap())
        .sum();
    assert_eq!(total, TokenUsage::from_models(&rows).total());
}

#[test]
fn per_model_ids_are_distinct_and_stable_across_re_emits() {
    let parent = TraceContext::root(true);
    let at = Utc::now();
    let emit = |scope| {
        model_usage_spans(
            &parent,
            (at, at + Duration::seconds(1)),
            &two_models(),
            scope,
            &TraceAttributes::new(),
            &Pricing::with(None),
        )
    };
    let first = emit(UsageScope::Execution);
    let again = emit(UsageScope::Execution);
    let attempt = emit(UsageScope::Attempt);
    assert_ne!(first[0].context.span_id, first[1].context.span_id);
    assert_eq!(first[0].context, again[0].context);
    assert_eq!(first[1].context, again[1].context);
    assert_ne!(first[0].context.span_id, attempt[0].context.span_id, "scope is in the key");
}

#[test]
fn a_re_emitted_execution_usage_is_not_journalled_twice() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let store = TraceStore::new(root);
    let saved = store.load_or_create(root, "sweep-9").unwrap();
    let journal = Journal::for_context(&store.path(root, "sweep-9"));
    let t0 = Utc::now() - Duration::minutes(5);
    let run = journal
        .start(
            saved.context.child(),
            Some(&saved.context),
            SpanName::RuntimeRun,
            t0,
            TraceAttributes::new(),
        )
        .unwrap();
    journal
        .finish(&run, t0 + Duration::minutes(1), SpanStatus::Ok, TraceAttributes::new())
        .unwrap();
    let window = (t0, Utc::now());
    let rows = two_models();
    let first = journal_usage(root, "sweep-9", window, Some(&rows), Some("claude")).unwrap();
    assert_eq!(first.len(), 2);
    assert!(first
        .iter()
        .all(|s| s.attributes["loom.usage.scope"] == "execution"
            && s.attributes["loom.sweep_id"] == "sweep-9"
            && s.attributes["loom.runtime"] == "claude"));
    let second = journal_usage(root, "sweep-9", window, Some(&rows), Some("claude")).unwrap();
    assert!(second.is_empty(), "same deterministic ids: {second:?}");
}

#[test]
fn every_new_usage_key_survives_both_allowlists() {
    let spans = spans_for(&two_models(), &Pricing::with(None));
    for span in &spans {
        assert_eq!(span.clone().bounded(), *span, "every key survives the span allowlist");
    }
    let collector = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../defaults/observability/collector/config.yaml"),
    )
    .unwrap();
    for key in crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS {
        assert!(collector.contains(&format!("\"{key}\"")), "{key} missing from keep_keys");
    }
}
