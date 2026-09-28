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
//! would double every Pi run's tokens. `entry_appended`, which re-emits every
//! persisted session entry (assistant messages included), is excluded for
//! exactly that reason — [`USAGE_EVENT_TYPES`] is an allowlist, so any further
//! repeat event is excluded by construction rather than by a rule to maintain.
//!
//! # Pi's real shape, and the two semantics that come with it (Issue #8934)
//!
//! Leniency is not a licence to guess: this module originally searched an event
//! for its counters at `tokens`, `usage` and the event root, while Pi 0.85.1
//! actually nests them on the **message** —
//! `{"type":"message_end","message":{"role":"assistant","usage":{…}}}` — so a
//! real Pi run read as *unmeasured*, the exact spend #8556 and ADR-0020 exist
//! to see. `message.usage` is now searched **in addition to** the flat scopes,
//! never instead of them, and two of Pi's semantics are honoured with it
//! (`crate::pi_usage`'s "Schema provenance" section is the authority for both,
//! read off the shipped `@earendil-works/pi-coding-agent` / `pi-ai` packages):
//!
//! - **`reasoning` is a subset of `output`**, not an addition to it, whereas
//!   OpenCode's is additive. [`TapUsage::total_tokens`] sums every counter it is
//!   given and a `TapUsage` carries no harness tag, so a `reasoning` read out of
//!   `message.usage` is **not recorded** rather than double-counted inside
//!   `output`. The counters then reconcile against Pi's own `totalTokens`.
//! - **`cost` is an object**, not a number — the estimate is its rollup
//!   `total` ([`read_cost`], which still reads OpenCode's bare number).
//!
//! A **`toolResult` message's `message_end` is counted**, which is where this
//! module parts company with [`crate::pi_usage`]. That reader excludes it
//! because such a reading names no model and it keys by model, never guessing
//! one; this one keys by *tap*, and the tap is named by the enclosing
//! `# LOOM_LAUNCH` record rather than by the message. LLM work done inside a
//! tool is real spend against that tap, so dropping it would be a **knowable**
//! undercount — different in kind from the unknowable ones this module reports
//! as unmeasured.
//!
//! # One region can hold several launches, and they need not share a tap
//!
//! An anchored region is one dispatch's slice of a log, not one launch's:
//! [`crate::api_keys_pool::ingest::parse_launch_record`] already records that a
//! region may hold more than one `# LOOM_LAUNCH` record (a re-dispatch inside
//! one sweep, a containment re-exec that re-announces itself inside the
//! container), and an orchestrated sweep whose phases spawn their own workers
//! adds another: `runtimes.rolePreference` / `LOOM_RUNTIME_<ROLE>` can pin one
//! phase to a different tap from the rest, so two records in one region may
//! name different taps.
//!
//! Attributing such a region wholesale to its last record's tap (Issue #8633)
//! is therefore **not** safely conservative. The tempting argument — "the last
//! launch is the metered backstop, so folding an earlier launch onto it only
//! overcounts the tap that is already metered" — does not hold: nothing orders
//! a region's records by tier. `runtime_preference`'s own module doc fixes
//! fall-through at **dispatch** ("one sweep, one runtime"), so a region's
//! second record comes from a per-phase pin or a re-exec, and a metered
//! builder followed by a subscription-pinned judge charges metered spend to a
//! flat-rate tap — the direction a fleet spend ceiling must never be wrong in.
//!
//! So usage is **sliced per record**: each `# LOOM_LAUNCH` line opens a block,
//! and only the usage events after it (up to the next record) are charged to
//! the tap it names. [`account_launch_logs`] returns every block;
//! [`account_launch_log`] keeps the one-row contract its callers have, which
//! is the region's **last** record — the launch the region's outcome belongs
//! to. Usage appearing *before* the first record in a region is charged to
//! nobody: no tap had announced itself yet, and "no opinion" beats a
//! fabricated reading here exactly as it does everywhere else in this module.
//!
//! [`account_region_by_tap`] is the shape a terminal journal wants (Issue
//! #8659): the region folded to one row per tap ([`RegionAccounting`]),
//! outcome's tap first. It closes the residual gap slicing left behind — an
//! earlier launch is no longer misattributed, but under a one-row reader it was
//! not recorded at all — without ever merging two taps into one row.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api_keys_pool::ingest::LAUNCH_RECORD_PREFIX;
use crate::launch_record::{parse_launch_tap, TapAttribution};

