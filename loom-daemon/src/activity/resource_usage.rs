//! Resource usage parsing and cost calculation.
//!
//! This module provides functionality for:
//! - Parsing token usage from Claude Code terminal output
//! - Calculating costs based on model pricing
//! - Creating `ResourceUsage` records
//!
//! # Usage
//!
//! ```ignore
//! use loom_daemon::activity::resource_usage::{parse_resource_usage, ModelPricing};
//!
//! let output = "Tokens: 1,234 in / 567 out\nModel: claude-3-5-sonnet";
//! if let Some(usage) = parse_resource_usage(output, Some(1500)) {
//!     println!("Cost: ${:.4}", usage.cost_usd);
//! }
//! ```

use chrono::{DateTime, Utc};
use regex::Regex;
use std::sync::LazyLock;

/// Parsed resource usage data from terminal output
#[derive(Debug, Clone, Default)]
pub struct ResourceUsage {
    pub input_id: Option<i64>,
    pub model: String,
    pub tokens_input: i64,
    pub tokens_output: i64,
    pub tokens_cache_read: Option<i64>,
    pub tokens_cache_write: Option<i64>,
    pub cost_usd: f64,
    pub duration_ms: Option<i64>,
    pub provider: String,
    pub timestamp: DateTime<Utc>,
}

/// Cache-rate multipliers, applied to a row's **own** base input price.
///
/// The vendor publishes cache rates as multiples of base input, not as
/// independent numbers, so deriving them keeps every row internally consistent
/// by construction. Hand-typing them is exactly how the pre-#8060 Haiku row
/// drifted to 0.12x / 1.2x of its own base instead of 0.1x / 1.25x.
const CACHE_WRITE_5M_MULTIPLIER: f64 = 1.25;

/// 1-hour-TTL cache writes cost 2x base input (vs. 1.25x for the 5m TTL).
const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;

/// Standard cache hit / refresh multiplier.
const CACHE_READ_MULTIPLIER: f64 = 0.1;

/// Cache hits and refreshes on Claude Fable 5.1 and Claude Mythos 5.1 are
/// priced at 0.025x base input; every other model uses the standard 0.1x.
const CACHE_READ_MULTIPLIER_REDUCED: f64 = 0.025;

/// Model pricing configuration (cost per 1000 tokens).
///
/// `cache_write_cost_per_1k` is the **5-minute-TTL** write rate, which is what
/// [`Self::calculate_cost`] charges: the token counters Loom parses out of
/// terminal output and transcripts report a single
/// `cache_creation_input_tokens` figure with no TTL breakdown, so there is
/// nothing to attribute to the 1h tier. `cache_write_1h_cost_per_1k` carries
/// the published 1h rate so a future caller that *can* distinguish the two
/// (e.g. a provider API that splits the counter) does not have to re-derive
/// it, and so the 1.6x under-count of a 1h-TTL write is visible in the type
/// rather than buried in a comment.
#[derive(Debug, Clone)]
#[allow(clippy::struct_field_names)]
pub struct ModelPricing {
    pub input_cost_per_1k: f64,
    pub output_cost_per_1k: f64,
    pub cache_read_cost_per_1k: f64,
    /// 5-minute-TTL cache write (1.25x base input).
    pub cache_write_cost_per_1k: f64,
    /// 1-hour-TTL cache write (2x base input). Not charged by
    /// [`Self::calculate_cost`] — see the struct doc.
    pub cache_write_1h_cost_per_1k: f64,
}

impl ModelPricing {
    /// Build a row from its published base input/output rates, deriving the
    /// three cache rates from `cache_read_multiplier` and the two cache-write
    /// multipliers so a row cannot silently desynchronize from its own base.
    fn from_base(
        input_cost_per_1k: f64,
        output_cost_per_1k: f64,
        cache_read_multiplier: f64,
    ) -> Self {
        Self {
            input_cost_per_1k,
            output_cost_per_1k,
            cache_read_cost_per_1k: input_cost_per_1k * cache_read_multiplier,
            cache_write_cost_per_1k: input_cost_per_1k * CACHE_WRITE_5M_MULTIPLIER,
            cache_write_1h_cost_per_1k: input_cost_per_1k * CACHE_WRITE_1H_MULTIPLIER,
        }
    }

