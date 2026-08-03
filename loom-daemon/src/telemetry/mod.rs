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

pub mod visibility;

/// Current telemetry wire-schema version. Bump on any breaking change to the
/// record shapes below so a Phase-2 backend ingesting a mixed-version fleet can
/// gate on a simple numeric compare (no semver parsing). See the module docs.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

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

/// The versioned wrapper every telemetry record is emitted inside. Carries the
/// [`schema_version`](Self::schema_version) a mixed-version fleet's backend gates
/// on, plus host-identifying context shared by every record kind, and the tagged
/// [`record`](Self::record) payload itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TelemetryEnvelope {
    /// Wire-schema version — [`CURRENT_SCHEMA_VERSION`] for a freshly constructed
    /// envelope. A `#[serde(default)]` is intentionally NOT applied: an envelope
    /// with no `schema_version` on the wire is a bug the backend should see,
    /// not silently coerce to version 0.
    pub schema_version: u32,
    /// When the emitting daemon produced this envelope.
    pub emitted_at: DateTime<Utc>,
    /// Stable identifier for the emitting host (e.g. hostname or a configured
    /// fleet host id). Populated by the exporter (#4705); opaque to the schema.
    pub host_id: String,
    /// The record payload — internally tagged on a `kind` discriminant so it
    /// serializes to a single flat object (see [`TelemetryRecord`]).
    pub record: TelemetryRecord,
}

impl TelemetryEnvelope {
    /// Wrap `record` in an envelope stamped with [`CURRENT_SCHEMA_VERSION`] and
    /// the current time. `host_id` identifies the emitting host.
    #[must_use]
    pub fn new(host_id: impl Into<String>, record: TelemetryRecord) -> Self {
        TelemetryEnvelope {
            schema_version: CURRENT_SCHEMA_VERSION,
            emitted_at: Utc::now(),
            host_id: host_id.into(),
            record,
        }
    }
}

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
}