/// Event types that carry a launch's usage exactly once.
///
/// `agent_end` and `entry_appended` are **not** here on purpose — see the
/// module doc. This is an allowlist, so any other repeat event Pi grows
/// (`turn_end`) is excluded by construction rather than by a rule to maintain.
const USAGE_EVENT_TYPES: &[&str] = &["step_finish", "step-finish", "message_end", "message-end"];

/// Objects a usage-bearing event may nest its counters under, plus the event
/// itself (some harnesses put the counters at the top level).
const USAGE_OBJECT_FIELDS: &[&str] = &["tokens", "usage"];

/// Where Pi nests a `message_end`'s counters: on the **message**, not the event
/// (Issue #8934). Searched in addition to [`USAGE_OBJECT_FIELDS`], never
/// instead of them.
const MESSAGE_USAGE_PATH: (&str, &str) = ("message", "usage");

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

/// How a `cost` **object** spells its rollup total. Pi reports
/// `cost: {"input":…,"output":…,"cacheRead":…,"cacheWrite":…,"total":…}` rather
/// than a bare number (Issue #8934), so a flat numeric read finds nothing there
/// and the whole cost estimate was silently dropped.
const COST_TOTAL_FIELDS: &[&str] = &["total", "total_cost", "totalCost"];

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
    for usage in text.lines().filter_map(line_usage) {
        total.absorb(&usage);
    }
    total
}

/// The usage one log line reports, or `None` when the line is not a
/// usage-bearing event at all — the per-line half of [`accumulate_usage`],
/// shared with the per-record slicing in [`launch_blocks`] so the two can
/// never disagree about what counts as a reading.
fn line_usage(line: &str) -> Option<TapUsage> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let event = serde_json::from_str::<Value>(line).ok()?;
    let kind = event.get("type").and_then(Value::as_str)?;
    if !USAGE_EVENT_TYPES.contains(&kind) {
        return None;
    }
    read_event_usage(&event)
}

