//! Versioned fleet-telemetry schema (Epic #4702, Phase 1 — issue #4703).
//!
//! This module defines the **wire schema** the whole observability epic builds
//! on: the record kinds a daemon durably records and later exports, wrapped in a
//! versioned [`TelemetryEnvelope`]. It is schema + serialization only — there is
//! deliberately no event-bus wiring, no persistence, and no exporter here. The
//! sibling Phase-1 issues consume these types: #4704 persists them to a local
//! journal, #4705 pushes them to a cloud backend. Keeping the public surface
//! narrow (the record structs/enums, the envelope, and one visibility-derivation
//! function in [`visibility`]) lets both depend on this module without depending
//! on daemon internals they do not own.
//!
//! # Design contracts
//!
//! - **Versioned envelope.** Every record is emitted inside a
//!   [`TelemetryEnvelope`] carrying a numeric [`schema_version`](TelemetryEnvelope::schema_version)
//!   ([`CURRENT_SCHEMA_VERSION`]). A plain `u32` (not a semver string) is what the
//!   Phase-2 TypeScript backend gates on, and it lets a mixed-version fleet — some
//!   hosts on an older daemon mid-rolling-upgrade — be ingested without the backend
//!   parsing semver. Bump [`CURRENT_SCHEMA_VERSION`] on any breaking wire change.
//!
//! - **Repo-visibility tag, private by default.** Every record that references a
//!   repository carries a [`RepoVisibility`] tag. The Phase-2 public view keys its
//!   redaction off this tag, so it is load-bearing for the epic's anti-leak
//!   control: an unknown, missing, or malformed `visibility` on the wire decodes to
//!   [`RepoVisibility::Private`], **never** `Public` (see the custom `Deserialize`
//!   impl). A partial or older-schema record can therefore never accidentally
//!   qualify for the redaction-sensitive public view.
//!
//! - **Superset of the frozen SSE `sweep.*` topics.** The lifecycle records
//!   ([`SweepStartedRecord`] / [`SweepPhaseRecord`] / [`SweepCompletedRecord`])
//!   mirror the six frozen `sweep.*` SSE moments (`event_bus.rs` /
//!   `serve.rs`), extended with the outcome/config metadata the live SSE tail does
//!   not carry ([`SweepOutcomeRecord`]) plus host-level records
//!   ([`TokenSnapshotRecord`], [`HostHealthRecord`]).
//!
//! # Wire format
//!
//! JSON, documented independently of these Rust types (for the Phase-2 Workers
//! backend) in `.loom/docs/telemetry-schema.md`. The record enum is internally
//! tagged on a `kind` discriminant, so each record serializes to a single flat
//! object the TypeScript backend can pattern-match on:
//!
//! ```json
//! { "schema_version": 1, "emitted_at": "...", "host_id": "...",
//!   "record": { "kind": "sweep.outcome", "repo": "owner/repo",
//!               "visibility": "public", ... } }
//! ```

use chrono::{DateTime, Utc};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::path::PathBuf;

use crate::script_helpers::sweep_experiment::ModelUsageTotals;

mod envelope;
mod sweep_identity;
pub use sweep_identity::SweepIdentityRecord;
pub mod fixture;
pub mod trace;
pub mod visibility;
pub use envelope::TelemetryEnvelope;

/// Current telemetry wire-schema version. Bump on any breaking change to the
/// record shapes below so a Phase-2 backend ingesting a mixed-version fleet can
/// gate on a simple numeric compare (no semver parsing). See the module docs.
///
/// **`2` since Issue #8056**: [`TelemetryRecord`] gained a seventh variant,
/// `role_tick.outcome` ([`RoleTickOutcomeRecord`]). A new *record kind* — not a
/// new optional field — is exactly the change a mixed-version backend has to
/// gate on: a `1`-era ingester pattern-matching exhaustively on `kind` has no
/// arm for it. Every pre-existing record shape is byte-identical to the `1`
/// wire format, so a `2` envelope carrying any of the original six kinds is
/// still parseable by a `1`-era reader, and a `1` envelope is still parseable
/// here (the `schema_version` field is read, never validated, on the read
/// path — see `sweep_outcomes::read_all_outcome_telemetry`).
pub const CURRENT_SCHEMA_VERSION: u32 = 2;

// ============================================================================
// Repository visibility — private-safe by construction
// ============================================================================

/// Whether the repository a record references is public or private. The Phase-2
/// public view exposes full detail for `Public` work and only redacted /
/// summarized aggregates for `Private` work, so this tag is the epic's schema-
/// level anti-leak control.
///
/// **Private by default (load-bearing).** [`Default`] is [`Private`](Self::Private),
/// and the custom [`Deserialize`] impl decodes any value that is not exactly the
/// string `"public"` — an unknown variant, a `null`, a wrong-typed scalar, or a
/// nested map/seq — to [`Private`](Self::Private). A partial or older-schema record
/// can therefore never *accidentally* decode to `Public` and leak into the public
/// view; leaking-by-default is impossible by construction, not by convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RepoVisibility {
    /// A public repository — full detail may appear in the Phase-2 public view.
    Public,
    /// A private repository — the public view exposes only redacted aggregates.
    Private,
}

impl Default for RepoVisibility {
    /// Private, never public — the safe default for a missing tag. Fields
    /// tagged `#[serde(default)]` therefore decode a *missing* `visibility` to
    /// `Private`, complementing the custom `Deserialize` impl's handling of a
    /// *present-but-unknown* value.
    fn default() -> Self {
        RepoVisibility::Private
    }
}

impl<'de> Deserialize<'de> for RepoVisibility {
    /// Decodes `"public"` (case-insensitively) to [`RepoVisibility::Public`] and
    /// **everything else** — any other string, `null`, a bool/number, or a
    /// map/seq — to [`RepoVisibility::Private`]. This is the private-safe default
    /// the epic calls load-bearing: an unknown or malformed value can never
    /// decode to `Public`.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // `deserialize_any` routes each concrete JSON shape to a visitor method;
        // every method except the `"public"` string returns `Private`, so no
        // wire value can fail to decode (a malformed tag defaults, never errors).
        deserializer.deserialize_any(RepoVisibilityVisitor)
    }
}

struct RepoVisibilityVisitor;

impl<'de> Visitor<'de> for RepoVisibilityVisitor {
    type Value = RepoVisibility;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the string \"public\" or \"private\" (any other value decodes to private)")
    }

    fn visit_str<E>(self, value: &str) -> Result<RepoVisibility, E>
    where
        E: de::Error,
    {
        // Only an exact (case-insensitive) "public" is public; anything else,
        // including an unknown label like "internal", is private.
        Ok(if value.eq_ignore_ascii_case("public") {
            RepoVisibility::Public
        } else {
            RepoVisibility::Private
        })
    }

    // Every non-string shape is treated as absent/malformed ⇒ Private. These
    // exist so a wrong-typed or structurally-malformed `visibility` on the wire
    // still decodes (to the safe default) rather than raising a decode error.
    fn visit_none<E>(self) -> Result<RepoVisibility, E>
    where
        E: de::Error,
    {
        Ok(RepoVisibility::Private)
    }

    fn visit_unit<E>(self) -> Result<RepoVisibility, E>
    where
        E: de::Error,
    {
        Ok(RepoVisibility::Private)
    }

    fn visit_bool<E>(self, _v: bool) -> Result<RepoVisibility, E>
    where
        E: de::Error,
    {
        Ok(RepoVisibility::Private)
    }

    fn visit_i64<E>(self, _v: i64) -> Result<RepoVisibility, E>
    where
        E: de::Error,
    {
        Ok(RepoVisibility::Private)
    }

    fn visit_u64<E>(self, _v: u64) -> Result<RepoVisibility, E>
    where
        E: de::Error,
    {
        Ok(RepoVisibility::Private)
    }

    fn visit_f64<E>(self, _v: f64) -> Result<RepoVisibility, E>
    where
        E: de::Error,
    {
        Ok(RepoVisibility::Private)
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<RepoVisibility, A::Error>
    where
        A: SeqAccess<'de>,
    {
        // Drain and ignore any elements so the parser stays well-formed.
        while seq.next_element::<de::IgnoredAny>()?.is_some() {}
        Ok(RepoVisibility::Private)
    }

    fn visit_map<A>(self, mut map: A) -> Result<RepoVisibility, A::Error>
    where
        A: MapAccess<'de>,
    {
        while map
            .next_entry::<de::IgnoredAny, de::IgnoredAny>()?
            .is_some()
        {}
        Ok(RepoVisibility::Private)
    }
}

