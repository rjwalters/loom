//! `session.output` (#9764): the **live** agent-output record kind — a
//! readable, redacted, issue-scoped stream published *while* a run is still
//! in flight.
//!
//! # Why this is not `session.summary`
//!
//! [`SessionSummaryRecord`](crate::telemetry::SessionSummaryRecord) and
//! `session.analysis` are post-hoc aggregates emitted by the transcript-ingest
//! pass, whose default cadence is 900 s
//! ([`crate::activity::transcript_ingest`]). Neither carries readable content,
//! and neither is incremental. A consumer that wants to show "what is this
//! agent doing right now" on an issue detail view has nothing to read. This
//! kind is that missing producer contract, and nothing else: it does **not**
//! replace, widen or relax either summary kind.
//!
//! # Wire safety
//!
//! This is the second kind (after `ci.job.log`) whose OTLP **body** is text the
//! daemon did not author, and the first whose text originates inside an agent
//! session. Three structural properties bound it, in this order:
//!
//! 1. **The producer selects.** Only assistant-authored text and tool
//!    start/finish *metadata* are ever turned into a record
//!    ([`OutputCategory`]). Prompts, thinking blocks, tool arguments and raw
//!    tool results have no representation here at all — they are dropped at
//!    parse time, not filtered later.
//! 2. **The producer redacts.** Every body passes [`redact::scrub`] before it
//!    is ever placed on a record ([`SessionOutputRecord::new_output`] is the
//!    only constructor that accepts text, and it always scrubs). The record
//!    carries the applied policy name in
//!    [`redaction`](SessionOutputRecord::redaction) so a consumer can tell
//!    which version produced a row.
//! 3. **OTLP only.** The row is declared `native: false` in
//!    [`telemetry_kind_table!`](crate::telemetry_kind_table): the native HTTPS
//!    `/ingest` backend — the managed-cloud sink — never receives it, on any
//!    configuration. Enabling live output cannot, by construction, start
//!    shipping session text to a managed sink; the collector's
//!    `transform/session_output_redaction` stage then re-scrubs the body at
//!    the gateway as defence in depth.
//!
//! # Identity, ordering and gaps
//!
//! Every record carries a [`stream_id`](SessionOutputRecord::stream_id) and a
//! monotonic [`sequence`](SessionOutputRecord::sequence) within it, plus an
//! [`event_id`](SessionOutputRecord::event_id) that is a pure function of the
//! two. A replay of the same source event therefore produces a
//! byte-identical id, which is what makes de-duplication possible on the
//! consumer side across a producer restart. Two records with the same
//! `source_at` (common — a transcript writes several records inside one
//! millisecond) are still distinguishable by `sequence`.
//!
//! Where delivery cannot be complete, that is **stated** rather than implied:
//! a [`OutputCategory::Gap`] record names the reason and the number of source
//! events lost, and [`truncated_bytes`](SessionOutputRecord::truncated_bytes)
//! reports per-record clipping. A consumer that renders a transcript without
//! reading those is choosing to imply completeness the producer never claimed.

pub mod redact;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::{RepoVisibility, SessionKind};

/// The producer-contract version carried on every record
/// ([`SessionOutputRecord::schema`]). Distinct from the envelope's
/// `schema_version`: the envelope gates *wire safety of the kind*, this gates
/// *the shape and redaction policy of the payload*, which a consumer pins.
pub const SESSION_OUTPUT_SCHEMA: u32 = 1;

/// Maximum characters of readable text on one record. A longer source event is
/// clipped and its record reports the clipped byte count in
/// [`SessionOutputRecord::truncated_bytes`] — the row is never silently short.
pub const MAX_TEXT_CHARS: usize = 2_000;

