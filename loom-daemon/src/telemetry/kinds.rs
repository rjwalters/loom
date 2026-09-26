//! The telemetry record-kind registry — **one declaration per kind** (#8921).
//!
//! # Why this file exists
//!
//! Before #8921, adding a record kind meant editing four shared files at
//! adjacent lines: the [`TelemetryRecord`](super::TelemetryRecord) variant plus
//! its `#[serde(rename)]` in `telemetry/mod.rs`, a hand-assigned
//! `schema_version` match arm in `telemetry/envelope.rs`, two exhaustive
//! `TelemetryRecord::X(_) | …` routing chains in
//! `observability/otlp/mapping.rs` (plus `signal_for` / `otlp_only_signal`),
//! and two append-point tables in `defaults/docs/telemetry-schema.md`. Two
//! concurrent PRs that each added a kind were unmergeable by construction even
//! though each was individually clean — #8915 and #8909 collided exactly that
//! way on 2026-09-25, and the `=> 10` "next number from one hand-maintained
//! sequence" arm conflicted *by definition*.
//!
//! Everything a kind registers now lives in the one-row-per-kind table below,
//! and that table is the only shared line a new kind touches. The row is
//! append-only and self-contained, so `.gitattributes` marks this file
//! `merge=union`: two branches that each append a row merge cleanly with no
//! conflict and no lost row (see the `kind_registry` tests, which assert both
//! the attribute and the merge behaviour).
//!
//! # Adding a record kind
//!
//! 1. Put the payload struct in its own module. Existing families live at
//!    `telemetry/<family>.rs` (declared in `telemetry/mod.rs`); a **new**
//!    family should live at `telemetry/kinds/<family>.rs` and be declared with
//!    a `pub mod <family>;` line *in this file* (below), so it costs zero
//!    lines in the over-threshold `telemetry/mod.rs`.
//! 2. Append one row to [`telemetry_kind_table!`]. That row generates the enum
//!    variant, the serde tag, the envelope's `schema_version`, the OTLP
//!    routing class, the native-ingest scope, and the [`TELEMETRY_KINDS`]
//!    metadata entry.
//! 3. Leave `gate:` as [`NEW_KIND_SCHEMA_VERSION`] — **do not invent a
//!    number**. Every kind added after #8921 shares that one gate, so no two
//!    PRs can claim the same next integer and no PR's gate value can shift
//!    under it when a sibling PR merges first. A kind whose *wire safety*
//!    genuinely needs its own gate (the `ci.job.log` free-text case) pins a
//!    fresh literal deliberately and documents it in
//!    `defaults/docs/telemetry-schema.md`; that is the rare exception, not the
//!    default path.
//! 4. Only if the kind needs an OTLP log/gauge/histogram mapping, add its
//!    mapping in the sibling module that owns it
//!    (`observability/otlp/mapping/<family>.rs`) — the routing *decision* is
//!    the `otlp:` column here, not a match arm in `mapping.rs`.
//!
//! # Columns
//!
//! | Column | Meaning |
//! |---|---|
//! | variant | The [`TelemetryRecord`](super::TelemetryRecord) variant name. |
//! | `"…"` | The wire `kind` tag (the `#[serde(rename)]` value). Must be unique. |
//! | payload | The record struct carried by the variant. |
//! | `gate:` | The envelope `schema_version` this kind's envelopes carry. |
//! | `otlp:` | Which OTLP signal shape this kind exports as ([`TelemetryKindOtlp`]). |
//! | `native:` | Whether the native HTTPS `/ingest` backend accepts it. |

use std::fmt;