// ============================================================================
// Versioned envelope
// ============================================================================

// ============================================================================
// Record kinds — internally tagged on `kind`
// ============================================================================

/// Every telemetry record kind, internally tagged on a `kind` discriminant. The
/// tag values match the frozen SSE `sweep.*` topic vocabulary where they overlap
/// (`sweep.started`/`sweep.phase`/`sweep.completed`) plus the epic's added
/// record kinds (`sweep.outcome`, `tokens.snapshot`, `host.health`), so the
/// Phase-2 backend pattern-matches one flat object per record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum TelemetryRecord {
    /// A sweep began (mirrors the dispatch moment of the frozen SSE topics).
    #[serde(rename = "sweep.started")]
    SweepStarted(SweepStartedRecord),
    /// Late-resolved launch identity; enriches an existing active sweep only.
    #[serde(rename = "sweep.identity")]
    SweepIdentity(SweepIdentityRecord),
    /// A sweep advanced to a new lifecycle phase (mirrors `sweep.issue.{N}.phase`).
    #[serde(rename = "sweep.phase")]
    SweepPhase(SweepPhaseRecord),
    /// A sweep reached a terminal state (mirrors the exited/crashed/completed
    /// frozen topics; the richer per-phase/config detail lives in the paired
    /// [`SweepOutcomeRecord`]).
    #[serde(rename = "sweep.completed")]
    SweepCompleted(SweepCompletedRecord),
    /// The full post-hoc outcome of a sweep: model/config/effort, per-phase
    /// durations, terminal result, and PR number.
    #[serde(rename = "sweep.outcome")]
    SweepOutcome(SweepOutcomeRecord),
    /// A snapshot of the multi-account token pool's per-account usage state.
    #[serde(rename = "tokens.snapshot")]
    TokensSnapshot(TokenSnapshotRecord),
    /// Host health: CPU/disk headroom, daemon version, uptime.
    #[serde(rename = "host.health")]
    HostHealth(HostHealthRecord),
    /// One role-runner tick's outcome (Issue #8056) — the per-`(root, role)`
    /// counterpart of [`SweepOutcome`](Self::SweepOutcome). The seventh
    /// variant, and the reason [`CURRENT_SCHEMA_VERSION`] is `2`.
    #[serde(rename = "role_tick.outcome")]
    RoleTickOutcome(RoleTickOutcomeRecord),
    /// One transcript's session shape (Issue #8757, G3 of #8714) — ids,
    /// attribution, models, token totals, and turn/tool counts, emitted by
    /// the transcript-ingest pass. Carries **no** prompt, tool-output, key
    /// or email content by construction (see [`SessionSummaryRecord`]).
    #[serde(rename = "session.summary")]
    SessionSummary(SessionSummaryRecord),
    /// A derived per-session anomaly/quality rollup (Issue #8760, G3 part 2
    /// of #8714) — retry-loop detection, the longest paired tool call, a USD
    /// cost estimate, and anomaly flags, computed from a
    /// [`SessionSummaryRecord`] plus the
    /// [`crate::activity::transcript_parse::ParsedTranscript`] that produced
    /// it. See [`SessionAnalysisRecord`] for the wire-safety contract (same
    /// as `session.summary`: no prompt, tool-output, key or email content,
    /// ever).
    #[serde(rename = "session.analysis")]
    SessionAnalysis(SessionAnalysisRecord),
    /// One of the four named event-bus topics that carried no telemetry
    /// record kind of their own (Issue #8760, G4 of #8714): `daemon.drain.*`,
    /// `daemon.capacity.advisory`, `daemon.preflight.advisory`, and
    /// `epic.issue.*`. See [`DaemonEventRecord`].
    #[serde(rename = "daemon.event")]
    DaemonEvent(DaemonEventRecord),
    #[serde(rename = "trace.span")]
    Span(trace::SpanRecord),
}

/// A sweep's terminal result. `#[serde(default)]`-friendly variants are not
/// needed here (unlike [`RepoVisibility`], an unknown result is not a privacy
/// hazard); a malformed value is a legitimate decode error the backend should
/// surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SweepResult {
    /// The sweep merged (or otherwise reached its successful terminal state).
    Success,
    /// The sweep ended without success (a failing/abandoned lifecycle).
    Failure,
    /// The sweep was cancelled by an operator or watchdog before completing.
    Cancelled,
    /// The sweep stopped because it hit a human-decision blocker.
    Blocked,
}

/// The wall-clock duration a sweep spent in one named lifecycle phase — the unit
/// [`SweepOutcomeRecord::phase_durations`] is a list of.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PhaseDuration {
    /// Lifecycle phase name (e.g. `"curator"`, `"builder"`, `"judge"`,
    /// `"doctor"`, `"merge"`).
    pub phase: String,
    /// Seconds spent in this phase.
    pub duration_sec: i64,
}

/// `sweep.started` — a sweep began work on an issue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepStartedRecord {
    /// Repository the sweep is working, `owner/repo` form.
    pub repo: String,
    /// Public/private tag for `repo`. Missing/unknown ⇒ [`RepoVisibility::Private`].
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// Issue number the sweep is working.
    pub issue: u32,
    /// Stable opaque sweep id assigned at dispatch time.
    pub sweep_id: String,
    /// When the sweep started.
    pub started_at: DateTime<Utc>,
    /// Selected Claude model, when one was chosen (mirrors `SweepInfo::model`'s
    /// empty-means-unset contract).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Selected reasoning-effort level, when one was chosen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Runtime adapter the sweep was dispatched on (`claude`, `codex`, …),
    /// when the dispatch event carried one — the same value
    /// `SweepInfo::runtime` records. Absent for a legacy dispatch that did
    /// not name its runtime; never fabricated as `"claude"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
}

/// `sweep.phase` — a sweep advanced to a new lifecycle phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepPhaseRecord {
    /// Repository the sweep is working, `owner/repo` form.
    pub repo: String,
    /// Public/private tag for `repo`. Missing/unknown ⇒ [`RepoVisibility::Private`].
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// Issue number the sweep is working.
    pub issue: u32,
    /// Stable opaque sweep id.
    pub sweep_id: String,
    /// The phase just entered (`"curator"`, `"builder"`, `"judge"`, …).
    pub phase: String,
    /// When the sweep entered this phase.
    pub entered_at: DateTime<Utc>,
}

/// `sweep.completed` — a sweep reached a terminal state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepCompletedRecord {
    /// Repository the sweep worked, `owner/repo` form.
    pub repo: String,
    /// Public/private tag for `repo`. Missing/unknown ⇒ [`RepoVisibility::Private`].
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// Issue number the sweep worked.
    pub issue: u32,
    /// Stable opaque sweep id.
    pub sweep_id: String,
    /// When the sweep reached its terminal state.
    pub completed_at: DateTime<Utc>,
    /// Terminal result.
    pub result: SweepResult,
    /// Per-`(model, speed, service_tier)` token totals for this sweep
    /// (Issue #6384), matching the shape the safehouse `completion-v1`
    /// envelope's `tokens_by_model` already carries (see
    /// [`crate::safehouse::fetch_transcript_tokens_by_model`] /
    /// [`crate::transcript_tokens::sum_sweep_tokens_by_model`]) so the two
    /// paths report identical per-sweep token data. Additive: omitted
    /// (never an empty vec, never a fabricated zero) when no attributable
    /// transcript was found for this sweep, same "unknown != zero"
    /// contract [`SweepOutcomeRecord::tokens_in`]/`tokens_out` already use.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_by_model: Option<Vec<ModelUsageTotals>>,
}