/// Every log attribute key this kind exports besides the generic
/// `loom.repo` / `loom.issue` and the already-allowlisted run-identity keys
/// (`loom.sweep_id`, `loom.session_id`, `loom.attempt`, `loom.runtime`,
/// `loom.role`, `loom.repo.visibility`, `loom.session_kind`). The collector's `transform/privacy` log `keep_keys` must list
/// each one (`defaults/observability/collector/config.yaml`), which
/// `collector_keeps_every_session_output_attribute` pins.
pub const SESSION_OUTPUT_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.session.output.schema",
    "loom.session.output.category",
    "loom.session.output.stream",
    "loom.session.output.stream_id",
    "loom.session.output.sequence",
    "loom.session.output.event_id",
    "loom.session.output.tool",
    "loom.session.output.tool_ok",
    "loom.session.output.coverage",
    "loom.session.output.state",
    "loom.session.output.truncated_bytes",
    "loom.session.output.dropped_events",
    "loom.session.output.gap_reason",
    "loom.session.output.redaction",
    "loom.session.output.producer_lag_ms",
    "loom.session.output.lag_samples",
    "loom.session.output.lag_p50_ms",
    "loom.session.output.lag_p95_ms",
    "loom.session.output.lag_max_ms",
    "loom.session.output.lag_historical_excluded",
];

/// What a record represents. The closed vocabulary a consumer switches on;
/// serialized snake_case and exported verbatim as
/// `loom.session.output.category`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputCategory {
    /// Readable assistant-authored text.
    Output,
    /// An agent started a tool call. Carries the tool *name* only — never its
    /// arguments.
    ToolStart,
    /// A tool call finished. Carries the tool name and an ok/error flag —
    /// never the result payload.
    ToolFinish,
    /// The run is still alive and the producer is still reading it, but no new
    /// source event arrived in this interval. This is what distinguishes a
    /// *quiet* run from a *stalled export*.
    Heartbeat,
    /// Source events exist that this producer could not deliver. Always names
    /// a [`gap_reason`](SessionOutputRecord::gap_reason) and a
    /// [`dropped_events`](SessionOutputRecord::dropped_events) count.
    Gap,
    /// A statement about whether this run is covered at all — emitted at the
    /// start and end of every tracked run, including
    /// [`Coverage::Unsupported`] for a runtime that has no adapter. An
    /// unsupported run is explicit, never silently reported as live.
    Coverage,
}

impl OutputCategory {
    /// The wire string, matching the serde spelling exactly.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OutputCategory::Output => "output",
            OutputCategory::ToolStart => "tool_start",
            OutputCategory::ToolFinish => "tool_finish",
            OutputCategory::Heartbeat => "heartbeat",
            OutputCategory::Gap => "gap",
            OutputCategory::Coverage => "coverage",
        }
    }
}

/// Which logical stream a record's content came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputStream {
    /// Assistant-authored text.
    Assistant,
    /// Tool lifecycle metadata.
    Tool,
    /// A producer-authored status record (heartbeat / gap / coverage).
    Status,
}

impl OutputStream {
    /// The wire string, matching the serde spelling exactly.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            OutputStream::Assistant => "assistant",
            OutputStream::Tool => "tool",
            OutputStream::Status => "status",
        }
    }
}

/// How well this producer covers the run the record belongs to. A consumer
/// showing a live feed must surface anything that is not [`Coverage::Live`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Coverage {
    /// A supported runtime whose output this producer is reading.
    Live,
    /// The runtime has no live-output adapter. No `output` record will ever be
    /// emitted for this run; saying so is the whole point of the record.
    Unsupported,
    /// A supported runtime whose source could not be located or read (no
    /// transcript found, unreadable directory). Recoverable — the producer
    /// keeps looking, and emits `Live` if it succeeds.
    Degraded,
    /// The run finished; this is the producer's last word on it.
    Ended,
}

impl Coverage {
    /// The wire string, matching the serde spelling exactly.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Coverage::Live => "live",
            Coverage::Unsupported => "unsupported",
            Coverage::Degraded => "degraded",
            Coverage::Ended => "ended",
        }
    }
}

/// The run's liveness as the producer last observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// New source events are arriving.
    Running,
    /// The run is alive but has produced nothing recently.
    Idle,
    /// The run reached a terminal state.
    Ended,
}

impl RunState {
    /// The wire string, matching the serde spelling exactly.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            RunState::Running => "running",
            RunState::Idle => "idle",
            RunState::Ended => "ended",
        }
    }
}

