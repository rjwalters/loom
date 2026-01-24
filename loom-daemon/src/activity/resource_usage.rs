//! Resource usage parsing and cost calculation.
//!
//! This module provides functionality for:
//! - Parsing token usage from Claude Code terminal output
//! - Calculating costs based on model pricing
//! - Creating ResourceUsage records
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
#[allow(dead_code)]
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

/// Model pricing configuration (cost per 1000 tokens)
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ModelPricing {
    pub input_cost_per_1k: f64,
    pub output_cost_per_1k: f64,
    pub cache_read_cost_per_1k: f64,
    pub cache_write_cost_per_1k: f64,
}

impl ModelPricing {
    /// Get pricing for a given model
    pub fn for_model(model: &str) -> Self {
        // Normalize model name for matching
        let model_lower = model.to_lowercase();

        // Anthropic models (prices as of Jan 2025)
        if model_lower.contains("claude-3-5-sonnet") || model_lower.contains("claude-sonnet-4") {
            Self {
                input_cost_per_1k: 0.003,
                output_cost_per_1k: 0.015,
                cache_read_cost_per_1k: 0.0003,
                cache_write_cost_per_1k: 0.00375,
            }
        } else if model_lower.contains("claude-3-opus") || model_lower.contains("claude-opus-4") {
            Self {
                input_cost_per_1k: 0.015,
                output_cost_per_1k: 0.075,
                cache_read_cost_per_1k: 0.0015,
                cache_write_cost_per_1k: 0.01875,
            }
        } else if model_lower.contains("claude-3-haiku") {
            Self {
                input_cost_per_1k: 0.00025,
                output_cost_per_1k: 0.00125,
                cache_read_cost_per_1k: 0.00003,
                cache_write_cost_per_1k: 0.0003,
            }
        } else if model_lower.contains("gpt-4o") {
            // OpenAI GPT-4o pricing
            Self {
                input_cost_per_1k: 0.005,
                output_cost_per_1k: 0.015,
                cache_read_cost_per_1k: 0.0025, // 50% discount for cached
                cache_write_cost_per_1k: 0.005,
            }
        } else if model_lower.contains("gpt-4-turbo") {
            Self {
                input_cost_per_1k: 0.01,
                output_cost_per_1k: 0.03,
                cache_read_cost_per_1k: 0.005,
                cache_write_cost_per_1k: 0.01,
            }
        } else if model_lower.contains("gpt-3.5") {
            Self {
                input_cost_per_1k: 0.0005,
                output_cost_per_1k: 0.0015,
                cache_read_cost_per_1k: 0.00025,
                cache_write_cost_per_1k: 0.0005,
            }
        } else {
            // Default to Claude Sonnet pricing as reasonable middle ground
            log::debug!("Unknown model '{}', using default Sonnet pricing", model);
            Self {
                input_cost_per_1k: 0.003,
                output_cost_per_1k: 0.015,
                cache_read_cost_per_1k: 0.0003,
                cache_write_cost_per_1k: 0.00375,
            }
        }
    }

    /// Calculate total cost for given token counts
    pub fn calculate_cost(
        &self,
        input_tokens: i64,
        output_tokens: i64,
        cache_read_tokens: Option<i64>,
        cache_write_tokens: Option<i64>,
    ) -> f64 {
        let input_cost = (input_tokens as f64 / 1000.0) * self.input_cost_per_1k;
        let output_cost = (output_tokens as f64 / 1000.0) * self.output_cost_per_1k;

        let cache_read_cost = cache_read_tokens
            .map(|t| (t as f64 / 1000.0) * self.cache_read_cost_per_1k)
            .unwrap_or(0.0);

        let cache_write_cost = cache_write_tokens
            .map(|t| (t as f64 / 1000.0) * self.cache_write_cost_per_1k)
            .unwrap_or(0.0);

        input_cost + output_cost + cache_read_cost + cache_write_cost
    }
}

/// Detect provider from model name
#[allow(dead_code)]
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
#[allow(dead_code)]

/// Pattern: "Total tokens: 1,234 in / 567 out"
static TOKEN_PATTERN_SLASH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:total\s+)?tokens?:\s*([\d,]+)\s*(?:in|input)\s*/\s*([\d,]+)\s*(?:out|output)",
    )
    .expect("Invalid regex")
});

/// Pattern: "Input: 1,234 tokens, Output: 567 tokens"
#[allow(dead_code)]
static TOKEN_PATTERN_LABELED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)input:\s*([\d,]+)\s*tokens?.*?output:\s*([\d,]+)\s*tokens?")
        .expect("Invalid regex")
});

/// Pattern: "Cache read: 1,234 tokens" or "cache_read_input_tokens: 1234"
#[allow(dead_code)]
static CACHE_READ_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // Match both "cache read: 200 tokens" and "cache_read_input_tokens: 200"
    Regex::new(r"(?i)cache[_\s]*read[_\s]*(?:input[_\s]*)?(?:tokens?)?[:\s]*([\d,]+)")
        .expect("Invalid regex")
});

/// Pattern: "Cache write: 1,234 tokens" or "cache_creation_input_tokens: 1234"
#[allow(dead_code)]
static CACHE_WRITE_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    // Match both "cache write: 50 tokens" and "cache_creation_input_tokens: 50"
    Regex::new(r"(?i)cache[_\s]*(?:write|creation)[_\s]*(?:input[_\s]*)?(?:tokens?)?[:\s]*([\d,]+)")
        .expect("Invalid regex")
});

/// Pattern: Model name detection - "Model: claude-3-5-sonnet" or "using claude-sonnet-4"
#[allow(dead_code)]
static MODEL_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:model[:\s]+|using\s+)(claude[a-z0-9-]+|gpt-[a-z0-9.-]+|o1[a-z0-9-]*|gemini[a-z0-9-]*)")
        .expect("Invalid regex")
});

/// Pattern for extracting duration: "Duration: 5.2s" or "took 5200ms"
#[allow(dead_code)]
static DURATION_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:duration|took|time)[:\s]*([\d.]+)\s*(ms|s|seconds?|milliseconds?)")
        .expect("Invalid regex")
});

/// Parse a number that may contain commas
#[allow(dead_code)]
fn parse_token_count(s: &str) -> Option<i64> {
    s.replace(',', "").parse().ok()
}

/// Parse resource usage from terminal output
///
/// Attempts to extract token usage, model information, and timing from
/// Claude Code or similar AI tool output.
#[allow(dead_code)]
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

    // Extract model name
    let model = MODEL_PATTERN
        .captures(output)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "claude-sonnet-4".to_string());

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
#[allow(dead_code)]
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
#[allow(dead_code)]
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
        assert!((cost - 0.0108375).abs() < 0.0001);
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