/// `sweep.outcome` — the full post-hoc outcome of a sweep, carrying the
/// model/config/effort/duration/result/PR detail the live SSE tail does not.
/// This is a *distinct* type from `sweep_outcomes::OutcomeRecord` (owned by
/// #4704's persistence layer); #4704 maps this schema record into its journal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepOutcomeRecord {
    /// Repository the sweep worked, `owner/repo` form.
    pub repo: String,
    /// Public/private tag for `repo`. Missing/unknown ⇒ [`RepoVisibility::Private`].
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// Issue number the sweep worked.
    pub issue: u32,
    /// Stable opaque sweep id.
    pub sweep_id: String,
    /// Selected Claude model, when one was chosen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Selected reasoning-effort level, when one was chosen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Free-form config key/value pairs captured at dispatch (e.g. runtime,
    /// concurrency knobs) — kept as a map so the schema does not need a bump
    /// every time an operator-tunable field is added.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub config: std::collections::BTreeMap<String, String>,
    /// Per-phase wall-clock durations, in lifecycle order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub phase_durations: Vec<PhaseDuration>,
    /// Total wall-clock seconds from dispatch to terminal outcome.
    pub total_duration_sec: i64,
    /// Terminal result.
    pub result: SweepResult,
    /// PR number produced by the sweep, when it opened one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u32>,
    /// Raw input-token count aggregated from the sweep's own Claude Code
    /// transcripts (Issue #5357): the sum of `input_tokens`,
    /// `cache_read_input_tokens`, and `cache_creation_input_tokens` across
    /// the parent session and every subagent transcript matched to this
    /// sweep (see `crate::transcript_tokens`). Deliberately **raw**, not
    /// cost-weighted — this record already carries `model`, so a consumer
    /// applies whatever per-model pricing table it wants without a backfill
    /// when that table changes. Omitted (never `0`) when no attributable
    /// transcript was found — a pruned/rotated log directory is "unknown",
    /// not "no tokens".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_in: Option<u64>,
    /// Raw output-token count (`output_tokens`), aggregated the same way as
    /// `tokens_in`. Kept as a separate field — not folded into one total —
    /// because input and output tokens price very differently per model, so
    /// a cost-weighted aggregate needs both counts plus `model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_out: Option<u64>,
    /// Lines added by the sweep's own commits, from a local `git diff
    /// --numstat` against the worktree's mainline merge base (Issue #5357) —
    /// never a forge API call. Omitted when the worktree was never sampled
    /// while live and no longer exists at outcome-write time (e.g. a
    /// `--merge`-mode sweep whose own merge already cleaned it up) — a
    /// legitimate "unavailable", never a fabricated `0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_added: Option<i64>,
    /// Lines deleted, alongside `lines_added` — two fields, not a net, so a
    /// large refactor that adds and deletes a similar number of lines does
    /// not read as "no work done".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines_deleted: Option<i64>,
    /// Per-`(model, speed, service_tier)` token totals (Issue #6384),
    /// aggregated the same way as `tokens_in`/`tokens_out` (raw counts, not
    /// cost-weighted) but grouped rather than flattened — see
    /// [`ModelUsageTotals`] and
    /// [`crate::transcript_tokens::sum_sweep_tokens_by_model`] for why a
    /// flat sum cannot be priced across models that are themselves 3-5x
    /// apart. Omitted (never an empty vec) when nothing attributable was
    /// found, same "unknown != zero" contract as `tokens_in`/`tokens_out`.
    /// This is [`SweepCompletedRecord::tokens_by_model`]'s upstream source:
    /// `backfill.rs`'s `synthesize_completed` copies this value verbatim
    /// rather than re-deriving it from a reconstructed window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_by_model: Option<Vec<ModelUsageTotals>>,
    /// Terminal failure classification (Issue #8056), copied verbatim at emit
    /// time from the SAME terminal transition's sibling
    /// [`crate::sweep_outcomes::OutcomeRecord`] — its `death_class` (the
    /// pre-flight classifier: `preflight-token-selection-failed`,
    /// `preflight-no-cli-start`, …) when it derived one, otherwise its
    /// `crash_classification` (`account-exhausted:model-credits-exhausted`,
    /// `no-usable-account`, …). The two classifiers are independent and rarely
    /// compete: account/credit exhaustion is deliberately excluded from the
    /// pre-flight one. The classification exists on both
    /// journals so a consumer can tell a real build failure from a <60s spawn
    /// death **without** joining `sweep-outcomes.jsonl` by `sweep_id` — the
    /// join #8056 measured as "every success-rate number is wrong until it is
    /// done". The sibling record still carries BOTH fields separately; this is
    /// the single most-specific label, not a replacement for them.
    ///
    /// Omitted (never `""`, never `"unknown"`) when the terminal transition
    /// carried no classification at all — including on every success, where
    /// there is nothing to classify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    /// The distinct model ids observed in [`tokens_by_model`](Self::tokens_by_model),
    /// sorted and deduped (Issue #8056) — the top-level answer to "did this
    /// sweep run more than one model?", which the grouped token rows carry
    /// only implicitly and the top-level [`model`](Self::model) field (the
    /// *dispatched* model) cannot answer at all. A sweep that escalated to
    /// `claude-opus-5` through the Doctor ladder has `model: "sonnet"` and
    /// `models_used: ["claude-opus-5", "claude-sonnet-5"]`.
    ///
    /// Derived from `tokens_by_model` rather than sampled independently, so it
    /// inherits exactly that field's contract: omitted (never an empty vec)
    /// when no attributable transcript was found. "Not observed" and "observed,
    /// one model" are therefore distinguishable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_used: Option<Vec<String>>,
    /// How many Doctor cycles this sweep's PR completed (Issue #8056,
    /// re-sourced by #8222) — the "sonnet passed" vs. "sonnet failed, the
    /// ladder's Doctor fixed it" discriminator.
    ///
    /// Read from the **forge label timeline** of the PR named by
    /// [`pr_number`](Self::pr_number), not from the sampled phase history: one
    /// cycle per `loom:changes-requested` arrival that a later
    /// `loom:review-requested` arrival closed the loop on (a Doctor handing a
    /// fixed PR back to Judge). A rejection nobody handed back — the
    /// Doctor-cycle cap, or a dead sweep — is not a cycle. The label events are
    /// written by the Judge/Doctor themselves and retained by the forge, so
    /// unlike the interim ~30s-sampled proxy this replaces, a cycle that opens
    /// and closes between two reaper ticks is still counted: this is a
    /// certified count, not a lower bound.
    ///
    /// `Some(0)` means "the PR's timeline was read and no Doctor cycle
    /// completed"; omitted means the timeline was not successfully read at all
    /// — the sweep opened no PR, the fetch was rate-limited/failed, or the
    /// daemon was told not to touch the forge. The two are deliberately
    /// distinguishable: the same "unknown != zero" contract as
    /// `tokens_in`/`tokens_by_model`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctor_cycles: Option<u32>,
    /// Every Judge verdict on this sweep's PR, in lifecycle order (Issue
    /// #8222) — what makes **first-pass judge approval rate** computable from
    /// this journal alone: `judge_verdicts[0].verdict == "pass"` over the
    /// sweeps that carry the field.
    ///
    /// Same source and same PR as [`doctor_cycles`](Self::doctor_cycles): the
    /// forge label timeline of the PR named by [`pr_number`](Self::pr_number).
    /// Deliberately NOT the sampled phase history — a first-pass approval rate
    /// computed from a lossy sample is worse than no number at all, because a
    /// silently-low verdict count reads as a silently-high approval rate.
    ///
    /// `Some([])` means "the timeline was read and carried no verdict" (a PR
    /// whose sweep died before Judge); omitted means the timeline was not
    /// successfully read. A consumer that coerces an absent list to `[]` counts
    /// an unobserved sweep as a judged-zero-times one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_verdicts: Option<Vec<JudgeVerdict>>,
    /// Runtime adapter this sweep actually launched on (Issue #8507) —
    /// `"claude"`, `"pi"`, `"opencode"`, … — read off the launch's own
    /// `# LOOM_LAUNCH` record (`crate::launch_record::RuntimeAttribution`),
    /// never re-derived from dispatch-time config. Omitted (never a fabricated
    /// `"claude"` default) when no launch record was found — a Claude/legacy
    /// spawn writes none, so the historical, byte-identical case for every
    /// Claude sweep is simply "these three keys absent".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// The runtime's resolved provider namespace (`"zai-coding-plan"`,
    /// `"friendli"`, …), when the launch resolved one. Same source and same
    /// omission contract as `runtime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The resolved model profile name, when one was selected. Same source
    /// and same omission contract as `runtime`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// One Judge verdict on a PR, as reconstructed from the forge label timeline