    /// Look up a model in the published rate card.
    ///
    /// Returns `None` only for an ID that matches no row at all — the caller
    /// decides what to do about that (see [`Self::for_model`]).
    ///
    /// # Anthropic rate card
    ///
    /// Prices verified against
    /// <https://platform.claude.com/docs/en/about-claude/pricing> on
    /// **2026-09-17**, re-verified against the same live page on **2026-09-18**
    /// (every row below unchanged). USD per 1k tokens (= the published $/MTok
    /// / 1000):
    ///
    /// | Row | Input | Output |
    /// |---|---|---|
    /// | Fable 5.1 / Mythos 5.1 | 0.010 | 0.050 |
    /// | Fable 5 / Mythos 5 | 0.010 | 0.050 |
    /// | Opus 5 / 4.8 / 4.7 / 4.6 / 4.5 | 0.005 | 0.025 |
    /// | Opus 4.1 / 4 / 3 *(retired)* | 0.015 | 0.075 |
    /// | Sonnet 5 | 0.002 | 0.010 |
    /// | Sonnet 4.6 / 4.5 / 4 / 3.5 / 3 | 0.003 | 0.015 |
    /// | Haiku 4.5 | 0.001 | 0.005 |
    /// | Haiku 3.5 *(retired)* | 0.0008 | 0.004 |
    /// | Haiku 3 *(retired)* | 0.00025 | 0.00125 |
    ///
    /// Sonnet 5's $2/$10 is the **standard** price, not a lapsing promo: the
    /// same page records that the increase to $3/$15 scheduled for
    /// 2026-09-01 will not occur.
    ///
    /// # Matching
    ///
    /// Matching is keyed by **generation**, not by family stem (#8060). The
    /// old family-stem table was not generation-agnostic in any useful sense
    /// — it froze one generation's rates per family and applied them to every
    /// other, which is how `claude-opus-*` ended up billed at the retired Opus
    /// 4.1 rate (3x over) and `claude-haiku-*` at the Haiku 3 rate (4x under).
    ///
    /// An ID whose family is recognized but whose generation is not (a future
    /// `claude-opus-6`) pins to the **newest** generation of that family, never
    /// the oldest: an unrecognized ID is far more likely to be newer than the
    /// card than to be a resurrected retired model, and the old behaviour of
    /// inheriting a retired price is the specific defect #8060 was filed over.
    /// Legacy `claude-3-5-sonnet` / `claude-3-opus` / `claude-3-haiku` IDs
    /// predate the `-<generation>-` naming scheme and are matched explicitly.
    ///
    /// Bare tier aliases (`opus`, `sonnet`, `haiku`, `fable`, `mythos`) can
    /// reach here unresolved from the experiment harvest
    /// ([`crate::script_helpers::sweep_experiment::model_pricing`]), so they
    /// resolve to the newest generation of that family too. That harvest is
    /// the only other pricing entry point in the tree and it delegates here —
    /// there is exactly ONE pricing table, and `sweep_experiment`'s
    /// `model_pricing_agrees_with_the_shared_daemon_table` test holds the two
    /// entry points together.
    fn lookup(model: &str) -> Option<Self> {
        let lowered = model.to_lowercase();
        let m: &str = match lowered.as_str() {
            "sonnet" => "claude-sonnet-5",
            "opus" => "claude-opus-5",
            "haiku" => "claude-haiku-4-5",
            "fable" => "claude-fable-5-1",
            "mythos" => "claude-mythos-5-1",
            other => other,
        };

        // ---- Fable / Mythos: $10 / $50 per MTok ----------------------------
        // #3702's "fable has no published per-token rate, so price it as Opus"
        // premise expired: Fable 5 and 5.1 are on the published card, and at
        // 2x Opus, so the Opus fallback over-reported rather than erring
        // conservatively upward as its comment claimed.
        if m.contains("claude-fable-5-1") || m.contains("claude-mythos-5-1") {
            return Some(Self::from_base(0.010, 0.050, CACHE_READ_MULTIPLIER_REDUCED));
        }
        if m.contains("claude-fable-5") || m.contains("claude-mythos-5") {
            return Some(Self::from_base(0.010, 0.050, CACHE_READ_MULTIPLIER));
        }
        if m.contains("claude-fable-") || m.contains("claude-mythos-") {
            // Unknown generation -> newest published row (5.1).
            return Some(Self::from_base(0.010, 0.050, CACHE_READ_MULTIPLIER_REDUCED));
        }

        // ---- Opus ----------------------------------------------------------
        if m.contains("claude-opus-5")
            || m.contains("claude-opus-4-8")
            || m.contains("claude-opus-4-7")
            || m.contains("claude-opus-4-6")
            || m.contains("claude-opus-4-5")
        {
            return Some(Self::from_base(0.005, 0.025, CACHE_READ_MULTIPLIER));
        }
        if m.contains("claude-opus-4-1")
            || m.contains("claude-opus-4")
            || m.contains("claude-3-opus")
        {
            // Retired: Opus 4.1 / 4 / 3 at $15 / $75.
            return Some(Self::from_base(0.015, 0.075, CACHE_READ_MULTIPLIER));
        }
        if m.contains("claude-opus-") {
            // Unknown generation -> newest published row (Opus 5).
            return Some(Self::from_base(0.005, 0.025, CACHE_READ_MULTIPLIER));
        }

        // ---- Sonnet --------------------------------------------------------
        if m.contains("claude-sonnet-5") {
            return Some(Self::from_base(0.002, 0.010, CACHE_READ_MULTIPLIER));
        }
        if m.contains("claude-sonnet-4")
            || m.contains("claude-3-5-sonnet")
            || m.contains("claude-3-sonnet")
        {
            return Some(Self::from_base(0.003, 0.015, CACHE_READ_MULTIPLIER));
        }
        if m.contains("claude-sonnet-") {
            // Unknown generation -> newest published row (Sonnet 5).
            return Some(Self::from_base(0.002, 0.010, CACHE_READ_MULTIPLIER));
        }

        // ---- Haiku ---------------------------------------------------------
        if m.contains("claude-haiku-4-5") {
            return Some(Self::from_base(0.001, 0.005, CACHE_READ_MULTIPLIER));
        }
        if m.contains("claude-haiku-3-5") || m.contains("claude-3-5-haiku") {
            return Some(Self::from_base(0.0008, 0.004, CACHE_READ_MULTIPLIER));
        }
        if m.contains("claude-3-haiku") {
            return Some(Self::from_base(0.00025, 0.00125, CACHE_READ_MULTIPLIER));
        }
        if m.contains("claude-haiku-") {
            // Unknown generation -> newest published row (Haiku 4.5).
            return Some(Self::from_base(0.001, 0.005, CACHE_READ_MULTIPLIER));
        }

        // ---- OpenAI --------------------------------------------------------
        // Not generation-keyed and not re-verified by #8060; these rows are
        // unchanged and are reached only if some future caller records a GPT
        // model, which Loom does not dispatch today.
        if m.contains("gpt-4o") {
            return Some(Self {
                input_cost_per_1k: 0.005,
                output_cost_per_1k: 0.015,
                cache_read_cost_per_1k: 0.0025, // 50% discount for cached
                cache_write_cost_per_1k: 0.005,
                cache_write_1h_cost_per_1k: 0.005,
            });
        }
        if m.contains("gpt-4-turbo") {
            return Some(Self {
                input_cost_per_1k: 0.01,
                output_cost_per_1k: 0.03,
                cache_read_cost_per_1k: 0.005,
                cache_write_cost_per_1k: 0.01,
                cache_write_1h_cost_per_1k: 0.01,
            });
        }
        if m.contains("gpt-3.5") {
            return Some(Self {
                input_cost_per_1k: 0.0005,
                output_cost_per_1k: 0.0015,
                cache_read_cost_per_1k: 0.00025,
                cache_write_cost_per_1k: 0.0005,
                cache_write_1h_cost_per_1k: 0.0005,
            });
        }

        None
    }