/// The counters on one usage-bearing event, or `None` when it carried none —
/// a `step_finish` with no `tokens` object at all is not a reading of zero.
fn read_event_usage(event: &Value) -> Option<TapUsage> {
    // The counters may sit on the event itself, under `tokens`/`usage`, or —
    // Pi's real shape (Issue #8934) — under `message.usage`; search the event
    // last so a nested object's value wins over a same-named sibling field on
    // the envelope.
    let mut scopes: Vec<&Value> = USAGE_OBJECT_FIELDS
        .iter()
        .filter_map(|field| event.get(*field))
        .collect();
    let message_scope = event
        .get(MESSAGE_USAGE_PATH.0)
        .and_then(|message| message.get(MESSAGE_USAGE_PATH.1))
        .map(|usage| {
            scopes.push(usage);
            scopes.len() - 1
        });
    scopes.push(event);

    // The winning scope's index alongside its value, because one counter's
    // meaning depends on where it was read from — see `reasoning` below.
    let reading = |fields: &[&str]| -> Option<(usize, u64)> {
        scopes.iter().enumerate().find_map(|(index, scope)| {
            fields
                .iter()
                .find_map(|field| read_u64(scope, field))
                .map(|value| (index, value))
        })
    };
    let counter = |fields: &[&str]| -> Option<u64> { reading(fields).map(|(_, value)| value) };
    // OpenCode nests its cache counters one level deeper (`tokens.cache.read`),
    // so look inside a `cache` object before falling back to flat spellings.
    let cache = |nested: &str, fields: &[&str]| -> Option<u64> {
        scopes
            .iter()
            .filter_map(|scope| scope.get("cache"))
            .find_map(|cache| read_u64(cache, nested))
            .or_else(|| counter(fields))
    };
    // Pi's `reasoning` is a **subset** of `output` (`pi-ai`'s `Usage`: "output
    // already includes these tokens"), while OpenCode's is additive.
    // `total_tokens` sums every counter it is given and `TapUsage` carries no
    // harness tag, so a reasoning count read out of Pi's `message.usage` is
    // dropped rather than counted a second time inside `output`.
    let reasoning = reading(REASONING_FIELDS)
        .filter(|(scope, _)| Some(*scope) != message_scope)
        .map(|(_, value)| value);
    let usage = TapUsage {
        input: counter(INPUT_FIELDS),
        output: counter(OUTPUT_FIELDS),
        reasoning,
        cache_read: cache("read", CACHE_READ_FIELDS),
        cache_write: cache("write", CACHE_WRITE_FIELDS),
        cost_estimate: scopes.iter().find_map(|scope| {
            COST_FIELDS
                .iter()
                .find_map(|field| scope.get(*field).and_then(read_cost))
        }),
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

/// One `cost` field's value as an estimate: a bare number (OpenCode), or a cost
/// **object**'s rollup total (Pi — Issue #8934). An object carrying no total is
/// no reading at all rather than a fabricated zero, the same rule every counter
/// here follows.
fn read_cost(value: &Value) -> Option<f64> {
    if let Some(cost) = value.as_f64().filter(|v| v.is_finite()) {
        return Some(cost);
    }
    COST_TOTAL_FIELDS
        .iter()
        .find_map(|field| read_f64(value, field))
}

/// The region at/after `header_anchor`, split into one block per
/// `# LOOM_LAUNCH` record: the tap that record names (`None` when it is
/// unattributable — a pre-#8401 record, or one naming no runtime) and the
/// usage reported after it, up to the next record.
///
/// `None` only when the anchor is absent, so a previous dispatch's output can
/// never be read as this one's — the same `rfind` anchoring every other reader
/// of these logs applies. An anchored region with no launch record at all is
/// an empty `Vec`, not `None`.
///
/// Record lines are matched **line-anchored** (trim, then
/// `strip_prefix`), exactly as
/// [`crate::api_keys_pool::ingest::parse_launch_record`] matches them, so a
/// line that merely *mentions* the marker mid-line — an agent transcript
/// echoing it — cannot open a phantom block.
fn launch_blocks(
    contents: &str,
    header_anchor: &str,
) -> Option<Vec<(Option<TapAttribution>, TapUsage)>> {
    let region = &contents[contents.rfind(header_anchor)?..];
    let mut blocks: Vec<(Option<TapAttribution>, TapUsage)> = Vec::new();
    for line in region.lines() {
        if let Some(record) = line.trim().strip_prefix(LAUNCH_RECORD_PREFIX) {
            blocks.push((parse_launch_tap(record), TapUsage::default()));
            continue;
        }
        let usage = line_usage(line);
        // Usage ahead of the first record belongs to no launch in this region
        // — `blocks` is still empty, so it is charged to nobody.
        if let (Some(block), Some(usage)) = (blocks.last_mut(), usage) {
            block.1.absorb(&usage);
        }
    }
    Some(blocks)
}

/// Attribute the usage in `contents` to the tap its own `# LOOM_LAUNCH` record
/// names, scanning only the region at/after `header_anchor`.
///
/// The region's **last** record, carrying **only its own** usage (Issue
/// #8633): a region may hold several launches on different taps, and the last
/// one is the launch the region's outcome belongs to — but not the one that
/// paid for an earlier launch's tokens. Use [`account_launch_logs`] when every
/// launch in the region matters rather than just the last.
///
/// `None` when the region carries no attributable launch record — the same
/// "no opinion rather than a fabricated reading" contract
/// [`crate::launch_record::parse_launch_tap`] has. The usage half never gates
/// the result: an attributable launch whose stream reported nothing yields a
/// row with [`TapUsage::is_measured`] `false`, which is exactly the
/// distinction a spend-governance reader needs to make ("this tap ran and we
/// cannot see what it cost" is not "this tap ran for free").
#[must_use]
pub fn account_launch_log(contents: &str, header_anchor: &str) -> Option<TapAccounting> {
    let (tap, usage) = launch_blocks(contents, header_anchor)?.pop()?;
    Some(TapAccounting { tap: tap?, usage })
}

/// Every attributable launch in the region at/after `header_anchor`, in log
/// order, each carrying only the usage its own block reported (Issue #8633).
///
/// The lossless counterpart of [`account_launch_log`], for a reader summing a
/// region's spend rather than naming the launch its outcome belongs to: feed
/// it straight to [`fold_by_tap`] and a re-dispatched or phase-pinned region
/// reports each tap's share separately instead of piling all of it onto one.
/// Unattributable records contribute no row at all, so their usage is dropped
/// rather than charged to a neighbouring tap.
#[must_use]
pub fn account_launch_logs(contents: &str, header_anchor: &str) -> Vec<TapAccounting> {
    launch_blocks(contents, header_anchor)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(tap, usage)| Some(TapAccounting { tap: tap?, usage }))
        .collect()
}

/// One region's usage folded to **one row per tap** — the shape a terminal
/// journal needs (Issue #8659) — together with the one thing a fold alone
/// cannot say: whether the region's *last* record, the launch its outcome
/// belongs to, was attributable at all.
///
/// Slicing per record (#8633) stopped an earlier launch's usage being billed to
/// a later launch's tap, but a one-row reader — [`account_launch_log`] and the
/// terminal journals built on it — then recorded only the region's *last*
/// launch, leaving the earlier ones invisible rather than wrong. Folding first
/// closes the common multi-record shape outright: a re-dispatch or a containment
/// re-exec re-announces the **same** tap, so its blocks belong in one row and a
/// one-row reader loses nothing at all. A genuinely multi-tap region (a phase
/// pinned by `runtimes.rolePreference` / `LOOM_RUNTIME_<ROLE>`) stays a *list*,
/// because the one thing this must never do is merge unlike taps into one row —
/// that is precisely the #8633 error, in the direction a fleet spend ceiling
/// must never be wrong in.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RegionAccounting {
    /// One row per tap, each carrying that tap's whole share of the region.
    ///
    /// Ordered with [`Self::outcome`]'s tap first when there is one, then the
    /// remaining taps in order of first appearance. Empty exactly when
    /// [`account_launch_logs`] is empty.
    pub per_tap: Vec<TapAccounting>,
    /// Whether the region's last `# LOOM_LAUNCH` record was attributable — in
    /// which case `per_tap[0]` is its tap. See [`Self::outcome`].
    pub outcome_attributed: bool,
}