/// (Issue #8222) — the element type of [`SweepOutcomeRecord::judge_verdicts`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgeVerdict {
    /// Which Judge pass this verdict settled: **1-based per PR**, counting
    /// `loom:review-requested` arrivals. Attempt 1 is the PR as first opened by
    /// the Builder; attempt 2 is the pass after the first Doctor hand-back, and
    /// so on. Numbering restarts at 1 for a different PR — a record covers
    /// exactly one PR and never aggregates across them.
    pub attempt: u32,
    /// `"pass"` (the Judge applied `loom:pr`) or `"fail"` (the Judge applied
    /// `loom:changes-requested`). A string rather than an enum so a future
    /// verdict shape is an additive value, not a breaking wire change for a
    /// backend that already pattern-matches this field.
    pub verdict: String,
}

/// How one role-runner tick ended (Issue #8056).
///
/// Deliberately **not** [`SweepResult`]: a sweep either finished or did not,
/// but a role tick has a third class the fleet cares about — the pre-spawn
/// *skips* (`crate::role_runner::RoleTickOutcome`'s `NoTokenPool`,
/// `PoolExhausted`, `ModelRuntimeMismatch`, and the post-spawn `LoadSkipped`)
/// that consumed no tokens and are not role failures. Folding those into
/// `failure` is exactly the mis-read #7607 documents for the in-memory ring:
/// hundreds of identical exit-78s from one fleet-wide exhausted pool must not
/// read as hundreds of broken roles. One variant per
/// `crate::role_runner::RoleTickOutcome` variant, so the mapping is total and
/// no outcome is silently reclassified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleTickResult {
    /// The invocation ran to completion with a zero exit code.
    Success,
    /// The invocation ran (or failed to start) and reported failure.
    Failure,
    /// Fail-closed runtime-admission rejection — never spawned.
    RuntimeRejected,
    /// Skipped pre-spawn: no token pool is provisioned for this workspace.
    SkippedNoTokenPool,
    /// Skipped pre-spawn: a pool exists but has zero spawnable accounts.
    SkippedPoolExhausted,
    /// Skipped pre-spawn: the resolved model provably conflicts with the
    /// admitted runtime.
    SkippedModelRuntimeMismatch,
    /// Terminated at the wall-clock ceiling while the host was measurably
    /// saturated — a starved tick, not a broken role.
    SkippedLoad,
}

impl RoleTickResult {
    /// Whether this tick actually launched a child session, and therefore
    /// *could* have consumed tokens. `false` for every pre-spawn skip — the
    /// discriminator a consumer needs before reading an absent
    /// [`RoleTickOutcomeRecord::tokens_by_model`] as anything at all.
    #[must_use]
    pub fn spawned(self) -> bool {
        matches!(self, Self::Success | Self::Failure | Self::SkippedLoad)
    }
}

/// Forge-mutating work one role-runner tick was observed doing (Issue #8056),
/// counted from the tick's own Claude Code transcripts.
///
/// **A lower bound, by construction.** The counts come from scanning the
/// transcript's `tool_use` blocks for the shell commands that perform each
/// action (`gh issue edit --add-label`, `gh pr comment`, `merge-pr.sh`, …), so
/// an action taken through a path this scanner does not recognize is not
/// counted. That is why the whole struct is optional on the record rather than
/// the individual counts: an absent `actions` means "no transcript was
/// attributable to this tick", while a present `actions` with a `0` means
/// "the transcript was read and no such command appeared in it". Never
/// synthesize the former from the latter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleTickActions {
    /// `gh issue edit … --add-label/--remove-label` invocations — the label
    /// transitions that drive Loom's whole state machine.
    pub issues_labeled: u32,
    /// PR merges (`merge-pr.sh`, `gh pr merge`, `gh api … /merge`).
    pub prs_merged: u32,
    /// Comments posted on an issue or PR (`gh issue comment`, `gh pr comment`,
    /// `gh api … /comments`).
    pub comments_posted: u32,
}

/// `role_tick.outcome` — one role-runner tick's outcome (Issue #8056).
///
/// The per-tick counterpart of [`SweepOutcomeRecord`]. Role ticks were, by
/// measurement, ~60% of fleet token spend and emitted **no** durable record at
/// all: `crate::types::RoleTickRecord` holds `{root, role, at, ok, detail,
/// pool_exhausted}` in a process-global 2048-entry ring that is lost on daemon
/// restart and carries no model, effort, duration, or token counts. This
/// record is the durable, experiment-gradeable version.
///
/// Every field that is *measured* rather than *decided* is optional and follows
/// the same "unknown != zero" contract [`SweepOutcomeRecord::tokens_in`]
/// establishes: omitted when not observed, never coerced to `0`/`[]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleTickOutcomeRecord {
    /// Repository this tick ran for, `owner/repo` form. Falls back to the
    /// workspace root's path when the slug cannot be resolved from the
    /// checkout's `origin` remote — same best-effort fallback
    /// `sweep.outcome` uses.
    pub repo: String,
    /// Public/private tag for `repo`. Missing/unknown ⇒ [`RepoVisibility::Private`].
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// The role name (`champion`, `curator`, `judge`, …) — the `/loom:<role>`
    /// slash command this tick invoked.
    pub role: String,
    /// When the tick started (the instant the runner began the invocation,
    /// not the interval boundary that scheduled it).
    pub started_at: DateTime<Utc>,
    /// Wall-clock seconds from invocation start to outcome, including the
    /// pre-spawn preflights. A skip is typically sub-second; the value is
    /// still real, so it is not optional.
    pub duration_sec: i64,
    /// How the tick ended — see [`RoleTickResult`].
    pub result: RoleTickResult,
    /// The model the runner actually resolved for this tick, when it got far
    /// enough to resolve one. Omitted for a skip that bailed out *before*
    /// model resolution (`no_token_pool`, `pool_exhausted`,
    /// `runtime_rejected`) — "not resolved", never a guessed default. This is
    /// the resolved value including #7894's unpinned-model reconciliation, not
    /// a re-read of config, so it can never disagree with what was launched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// The reasoning-effort level resolved for this tick (#8054), when one was
    /// configured. Unconfigured resolves to no `--effort` argument at all, and
    /// is reported here as an absent key — the honest "inherited the runtime
    /// default", never a fabricated `"medium"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Short failure/skip detail — the same string
    /// `crate::types::RoleTickRecord::detail` carries (the failure reason, the
    /// runtime rejection, the mismatch description, the `no-token-pool`
    /// sentinel). Always absent for [`RoleTickResult::Success`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Which credential pool gated a pre-spawn pool skip (Issue #8408):
    /// `claude_tokens` (the `.loom/tokens/` OAuth pool) or `codex_accounts`
    /// (the `loom-daemon accounts` codex profiles) — the pool the role's
    /// **admitted runtime** draws from, which is no longer always Claude's.
    /// Present only on [`RoleTickResult::SkippedNoTokenPool`] and
    /// [`RoleTickResult::SkippedPoolExhausted`]; absent on every other result,
    /// and on every record written before #8408 (additive — no
    /// `schema_version` bump, exactly as the schema doc prescribes for a new
    /// optional field).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gated_pool: Option<String>,
    /// Runtime adapter this tick actually launched on (Issue #8507), read off
    /// the tick's own `# LOOM_LAUNCH` record — same source and the same
    /// "absent, never a fabricated `claude` default" contract as
    /// [`SweepOutcomeRecord::runtime`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// The runtime's resolved provider namespace, when the launch resolved
    /// one — same source and contract as [`SweepOutcomeRecord::provider`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The resolved model profile name, when one was selected — same source
    /// and contract as [`SweepOutcomeRecord::profile`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Per-`(model, speed, service_tier)` token totals for this tick, summed
    /// from the `/loom:<role>` Claude Code transcripts whose mtime falls in
    /// this tick's own window — the same grouped shape, and the same raw
    /// (not cost-weighted) counts, as
    /// [`SweepOutcomeRecord::tokens_by_model`].
    ///
    /// Omitted (never an empty vec, never a fabricated zero) when nothing was
    /// attributable — which is *always* the case for a pre-spawn skip, since
    /// no session existed to produce a transcript. A consumer must read an
    /// absent value together with [`RoleTickResult::spawned`]: absent on a
    /// skip means "correctly nothing", absent on a `success` means "unknown".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens_by_model: Option<Vec<ModelUsageTotals>>,
    /// The distinct model ids in [`tokens_by_model`](Self::tokens_by_model),
    /// sorted and deduped — the same derivation, and the same contract, as
    /// [`SweepOutcomeRecord::models_used`]. Answers "did this tick's session
    /// escalate past the model it was launched with?" without the consumer
    /// re-folding the grouped rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_used: Option<Vec<String>>,
    /// Forge-mutating work observed in this tick's transcripts — see
    /// [`RoleTickActions`], including why the struct (not each count) is the
    /// optional unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actions: Option<RoleTickActions>,
}

