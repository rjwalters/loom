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

/// `auto_update.tick` (#10414).
pub mod auto_update_tick;

/// `eta.estimate` / `eta.outcome` (#9289).
pub mod eta;

/// `eta.backtest.fold` / `eta.backtest.summary` (#10492).
pub mod eta_backtest;

/// `eta.fit` (#10391).
pub mod eta_fit;
/// `eta.fleet_refresh` (#10263).
pub mod eta_fleet_refresh;

/// `eta.snapshot` (#9329).
pub mod eta_snapshot;

/// `eta.stage_outcome` (#10929) — one stage an item left, from the ETA tracker.
pub mod eta_stage_attribution;
pub mod eta_stage_outcome;

/// `pass.summary` / `pass.verdict` (#10752) — what a pass over artifacts did.
pub mod pass;

/// `pick.decision` (#10212) — what a role / the work finder looked at per tick.
pub mod pick_decision;

/// `pr.resolved` (#10519) — a PR's merge or close instant, from the ETA pass.
pub mod pr_resolved;

/// `session.output` (#9764) — the live, redacted agent-output feed.
pub mod session_output;

/// `token_ranking.refresh` (#10744) — one token-ranking refresh round.
pub mod token_ranking_refresh;

/// The export-coverage pair `(exporters, exported_kinds)` for `host.health`
/// (Issue #10196), derived from the per-exporter status map
/// (`crate::observability::global_export_statuses()`, keyed by
/// `ExporterKind::name`) as of `now`.
///
/// Only exporters that **actually started** count: an entry whose status
/// classifies as `Misconfigured` (policy rejection, unreadable ingest key,
/// `otlp` on a build without the feature) or `Disabled` (no `started_at`)
/// never ran, so advertising it — or the kinds only it carries — would let a
/// replay reader read that sink's silence as "nothing happened". A started
/// exporter that is `Failing`/`NeverExported` still counts: its records are
/// queued durably and retried. When nothing started both lists are empty,
/// which the contract reads as **unknown**, never "exports nothing".
#[must_use]
pub fn export_coverage(
    statuses: &std::collections::BTreeMap<String, crate::types::ObservabilityExportStatus>,
    now: chrono::DateTime<chrono::Utc>,
) -> (Vec<String>, Vec<String>) {
    use crate::types::ObservabilityExportState as State;
    let exporters: Vec<String> = statuses
        .iter()
        .filter(|(_, status)| {
            !matches!(status.classify(now), State::Misconfigured | State::Disabled)
        })
        .map(|(name, _)| name.clone())
        .collect();
    let kinds = exported_kinds_for(&exporters);
    (exporters, kinds)
}

