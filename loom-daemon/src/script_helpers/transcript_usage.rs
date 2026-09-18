//! One shared decoding of a Claude Code transcript record's `usage` block
//! (issue #8059).
//!
//! Two consumers read the same records for different reasons and must not
//! drift apart about what a record cost:
//!
//! - [`super::sweep_experiment::sum_transcript_usage_by_model`] folds them into
//!   per-`(model, speed, service_tier)` totals for the safehouse completion
//!   feed and `sweep.outcome` (#5740).
//! - [`crate::activity::transcript_ingest`] folds them — deduped on
//!   `message.id` — into `activity.db`'s `resource_usage` rows (#8059).
//!
//! The fiddly parts live here once: the `message`-or-self container unwrap,
//! the `<synthetic>`/empty-model bucketing, the `speed`/`service_tier`
//! defaults, and the 5-minute/1-hour cache-write split with its older
//! flat-only fallback.

use serde_json::Value;

/// Bucket for a usage block whose `model` is absent, or is the literal
/// `"<synthetic>"` Claude Code stamps on certain internal/tool-echo messages
/// (issue #5740). Grouped explicitly rather than dropped, so the sum across
/// every [`super::sweep_experiment::sum_transcript_usage_by_model`] entry still
/// reconciles against the flat total
/// [`super::sweep_experiment::sum_transcript_usage`] reports for the same file.
pub const UNATTRIBUTED_MODEL: &str = "<unattributed>";

/// One transcript record's `usage` block, normalized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRecord {
    /// `message.id` — the API message identifier. Streamed chunks repeat the
    /// id, each carrying the **cumulative** usage for that message, so a
    /// consumer that must not over-count folds by this key instead of summing
    /// (see [`crate::activity::transcript_ingest`]). `None` for a record that
    /// carries usage without an id.
    pub message_id: Option<String>,
    /// Record-level ISO-8601 `timestamp`, when present.
    pub timestamp: Option<String>,
    /// Model as grouped: the raw model id, or [`UNATTRIBUTED_MODEL`] when the
    /// record's model is absent, empty, or the literal `"<synthetic>"`.
    pub model: String,
    /// Whether the raw model was the literal `"<synthetic>"` — Claude Code's
    /// marker for internal/tool-echo messages, which #8052's method notes
    /// exclude from billing-shaped accounting.
    pub synthetic: bool,
    pub speed: String,
    pub service_tier: String,
    pub input: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub output: i64,
}

