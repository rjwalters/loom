//! Tap-attributed usage accounting (Issue #8556): what a native-harness launch
//! reported it consumed, keyed by the **tap** that paid for it.
//!
//! # Why the tap is the key
//!
//! #8556 names a structural gap the fleet cannot close with per-host state: a
//! metered OpenAI-compatible key is **one credential shared across every fleet
//! host**, while every credential-governance mechanism Loom has is host-local
//! (`.loom/tokens/`, the #8401 API-key pool, the #8555 per-host concurrency
//! ceiling). N hosts each honouring a local ceiling of K still permit N×K
//! concurrent metered launches, and nothing knows the aggregate — **a per-host
//! cap is not a spend cap.**
//!
//! Whichever way that gap is eventually closed (see
//! `docs/adr/0020-fleet-metered-spend-ceiling.md` for the recorded decision),
//! the prerequisite is the same and is worth landing on its own: usage has to
//! carry the tap. The `# LOOM_LAUNCH` record already names harness, provider,
//! model and profile; what it did not carry was the one composite identity
//! whose economics actually differ —
//! [`Tap`](crate::runtime_preference::Tap) + credential source, as
//! [`crate::launch_record::TapAttribution`] renders it. With that, *"how much
//! went to the metered backstop vs. the subscriptions"* is a fold over a key
//! rather than a reconstruction from runtime names.
//!
//! # A harness estimate is not an invoice
//!
//! `defaults/docs/runtime-model-trials.md` states the rule this module obeys
//! literally: **missing counters mean unmeasured, not zero**, and a harness
//! **cost estimate is not a measured charge** — directionally meaningful for a
//! metered tap, not a charge at all for a flat-rate one. So every counter here
//! is an `Option`, a stream with no usage events reports
//! [`TapUsage::is_measured`] `false` rather than a row of zeros, and the cost
//! field is spelled [`TapUsage::cost_estimate`] so no caller can read it as
//! billed spend.
//!
//! # Post-hoc, like every other reader of a launch's own log
//!
//! A native harness launch runs under Unix `exec` (`worker_spawn::exec`), so no
//! Loom code is in that process to watch the stream live. This module is
//! therefore a pure function of already-captured text, region-anchored by the
//! caller's own dispatch header — the fifth module in this tree with exactly
//! that shape, and for the same reason
//! ([`crate::api_keys_pool::ingest`]'s module doc lays out the rejected
//! alternative in full).
//!
//! # Leniency is deliberate
//!
//! The exact JSON of each harness's native event stream is not something a
//! fixture can pin down for a real CLI (`.loom/docs/guardrail-parity-native.md`),
//! and `runtime-model-trials.md` records that Pi reports usage on assistant
//! `message_end` events while OpenCode reports it on `step_finish`. So event
//! spellings and field names are matched permissively, and **`agent_end` is
//! deliberately excluded**: Pi repeats the same usage there, and counting it
//! would double every Pi run's tokens.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::launch_record::{parse_launch_tap_after, TapAttribution};

/// Event types that carry a launch's usage exactly once.
///
/// `agent_end` is **not** here on purpose — see the module doc.
const USAGE_EVENT_TYPES: &[&str] = &["step_finish", "step-finish", "message_end", "message-end"];

/// Objects a usage-bearing event may nest its counters under, plus the event
/// itself (some harnesses put the counters at the top level).
const USAGE_OBJECT_FIELDS: &[&str] = &["tokens", "usage"];

const INPUT_FIELDS: &[&str] = &[
    "input",
    "input_tokens",
    "inputTokens",
    "prompt_tokens",
    "promptTokens",
];
const OUTPUT_FIELDS: &[&str] = &[
    "output",
    "output_tokens",
    "outputTokens",
    "completion_tokens",
    "completionTokens",
];
const REASONING_FIELDS: &[&str] = &["reasoning", "reasoning_tokens", "reasoningTokens"];
const CACHE_READ_FIELDS: &[&str] = &[
    "cache_read",
    "cacheRead",
    "cache_read_input_tokens",
    "cacheReadInputTokens",
];
const CACHE_WRITE_FIELDS: &[&str] = &[
    "cache_write",
    "cacheWrite",
    "cache_creation_input_tokens",
    "cacheCreationInputTokens",
];
const COST_FIELDS: &[&str] = &["cost", "cost_usd", "costUsd", "total_cost", "totalCost"];