/// One account's slice of a `tokens.snapshot` — the per-account usage /
/// limit-window state matching what `loom-daemon tokens check --ranking` knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenAccountState {
    /// Token account name (the `<account>.token` basename in `.loom/tokens/`,
    /// or the profile name from the multi-provider account registry).
    pub account: String,
    /// Which provider's pool this account belongs to (`"claude"`, `"codex"`,
    /// …) — the lowercase [`AccountProvider`] name. Defaults to `"claude"` on
    /// deserialization so a record from a daemon that predates per-provider
    /// pools (which only ever sampled the Claude `.ranking` file) still reads
    /// as what it was.
    #[serde(default = "default_token_provider")]
    pub provider: String,
    /// The account's rank in the rotation pool, when ranking data exists
    /// (lower = preferred).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    /// Fraction of the 5h limit window consumed (`0.0..=1.0`), when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_fraction: Option<f64>,
    /// When the window **currently gating this account** resets, when known —
    /// the 7d window for an `exhausted` account (the instant it regains
    /// capacity), the 5h window otherwise (the rollover `usage_fraction` is
    /// racing). The producer resolves which one, so a consumer reads this as
    /// the single answer to "when does this account's constraint lift?"
    /// (`tokens_pool::check::limit_reset`, issue #4874). Absent means
    /// *unknown*, never "resets now".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_window_reset_at: Option<DateTime<Utc>>,
    /// Whether the account is currently considered exhausted (excluded from the
    /// usable pool).
    pub exhausted: bool,
}

/// `tokens.snapshot` — a point-in-time view of the multi-account token pool.
/// The provider a pre-per-provider `tokens.snapshot` row implicitly belonged
/// to — see [`TokenAccountState::provider`].
fn default_token_provider() -> String {
    "claude".to_string()
}

/// Host-level: it references no repository, so it carries no visibility tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSnapshotRecord {
    /// When the snapshot was taken.
    pub captured_at: DateTime<Utc>,
    /// Per-account state for every account in the pool.
    pub accounts: Vec<TokenAccountState>,
}

/// One `(root, role)` pair's persistent tick-failure detail inside a host's
/// role-tick health summary (`host.health`'s `roles` field, Issue #5022).
/// Mirrors `crate::health::RoleFailure`'s shape — `loom-daemon health`'s
/// `roles` section already classifies exactly this
/// (`crate::health::summarize_role_ticks`), so the telemetry pipeline carries
/// the same classification rather than inventing a second one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleTickFailureEntry {
    /// The workspace root this tick ran for.
    pub root: PathBuf,
    /// The role name (`champion`, `curator`, …).
    pub role: String,
    /// How many ticks failed for this pair inside the sampled window.
    pub failures: usize,
    /// When the most recent record for this pair landed.
    pub last_at: DateTime<Utc>,
    /// The most recent failure detail, when the tick reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// `host.health`'s role-tick health summary (Issue #5022): the same
/// transient-vs-persistent classification `loom-daemon health`'s `roles`
/// section already computes (`crate::health::summarize_role_ticks`), carried
/// through the telemetry pipeline so a role dying on one host is observable
/// fleet-wide — not only to an operator who happens to run `loom-daemon
/// health` locally on that one host. That gap is exactly what #5004 found: a
/// Judge outage stayed green on every other signal for most of a day.
///
/// Deliberately narrower than `crate::health::RoleTickSummary`: only
/// `persistent` failures are carried — the `(root, role)` pairs whose most
/// recent tick in the sampled window is still a failure, i.e. the ones that
/// make `loom-daemon health`'s `roles` section report `DEGRADED`. `transient`
/// (self-recovered) pairs are folded into `total`/`ok` like every other tick,
/// exactly as the rendered `roles` summary line already treats them: a count,
/// not alarming detail.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RoleTickHealth {
    /// Total tick records sampled.
    pub total: usize,
    /// Successful tick records sampled.
    pub ok: usize,
    /// `(root, role)` pairs whose latest sampled record is a failure.
    ///
    /// `total: 0` (no ticks sampled — the role runner idle or disabled) means
    /// "nothing to report", not "healthy": a consumer must not read an empty
    /// `persistent` list on its own as proof the role runner is even running.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub persistent: Vec<RoleTickFailureEntry>,
}

/// `host.health`'s watchdog/crash-protection summary (Issue #5352) — a
/// deliberately narrower wire projection of
/// [`crate::daemon_install_state::ProtectionReport`], carrying only the two
/// facts a remote consumer can act on (the classification and the raw
/// watchdog-provisioned bit). Reuses the *exact* classification
/// `loom-daemon status`'s own `Protection:` line and `--json`'s `protection`
/// object already compute
/// ([`crate::daemon_install_state::probe_protection`]) rather than
/// re-deriving it, so the telemetry pipeline can never disagree with the
/// host-local CLI verdict about the same host.
///
/// Host-local fields with no meaning off-host (`marker_path`, the resolved
/// `job` identifier) are deliberately dropped — mirrors how `managed_repos`/
/// `roles` above only carry the fleet-relevant subset of their host-local
/// source of truth.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostProtectionSummary {
    /// The wire-level verdict string — [`crate::daemon_install_state::ProtectionState::as_str`]'s
    /// exact output (`"protected"`, `"no-marker"`, `"watchdog-not-provisioned"`,
    /// or `"unknown"`).
    pub state: String,
    /// Whether the watchdog job/timer was found provisioned, when the
    /// probe could answer at all. `None` when `state` is `"unknown"` (the
    /// probe ran but the provisioning check itself could not answer — no
    /// `launchctl`/`systemctl`, or an unreachable `systemctl --user` bus).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub watchdog_provisioned: Option<bool>,
}