/// One account's slice of a `tokens.snapshot` — the per-account usage /
/// limit-window state matching what `loom-daemon tokens check --ranking` knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenAccountState {
    /// Token account name (the `<account>.token` basename in `.loom/tokens/`).
    pub account: String,
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
/// Host-level: it references no repository, so it carries no visibility tag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenSnapshotRecord {
    /// When the snapshot was taken.
    pub captured_at: DateTime<Utc>,
    /// Per-account state for every account in the pool.
    pub accounts: Vec<TokenAccountState>,
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Test fixtures — one freshly-constructed record per kind.
    // ------------------------------------------------------------------

    fn ts() -> DateTime<Utc> {
        // A fixed instant keeps round-trip equality deterministic.
        DateTime::parse_from_rfc3339("2026-07-30T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn sweep_started() -> TelemetryRecord {
        TelemetryRecord::SweepStarted(SweepStartedRecord {
            repo: "rjwalters/loom".to_string(),
            visibility: RepoVisibility::Public,
            issue: 4703,
            sweep_id: "sweep-issue-4703-0".to_string(),
            started_at: ts(),
            model: Some("opus".to_string()),
            effort: Some("high".to_string()),
        })
    }

    fn sweep_phase() -> TelemetryRecord {
        TelemetryRecord::SweepPhase(SweepPhaseRecord {
            repo: "rjwalters/loom".to_string(),
            visibility: RepoVisibility::Private,
            issue: 4703,
            sweep_id: "sweep-issue-4703-0".to_string(),
            phase: "builder".to_string(),
            entered_at: ts(),
        })
    }

    fn sweep_completed() -> TelemetryRecord {
        TelemetryRecord::SweepCompleted(SweepCompletedRecord {
            repo: "rjwalters/loom".to_string(),
            visibility: RepoVisibility::Public,
            issue: 4703,
            sweep_id: "sweep-issue-4703-0".to_string(),
            completed_at: ts(),
            result: SweepResult::Success,
        })
    }

    fn sweep_outcome() -> TelemetryRecord {
        let mut config = std::collections::BTreeMap::new();
        config.insert("runtime".to_string(), "claude".to_string());
        TelemetryRecord::SweepOutcome(SweepOutcomeRecord {
            repo: "rjwalters/loom".to_string(),
            visibility: RepoVisibility::Public,
            issue: 4703,
            sweep_id: "sweep-issue-4703-0".to_string(),
            model: Some("opus".to_string()),
            effort: Some("high".to_string()),
            config,
            phase_durations: vec![
                PhaseDuration {
                    phase: "curator".to_string(),
                    duration_sec: 12,
                },
                PhaseDuration {
                    phase: "builder".to_string(),
                    duration_sec: 340,
                },
            ],
            total_duration_sec: 512,
            result: SweepResult::Success,
            pr_number: Some(4710),
        })
    }

    fn tokens_snapshot() -> TelemetryRecord {
        TelemetryRecord::TokensSnapshot(TokenSnapshotRecord {
            captured_at: ts(),
            accounts: vec![
                TokenAccountState {
                    account: "agent-1".to_string(),
                    rank: Some(0),
                    usage_fraction: Some(0.42),
                    limit_window_reset_at: Some(ts()),
                    exhausted: false,
                },
                TokenAccountState {
                    account: "agent-2".to_string(),
                    rank: None,
                    usage_fraction: None,
                    limit_window_reset_at: None,
                    exhausted: true,
                },
            ],
        })
    }

    fn host_health() -> TelemetryRecord {
        TelemetryRecord::HostHealth(HostHealthRecord {
            captured_at: ts(),
            daemon_version: "0.16.0".to_string(),
            build_commit: "8c16fb5b".to_string(),
            built_at: Some(ts()),
            uptime_sec: 86_400,
            logical_cpus: 28,
            cpu_idle_fraction: Some(0.83),
            load_per_core: Some(0.51),
            worktree_root_free_gb: Some(200),
            active_sweep_ids: vec!["sweep-issue-4703-0".to_string()],
            dispatch_halted: false,
            halt_reason: None,
            managed_repos: vec![
                ManagedRepoEntry {
                    slug: "rjwalters/loom".to_string(),
                    visibility: RepoVisibility::Public,
                },
                ManagedRepoEntry {
                    slug: "2AMLogic/gf180-pll".to_string(),
                    visibility: RepoVisibility::Private,
                },
            ],
        })
    }

    fn every_record() -> Vec<TelemetryRecord> {
        vec![
            sweep_started(),
            sweep_phase(),
            sweep_completed(),
            sweep_outcome(),
            tokens_snapshot(),
            host_health(),
        ]
    }

    // ------------------------------------------------------------------
    // Serde round-trip — every record kind, wrapped in the envelope.
    // ------------------------------------------------------------------

    #[test]
    fn envelope_round_trips_for_every_record_kind() {
        for record in every_record() {
            let envelope = TelemetryEnvelope::new("host-abc", record.clone());
            let json = serde_json::to_string(&envelope).unwrap();
            let decoded: TelemetryEnvelope = serde_json::from_str(&json).unwrap();
            assert_eq!(envelope, decoded, "round-trip mismatch for {record:?}");
        }
    }

    #[test]
    fn bare_record_round_trips_for_every_record_kind() {
        // The record enum is consumed directly by #4704/#4705 too, not only
        // inside the envelope, so it must round-trip on its own.
        for record in every_record() {
            let json = serde_json::to_string(&record).unwrap();
            let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
            assert_eq!(record, decoded);
        }
    }

    // ------------------------------------------------------------------
    // schema_version presence.
    // ------------------------------------------------------------------

    #[test]
    fn fresh_envelope_carries_current_schema_version() {
        for record in every_record() {
            let envelope = TelemetryEnvelope::new("host-abc", record);
            assert_eq!(envelope.schema_version, CURRENT_SCHEMA_VERSION);
            let value: serde_json::Value = serde_json::to_value(&envelope).unwrap();
            assert_eq!(
                value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_u64),
                Some(u64::from(CURRENT_SCHEMA_VERSION)),
                "serialized envelope must carry schema_version"
            );
        }
    }

    // ------------------------------------------------------------------
    // The `kind` discriminant is on the wire for pattern-matching.
    // ------------------------------------------------------------------

    #[test]
    fn record_serializes_with_kind_tag() {
        let value = serde_json::to_value(sweep_outcome()).unwrap();
        assert_eq!(value.get("kind").and_then(serde_json::Value::as_str), Some("sweep.outcome"));
        // The tag flattens into the same object as the payload fields.
        assert_eq!(value.get("issue").and_then(serde_json::Value::as_u64), Some(4703));
    }

    // ------------------------------------------------------------------
    // RepoVisibility — private-safe decoding (the load-bearing property).
    // ------------------------------------------------------------------

    #[test]
    fn visibility_default_is_private() {
        assert_eq!(RepoVisibility::default(), RepoVisibility::Private);
    }

    #[test]
    fn visibility_public_string_decodes_public() {
        assert_eq!(
            serde_json::from_str::<RepoVisibility>("\"public\"").unwrap(),
            RepoVisibility::Public
        );
        // Case-insensitive.
        assert_eq!(
            serde_json::from_str::<RepoVisibility>("\"PUBLIC\"").unwrap(),
            RepoVisibility::Public
        );
    }

    #[test]
    fn visibility_unknown_or_malformed_decodes_private_never_public() {
        // An explicit "private".
        assert_eq!(
            serde_json::from_str::<RepoVisibility>("\"private\"").unwrap(),
            RepoVisibility::Private
        );
        // Every one of these MUST decode (not error) and MUST be Private.
        for raw in [
            "\"internal\"",       // unknown label
            "\"Public \"",        // trailing space — not exactly "public"
            "\"\"",               // empty string
            "null",               // null
            "true",               // wrong-typed scalar
            "0",                  // number
            "1",                  // number (must never map to public)
            "[\"public\"]",       // array
            "{\"v\":\"public\"}", // object
        ] {
            let decoded: RepoVisibility = serde_json::from_str(raw).unwrap_or_else(|e| {
                panic!("visibility {raw:?} should decode (defaulting to Private), got error: {e}")
            });
            assert_eq!(
                decoded,
                RepoVisibility::Private,
                "visibility {raw:?} must decode to Private, never Public"
            );
        }
    }

    #[test]
    fn record_with_missing_visibility_defaults_to_private() {
        // A record object with NO `visibility` key at all (an older-schema or
        // partial record) must decode with visibility = Private.
        let json = r#"{
            "kind": "sweep.started",
            "repo": "rjwalters/loom",
            "issue": 4703,
            "sweep_id": "sweep-issue-4703-0",
            "started_at": "2026-07-30T12:00:00Z"
        }"#;
        let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
        match decoded {
            TelemetryRecord::SweepStarted(r) => {
                assert_eq!(r.visibility, RepoVisibility::Private);
            }
            other => panic!("expected SweepStarted, got {other:?}"),
        }
    }

    #[test]
    fn record_with_unknown_visibility_defaults_to_private() {
        // A record whose `visibility` is a value this daemon doesn't recognize
        // must still decode, as Private (never leaking into the public view).
        let json = r#"{
            "kind": "sweep.completed",
            "repo": "rjwalters/loom",
            "visibility": "internal-only",
            "issue": 4703,
            "sweep_id": "sweep-issue-4703-0",
            "completed_at": "2026-07-30T12:00:00Z",
            "result": "success"
        }"#;
        let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
        match decoded {
            TelemetryRecord::SweepCompleted(r) => {
                assert_eq!(r.visibility, RepoVisibility::Private);
            }
            other => panic!("expected SweepCompleted, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // host.health build identity (#4956).
    // ------------------------------------------------------------------

    #[test]
    fn host_health_serializes_build_identity_alongside_version() {
        let value = serde_json::to_value(host_health()).unwrap();
        assert_eq!(
            value
                .get("daemon_version")
                .and_then(serde_json::Value::as_str),
            Some("0.16.0")
        );
        // The whole point of #4956: two builds sharing `daemon_version` are
        // told apart by the commit, so it must be on the wire.
        assert_eq!(
            value
                .get("build_commit")
                .and_then(serde_json::Value::as_str),
            Some("8c16fb5b")
        );
        assert_eq!(
            value.get("built_at").and_then(serde_json::Value::as_str),
            Some("2026-07-30T12:00:00Z")
        );
    }

    #[test]
    fn host_health_round_trips_build_identity() {
        let json = serde_json::to_string(&host_health()).unwrap();
        let decoded: TelemetryRecord = serde_json::from_str(&json).unwrap();
        match decoded {
            TelemetryRecord::HostHealth(r) => {
                assert_eq!(r.build_commit, "8c16fb5b");
                assert_eq!(r.built_at, Some(ts()));
            }
            other => panic!("expected HostHealth, got {other:?}"),
        }
    }

    #[test]
    fn host_health_omits_built_at_when_unknown() {
        // `build.rs` stamps the literal "unknown" when the build host had no
        // usable `date`; that must serialize as an ABSENT field, never as a
        // fabricated instant (the struct's "unknown != zero" contract).
        let record = TelemetryRecord::HostHealth(HostHealthRecord {
            captured_at: ts(),
            daemon_version: "0.17.0".to_string(),
            build_commit: "unknown".to_string(),
            built_at: None,
            uptime_sec: 1,
            logical_cpus: 4,
            cpu_idle_fraction: None,
            load_per_core: None,
            worktree_root_free_gb: None,
            active_sweep_ids: Vec::new(),
            dispatch_halted: false,
            halt_reason: None,
            managed_repos: Vec::new(),
        });
        let value = serde_json::to_value(&record).unwrap();
        assert!(
            value.get("built_at").is_none(),
            "an unknown build time must be absent, not a fabricated instant"
        );
        // The commit sentinel, unlike the instant, IS sent — "unknown" is a
        // meaningful answer for a tarball build, not a missing measurement.
        assert_eq!(
            value
                .get("build_commit")
                .and_then(serde_json::Value::as_str),
            Some("unknown")
        );
        // Still round-trips.
        let decoded: TelemetryRecord = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, record);
    }

    #[test]
    fn host_health_from_a_pre_4956_daemon_still_decodes() {
        // Backward compatibility: a record emitted by a daemon that predates
        // the build-identity fields must decode, not poison the batch.
        let json = r#"{
            "kind": "host.health",
            "captured_at": "2026-07-30T12:00:00Z",
            "daemon_version": "0.16.0",
            "uptime_sec": 86400,
            "logical_cpus": 28
        }"#;
        let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
        match decoded {
            TelemetryRecord::HostHealth(r) => {
                assert_eq!(r.daemon_version, "0.16.0");
                assert_eq!(r.build_commit, "");
                assert_eq!(r.built_at, None);
            }
            other => panic!("expected HostHealth, got {other:?}"),
        }
    }

    // ------------------------------------------------------------------
    // host.health managed_repos roster (#4976).
    // ------------------------------------------------------------------

    #[test]
    fn host_health_serializes_managed_repos_with_slug_and_visibility() {
        let value = serde_json::to_value(host_health()).unwrap();
        let repos = value
            .get("managed_repos")
            .and_then(serde_json::Value::as_array)
            .unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(
            repos[0].get("slug").and_then(serde_json::Value::as_str),
            Some("rjwalters/loom")
        );
        assert_eq!(
            repos[0]
                .get("visibility")
                .and_then(serde_json::Value::as_str),
            Some("public")
        );
        assert_eq!(
            repos[1].get("slug").and_then(serde_json::Value::as_str),
            Some("2AMLogic/gf180-pll")
        );
        assert_eq!(
            repos[1]
                .get("visibility")
                .and_then(serde_json::Value::as_str),
            Some("private")
        );
    }

    #[test]
    fn host_health_omits_managed_repos_when_empty() {
        // `skip_serializing_if = "Vec::is_empty"` — a host with no registered
        // workspaces sends no key at all, mirroring `active_sweep_ids`.
        let record = TelemetryRecord::HostHealth(HostHealthRecord {
            captured_at: ts(),
            daemon_version: "0.17.0".to_string(),
            build_commit: "unknown".to_string(),
            built_at: None,
            uptime_sec: 1,
            logical_cpus: 4,
            cpu_idle_fraction: None,
            load_per_core: None,
            worktree_root_free_gb: None,
            active_sweep_ids: Vec::new(),
            dispatch_halted: false,
            halt_reason: None,
            managed_repos: Vec::new(),
        });
        let value = serde_json::to_value(&record).unwrap();
        assert!(
            value.get("managed_repos").is_none(),
            "an empty roster must be absent from the wire, not an empty array"
        );
    }

    #[test]
    fn host_health_from_a_pre_4976_daemon_still_decodes() {
        // Backward compatibility: a record emitted by a daemon that predates
        // the managed-repo roster must decode with an empty roster, not fail
        // the whole envelope.
        let json = r#"{
            "kind": "host.health",
            "captured_at": "2026-07-30T12:00:00Z",
            "daemon_version": "0.16.0",
            "uptime_sec": 86400,
            "logical_cpus": 28
        }"#;
        let decoded: TelemetryRecord = serde_json::from_str(json).unwrap();
        match decoded {
            TelemetryRecord::HostHealth(r) => {
                assert!(r.managed_repos.is_empty());
            }
            other => panic!("expected HostHealth, got {other:?}"),
        }
    }

    #[test]
    fn managed_repo_entry_with_missing_visibility_defaults_to_private() {
        // A repo entry with no `visibility` tag at all must decode Private —
        // the same private-safe-default every other visibility field holds.
        let json = r#"{"slug": "owner/repo"}"#;
        let decoded: ManagedRepoEntry = serde_json::from_str(json).unwrap();
        assert_eq!(decoded.visibility, RepoVisibility::Private);
        assert_eq!(decoded.slug, "owner/repo");
    }
}