impl RegionAccounting {
    /// The row for the launch the region's **outcome** belongs to: its last
    /// record, carrying that tap's whole share of the region rather than only
    /// its final block.
    ///
    /// `None` under exactly the conditions [`account_launch_log`] returns `None`
    /// — no record, or a last record naming no attributable tap. An earlier
    /// launch is never promoted into this slot: "which tap did this sweep run
    /// on" has no answer then, and a neighbouring tap's name is a fabricated one
    /// (the same rule the rest of this module follows). That usage is not lost —
    /// it is exactly what [`Self::breakdown`] carries.
    #[must_use]
    pub fn outcome(&self) -> Option<&TapAccounting> {
        self.outcome_attributed.then(|| self.per_tap.first())?
    }

    /// The per-tap breakdown a one-row reader cannot represent, and **empty**
    /// when [`Self::outcome`] already accounts for the whole region.
    ///
    /// Non-empty in exactly two cases: the region named more than one tap, or
    /// its last record was unattributable while an earlier one was. That is the
    /// invariant both journals are built on — *`breakdown` is empty ⟺ `outcome`
    /// is the region's whole attributable usage* — so a reader never has to
    /// consult two fields to know whether it has the full picture.
    #[must_use]
    pub fn breakdown(&self) -> &[TapAccounting] {
        if self.outcome().is_some() && self.per_tap.len() <= 1 {
            &[]
        } else {
            &self.per_tap
        }
    }
}

/// The region at/after `header_anchor` folded per tap — see
/// [`RegionAccounting`] for the contract and Issue #8659 for why the journals
/// want this shape rather than [`account_launch_log`]'s single row.
///
/// One pass over the same blocks every other reader here uses, so this cannot
/// disagree with [`account_launch_log`] / [`account_launch_logs`] about what a
/// region contains. Rows sharing a [`TapAttribution::key`] fold together even
/// when they differ in a field the key omits (the account): the key is the
/// accounting identity on purpose — see [`TapAttribution::key`] for why a fleet
/// ceiling must not split a shared credential per account. The outcome row keeps
/// its own record's full attribution, so the #8447 credential read off it is
/// byte-identical to what [`account_launch_log`] reported.
#[must_use]
pub fn account_region_by_tap(contents: &str, header_anchor: &str) -> RegionAccounting {
    let blocks = launch_blocks(contents, header_anchor).unwrap_or_default();
    // The launch the region's outcome belongs to is its LAST record, exactly as
    // `account_launch_log` reads it — `None` when that record named no tap.
    let outcome_tap = blocks.last().and_then(|(tap, _)| tap.clone());
    let rows: Vec<TapAccounting> = blocks
        .into_iter()
        .filter_map(|(tap, usage)| Some(TapAccounting { tap: tap?, usage }))
        .collect();
    let mut folded = fold_by_tap(&rows);
    let mut taps: Vec<TapAttribution> = outcome_tap.iter().cloned().collect();
    for row in &rows {
        if !taps.iter().any(|tap| tap.key() == row.key()) {
            taps.push(row.tap.clone());
        }
    }
    // `remove` both supplies the fold and guarantees one row per key, so a tap
    // can never be emitted twice however the region's records are ordered.
    let per_tap = taps
        .into_iter()
        .filter_map(|tap| {
            let usage = folded.remove(&tap.key())?;
            Some(TapAccounting { tap, usage })
        })
        .collect();
    RegionAccounting {
        per_tap,
        outcome_attributed: outcome_tap.is_some(),
    }
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