/// `host.health`'s saturation admission-brake summary (Issue #8478) — a narrow
/// wire projection of [`crate::admission_brake::BrakeSnapshot`], carrying the
/// facts a *fleet-level* consumer needs to tell "this host is quiet" apart from
/// "this host's dispatch has been suppressed for hours by load Loom does not
/// own".
///
/// # Why the existing fields were not enough
///
/// `dispatch_halted`/`halt_reason` (#4975) already report the host-distress
/// **breaker**, and #8478 extends them to cover a sustained-starving brake too
/// — that is what makes such a host render as degraded in the existing fleet
/// view with no consumer change. But a boolean plus a prose reason cannot answer
/// the question the 12-hour incident actually raised: *how long*. That duration
/// existed only in the host-local `status --json` payload
/// ([`crate::types::AdmissionBrakeStatus::starving_since`], #5715); nothing
/// pushed it off-host, so a fleet check could not distinguish a brake that
/// engaged this minute from one wedged since yesterday.
///
/// # Every field is a fact, never a verdict
///
/// `starving_secs` is computed at capture time against the emitting host's own
/// clock so a consumer never has to subtract a remote timestamp from its own
/// (clock skew across a fleet would otherwise make short streaks negative).
/// `starvation_warn_secs` is that host's own resolved threshold, so
/// "longer than N minutes" is evaluated against what *this* host considers
/// alarming rather than a hardcoded fleet constant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdmissionBrakeSummary {
    /// Whether new sweep admissions are currently held. In-flight sweeps are
    /// never affected — the brake has no path to running work.
    pub held: bool,
    /// When the current starvation streak began (held with **zero** sweeps in
    /// flight, continuously). `None` whenever the host is not starving,
    /// including a brake held while sweeps genuinely drain (healthy
    /// backpressure never starves, however long it holds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starving_since: Option<DateTime<Utc>>,
    /// Seconds the current starvation streak has run, as measured on the
    /// emitting host at capture time. `None` when not starving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starving_secs: Option<i64>,
    /// This host's resolved starvation **warn** threshold in seconds — the
    /// duration past which it logs `STARVING` locally.
    pub starvation_warn_secs: i64,
    /// Cumulative starvation-escape-hatch grants this daemon process's
    /// lifetime. `0` on a healthy host forever; nonzero means the brake has
    /// had to force at least one admission through a still-saturated host.
    pub escape_hatch_grants: u32,
    /// `true` once this host has been starving for at least its own
    /// `starvation_warn_secs` — i.e. dispatch is suppressed and **nothing Loom
    /// admitted** is producing the load. This is the single field a fleet-level
    /// check should alert on; the rest are for the diagnosis that follows.
    pub dispatch_suppressed_by_foreign_load: bool,
    /// Which processes the CPU actually belongs to, as
    /// [`crate::foreign_load::attribution_clause`] renders it. Sampled **only**
    /// while `dispatch_suppressed_by_foreign_load` is true, so an ordinary
    /// healthy host never pays for a `ps` shellout, and `None` whenever the
    /// probe could not answer (no `ps`, a timeout, unparseable output) — an
    /// absent attribution must never be read as "no foreign load".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_cpu_consumers: Option<String>,
}

/// `host.health` — host CPU/disk headroom plus the emitting binary's identity
/// (version + build commit + build time) and uptime.
/// Host-level: it references no repository, so it carries no visibility tag.
/// Every measured field is optional so an unmeasurable probe stays absent rather
/// than being coerced to a fake zero (matching `cpu_headroom` / `disk_headroom`'s
/// "unknown != zero" contract).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HostHealthRecord {
    /// When the sample was taken.
    pub captured_at: DateTime<Utc>,
    /// The emitting daemon's version (`CARGO_PKG_VERSION`).
    pub daemon_version: String,
    /// The short git commit the emitting binary was BUILT from
    /// (`self_update::BUILT_COMMIT`, baked in by `build.rs`), or `"unknown"`
    /// when the build host had no git.
    ///
    /// `daemon_version` alone cannot answer "is this host's daemon current?" —
    /// it only moves once per release, so every build between two releases
    /// reports the same string and a day-stale binary is indistinguishable
    /// from `main` (#4956). The commit is the precise identity.
    ///
    /// `#[serde(default)]` so a record emitted by a pre-#4956 daemon still
    /// decodes (as an empty string) rather than failing the whole envelope.
    #[serde(default)]
    pub build_commit: String,
    /// When the emitting binary was compiled (`LOOM_DAEMON_BUILD_TIME`), when
    /// that stamp is present and parseable.
    ///
    /// `Option` rather than a bare `DateTime<Utc>` on purpose: `build.rs`
    /// falls back to the literal string `"unknown"` when `date` is
    /// unavailable, and this struct's contract is that an unavailable value
    /// stays *absent* rather than being coerced to a fabricated instant (the
    /// same "unknown != zero" rule the measured fields below follow).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_at: Option<DateTime<Utc>>,
    /// Daemon uptime in seconds.
    pub uptime_sec: u64,
    /// Logical CPU count.
    pub logical_cpus: usize,
    /// Measured CPU idle fraction (`0.0..=1.0`), when a sample exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_idle_fraction: Option<f64>,
    /// 1-minute load average per logical core, when a load reading exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub load_per_core: Option<f64>,
    /// Free space (GB) on the worktree-root scratch volume, when measurable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_root_free_gb: Option<u64>,
    /// Total capacity (GB) of the worktree-root scratch volume, when
    /// measurable — the denominator a consumer needs to render
    /// `worktree_root_free_gb` as a percentage instead of a bare absolute
    /// number that is not comparable across a heterogeneous fleet (Issue
    /// #5356). Sourced from the same `df -Pk` sample as the free-space
    /// reading (`crate::disk_headroom::worktree_root_disk_gb`).
    ///
    /// Follows the exact "unknown != zero" contract `worktree_root_free_gb`
    /// already established: **omitted**, never a fabricated `0`, when the
    /// probe cannot measure it. A consumer that sees free-but-no-total must
    /// render GB only and never compute a percentage against a made-up
    /// denominator. No `#[serde(default)]` needed — `Option<T>` fields
    /// already decode as `None` when the wire key is entirely absent, so a
    /// pre-#5356 daemon's record (which never sends this key) still decodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_root_total_gb: Option<u64>,
    /// This host's currently in-flight (non-terminal) sweep IDs, across every
    /// repo this daemon actively tracks — the daemon's own authoritative
    /// registry view (Issue #4955). Consumed by the Phase-2 dashboard's
    /// `FleetState` Durable Object to reconcile its live `sweep:` entries
    /// against ground truth on every `host.health` update, so a sweep whose
    /// `sweep.completed` record was lost (e.g. across a daemon restart) does
    /// not linger forever as a phantom "in flight" entry.
    ///
    /// `#[serde(default)]` so a pre-#4955 queued record still surviving in a
    /// host's on-disk `DurableQueue` past an upgrade decodes cleanly (empty
    /// list) rather than failing to send at all. An **empty** list is
    /// therefore ambiguous between "genuinely zero sweeps running" and "this
    /// daemon predates the field" / "the registry was not yet queried" —
    /// callers that reconcile against this field must never treat an empty
    /// list as proof of zero in-flight sweeps on its own; see the dashboard's
    /// `applyUpdate` doc comment for the exact caveat it applies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_sweep_ids: Vec<String>,
    /// Whether this host's own dispatch is currently halted for a
    /// non-idle reason — i.e. the host-distress breaker
    /// ([`crate::host_breaker`], Issue #4235) has tripped `Open` or is still
    /// `CoolDown`ing (see [`crate::host_breaker::BreakerPhase::suppresses_dispatch`]).
    /// `false` when the breaker is `Closed`, disabled, or has never been
    /// registered (no work-finder loop running on this host) — a repo that
    /// never enables autonomy sees no behavior change (Issue #4975).
    ///
    /// `#[serde(default)]` so a record from a pre-#4975 daemon still decodes
    /// (as `false`, i.e. "not known to be halted") rather than failing.
    #[serde(default)]
    pub dispatch_halted: bool,
    /// Human-readable reason for the current halt — the breaker's own
    /// transition message (e.g. `"load-per-core 4.24 ≥ 2.50 sustained for 3
    /// consecutive tick(s)"`), sourced straight from
    /// [`crate::host_breaker::BreakerSnapshot::reason`]. Always `None` while
    /// `dispatch_halted` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub halt_reason: Option<String>,
    /// This host's managed-repository roster (Issue #4976): every workspace
    /// root the daemon's [`crate::workspace_pool::WorkspacePool`] has a
    /// provisioned registry for, resolved to its forge `owner/repo` slug and
    /// [`RepoVisibility`] — sourced from the workspace registry itself, not
    /// inferred from `active_sweep_ids`, so an idle-but-registered repo still
    /// appears. Feeds the Phase-2 dashboard's "Repositories" fleet-card
    /// section.
    ///
    /// `#[serde(default)]` so a pre-#4976 record still decodes (as an empty
    /// roster) rather than failing the whole envelope — the same
    /// backward-compatibility contract `active_sweep_ids` established.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub managed_repos: Vec<ManagedRepoEntry>,
    /// This host's role-tick health (Issue #5022): mirrors `loom-daemon
    /// health`'s `roles` section verdict inputs, carried through the
    /// telemetry pipeline so a role dying on one host is observable
    /// fleet-wide rather than only to an operator running `loom-daemon
    /// health` locally on that host.
    ///
    /// `#[serde(default)]` so a record from a pre-#5022 daemon still decodes
    /// (as the zero-value "no role ticks sampled" summary) rather than
    /// failing the whole envelope.
    #[serde(default)]
    pub roles: RoleTickHealth,
    /// This host's watchdog/crash-protection state (Issue #5352) — see
    /// [`HostProtectionSummary`]. `None` when the host-local probe itself
    /// could not construct a report at all
    /// ([`crate::daemon_install_state::probe_protection`] returning `None` —
    /// e.g. `resolve_loom_dir` failed), which is distinct from
    /// `Some(HostProtectionSummary { state: "unknown", .. })` (the probe ran
    /// but the watchdog-provisioning check specifically could not answer).
    /// Both must degrade gracefully on the consuming side: an absent value
    /// must never render as "unprotected", only as "not reported" — the same
    /// contract every other optional `host.health` field already holds.
    ///
    /// `#[serde(default)]` so a record from a pre-#5352 daemon still decodes
    /// (as `None`) rather than failing the whole envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protection: Option<HostProtectionSummary>,
    /// This host's saturation admission-brake state (Issue #8478) — see
    /// [`AdmissionBrakeSummary`]. `None` when no brake has been registered on
    /// this host at all (no work-finder loop running), which is distinct from
    /// `Some(..)` with `held: false` (a brake exists and is admitting). Both
    /// must degrade gracefully on the consuming side: absent means "not
    /// reported", never "not suppressed".
    ///
    /// `#[serde(default)]` so a record from a pre-#8478 daemon still decodes
    /// (as `None`) rather than failing the whole envelope — the same
    /// backward-compatibility contract `protection` established.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_brake: Option<AdmissionBrakeSummary>,
}