    /// Whether `model` matches a row in the published rate card rather than
    /// falling through to the unknown-model default.
    ///
    /// Exposed so a test can assert the card still knows every model ID the
    /// fleet can dispatch — a fall-through is silent in production by design
    /// (the cost record still gets *a* number), so only a test can catch it.
    #[must_use]
    pub fn is_known_model(model: &str) -> bool {
        Self::lookup(model).is_some()
    }

    /// Get pricing for a given model.
    ///
    /// See [`Self::lookup`] for the rate card, its verification date and
    /// source, and the generation-matching rules.
    ///
    /// An ID that matches no family at all is logged at **warn** level and
    /// priced at the newest Sonnet row. It is warn rather than debug because
    /// the failure is otherwise invisible: a Mythos-class ID silently priced
    /// as Sonnet under-reports ~3.3x on input and 5x on output, and the only
    /// evidence would be a debug line nobody has enabled. An empty model
    /// string is the one exception — "no model was recorded" is a parse gap,
    /// not an unknown model, and is logged at debug.
    #[must_use]
    pub fn for_model(model: &str) -> Self {
        if let Some(pricing) = Self::lookup(model) {
            return pricing;
        }
        if model.is_empty() {
            log::debug!("No model recorded; using default Sonnet pricing");
        } else {
            log::warn!(
                "Unknown model '{model}' is not on the pricing card (checked 2026-09-17); \
                 cost is being estimated at the newest Sonnet rate and may be badly wrong"
            );
        }
        Self::from_base(0.002, 0.010, CACHE_READ_MULTIPLIER)
    }