/// The identity every record of one run repeats — resolved **once**, when the
/// run is first observed, and cloned onto each record so two concurrent issues
/// and two attempts of the same issue can never be conflated.
///
/// `repo` is the canonical forge slug (`gh repo view --json nameWithOwner`),
/// resolved through `observability::collector::resolve_repo_slug_cached`.
/// It is **never** derived from a directory basename: a worktree named
/// `issue-9764` under a checkout named `loom-two` is still `rjwalters/loom`,
/// and a basename would silently mint a second, wrong repo identity.
/// `None` means genuinely unknown — an unattributed interactive session stays
/// explicitly unscoped rather than being guessed at from output text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct RunIdentity {
    /// Canonical `owner/repo`, or `None` when unresolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Visibility tag for `repo`, exported as `loom.repo.visibility`.
    ///
    /// Always the fail-closed [`RepoVisibility::Private`] default here: like
    /// the `session.summary` pass, this producer makes no forge round trip to
    /// ask. The tag matters because this is the one kind whose body is readable
    /// session text — a public view must be able to exclude it without
    /// inspecting the body, and a missing or malformed tag decodes to `Private`
    /// by construction, so it can never leak by omission.
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// Forge issue number, or `None` for a session with no explicit issue
    /// target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issue: Option<u32>,
    /// Why [`issue`](Self::issue) is set — or deliberately is not
    /// ([`SessionKind`], the #9445 vocabulary).
    ///
    /// This is what makes "unattributed sessions stay explicitly unscoped" a
    /// statement rather than an absence: `Interactive` says the missing issue is
    /// *intended*, while a `Sweep`/`Role` row with no issue says attribution was
    /// *lost*. A consumer cannot tell those apart from a null `loom.issue`
    /// alone, and guessing the issue from output text is exactly what this
    /// field exists to avoid needing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<SessionKind>,
    /// The dispatching sweep's id, when the run is a sweep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_id: Option<String>,
    /// The runtime session id, when known (the transcript's own id for the
    /// Claude adapter).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 1-based attempt index for this `(repo, issue)` within this producer's
    /// lifetime — what separates a retry's stream from the original's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
    /// The runtime adapter (`claude`, `codex`, `pi`, `opencode`, …).
    pub runtime: String,
    /// The agent role, when the run declares one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

/// The producer-lag distribution over a run's recent source events, attached
/// to status records only (a content record reports its own single
/// [`producer_lag_ms`](SessionOutputRecord::producer_lag_ms) instead).
///
/// # Why a run reports its own percentiles
///
/// End-to-end source-to-queryable latency is the sum of two independently
/// observable halves: `observed_at - source_at` (this producer's read lag) and
/// `backend_ingest - observed_at` (export + gateway + backend). A consumer can
/// compute the second half from the two timestamps every record already
/// carries, but the first half is only cheaply summarizable *here*, where the
/// samples are. Shipping p50/p95/max on each status record means a dashboard
/// can show the producer half of its own latency budget without aggregating
/// every content row in the window.
///
/// # Historical events are excluded, on purpose
///
/// `observed_at - source_at` is only a *latency* measurement when the producer
/// was already watching when the source event happened. For an event replayed
/// out of a transcript that existed before this producer attached, the same
/// subtraction yields the transcript's **age** — minutes or hours — which
/// would dominate any percentile it entered and report a stall that never
/// happened. Those samples are counted in
/// [`historical_excluded`](Self::historical_excluded) and kept out of the
/// distribution; the count is exported so a consumer can see that the
/// exclusion happened rather than wonder where the samples went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LagStats {
    /// Fresh samples in the window the percentiles were computed over.
    pub samples: u64,
    /// Median producer lag, milliseconds.
    pub p50_ms: i64,
    /// 95th-percentile producer lag, milliseconds — the figure the #9764
    /// budget is stated against.
    pub p95_ms: i64,
    /// Worst producer lag in the window, milliseconds.
    pub max_ms: i64,
    /// Source events excluded from the distribution for predating this
    /// producer's attach. Cumulative for the run, never reset by a window
    /// eviction, so it cannot silently return to zero.
    pub historical_excluded: u64,
}