/// How a record kind exports over OTLP — the routing *class*, declared once
/// next to the kind itself instead of being spelled out in the exhaustive
/// match chains `observability/otlp/` used to carry per kind.
///
/// This lives with the schema declaration (not in `observability/`) because it
/// is **registration metadata**, not behaviour: it says which shape a kind
/// exports as, while every field-by-field mapping stays in
/// `observability/otlp/mapping/`. Keeping it here is what lets one row own a
/// kind's whole registration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryKindOtlp {
    /// An OTLP log record (`/v1/logs`) — one event per record.
    Logs,
    /// OTLP gauge data points (`/v1/metrics`) — a host/pool sample.
    Gauges,
    /// An OTLP histogram data point (`/v1/metrics`) — a duration sample.
    Histograms,
    /// Generic `metric.points` carrier points, grouped by
    /// `observability::otlp::mapping::ops` (`/v1/metrics`).
    OpsPoints,
    /// An OTLP span (`/v1/traces`).
    Spans,
    /// Not exported over OTLP at all (a native-HTTPS-only kind).
    NotExported,
}

impl TelemetryKindOtlp {
    /// The OTLP signal name this class belongs to — `"logs"`, `"metrics"`,
    /// `"spans"`, or `None` for a kind OTLP never carries.
    #[must_use]
    pub fn signal(self) -> Option<&'static str> {
        match self {
            TelemetryKindOtlp::Logs => Some("logs"),
            TelemetryKindOtlp::Gauges
            | TelemetryKindOtlp::Histograms
            | TelemetryKindOtlp::OpsPoints => Some("metrics"),
            TelemetryKindOtlp::Spans => Some("spans"),
            TelemetryKindOtlp::NotExported => None,
        }
    }
}

impl fmt::Display for TelemetryKindOtlp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TelemetryKindOtlp::Logs => "Logs",
            TelemetryKindOtlp::Gauges => "Gauges",
            TelemetryKindOtlp::Histograms => "Histograms",
            TelemetryKindOtlp::OpsPoints => "OpsPoints",
            TelemetryKindOtlp::Spans => "Spans",
            TelemetryKindOtlp::NotExported => "NotExported",
        })
    }
}

/// The envelope `schema_version` every record kind added **after #8921**
/// carries, unless it deliberately pins its own.
///
/// Gates `1`–`11` were allocated one-per-kind by hand, each PR taking "the next
/// number" from a single sequence in `envelope.rs` — the line two concurrent
/// PRs conflicted on by construction. `12` ends that: a new kind writes this
/// *symbolic* value, so there is no integer to claim, no ordering to contend
/// for, and no chance a PR's gate silently changes when a sibling merges first.
///
/// A backend that needs to refuse one specific post-#8921 kind gates on the
/// record's `kind` tag (always unique — see [`TELEMETRY_KINDS`]), not on this
/// number. A kind whose wire content is risky enough to deserve its own
/// version-level gate (the `ci.job.log` free-text precedent at `9`) pins a
/// fresh literal in its row and documents the row in
/// `defaults/docs/telemetry-schema.md`.
pub const NEW_KIND_SCHEMA_VERSION: u32 = 12;

/// One registry row, reflected at runtime — what [`TELEMETRY_KINDS`] is a slice
/// of. Generated from the same table that generates the enum, so it can never
/// drift from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TelemetryKindMeta {
    /// The `TelemetryRecord` variant name (Rust-side identity).
    pub variant: &'static str,
    /// The wire `kind` tag (schema-side identity).
    pub kind: &'static str,
    /// The envelope `schema_version` this kind's envelopes carry.
    pub schema_version: u32,
    /// How this kind exports over OTLP.
    pub otlp: TelemetryKindOtlp,
    /// Whether the native HTTPS `/ingest` backend accepts this kind.
    pub native_ingest: bool,
}

// ============================================================================
// Payload modules for kind families added after #8921
// ============================================================================
//
// Declared HERE (not in `telemetry/mod.rs`) so a new family costs zero lines
// in a shared file: this file is `merge=union`, so two branches each adding a
// `pub mod` line merge cleanly. Files live at `telemetry/kinds/<family>.rs`.
// (None yet — the pre-#8921 families stay where they are, declared in
// `telemetry/mod.rs`; moving them would churn every in-flight PR that touches
// them for no schema benefit.)

// ============================================================================
// THE REGISTRY
// ============================================================================