    /// Calculate total cost for given token counts
    #[allow(clippy::cast_precision_loss)]
    pub fn calculate_cost(
        &self,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: Option<i64>,
        cache_write_tokens: Option<i64>,
    ) -> f64 {
        let input_cost = (input_tokens as f64 / 1000.0) * self.input_cost_per_1k;
        let output_cost = (output_tokens as f64 / 1000.0) * self.output_cost_per_1k;

        let cache_read_cost =
            cache_read_tokens.map_or(0.0, |t| (t as f64 / 1000.0) * self.cache_read_cost_per_1k);

        let cache_write_cost =
            cache_write_tokens.map_or(0.0, |t| (t as f64 / 1000.0) * self.cache_write_cost_per_1k);

        input_cost + output_cost + cache_read_cost + cache_write_cost
    }
}

/// Detect provider from model name
pub fn detect_provider(model: &str) -> &'static str {
    let model_lower = model.to_lowercase();
    if model_lower.contains("claude") {
        "anthropic"
    } else if model_lower.contains("gpt") || model_lower.contains("o1") {
        "openai"
    } else if model_lower.contains("gemini") {
        "google"
    } else if model_lower.contains("llama") || model_lower.contains("mistral") {
        "meta"
    } else {
        "unknown"
    }
}

// Regex patterns for parsing Claude Code output
// These are compiled once and reused
// Note: expect() is appropriate here since these are compile-time constant patterns

/// Pattern: "Total tokens: 1,234 in / 567 out"
#[allow(clippy::expect_used)]
static TOKEN_PATTERN_SLASH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:total\s+)?tokens?:\s*([\d,]+)\s*(?:in|input)\s*/\s*([\d,]+)\s*(?:out|output)",
    )
    .expect("Invalid regex")
});

/// Pattern: "Input: 1,234 tokens, Output: 567 tokens"
#[allow(clippy::expect_used)]
static TOKEN_PATTERN_LABELED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)input:\s*([\d,]+)\s*tokens?.*?output:\s*([\d,]+)\s*tokens?")
        .expect("Invalid regex")
});

/// Pattern: "Cache read: 1,234 tokens" or `cache_read_input_tokens`: 1234
#[allow(clippy::expect_used)]
static CACHE_READ_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // Match both "cache read: 200 tokens" and "cache_read_input_tokens: 200"
    Regex::new(r"(?i)cache[_\s]*read[_\s]*(?:input[_\s]*)?(?:tokens?)?[:\s]*([\d,]+)")
        .expect("Invalid regex")
});

/// Pattern: "Cache write: 1,234 tokens" or `cache_creation_input_tokens`: 1234
#[allow(clippy::expect_used)]
static CACHE_WRITE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // Match both "cache write: 50 tokens" and "cache_creation_input_tokens: 50"
    Regex::new(r"(?i)cache[_\s]*(?:write|creation)[_\s]*(?:input[_\s]*)?(?:tokens?)?[:\s]*([\d,]+)")
        .expect("Invalid regex")
});

/// Pattern: Model name detection - "Model: claude-3-5-sonnet" or "using claude-sonnet-4"
#[allow(clippy::expect_used)]
static MODEL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:model[:\s]+|using\s+)(claude[a-z0-9-]+|gpt-[a-z0-9.-]+|o1[a-z0-9-]*|gemini[a-z0-9-]*)")
        .expect("Invalid regex")
});

/// Pattern for extracting duration: "Duration: 5.2s" or "took 5200ms"
#[allow(clippy::expect_used)]
static DURATION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:duration|took|time)[:\s]*([\d.]+)\s*(ms|s|seconds?|milliseconds?)")
        .expect("Invalid regex")
});

/// Parse a number that may contain commas
fn parse_token_count(s: &str) -> Option<i64> {
    s.replace(',', "").parse().ok()
}