/// Decode one already-parsed transcript record, or `None` when it carries no
/// `usage` block.
///
/// Takes the parsed `Value` rather than the raw line so a caller that also
/// needs other fields of the same record (`sessionId`, `cwd`, the first user
/// message) parses each line exactly once.
///
/// When the nested `cache_creation` object is absent (an older transcript
/// format that only wrote the flat `cache_creation_input_tokens`), the whole
/// flat value is attributed to the 1-hour bucket — measured transcripts show
/// the 1-hour bucket outweighing the 5-minute one by ~212x, so that is the
/// safer default and it keeps the split reconciling against the flat total
/// rather than silently under-counting. `speed`/`service_tier` default to
/// `"standard"` when absent, matching both observed values and the pricing
/// table's own default.
#[must_use]
#[allow(clippy::cast_possible_truncation)]
pub fn usage_from_record(obj: &Value) -> Option<UsageRecord> {
    let container = obj.get("message").filter(|m| m.is_object()).unwrap_or(obj);
    let container = container.as_object()?;
    let usage = container.get("usage").and_then(Value::as_object)?;

    let raw_model = container.get("model").and_then(Value::as_str).unwrap_or("");
    let synthetic = raw_model == "<synthetic>";
    let model = if raw_model.is_empty() || synthetic {
        UNATTRIBUTED_MODEL.to_string()
    } else {
        raw_model.to_string()
    };
    let text_field = |map: &serde_json::Map<String, Value>, key: &str, default: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_string()
    };

    let get = |k: &str| usage.get(k).and_then(Value::as_f64).unwrap_or(0.0) as i64;
    let (cache_write_5m, cache_write_1h) =
        match usage.get("cache_creation").and_then(Value::as_object) {
            Some(c) => {
                let field = |k: &str| c.get(k).and_then(Value::as_f64).unwrap_or(0.0) as i64;
                (field("ephemeral_5m_input_tokens"), field("ephemeral_1h_input_tokens"))
            }
            None => (0, get("cache_creation_input_tokens")),
        };

    Some(UsageRecord {
        message_id: container
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string),
        timestamp: obj
            .get("timestamp")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string),
        model,
        synthetic,
        speed: text_field(usage, "speed", "standard"),
        service_tier: text_field(usage, "service_tier", "standard"),
        input: get("input_tokens"),
        cache_read: get("cache_read_input_tokens"),
        cache_write_5m,
        cache_write_1h,
        output: get("output_tokens"),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn record(raw: &str) -> Option<UsageRecord> {
        usage_from_record(&serde_json::from_str::<Value>(raw).unwrap())
    }

    #[test]
    fn decodes_a_full_assistant_record() {
        let rec = record(
            r#"{"type":"assistant","timestamp":"2026-09-18T03:59:57.121Z","sessionId":"s1",
                "message":{"model":"claude-sonnet-5","id":"msg_1","usage":{
                  "input_tokens":2,"output_tokens":182,"cache_read_input_tokens":29616,
                  "cache_creation_input_tokens":19868,"service_tier":"standard",
                  "cache_creation":{"ephemeral_1h_input_tokens":19868,"ephemeral_5m_input_tokens":0}}}}"#,
        )
        .expect("record carries usage");

        assert_eq!(rec.message_id.as_deref(), Some("msg_1"));
        assert_eq!(rec.timestamp.as_deref(), Some("2026-09-18T03:59:57.121Z"));
        assert_eq!(rec.model, "claude-sonnet-5");
        assert!(!rec.synthetic);
        assert_eq!(rec.input, 2);
        assert_eq!(rec.output, 182);
        assert_eq!(rec.cache_read, 29616);
        assert_eq!(rec.cache_write_5m, 0);
        assert_eq!(rec.cache_write_1h, 19868);
        assert_eq!(rec.speed, "standard");
        assert_eq!(rec.service_tier, "standard");
    }

    #[test]
    fn a_record_without_usage_is_not_a_usage_record() {
        assert!(record(r#"{"type":"user","message":{"role":"user","content":"hi"}}"#).is_none());
        assert!(record(r#"{"type":"queue-operation","content":"/loom:sweep 1"}"#).is_none());
    }

    #[test]
    fn synthetic_and_missing_models_bucket_as_unattributed_but_stay_distinguishable() {
        let synthetic = record(
            r#"{"message":{"model":"<synthetic>","id":"m","usage":{"input_tokens":5,"output_tokens":1}}}"#,
        )
        .unwrap();
        assert_eq!(synthetic.model, UNATTRIBUTED_MODEL);
        assert!(synthetic.synthetic, "the <synthetic> marker must survive decoding");

        let missing = record(r#"{"message":{"id":"m","usage":{"input_tokens":5}}}"#).unwrap();
        assert_eq!(missing.model, UNATTRIBUTED_MODEL);
        assert!(
            !missing.synthetic,
            "an absent model is unattributed but is NOT the <synthetic> marker"
        );
    }

    #[test]
    fn a_flat_only_cache_creation_lands_entirely_in_the_one_hour_bucket() {
        let rec = record(
            r#"{"message":{"model":"claude-opus-5","id":"m","usage":{
                "input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":900}}}"#,
        )
        .unwrap();
        assert_eq!(rec.cache_write_5m, 0);
        assert_eq!(rec.cache_write_1h, 900);
    }

    #[test]
    fn usage_without_a_message_wrapper_is_still_decoded() {
        // Some records carry `usage` at the top level rather than under
        // `message` — the container unwrap must accept both shapes.
        let rec = record(r#"{"model":"claude-haiku-5","usage":{"input_tokens":7}}"#).unwrap();
        assert_eq!(rec.model, "claude-haiku-5");
        assert_eq!(rec.input, 7);
        assert_eq!(rec.message_id, None);
    }
}