/// One repository this host's daemon is currently managing (Issue #4976) —
/// the machine-level workspace registry, surfaced in `status --json`'s
/// `per_repo` but otherwise reaching the backend only via individual sweep
/// records. `visibility` is derived exactly the way sweep records already
/// derive theirs ([`visibility::derive_visibility`]), so the same
/// private-safe-default tag governs redaction here too.
///
/// This struct carries the repo's **real** slug regardless of visibility —
/// exactly like [`SweepStartedRecord::repo`] always carries the real slug.
/// The anti-leak control is enforced at the Phase-2 dashboard's redaction
/// boundary (`dashboard/src/redaction.ts`), not here; the daemon's own push
/// to the observability backend is authenticated and never reaches an
/// unauthenticated viewer directly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedRepoEntry {
    /// The `owner/repo` forge slug.
    pub slug: String,
    /// This repo's visibility class. `#[serde(default)]` so a repo entry
    /// missing the tag (should never happen from this daemon, but matches
    /// every other visibility field's defensive posture) decodes to
    /// `Private`, never `Public`.
    #[serde(default)]
    pub visibility: RepoVisibility,
}

/// One entry of a [`SessionSummaryRecord`]'s tool-call histogram: how many
/// times the session invoked one named tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallCount {
    /// Tool name exactly as the runtime recorded it (e.g. `"Bash"`) — an
    /// allowlisted shape, never tool arguments or output.
    pub tool: String,
    /// Invocations of `tool` across the whole transcript.
    pub count: u64,
}

/// `session.summary` — one transcript's session shape (Issue #8757, G3 of
/// epic #8714). Emitted by the transcript-ingest pass
/// ([`crate::activity::transcript_ingest`]), one record per ingested
/// transcript (parent session or subagent), and exported through whatever
/// exporter(s) `observability` configures — the same
/// [`crate::observability::queue::DurableQueue`] the collector feeds.
///
/// # Wire safety — a summary, never a transcript excerpt
///
/// Every field is a count, an id, an allowlisted name, or a timestamp. The
/// parse that produces it ([`crate::activity::transcript_parse`]) never
/// copies message text, tool arguments, tool output, or any free-form string
/// beyond role/model/tool **names** into the record, so no prompt, generated
/// code, key, or email can appear on the wire for this kind. The redaction
/// test suite pins this by fixture.
///
/// # Field presence contract
///
/// Optional fields (`role`, `issue`, `pr_number`, `outcome`,
/// `parent_session_id`) are **omitted** when unknown, never fabricated —
/// the same "unknown != zero" contract `host.health` established. `outcome`
/// in particular is reserved: this pass has no positive terminal-outcome
/// signal to read from a transcript, so it stays absent until a later slice
/// (`session.analysis`, or registry correlation) can populate it honestly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummaryRecord {
    /// Repository the session worked — the final path component of the
    /// session's cwd (a Loom agent's cwd is the workspace root or a worktree
    /// inside it; both map to the same repo name), matching
    /// `activity::transcript_parse::repo_from_cwd`. Not an `owner/repo`
    /// slug: the ingest pass has no forge round-trip to resolve one.
    pub repo: String,
    /// Visibility tag for `repo`. The schema contract (every record that
    /// references a repository carries one) applies; the ingest pass has no
    /// `owner/repo` slug to key [`visibility::derive_visibility`]'s cache
    /// on, so it stamps the fail-closed default — `Private` — exactly what
    /// every absent/unknown visibility decodes to anyway.
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// The session's own stable id: the transcript's `sessionId`, or the
    /// subagent file's stem for a `subagents/` transcript whose records
    /// carry no id of their own.
    pub session_id: String,
    /// The enclosing parent session's id, for a `subagents/` transcript;
    /// absent for a parent-session transcript.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Runtime that wrote the transcript (#8664's `loom.runtime` vocabulary).
    /// This pass reads Claude Code transcripts only, so today it is always
    /// `"claude"`; the field exists so sibling per-runtime tails (#8669)
    /// and this record share one shape.
    pub runtime: String,
    /// Attributed Loom role (`builder`, `judge`, …), when the first user
    /// message names one (`activity::transcript_parse::attribute_role`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Issue number, when the session is a `/loom:<role> <N>` invocation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    /// PR number, when known. Not derivable from a transcript; reserved for
    /// registry correlation (a later slice).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u32>,
    /// Distinct models used across the transcript, sorted — the `(model, day)`
    /// bucket keys collapsed to their model axis.
    pub models: Vec<String>,
    /// Token totals over the whole transcript — the same four counters
    /// `activity.db`'s `resource_usage` rows already track, summed across
    /// buckets (deduped by `message.id`, so a streamed message counts once).
    pub tokens_input: i64,
    /// See [`Self::tokens_input`].
    pub tokens_output: i64,
    /// See [`Self::tokens_input`].
    pub tokens_cache_read: i64,
    /// See [`Self::tokens_input`].
    pub tokens_cache_write: i64,
    /// Wall-clock span of the session, milliseconds — last record timestamp
    /// minus first, across every record carrying a timestamp.
    pub wall_ms: i64,
    /// Real user turns: user records whose content is **not** a tool result
    /// (the first slash-command prompt and every subsequent human turn).
    pub turns: u64,
    /// Tool invocations, histogram by tool name — assistant `tool_use`
    /// content blocks, deduped by `message.id` exactly like the token
    /// counters so a streamed message's blocks count once.
    pub tool_calls: Vec<ToolCallCount>,
    /// Tool results flagged `is_error` by the runtime.
    pub tool_errors: u64,
    /// Terminal outcome, when this pass can know one. See the struct doc:
    /// nothing populates it yet, and it serializes away while unknown.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
}