/// Parse resource usage from terminal output
///
/// Attempts to extract token usage, model information, and timing from
/// Claude Code or similar AI tool output.
pub fn parse_resource_usage(output: &str, duration_ms: Option<i64>) -> Option<ResourceUsage> {
    // Try to extract input/output tokens
    let (tokens_input, tokens_output) = extract_tokens(output)?;

    // Extract cache tokens (optional)
    let tokens_cache_read = CACHE_READ_PATTERN
        .captures(output)
        .and_then(|c| c.get(1))
        .and_then(|m| parse_token_count(m.as_str()));

    let tokens_cache_write = CACHE_WRITE_PATTERN
        .captures(output)
        .and_then(|c| c.get(1))
        .and_then(|m| parse_token_count(m.as_str()));

    // Extract model name. When the output carries none, fall back to the
    // current default tier rather than to a retired generation: before #8060
    // this named `claude-sonnet-4`, so every model-less record was costed at a
    // superseded rate forever.
    let model = MODEL_PATTERN
        .captures(output)
        .and_then(|c| c.get(1))
        .map_or_else(|| "claude-sonnet-5".to_string(), |m| m.as_str().to_string());

    // Extract or use provided duration
    let duration = duration_ms.or_else(|| extract_duration(output));

    // Determine provider
    let provider = detect_provider(&model).to_string();

    // Calculate cost
    let pricing = ModelPricing::for_model(&model);
    let cost_usd =
        pricing.calculate_cost(tokens_input, tokens_output, tokens_cache_read, tokens_cache_write);

    Some(ResourceUsage {
        input_id: None,
        model,
        tokens_input,
        tokens_output,
        tokens_cache_read,
        tokens_cache_write,
        cost_usd,
        duration_ms: duration,
        provider,
        timestamp: Utc::now(),
    })
}

/// Extract input and output token counts from text
fn extract_tokens(text: &str) -> Option<(i64, i64)> {
    // Try "X in / Y out" pattern first
    if let Some(caps) = TOKEN_PATTERN_SLASH.captures(text) {
        let input = caps.get(1).and_then(|m| parse_token_count(m.as_str()))?;
        let output = caps.get(2).and_then(|m| parse_token_count(m.as_str()))?;
        return Some((input, output));
    }

    // Try "Input: X, Output: Y" pattern
    if let Some(caps) = TOKEN_PATTERN_LABELED.captures(text) {
        let input = caps.get(1).and_then(|m| parse_token_count(m.as_str()))?;
        let output = caps.get(2).and_then(|m| parse_token_count(m.as_str()))?;
        return Some((input, output));
    }

    None
}