/// The sorted wire `kind` tags the given exporters (by
/// `ExporterKind::name`: `"https"`, `"otlp"`) carry, derived from the registry
/// rows (Issue #10196). `https` carries `native_ingest` kinds; `otlp` carries
/// every kind whose class is not [`TelemetryKindOtlp::NotExported`]. Unknown
/// exporter names contribute nothing.
#[must_use]
pub fn exported_kinds_for(exporters: &[String]) -> Vec<String> {
    let https = exporters.iter().any(|e| e == "https");
    let otlp = exporters.iter().any(|e| e == "otlp");
    let mut kinds: Vec<String> = TELEMETRY_KINDS
        .iter()
        .filter(|k| {
            (https && k.native_ingest) || (otlp && k.otlp != TelemetryKindOtlp::NotExported)
        })
        .map(|k| k.kind.to_string())
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

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

            /// One ETA estimate with its `eta-explanation/v1` record (Issue #9289).
            /// OTLP-only: explanations live in SigNoz. See [`eta`].
            EtaEstimate = "eta.estimate" => $crate::telemetry::kinds::eta::EtaEstimateRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One ETA estimate's scored outcome (Issue #9289). OTLP-only.
            EtaOutcome = "eta.outcome" => $crate::telemetry::kinds::eta::EtaOutcomeRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// This host's live per-issue ETA estimate set (Issue #9329).
            /// Native-HTTPS only — the dashboard's `eta:<hostId>` state key,
            /// the mirror of [`QueueSnapshot`](Self::QueueSnapshot). SigNoz
            /// gets every estimate as [`EtaEstimate`](Self::EtaEstimate)
            /// instead. See [`eta_snapshot`].
            EtaSnapshot = "eta.snapshot" => $crate::telemetry::kinds::eta_snapshot::EtaSnapshotRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: NotExported, native: true;

            /// One live agent-output event during an in-flight run (Issue
            /// #9764) — readable, producer-redacted, issue-scoped. The second
            /// kind whose OTLP body is text the daemon did not author, so it
            /// pins its own gate for the same reason `ci.job.log` pinned `9`:
            /// a backend must be able to refuse free-text session content at
            /// the version level without also refusing every other post-#8921
            /// kind sharing `NEW_KIND_SCHEMA_VERSION`. OTLP-only by design —
            /// the native HTTPS ingest backend never receives session text.
            /// See [`session_output`].
            SessionOutput = "session.output" => $crate::telemetry::kinds::session_output::SessionOutputRecord,
                gate: 13, otlp: Logs, native: false;

            /// One repo's outcome in one cycle of the daemon's fleet snapshot
            /// refresh (Issue #10263). OTLP-only, like the other `eta.*` log
            /// kinds. See [`eta_fleet_refresh`].
            EtaFleetRefresh = "eta.fleet_refresh" => $crate::telemetry::kinds::eta_fleet_refresh::EtaFleetRefreshRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One self-update loop decision (Issue #10414): decision, installed
            /// and target versions, defer reason, drain state. OTLP-only. See
            /// [`auto_update_tick`].
            AutoUpdateTick = "auto_update.tick" => $crate::telemetry::kinds::auto_update_tick::AutoUpdateTickRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One daily-fit check, whether it fitted or skipped (Issue
            /// #10391). OTLP-only, like the other `eta.*` log kinds. See
            /// [`eta_fit`].
            EtaFit = "eta.fit" => $crate::telemetry::kinds::eta_fit::EtaFitRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One role tick's (or work-finder tick's) pick decision (Issue #10212):
            /// the ranked candidates it considered, what it acted on, and a
            /// closed-set reason per skip. OTLP-only. See [`pick_decision`].
            PickDecision = "pick.decision" => $crate::telemetry::kinds::pick_decision::PickDecisionRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// A PR the ETA pass saw leave the review listings, with its merge
            /// or close instant (Issue #10519). Built from rows the pass
            /// already journals, so no new forge read. OTLP-only. See
            /// [`pr_resolved`].
            PrResolved = "pr.resolved" => $crate::telemetry::kinds::pr_resolved::PrResolvedRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One stage an item left, with its entry and exit instants and the
            /// estimates open for the item then (Issue #10929). Built from rows
            /// the ETA tracker already journals, so no new forge read.
            /// OTLP-only. See [`eta_stage_outcome`].
            EtaStageOutcome = "eta.stage_outcome" => $crate::telemetry::kinds::eta_stage_outcome::EtaStageOutcomeRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One (heuristic, stage) row of the nightly stage-error rollup
            /// (Issue #10957). OTLP-only. See [`eta_stage_attribution`].
            EtaStageAttribution = "eta.stage_attribution" => $crate::telemetry::kinds::eta_stage_attribution::EtaStageAttributionRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One heuristic's nightly walk-forward fold for one UTC day (Issue
            /// #10492). OTLP-only. See [`eta_backtest`].
            EtaBacktestFold = "eta.backtest.fold" => $crate::telemetry::kinds::eta_backtest::EtaBacktestFoldRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One challenger's rolling backtest standing against `current`
            /// (Issue #10492). OTLP-only. See [`eta_backtest`].
            EtaBacktestSummary = "eta.backtest.summary" => $crate::telemetry::kinds::eta_backtest::EtaBacktestSummaryRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One pass over a workspace's artifacts (Issue #10752): mechanism,
            /// mode, outcome, counts by verdict and skip reason, write cap,
            /// duration and GitHub calls. OTLP-only. See [`pass`].
            PassSummary = "pass.summary" => $crate::telemetry::kinds::pass::PassSummaryRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One artifact's verdict in a pass (Issue #10752): repo#n, verdict,
            /// reason, blockers with their states, labels changed. OTLP-only.
            /// See [`pass`].
            PassVerdict = "pass.verdict" => $crate::telemetry::kinds::pass::PassVerdictRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

            /// One workspace's token-ranking refresh round (Issue #10744):
            /// accounts probed, per-account outcome and credential kind, and
            /// how many probes used a metered API key. OTLP-only. See
            /// [`token_ranking_refresh`].
            TokenRankingRefresh = "token_ranking.refresh" => $crate::telemetry::kinds::token_ranking_refresh::TokenRankingRefreshRecord,
                gate: $crate::telemetry::NEW_KIND_SCHEMA_VERSION, otlp: Logs, native: false;

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