/// Usage counters a native harness reported for one launch, in the taxonomy
/// `runtime-model-trials.md` fixes: input, output, reasoning, cache read and
/// cache write, plus the harness's own cost **estimate**.
///
/// Every counter is `Option` because **a missing counter means unmeasured, not
/// zero**. A `TapUsage` with every field `None` is this module saying "I read
/// the stream and it reported nothing", which is a materially different claim
/// from "this launch consumed nothing".
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct TapUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<u64>,
    /// The harness's own cost estimate, summed. **Not a measured charge** —
    /// directionally meaningful for a metered tap, and not a charge at all for
    /// a flat-rate one (`runtime-model-trials.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_estimate: Option<f64>,
    /// How many usage-bearing events were folded in. `0` ⇒ nothing was
    /// measured; see [`Self::is_measured`].
    #[serde(default)]
    pub usage_events: usize,
}

impl TapUsage {
    /// `true` when at least one usage-bearing event was read. The guard every
    /// caller must apply before presenting a row: an unparsed or usage-free
    /// stream is a gap in Loom's observation, never evidence of zero spend.
    #[must_use]
    pub fn is_measured(&self) -> bool {
        self.usage_events > 0
    }

    /// Sum of the token counters that were actually reported.
    ///
    /// A **floor**, not a total: a harness that reports only `input` yields a
    /// value that omits its output entirely. `None` when no token counter was
    /// reported at all, rather than `Some(0)`.
    #[must_use]
    pub fn total_tokens(&self) -> Option<u64> {
        [
            self.input,
            self.output,
            self.reasoning,
            self.cache_read,
            self.cache_write,
        ]
        .into_iter()
        .flatten()
        .reduce(u64::saturating_add)
    }

    /// Fold `other` into `self`, treating a `None` on either side as "nothing
    /// to add" rather than as a zero — so summing a measured launch with an
    /// unmeasured one never turns an unmeasured counter into `0`.
    pub fn absorb(&mut self, other: &Self) {
        fn add(into: &mut Option<u64>, value: Option<u64>) {
            if let Some(value) = value {
                *into = Some(into.unwrap_or(0).saturating_add(value));
            }
        }
        add(&mut self.input, other.input);
        add(&mut self.output, other.output);
        add(&mut self.reasoning, other.reasoning);
        add(&mut self.cache_read, other.cache_read);
        add(&mut self.cache_write, other.cache_write);
        if let Some(cost) = other.cost_estimate {
            self.cost_estimate = Some(self.cost_estimate.unwrap_or(0.0) + cost);
        }
        self.usage_events = self.usage_events.saturating_add(other.usage_events);
    }
}

/// One launch's usage, attributed to the tap that paid for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TapAccounting {
    pub tap: TapAttribution,
    pub usage: TapUsage,
}

impl TapAccounting {
    /// The accounting key this row folds under — see
    /// [`TapAttribution::key`].
    #[must_use]
    pub fn key(&self) -> String {
        self.tap.key()
    }
}

/// Read every usage-bearing event out of a captured native event stream and
/// sum them.
///
/// Line-oriented and order-independent, matching
/// [`crate::worker_spawn::launch_outcome::classify_native_stream`]: any line
/// that parses as a JSON object is inspected, and every other line — Loom's own
/// `# LOOM_LAUNCH` header, `spawn-worker:` prose, harness chatter — is ignored
/// rather than rejected.
#[must_use]
pub fn accumulate_usage(text: &str) -> TapUsage {
    let mut total = TapUsage::default();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(kind) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        if !USAGE_EVENT_TYPES.contains(&kind) {
            continue;
        }
        if let Some(usage) = read_event_usage(&event) {
            total.absorb(&usage);
        }
    }
    total
}

