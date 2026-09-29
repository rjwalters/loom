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

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::sweep_experiment::ModelUsageTotals;

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

impl UsageRecord {
    /// [`timestamp`](Self::timestamp) parsed to an instant, or `None` when the
    /// record carried none or carried one that is not RFC 3339 (Issue #9443).
    ///
    /// An absent instant is what makes a record **unattributable** to any
    /// per-phase window: it is counted in the sweep's totals and reported in
    /// `tokens_unattributed`, never guessed into a phase.
    #[must_use]
    pub fn at(&self) -> Option<DateTime<Utc>> {
        let raw = self.timestamp.as_deref()?;
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|t| t.with_timezone(&Utc))
    }
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

/// Folds usage records into per-`(model, speed, service_tier)` totals,
/// **deduped on `message.id`** (issue #8186, folded into #9303).
///
/// A streamed assistant message is written once per chunk, and every chunk
/// repeats the same `message.id` carrying that message's **cumulative** usage.
/// Summing every block therefore counted a streamed message once per chunk
/// (~2x high on real transcripts). This fold keeps one entry per id and takes
/// the per-counter maximum across its chunks — correct for both identical
/// repeats and genuinely growing cumulative chunks, and the same rule
/// [`crate::activity::transcript_parse`] applies. A record without an id cannot
/// collide with another message, so it is counted as its own message.
///
/// The one reader every token path shares: `sum_transcript_usage{,_by_model}`
/// (sweep outcomes, `loom.runtime.usage` execution spans, `usage-record`
/// attempt spans) and the role-tick transcript scan — so an execution total
/// and the attempt totals it is compared against can never disagree about how
/// a message is counted.
#[derive(Debug, Default)]
pub struct UsageFold {
    by_id: HashMap<String, usize>,
    messages: Vec<UsageRecord>,
    /// Usage blocks seen, before dedupe.
    pub blocks: usize,
    /// The first and last record-level `timestamp` of any usage record, as
    /// written (RFC 3339).
    pub first_timestamp: Option<String>,
    pub last_timestamp: Option<String>,
}

impl UsageFold {
    /// Fold one decoded record in.
    pub fn add(&mut self, record: UsageRecord) {
        self.blocks += 1;
        if let Some(ts) = &record.timestamp {
            if self.first_timestamp.is_none() {
                self.first_timestamp = Some(ts.clone());
            }
            self.last_timestamp = Some(ts.clone());
        }
        let Some(id) = record.message_id.clone() else {
            self.messages.push(record);
            return;
        };
        if let Some(&at) = self.by_id.get(&id) {
            let kept = &mut self.messages[at];
            kept.input = kept.input.max(record.input);
            kept.cache_read = kept.cache_read.max(record.cache_read);
            kept.cache_write_5m = kept.cache_write_5m.max(record.cache_write_5m);
            kept.cache_write_1h = kept.cache_write_1h.max(record.cache_write_1h);
            kept.output = kept.output.max(record.output);
            return;
        }
        self.by_id.insert(id, self.messages.len());
        self.messages.push(record);
    }

    /// Fold every usage-bearing line of a transcript's text.
    pub fn add_text(&mut self, text: &str) {
        for raw in text.lines() {
            let raw = raw.trim();
            if raw.is_empty() {
                continue;
            }
            let Ok(obj) = serde_json::from_str::<Value>(raw) else {
                continue;
            };
            if let Some(record) = usage_from_record(&obj) {
                self.add(record);
            }
        }
    }

    /// The deduped per-`(model, speed, service_tier)` totals, sorted by that
    /// tuple.
    #[must_use]
    pub fn rows(&self) -> Vec<ModelUsageTotals> {
        let mut totals = BTreeMap::new();
        merge_records(&mut totals, &self.messages);
        totals.into_values().collect()
    }

    /// The deduped messages, one per distinct `message.id` (plus every id-less
    /// record), in first-seen order — exposed for per-phase attribution (Issue
    /// #9443).
    ///
    /// The load-bearing property: because dedupe has already happened, Σ over
    /// **any partition** of this slice (via [`merge_records`]) equals
    /// [`rows`](Self::rows) exactly. That is what lets a caller split one
    /// transcript's usage into per-phase windows and still reconcile against
    /// the sweep total — splitting the raw lines instead would double-count a
    /// streamed message whose chunks straddle a phase boundary.
    ///
    /// A deduped message keeps the `timestamp` of its **first** chunk (see
    /// [`add`](Self::add)), so [`UsageRecord::at`] is the instant the message
    /// started, and a message is attributed wholly to the phase it started in.
    #[must_use]
    pub fn messages(&self) -> &[UsageRecord] {
        &self.messages
    }
}

/// Accumulate already-deduped [`UsageRecord`]s into the per-`(model, speed,
/// service_tier)` accumulator `into` — the grouping [`UsageFold::rows`]
/// performs, exposed (Issue #9443) so a caller partitioning one fold's
/// [`messages`](UsageFold::messages) across several buckets groups them
/// identically in each.
pub fn merge_records<'a>(
    into: &mut BTreeMap<(String, String, String), ModelUsageTotals>,
    records: impl IntoIterator<Item = &'a UsageRecord>,
) {
    for rec in records {
        let key = (rec.model.clone(), rec.speed.clone(), rec.service_tier.clone());
        let entry = into.entry(key).or_insert_with(|| ModelUsageTotals {
            model: rec.model.clone(),
            speed: rec.speed.clone(),
            service_tier: rec.service_tier.clone(),
            ..ModelUsageTotals::default()
        });
        entry.input += rec.input;
        entry.cache_read += rec.cache_read;
        entry.cache_write_5m += rec.cache_write_5m;
        entry.cache_write_1h += rec.cache_write_1h;
        entry.output += rec.output;
    }
}

/// Add `rows` into the per-tuple accumulator `into` (used to total several
/// transcripts' already-deduped rows).
pub fn merge_rows(
    into: &mut BTreeMap<(String, String, String), ModelUsageTotals>,
    rows: Vec<ModelUsageTotals>,
) {
    for row in rows {
        let key = (row.model.clone(), row.speed.clone(), row.service_tier.clone());
        match into.get_mut(&key) {
            Some(entry) => {
                entry.input += row.input;
                entry.cache_read += row.cache_read;
                entry.cache_write_5m += row.cache_write_5m;
                entry.cache_write_1h += row.cache_write_1h;
                entry.output += row.output;
            }
            None => {
                into.insert(key, row);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "transcript_usage_fold_tests.rs"]
mod fold_tests;

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