/// The record-kind table — **the single registration site for a telemetry
/// record kind**.
///
/// This is a push-down/callback macro: it does not expand to items itself, it
/// hands its rows to a `$callback` macro that does. That is what lets one table
/// generate several independent things (the enum + its serde tags, the
/// `schema_version` / routing accessors, the [`TELEMETRY_KINDS`] metadata)
/// without the table itself living in — or being duplicated across — the files
/// those things are defined in.
///
/// ```ignore
/// macro_rules! count_kinds {
///     ($( $(#[$attr:meta])* $variant:ident = $kind:literal => $payload:ty,
///         gate: $gate:expr, otlp: $class:ident, native: $native:literal; )+) => {
///         const KIND_COUNT: usize = [$( $kind ),+].len();
///     };
/// }
/// crate::telemetry_kind_table!(count_kinds);
/// ```
#[macro_export]
macro_rules! telemetry_kind_table {
    ($callback:ident) => {
        $callback! {
            /// A sweep began (mirrors the dispatch moment of the frozen SSE topics).
            SweepStarted = "sweep.started" => $crate::telemetry::SweepStartedRecord,
                gate: $crate::telemetry::CURRENT_SCHEMA_VERSION, otlp: Logs, native: true;

            /// Late-resolved launch identity; enriches an existing active sweep only.
            SweepIdentity = "sweep.identity" => $crate::telemetry::SweepIdentityRecord,
                gate: 4, otlp: Logs, native: true;

            /// A sweep advanced to a new lifecycle phase (mirrors `sweep.issue.{N}.phase`).
            SweepPhase = "sweep.phase" => $crate::telemetry::SweepPhaseRecord,
                gate: $crate::telemetry::CURRENT_SCHEMA_VERSION, otlp: Logs, native: true;

            /// A sweep reached a terminal state (mirrors the exited/crashed/completed
            /// frozen topics; the richer per-phase/config detail lives in the paired
            /// [`SweepOutcomeRecord`]).
            SweepCompleted = "sweep.completed" => $crate::telemetry::SweepCompletedRecord,
                gate: $crate::telemetry::CURRENT_SCHEMA_VERSION, otlp: Logs, native: true;

            /// The full post-hoc outcome of a sweep: model/config/effort, per-phase
            /// durations, terminal result, and PR number.
            SweepOutcome = "sweep.outcome" => $crate::telemetry::SweepOutcomeRecord,
                gate: $crate::telemetry::CURRENT_SCHEMA_VERSION, otlp: Logs, native: true;

            /// A snapshot of the multi-account token pool's per-account usage state.
            TokensSnapshot = "tokens.snapshot" => $crate::telemetry::TokenSnapshotRecord,
                gate: $crate::telemetry::CURRENT_SCHEMA_VERSION, otlp: Gauges, native: true;

            /// Host health: CPU/disk headroom, daemon version, uptime.
            HostHealth = "host.health" => $crate::telemetry::HostHealthRecord,
                gate: $crate::telemetry::CURRENT_SCHEMA_VERSION, otlp: Gauges, native: true;

            /// One role-runner tick's outcome (Issue #8056) — the per-`(root, role)`
            /// counterpart of [`SweepOutcome`](Self::SweepOutcome). The seventh
            /// variant, and the reason [`CURRENT_SCHEMA_VERSION`] is `2`.
            RoleTickOutcome = "role_tick.outcome" => $crate::telemetry::RoleTickOutcomeRecord,
                gate: $crate::telemetry::CURRENT_SCHEMA_VERSION, otlp: Logs, native: true;

            /// One transcript's session shape (Issue #8757, G3 of #8714) — ids,
            /// attribution, models, token totals, and turn/tool counts, emitted by
            /// the transcript-ingest pass. Carries **no** prompt, tool-output, key
            /// or email content by construction (see [`SessionSummaryRecord`]).
            SessionSummary = "session.summary" => $crate::telemetry::SessionSummaryRecord,
                gate: 5, otlp: Logs, native: true;

            /// A derived per-session anomaly/quality rollup (Issue #8760, G3 part 2
            /// of #8714) — retry-loop detection, the longest paired tool call, a USD
            /// cost estimate, and anomaly flags, computed from a
            /// [`SessionSummaryRecord`] plus the
            /// [`crate::activity::transcript_parse::ParsedTranscript`] that produced
            /// it. See [`SessionAnalysisRecord`] for the wire-safety contract (same
            /// as `session.summary`: no prompt, tool-output, key or email content,
            /// ever).
            SessionAnalysis = "session.analysis" => $crate::telemetry::SessionAnalysisRecord,
                gate: 6, otlp: Logs, native: true;

            /// One of the four named event-bus topics that carried no telemetry
            /// record kind of their own (Issue #8760, G4 of #8714): `daemon.drain.*`,
            /// `daemon.capacity.advisory`, `daemon.preflight.advisory`, and
            /// `epic.issue.*`. See [`DaemonEventRecord`].
            DaemonEvent = "daemon.event" => $crate::telemetry::DaemonEventRecord,
                gate: 7, otlp: Logs, native: true;

            /// One distributed-tracing span (Issue #8631). OTLP-only — the native
            /// HTTPS `/ingest` backend never receives spans.
            Span = "trace.span" => $crate::telemetry::trace::SpanRecord,
                gate: 3, otlp: Spans, native: false;

            /// One completed GitHub Actions workflow run (Issue #8824). See
            /// [`ci`] for the CI record family and its attribute allowlist.
            CiRun = "ci.run" => $crate::telemetry::CiRunRecord,
                gate: 8, otlp: Logs, native: true;

            /// One completed job of a GitHub Actions run (Issue #8824).
            CiJob = "ci.job" => $crate::telemetry::CiJobRecord,
                gate: 8, otlp: Logs, native: true;

            /// One CI run/job duration sample — the carrier for the
            /// `loom.ci.{run,job}.duration_ms` histograms (Issue #8824).
            CiDuration = "ci.duration" => $crate::telemetry::CiDurationRecord,
                gate: 8, otlp: Histograms, native: true;

            /// One ≤ 8 KiB chunk of a completed job's log text (Issue #8825). The
            /// only kind carrying free text the daemon did not author — see
            /// [`CiJobLogRecord`] for why the gateway, not the source, scrubs it,
            /// and why it pins its own gate instead of sharing the CI family's `8`.
            CiJobLog = "ci.job.log" => $crate::telemetry::CiJobLogRecord,
                gate: 9, otlp: Logs, native: true;

            /// A batch of generic operational metric points (Issue #8860) — the shared
            /// carrier any daemon loop emits through `observability::ops`. OTLP-only;
            /// see [`ops`] for the fixed name vocabulary and label policy.
            MetricPoints = "metric.points" => $crate::telemetry::MetricPointsRecord,
                gate: 10, otlp: OpsPoints, native: false;

            /// The work finder's ranked ready queue as of its last tick (Issue #8852,
            /// phase 2). Native-HTTPS only; see [`queue_snapshot`].
            QueueSnapshot = "queue.snapshot" => $crate::telemetry::QueueSnapshotRecord,
                gate: 11, otlp: NotExported, native: true;

            // APPEND NEW KINDS ABOVE THIS LINE (one row; `gate:` stays
            // NEW_KIND_SCHEMA_VERSION). Do not renumber or reorder existing rows —
            // `gate:` values are wire contract. This marker is load-bearing: the
            // `kind_registry` merge test appends synthetic rows here to prove two
            // independent branches still merge cleanly (#8921).
        }
    };
}

macro_rules! declare_kind_metadata {
    ($( $(#[$attr:meta])* $variant:ident = $kind:literal => $payload:ty,
        gate: $gate:expr, otlp: $class:ident, native: $native:literal; )+) => {
        /// Every registered record kind, in declaration order — the runtime
        /// reflection of [`telemetry_kind_table!`]. Generated from the same
        /// rows as the enum, so a kind cannot be in one and not the other.
        pub const TELEMETRY_KINDS: &[TelemetryKindMeta] = &[
            $(
                TelemetryKindMeta {
                    variant: stringify!($variant),
                    kind: $kind,
                    schema_version: $gate,
                    otlp: TelemetryKindOtlp::$class,
                    native_ingest: $native,
                },
            )+
        ];
    };
}

crate::telemetry_kind_table!(declare_kind_metadata);