/// The counters on one usage-bearing event, or `None` when it carried none —
/// a `step_finish` with no `tokens` object at all is not a reading of zero.
fn read_event_usage(event: &Value) -> Option<TapUsage> {
    // The counters may sit on the event itself or under `tokens`/`usage`;
    // search the event last so a nested object's value wins over a same-named
    // sibling field on the envelope.
    let mut scopes: Vec<&Value> = USAGE_OBJECT_FIELDS
        .iter()
        .filter_map(|field| event.get(*field))
        .collect();
    scopes.push(event);

    let counter = |fields: &[&str]| -> Option<u64> {
        scopes
            .iter()
            .find_map(|scope| fields.iter().find_map(|field| read_u64(scope, field)))
    };
    // OpenCode nests its cache counters one level deeper (`tokens.cache.read`),
    // so look inside a `cache` object before falling back to flat spellings.
    let cache = |nested: &str, fields: &[&str]| -> Option<u64> {
        scopes
            .iter()
            .filter_map(|scope| scope.get("cache"))
            .find_map(|cache| read_u64(cache, nested))
            .or_else(|| counter(fields))
    };
    let usage = TapUsage {
        input: counter(INPUT_FIELDS),
        output: counter(OUTPUT_FIELDS),
        reasoning: counter(REASONING_FIELDS),
        cache_read: cache("read", CACHE_READ_FIELDS),
        cache_write: cache("write", CACHE_WRITE_FIELDS),
        cost_estimate: scopes
            .iter()
            .find_map(|scope| COST_FIELDS.iter().find_map(|field| read_f64(scope, field))),
        usage_events: 1,
    };
    let reported = usage.total_tokens().is_some() || usage.cost_estimate.is_some();
    reported.then_some(usage)
}

fn read_u64(scope: &Value, field: &str) -> Option<u64> {
    let value = scope.get(field)?;
    value
        .as_u64()
        .or_else(|| value.as_f64().filter(|v| *v >= 0.0).map(|v| v as u64))
}

fn read_f64(scope: &Value, field: &str) -> Option<f64> {
    scope.get(field)?.as_f64().filter(|v| v.is_finite())
}

/// Attribute the usage in `contents` to the tap its own `# LOOM_LAUNCH` record
/// names, scanning only the region at/after `header_anchor`.
///
/// `None` when the region carries no attributable launch record — the same
/// "no opinion rather than a fabricated reading" contract
/// [`parse_launch_tap_after`] has. The usage half never gates the result: an
/// attributable launch whose stream reported nothing yields a row with
/// [`TapUsage::is_measured`] `false`, which is exactly the distinction a
/// spend-governance reader needs to make ("this tap ran and we cannot see what
/// it cost" is not "this tap ran for free").
#[must_use]
pub fn account_launch_log(contents: &str, header_anchor: &str) -> Option<TapAccounting> {
    let tap = parse_launch_tap_after(contents, header_anchor)?;
    let region = &contents[contents.rfind(header_anchor)?..];
    Some(TapAccounting {
        tap,
        usage: accumulate_usage(region),
    })
}

/// Filesystem wrapper for a caller holding a log *path*. An unreadable log is
/// "no opinion", never a failure.
#[must_use]
pub fn account_launch_log_at(
    log_path: &std::path::Path,
    header_anchor: &str,
) -> Option<TapAccounting> {
    let contents = std::fs::read_to_string(log_path).ok()?;
    account_launch_log(&contents, header_anchor)
}

/// Fold rows by [`TapAttribution::key`] — the #8556 query, *"how much went to
/// the metered backstop vs. the subscriptions"*, in one call.
///
/// Rows whose stream reported nothing are still folded in, so a bucket's
/// `usage_events` count tells a reader how much of it is measured.
#[must_use]
pub fn fold_by_tap<'a, I>(rows: I) -> BTreeMap<String, TapUsage>
where
    I: IntoIterator<Item = &'a TapAccounting>,
{
    let mut folded: BTreeMap<String, TapUsage> = BTreeMap::new();
    for row in rows {
        folded.entry(row.key()).or_default().absorb(&row.usage);
    }
    folded
}

#[cfg(test)]
#[path = "tap_usage_tests.rs"]
mod tests;