/// Extract duration from text
#[allow(clippy::cast_possible_truncation)]
fn extract_duration(text: &str) -> Option<i64> {
    let caps = DURATION_PATTERN.captures(text)?;
    let value: f64 = caps.get(1)?.as_str().parse().ok()?;
    let unit = caps.get(2)?.as_str().to_lowercase();

    let ms = if unit.starts_with("ms") || unit.starts_with("milli") {
        value as i64
    } else {
        // Assume seconds
        (value * 1000.0) as i64
    };

    Some(ms)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tokens_slash_format() {
        let output = "Tokens: 1,234 in / 567 out";
        let usage = parse_resource_usage(output, None).unwrap();
        assert_eq!(usage.tokens_input, 1234);
        assert_eq!(usage.tokens_output, 567);
    }

    #[test]
    fn test_parse_tokens_labeled_format() {
        let output = "Input: 1500 tokens, Output: 800 tokens";
        let usage = parse_resource_usage(output, None).unwrap();
        assert_eq!(usage.tokens_input, 1500);
        assert_eq!(usage.tokens_output, 800);
    }

    #[test]
    fn test_parse_cache_tokens() {
        let output = "Tokens: 1000 in / 500 out\nCache read: 200 tokens\nCache write: 50 tokens";
        let usage = parse_resource_usage(output, None).unwrap();
        assert_eq!(usage.tokens_input, 1000);
        assert_eq!(usage.tokens_output, 500);
        assert_eq!(usage.tokens_cache_read, Some(200));
        assert_eq!(usage.tokens_cache_write, Some(50));
    }

    #[test]
    fn test_parse_model_name() {
        let output = "Model: claude-3-5-sonnet\nTokens: 1000 in / 500 out";
        let usage = parse_resource_usage(output, None).unwrap();
        assert_eq!(usage.model, "claude-3-5-sonnet");
    }

    #[test]
    fn test_detect_provider() {
        assert_eq!(detect_provider("claude-3-5-sonnet"), "anthropic");
        assert_eq!(detect_provider("gpt-4o"), "openai");
        assert_eq!(detect_provider("gemini-pro"), "google");
        assert_eq!(detect_provider("unknown-model"), "unknown");
    }

    #[test]
    fn test_calculate_cost_sonnet() {
        let pricing = ModelPricing::for_model("claude-3-5-sonnet");
        // 1000 input tokens @ $0.003/1k = $0.003
        // 500 output tokens @ $0.015/1k = $0.0075
        // Total = $0.0105
        let cost = pricing.calculate_cost(1000, 500, None, None);
        assert!((cost - 0.0105).abs() < 0.0001);
    }

    #[test]
    fn test_calculate_cost_with_cache() {
        let pricing = ModelPricing::for_model("claude-3-5-sonnet");
        // 1000 input @ $0.003/1k = $0.003
        // 500 output @ $0.015/1k = $0.0075
        // 200 cache read @ $0.0003/1k = $0.00006
        // 50 cache write @ $0.00375/1k = $0.0001875
        // Total = $0.0108375
        let cost = pricing.calculate_cost(1000, 500, Some(200), Some(50));
        assert!((cost - 0.010_837_5).abs() < 0.0001);
    }

    // ---- #8060: generation-keyed rate card -------------------------------
    //
    // Supersedes the #3981 regression tests that asserted the *family-stem*
    // behaviour (one frozen generation's rates applied to every generation of
    // a family). #3981's actual invariant — a gen-5 ID must not fall through
    // to the unknown-model default — is preserved below; what changed is that
    // each generation now carries its own published rate.

    /// Every model ID the fleet can dispatch, or that appears as a pinned ID
    /// anywhere in the tree, plus the bare tier aliases. A fall-through to the
    /// unknown-model default is silent in production (the record still gets a
    /// number), so this list is the only thing that can catch one.
    const KNOWN_MODEL_IDS: &[&str] = &[
        "sonnet",
        "opus",
        "haiku",
        "fable",
        "mythos",
        "claude-sonnet-5",
        "claude-sonnet-4-6",
        "claude-sonnet-4-5",
        "claude-sonnet-4",
        "claude-opus-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-opus-4-5",
        "claude-opus-4-1",
        "claude-opus-4",
        "claude-fable-5-1",
        "claude-fable-5",
        "claude-mythos-5-1",
        "claude-mythos-5",
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
        "claude-haiku-3-5",
        "claude-3-5-sonnet",
        "claude-3-opus",
        "claude-3-haiku",
    ];

    #[test]
    fn pricing_card_knows_every_dispatchable_model_id() {
        for id in KNOWN_MODEL_IDS {
            assert!(
                ModelPricing::is_known_model(id),
                "'{id}' is not on the pricing card — it would be costed at the \
                 unknown-model default. Add its row (rates verified against \
                 https://platform.claude.com/docs/en/about-claude/pricing)."
            );
        }
        // Control: the detector is not vacuously true.
        assert!(!ModelPricing::is_known_model("totally-not-a-model"));
    }

    #[test]
    fn pricing_rows_carry_their_published_rates() {
        // USD per 1k tokens, verified 2026-09-17 against
        // https://platform.claude.com/docs/en/about-claude/pricing
        let expected: &[(&str, f64, f64)] = &[
            ("claude-sonnet-5", 0.002, 0.010),
            ("claude-sonnet-4-6", 0.003, 0.015),
            ("claude-sonnet-4-5", 0.003, 0.015),
            ("claude-sonnet-4", 0.003, 0.015),
            ("claude-3-5-sonnet", 0.003, 0.015),
            ("claude-opus-5", 0.005, 0.025),
            ("claude-opus-4-8", 0.005, 0.025),
            ("claude-opus-4-7", 0.005, 0.025),
            ("claude-opus-4-6", 0.005, 0.025),
            ("claude-opus-4-5", 0.005, 0.025),
            ("claude-opus-4-1", 0.015, 0.075),
            ("claude-opus-4", 0.015, 0.075),
            ("claude-3-opus", 0.015, 0.075),
            ("claude-fable-5-1", 0.010, 0.050),
            ("claude-fable-5", 0.010, 0.050),
            ("claude-mythos-5-1", 0.010, 0.050),
            ("claude-mythos-5", 0.010, 0.050),
            ("claude-haiku-4-5", 0.001, 0.005),
            ("claude-haiku-4-5-20251001", 0.001, 0.005),
            ("claude-haiku-3-5", 0.0008, 0.004),
            ("claude-3-haiku", 0.00025, 0.00125),
        ];
        for (id, input, output) in expected {
            let p = ModelPricing::for_model(id);
            assert!(
                (p.input_cost_per_1k - input).abs() < f64::EPSILON,
                "{id}: input {} != {input}",
                p.input_cost_per_1k
            );
            assert!(
                (p.output_cost_per_1k - output).abs() < f64::EPSILON,
                "{id}: output {} != {output}",
                p.output_cost_per_1k
            );
        }
    }

    #[test]
    fn cache_rates_are_derived_from_each_row_own_base() {
        // The defect this test exists to prevent: before #8060 the Haiku row's
        // hand-typed cache rates were 0.12x / 1.2x of its own base instead of
        // 0.1x / 1.25x — internally inconsistent, and invisible without this.
        for id in KNOWN_MODEL_IDS {
            let p = ModelPricing::for_model(id);
            let reduced = id.contains("fable") || id.contains("mythos");
            let expected_read_multiplier = if reduced && !id.ends_with("-5") {
                // Fable/Mythos 5.1 (and the bare `fable`/`mythos` aliases,
                // which resolve to 5.1) price cache hits at 0.025x.
                CACHE_READ_MULTIPLIER_REDUCED
            } else {
                CACHE_READ_MULTIPLIER
            };
            let base = p.input_cost_per_1k;
            assert!(
                (p.cache_read_cost_per_1k - base * expected_read_multiplier).abs() < f64::EPSILON,
                "{id}: cache read {} is not {expected_read_multiplier}x of base {base}",
                p.cache_read_cost_per_1k
            );
            assert!(
                (p.cache_write_cost_per_1k - base * CACHE_WRITE_5M_MULTIPLIER).abs() < f64::EPSILON,
                "{id}: 5m cache write {} is not 1.25x of base {base}",
                p.cache_write_cost_per_1k
            );
            assert!(
                (p.cache_write_1h_cost_per_1k - base * CACHE_WRITE_1H_MULTIPLIER).abs()
                    < f64::EPSILON,
                "{id}: 1h cache write {} is not 2x of base {base}",
                p.cache_write_1h_cost_per_1k
            );
        }
    }

    #[test]
    fn fable_5_1_cache_hits_use_the_reduced_multiplier() {
        // The published footnote: Fable 5.1 and Mythos 5.1 price cache hits at
        // 0.025x base input; everything else uses 0.1x.
        let f51 = ModelPricing::for_model("claude-fable-5-1");
        assert!((f51.cache_read_cost_per_1k - 0.000_25).abs() < f64::EPSILON);
        let f5 = ModelPricing::for_model("claude-fable-5");
        assert!((f5.cache_read_cost_per_1k - 0.001).abs() < f64::EPSILON);
        let m51 = ModelPricing::for_model("claude-mythos-5-1");
        assert!((m51.cache_read_cost_per_1k - 0.000_25).abs() < f64::EPSILON);
    }

    #[test]
    fn fable_is_not_priced_as_opus() {
        // #3702's "no published per-token rate, so price it as Opus" premise
        // expired: Fable is on the published card at 2x Opus, so the old
        // fallback over-reported rather than erring conservatively upward.
        let fable = ModelPricing::for_model("claude-fable-5-1");
        let opus = ModelPricing::for_model("claude-opus-5");
        assert!((fable.input_cost_per_1k - 0.010).abs() < f64::EPSILON);
        assert!((fable.input_cost_per_1k - opus.input_cost_per_1k).abs() > f64::EPSILON);
        assert!(fable.input_cost_per_1k > opus.input_cost_per_1k);
    }

    #[test]
    fn gen5_ids_do_not_fall_through_to_the_default() {
        // The #3981 invariant, preserved: a gen-5 ID must be recognized, not
        // silently priced at the unknown-model default.
        for id in ["claude-sonnet-5", "claude-opus-5", "claude-fable-5"] {
            assert!(ModelPricing::is_known_model(id), "{id} fell through");
        }
        let opus5 = ModelPricing::for_model("claude-opus-5");
        assert!((opus5.input_cost_per_1k - 0.005).abs() < f64::EPSILON);
        assert!((opus5.output_cost_per_1k - 0.025).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_generation_pins_to_the_newest_row_not_the_oldest() {
        // The specific #8060 defect: the family row was frozen at a RETIRED
        // generation, so every unrecognized ID inherited a retired price.
        let opus6 = ModelPricing::for_model("claude-opus-6");
        assert!(
            (opus6.input_cost_per_1k - 0.005).abs() < f64::EPSILON,
            "opus fallback is retired"
        );
        let sonnet99 = ModelPricing::for_model("claude-sonnet-99");
        assert!((sonnet99.input_cost_per_1k - 0.002).abs() < f64::EPSILON);
        let haiku9 = ModelPricing::for_model("claude-haiku-9");
        assert!((haiku9.input_cost_per_1k - 0.001).abs() < f64::EPSILON);
        let fable9 = ModelPricing::for_model("claude-fable-9");
        assert!((fable9.input_cost_per_1k - 0.010).abs() < f64::EPSILON);
        assert!((fable9.cache_read_cost_per_1k - 0.000_25).abs() < f64::EPSILON);
    }

    #[test]
    fn bare_tier_aliases_resolve_to_the_newest_generation() {
        assert!((ModelPricing::for_model("sonnet").input_cost_per_1k - 0.002).abs() < f64::EPSILON);
        assert!((ModelPricing::for_model("opus").input_cost_per_1k - 0.005).abs() < f64::EPSILON);
        assert!((ModelPricing::for_model("haiku").input_cost_per_1k - 0.001).abs() < f64::EPSILON);
        assert!((ModelPricing::for_model("fable").input_cost_per_1k - 0.010).abs() < f64::EPSILON);
        // Case-insensitively, too.
        assert!((ModelPricing::for_model("OPUS").input_cost_per_1k - 0.005).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_model_falls_back_to_the_newest_sonnet_row() {
        let p = ModelPricing::for_model("totally-unknown");
        assert!(!ModelPricing::is_known_model("totally-unknown"));
        assert!((p.input_cost_per_1k - 0.002).abs() < f64::EPSILON);
        assert!((p.output_cost_per_1k - 0.010).abs() < f64::EPSILON);
        // An absent model string takes the same default.
        let empty = ModelPricing::for_model("");
        assert!((empty.input_cost_per_1k - 0.002).abs() < f64::EPSILON);
    }

    #[test]
    fn opus_is_two_and_a_half_times_sonnet() {
        // The figure the issue body asks to be re-derivable: Opus 5 $5/$25 vs
        // Sonnet 5 $2/$10 is 2.5x on both axes, not the 5x the retired-Opus
        // row produced.
        let opus = ModelPricing::for_model("claude-opus-5");
        let sonnet = ModelPricing::for_model("claude-sonnet-5");
        assert!((opus.input_cost_per_1k / sonnet.input_cost_per_1k - 2.5).abs() < 1e-9);
        assert!((opus.output_cost_per_1k / sonnet.output_cost_per_1k - 2.5).abs() < 1e-9);
    }

    #[test]
    fn test_parse_duration_seconds() {
        let output = "Tokens: 1000 in / 500 out\nDuration: 5.2s";
        let usage = parse_resource_usage(output, None).unwrap();
        assert_eq!(usage.duration_ms, Some(5200));
    }

    #[test]
    fn test_parse_duration_milliseconds() {
        let output = "Tokens: 1000 in / 500 out\ntook 3500ms";
        let usage = parse_resource_usage(output, None).unwrap();
        assert_eq!(usage.duration_ms, Some(3500));
    }

    #[test]
    fn test_provided_duration_overrides() {
        let output = "Tokens: 1000 in / 500 out\nDuration: 5s";
        let usage = parse_resource_usage(output, Some(1234)).unwrap();
        assert_eq!(usage.duration_ms, Some(1234));
    }

    #[test]
    fn test_no_tokens_returns_none() {
        let output = "Some random output without token information";
        assert!(parse_resource_usage(output, None).is_none());
    }

    #[test]
    fn test_cost_calculation_included() {
        let output = "Model: claude-3-5-sonnet\nTokens: 1000 in / 500 out";
        let usage = parse_resource_usage(output, None).unwrap();
        assert!(usage.cost_usd > 0.0);
        assert!((usage.cost_usd - 0.0105).abs() < 0.0001);
    }

    #[test]
    fn test_provider_set_correctly() {
        let output = "Model: claude-opus-4\nTokens: 1000 in / 500 out";
        let usage = parse_resource_usage(output, None).unwrap();
        assert_eq!(usage.provider, "anthropic");
    }
}