/// One live agent-output event.
///
/// Construct through [`SessionOutputRecord::new_output`] (the only path that
/// accepts free text, and therefore the only path that must redact) or
/// [`SessionOutputRecord::status`] / [`SessionOutputRecord::tool`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOutputRecord {
    /// [`SESSION_OUTPUT_SCHEMA`] — the payload contract a consumer pins.
    pub schema: u32,
    /// Run identity; see [`RunIdentity`].
    #[serde(flatten)]
    pub identity: RunIdentity,
    /// What this record represents.
    pub category: OutputCategory,
    /// Which logical stream it came from.
    pub stream: OutputStream,
    /// The ordered stream this record belongs to — the transcript file's
    /// logical key, stable for the life of the run.
    pub stream_id: String,
    /// 0-based position within [`stream_id`](Self::stream_id). Monotonic, and
    /// what disambiguates two records sharing one `source_at`.
    pub sequence: u64,
    /// `{stream_id}#{sequence}` — a pure function of the source event, so a
    /// replay reproduces it byte-identically.
    pub event_id: String,
    /// When the source event happened (the transcript record's own timestamp,
    /// or the producer's clock for a status record).
    pub source_at: DateTime<Utc>,
    /// When this producer read it. Kept separate from `source_at` on purpose:
    /// the difference is producer lag, and collapsing them would make a quiet
    /// run indistinguishable from a stalled one.
    pub observed_at: DateTime<Utc>,
    /// Readable, **already-redacted**, length-bounded content. Absent on
    /// status records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Tool name for `tool_start` / `tool_finish`. Never its arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Whether a `tool_finish` succeeded. Never the result payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_ok: Option<bool>,
    /// Coverage status of the run as of this record.
    pub coverage: Coverage,
    /// Run liveness as of this record.
    pub state: RunState,
    /// Bytes of source text this record dropped to fit [`MAX_TEXT_CHARS`].
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub truncated_bytes: u64,
    /// Source events this producer could not deliver, cumulative for the run.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub dropped_events: u64,
    /// Why events were lost — required on [`OutputCategory::Gap`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gap_reason: Option<String>,
    /// The redaction policy applied at the producer edge, e.g. `producer/v1`.
    pub redaction: String,
    /// Producer-lag percentiles for the run, on status records only. `None` on
    /// a content record, and `None` on a status record for a run that has not
    /// yet observed a fresh source event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lag: Option<LagStats>,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// The fields every [`SessionOutputRecord`] constructor must supply.
///
/// A parameter object rather than a nine-argument helper, and not only to
/// satisfy `clippy::too_many_arguments`: positionally, `source_at` and
/// `observed_at` are adjacent values of the *same* type, as are `coverage` and
/// `state`. Transposing either pair compiles cleanly and silently inverts
/// producer lag or mislabels a run's health. Named fields make that
/// transposition impossible to write by accident.
struct Base {
    identity: RunIdentity,
    category: OutputCategory,
    stream: OutputStream,
    stream_id: String,
    sequence: u64,
    source_at: DateTime<Utc>,
    observed_at: DateTime<Utc>,
    coverage: Coverage,
    state: RunState,
}

impl SessionOutputRecord {
    fn base(base: Base) -> Self {
        let Base {
            identity,
            category,
            stream,
            stream_id,
            sequence,
            source_at,
            observed_at,
            coverage,
            state,
        } = base;
        let event_id = event_id(&stream_id, sequence);
        SessionOutputRecord {
            schema: SESSION_OUTPUT_SCHEMA,
            identity,
            category,
            stream,
            stream_id,
            sequence,
            event_id,
            source_at,
            observed_at,
            text: None,
            tool: None,
            tool_ok: None,
            coverage,
            state,
            truncated_bytes: 0,
            dropped_events: 0,
            gap_reason: None,
            redaction: redact::POLICY.to_string(),
            lag: None,
        }
    }

    /// One readable-output record. **This is the only constructor that accepts
    /// free text, and it always redacts and bounds it** — there is deliberately
    /// no way to place un-scrubbed text on a record.
    #[must_use]
    pub fn new_output(
        identity: RunIdentity,
        stream_id: impl Into<String>,
        sequence: u64,
        source_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
        text: &str,
    ) -> Self {
        let (body, truncated_bytes) = redact::scrub_bounded(text, MAX_TEXT_CHARS);
        let mut record = Self::base(Base {
            identity,
            category: OutputCategory::Output,
            stream: OutputStream::Assistant,
            stream_id: stream_id.into(),
            sequence,
            source_at,
            observed_at,
            coverage: Coverage::Live,
            state: RunState::Running,
        });
        record.text = Some(body);
        record.truncated_bytes = truncated_bytes;
        record
    }