// ============================================================================
// `session.analysis` (Issue #8760, G3 part 2 of #8714)
// ============================================================================

/// One retry-loop candidate detected in a session's tool-call order (Issue
/// #8760): a run of [`length`](Self::length) consecutive invocations of the
/// identical tool name (`length >= `[`RETRY_LOOP_MIN_RUN`]). Detected purely
/// from call **order and name** — never tool arguments or output — so this
/// is a mechanical signal to investigate, not a verdict: a session that
/// legitimately calls the same tool several times in a row for unrelated
/// reasons looks identical to an actual failure-retry loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryLoop {
    /// The tool invoked repeatedly.
    pub tool: String,
    /// Consecutive invocations in the run.
    pub length: u32,
}

/// Minimum consecutive same-tool run length that counts as a retry loop
/// (Issue #8760). Two calls in a row is ordinary (e.g. `Read` then `Read` on
/// two different files); three or more consecutive identical invocations is
/// the threshold this analysis flags as loop-shaped.
pub const RETRY_LOOP_MIN_RUN: u32 = 3;

/// The single `tool_use` -> `tool_result` pairing with the largest elapsed
/// wall time in the session (Issue #8760) — the wire counterpart of
/// [`crate::activity::transcript_parse::ToolCallSpan`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongestToolCall {
    /// The tool that took the longest to return a result.
    pub tool: String,
    /// Elapsed wall time between the `tool_use` and its paired
    /// `tool_result`, milliseconds.
    pub duration_ms: i64,
}

/// An anomaly flag class a `session.analysis` record can carry (Issue
/// #8760). One variant today; additive — a future flag class is a new
/// variant, never a repurposed existing one, so an older consumer's
/// exhaustive match degrades to a decode error on an unrecognized flag
/// rather than silently misreading it as a known one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyFlag {
    /// Combined `tokens_input + tokens_output` for the session exceeded
    /// [`HIGH_TOKEN_USAGE_THRESHOLD`].
    HighTokenUsage,
}

/// Static initial calibration for [`AnomalyFlag::HighTokenUsage`] (Issue
/// #8760): combined `tokens_input + tokens_output` above this value flags
/// the session. A fixed round number, **not** a true per-role fleet
/// percentile (#8714's own illustrative "tokens > p99 for role" example) —
/// this derivation has no access to a fleet-wide token distribution, only
/// the one session's own `session.summary` fields, so a live percentile
/// needs a historical query this bounded, mechanical slice does not add.
/// Chosen well above a typical multi-hour Builder/Judge session's observed
/// token volume so the flag stays rare and worth investigating rather than
/// routine.
pub const HIGH_TOKEN_USAGE_THRESHOLD: i64 = 300_000;

/// `session.analysis` — a derived per-session anomaly/quality rollup (Issue
/// #8760, G3 part 2 of epic #8714), computed from a landed
/// [`SessionSummaryRecord`] plus the
/// [`crate::activity::transcript_parse::ParsedTranscript`] that produced it
/// (see [`crate::activity::session_analysis::build_session_analysis`]).
/// Rides the same transcript-ingest emission point as `session.summary`, so
/// a still-growing session is re-analyzed on each pass that re-reads it,
/// mirroring that record's own replace-never-append semantics.
///
/// **Bounded, mechanical derivation only** — retry-loop detection, the
/// longest paired tool call, a USD cost estimate (from the existing single
/// [`crate::activity::resource_usage::ModelPricing`] rate card, applied
/// per-model to the transcript's own usage buckets so a multi-model session
/// is costed correctly rather than approximated from a flat total), and a
/// fixed-threshold anomaly flag. **No LLM-written prose summary** — that is
/// explicitly a later slice, per #8714's own G3 proposal.
///
/// # Wire safety — same contract as `session.summary`
///
/// Every field here is a count, an id, an allowlisted tool name, a
/// duration, or a derived dollar figure. Nothing here is, or is derived
/// from, message text, tool arguments, tool output, or any free-form
/// transcript content — the redaction test suite pins this by fixture,
/// mirroring `session_summary`'s own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionAnalysisRecord {
    /// Same value as the source `session.summary` record's `repo`.
    pub repo: String,
    /// Same value as the source `session.summary` record's `visibility`.
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// Same value as the source `session.summary` record's `session_id` —
    /// the join key a consumer uses to correlate the two records.
    pub session_id: String,
    /// Same value as the source `session.summary` record's
    /// `parent_session_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Retry-loop candidates detected in the session's tool-call order.
    /// Empty when none were detected — never omitted, so "computed and
    /// found none" is distinguishable on the wire from "not computed".
    #[serde(default)]
    pub retry_loops: Vec<RetryLoop>,
    /// The longest paired `tool_use` -> `tool_result` call in the session.
    /// Absent when no pair could be matched (see
    /// [`crate::activity::transcript_parse::ParsedTranscript::longest_tool_call`]'s
    /// doc for why that can happen) — never a fabricated zero duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longest_tool_call: Option<LongestToolCall>,
    /// USD cost estimate, summed per-model across the session's own usage
    /// buckets via the shared rate card
    /// ([`crate::activity::resource_usage::ModelPricing`]). Absent when the
    /// session contributed no usage buckets at all — never a fabricated
    /// `0.0`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Anomaly flags raised for this session. Empty when none were raised.
    #[serde(default)]
    pub anomalies: Vec<AnomalyFlag>,
}

// ============================================================================
// `daemon.event` (Issue #8760, G4 of #8714)
// ============================================================================

/// `daemon.event` — one of the four named event-bus topics that carried no
/// telemetry record kind of their own (Issue #8760, G4 of epic #8714):
/// `daemon.drain.*`, `daemon.capacity.advisory`, `daemon.preflight.advisory`,
/// and `epic.issue.*`. Host/daemon-level operational signals, not per-session
/// user work — carries no [`RepoVisibility`] tag, the same "references no
/// repository" contract [`TokenSnapshotRecord`]/[`HostHealthRecord`] already
/// establish.
///
/// Deliberately generic — one record shape for four topic families — rather
/// than four new per-topic record types: every one of these topics is
/// already a small, frozen, operator-facing payload
/// ([`crate::event_bus`]'s own documented taxonomy) with no
/// prompt/tool-output/secret content by construction, the same shape the
/// live SSE event-bus tail already exposes to an authenticated operator.
/// Wrapping that payload, rather than re-typing it four times, keeps this
/// record kind additive to an already-reviewed shape instead of forking a
/// second schema for the same data.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DaemonEventRecord {
    /// The exact bus topic this record mirrors, e.g.
    /// `"daemon.drain.started"`, `"epic.issue.123.decompose"`.
    pub topic: String,
    /// The event's own payload, exactly as published on the bus.
    pub payload: serde_json::Value,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