    /// One tool lifecycle record. `ok` is `None` for a start, `Some(_)` for a
    /// finish. No arguments and no result payload are accepted by this
    /// signature, so neither can be carried by accident.
    #[must_use]
    pub fn tool(
        identity: RunIdentity,
        stream_id: impl Into<String>,
        sequence: u64,
        source_at: DateTime<Utc>,
        observed_at: DateTime<Utc>,
        tool: &str,
        ok: Option<bool>,
    ) -> Self {
        let category = if ok.is_some() {
            OutputCategory::ToolFinish
        } else {
            OutputCategory::ToolStart
        };
        let mut record = Self::base(Base {
            identity,
            category,
            stream: OutputStream::Tool,
            stream_id: stream_id.into(),
            sequence,
            source_at,
            observed_at,
            coverage: Coverage::Live,
            state: RunState::Running,
        });
        // A tool name is short, daemon-adjacent metadata, but it is still not
        // daemon-authored — scrub and bound it like any other foreign text.
        let (name, _) = redact::scrub_bounded(tool, 120);
        record.tool = Some(name);
        record.tool_ok = ok;
        record
    }

    /// One producer-authored status record (heartbeat / gap / coverage). These
    /// never carry session content, only the producer's own statement about
    /// what it can and cannot see.
    #[must_use]
    pub fn status(
        identity: RunIdentity,
        category: OutputCategory,
        stream_id: impl Into<String>,
        sequence: u64,
        at: DateTime<Utc>,
        coverage: Coverage,
        state: RunState,
    ) -> Self {
        Self::base(Base {
            identity,
            category,
            stream: OutputStream::Status,
            stream_id: stream_id.into(),
            sequence,
            // A status record is producer-authored: it has no separate source
            // event, so its source and observed times are the same instant by
            // definition, not by omission.
            source_at: at,
            observed_at: at,
            coverage,
            state,
        })
    }

    /// Attach loss information. Returns `self` so a gap record reads as one
    /// expression at the call site.
    #[must_use]
    pub fn with_gap(mut self, reason: impl Into<String>, dropped_events: u64) -> Self {
        self.gap_reason = Some(reason.into());
        self.dropped_events = dropped_events;
        self
    }

    /// Attach the run's producer-lag distribution. Only meaningful on a status
    /// record; a content record reports its own single
    /// [`producer_lag_ms`](Self::producer_lag_ms) and is left alone so one
    /// source event never carries a whole window's summary.
    #[must_use]
    pub fn with_lag(mut self, lag: Option<LagStats>) -> Self {
        if self.stream == OutputStream::Status {
            self.lag = lag;
        }
        self
    }

    /// Producer lag in milliseconds — `observed_at - source_at`, floored at 0.
    /// Exported as `loom.session.output.producer_lag_ms` so the source-side
    /// half of end-to-end latency is measurable without joining two rows.
    #[must_use]
    pub fn producer_lag_ms(&self) -> i64 {
        (self.observed_at - self.source_at)
            .num_milliseconds()
            .max(0)
    }

    /// Whether this record can be shown as part of a live feed. `false` for
    /// every status category, which a consumer must render as status, not as
    /// transcript content.
    #[must_use]
    pub fn is_content(&self) -> bool {
        matches!(
            self.category,
            OutputCategory::Output | OutputCategory::ToolStart | OutputCategory::ToolFinish
        )
    }
}

/// The stable event id for a source event: a pure function of its stream and
/// position, so the same source event replayed after a producer restart lands
/// on the same id and a consumer can de-duplicate it.
#[must_use]
pub fn event_id(stream_id: &str, sequence: u64) -> String {
    format!("{stream_id}#{sequence}")
}

#[cfg(test)]
#[path = "session_output_tests.rs"]
mod tests;
