//! Pluggable telemetry exporter + `observability` config block (Epic #4702,
//! Phase 1 — issue #4705).
//!
//! Wires three additive pieces together, on top of the versioned schema
//! [`crate::telemetry`] already defines (issue #4703):
//!
//! - [`collector`] — a pure [`crate::event_bus::EventBus`] subscriber (adds
//!   no new emit call sites anywhere else in the daemon, mirroring
//!   [`crate::safehouse`]'s design) that maps sweep-lifecycle events plus
//!   periodic host/token samples into [`crate::telemetry::TelemetryEnvelope`]s.
//! - [`queue`] — a bounded, disk-backed offline queue those envelopes land in,
//!   so a sink outage (or a sleeping/idle-shut-down host, #4467/#4697) never
//!   silently loses data up to `queueCapacity`.
//! - [`exporter`] + [`sender`] — the [`exporter::Exporter`] trait, its
//!   [`exporter::HttpsExporter`] JSON-over-HTTPS push implementation, and the
//!   jittered-retry drain loop that pulls batches off the queue and pushes
//!   them to the sink.
//! - `otlp` (Epic #4702, Phase 4 — issue #4858, behind the `otlp` Cargo
//!   feature) — a second [`exporter::Exporter`] implementation for operators
//!   with an existing OpenTelemetry stack, selected via
//!   `observability.exporter = "otlp"` or an `observability.exporters` entry
//!   ([`resolve_exporters`], which since #8756 fans one envelope out to N
//!   configured sinks at once). Off by
//!   default; [`exporter::HttpsExporter`] stays the default sink.
//! - [`backfill`] (Issue #5084) — the local `sweep-outcome-telemetry.jsonl`
//!   journal ([`crate::sweep_outcomes`]) is written unconditionally at every
//!   sweep's terminal transition, independent of whether [`collector`]'s
//!   live event-bus pipeline observed it. `backfill` treats that journal as
//!   the export queue-of-record: it periodically pushes any record the queue
//!   has not yet been offered onto [`queue::DurableQueue`] alongside a
//!   synthesized `sweep.completed`, so a sweep adopted across a daemon
//!   restart (whose dispatch this process never saw) still exports under its
//!   real `sweep_id` instead of being silently under-counted.
//! - [`session_summary`] (Issue #8757, G3 of #8714) — the process-global
//!   sink the transcript-ingest thread pushes `session.summary` records
//!   onto, sharing this same [`queue::DurableQueue`] so the new record kind
//!   rides whichever exporter is configured with no egress code of its own.
//! - [`session_analysis`] (Issue #8760, G3 part 2 of #8714) — the same
//!   process-global-sink pattern as `session_summary`, one slice later: the
//!   transcript-ingest thread pushes the derived `session.analysis` rollup
//!   (retry loops, longest tool call, USD cost, anomaly flags) alongside
//!   each `session.summary` it emits.
//! - [`daemon_event`] (Issue #8760, G4 of #8714) — a second, narrower
//!   [`crate::event_bus::EventBus`] subscriber alongside [`collector`],
//!   covering the four named topics that carried no telemetry record kind
//!   at all: `daemon.drain.*`, `daemon.capacity.advisory`,
//!   `daemon.preflight.advisory`, `epic.issue.*`.
//!
//! # Off by default (FLAGS-OFF posture)
//!
//! Mirrors every other `autonomous.*`-style daemon subsystem
//! (`config_resolver.rs`'s documented precedence): **env > config > default**,
//! default `enabled = false`. [`spawn_task`] returns `None` — no
//! subscription, no queue file, no HTTP client construction, zero syscalls —
//! whenever the resolved config is disabled or under-configured (no endpoint,
//! no readable ingest key file). This is the same "disabled means truly
//! inert" contract [`crate::safehouse::spawn_sink`] and
//! [`crate::idle_exit::spawn_task`]'s callers already rely on.
//!
//! # Read-only invariant
//!
//! This module only ever originates outbound HTTP POSTs
//! ([`exporter::HttpsExporter::emit_batch`]); nothing here parses a response
//! body for anything but a batch-accepted/rejected status **plus the
//! self-diagnostic host-identity echo below**, and no daemon *behavior* is
//! ever driven by data received over this channel.
//!
//! The one thing read out of a response body (Issue #4830) is the `host_id`
//! the backend echoes for the key it authenticated. It is compared against
//! this daemon's own [`crate::sweep_registry::host_identity`] and, on a
//! mismatch, published to [`HostIdStatus`] + logged once — it never selects an
//! endpoint, never changes what is exported, and is never written into a
//! record. A hostile/broken sink can therefore, at worst, make this daemon
//! report a mismatch that is not real; it cannot steer the daemon.
//!
//! That check is **specific to the native HTTPS ingest protocol**, which is
//! what defines the echo: the `otlp` sink below has no equivalent (OTLP/HTTP
//! success responses carry only `partial_success`), so under
//! `observability.exporter = "otlp"` no [`HostIdStatus`] is registered and
//! [`global_host_id_mismatch`] reads `None` — see the OTLP arm of
//! [`spawn_task`] for the full rationale.
//!
//! # Ingest key handling
//!
//! The ingest key is read once at startup from `ingestKeyFile` (never
//! accepted inline in config) and held only in memory as an
//! [`exporter::HttpsExporter`] field, sent solely as an `Authorization:
//! Bearer` HTTP header value. Every log line and [`exporter::ExportError`]
//! variant in this module tree names the *file path*, never the key
//! contents.
//!
//! `ingestKeyFile` defaults to `$HOME/.loom/observability/ingest.key`
//! ([`resolve_ingest_key_file`]) when no tier sets it explicitly, so the
//! path is always resolved against *this* host's own home directory rather
//! than requiring — or risking — a value copied verbatim from a different
//! host (#5336).

pub mod backfill;
pub mod collector;
pub mod daemon_event;
pub mod endpoint_policy;
pub mod exporter;
pub mod lifecycle;
#[cfg(feature = "otlp")]
pub mod otlp;
pub mod outcome;
pub mod overhead;
pub mod queue;
pub mod sender;
pub mod session_analysis;
pub mod session_summary;
pub mod shutdown;
pub mod tracing;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::event_bus::EventBus;
use crate::workspace_pool::WorkspacePool;

use endpoint_policy::reserved_placeholder_host;
use exporter::HttpsExporter;
use queue::DurableQueue;

/// `observability.enabled` env override.
pub const ENABLED_ENV: &str = "LOOM_OBSERVABILITY_ENABLED";
/// `observability.endpoint` env override.
pub const ENDPOINT_ENV: &str = "LOOM_OBSERVABILITY_ENDPOINT";
/// `observability.ingestKeyFile` env override.
pub const INGEST_KEY_FILE_ENV: &str = "LOOM_OBSERVABILITY_INGEST_KEY_FILE";
/// `observability.batchSize` env override.
pub const BATCH_SIZE_ENV: &str = "LOOM_OBSERVABILITY_BATCH_SIZE";
/// `observability.flushIntervalSecs` env override.
pub const FLUSH_INTERVAL_SECS_ENV: &str = "LOOM_OBSERVABILITY_FLUSH_INTERVAL_SECS";
/// `observability.queueCapacity` env override.
pub const QUEUE_CAPACITY_ENV: &str = "LOOM_OBSERVABILITY_QUEUE_CAPACITY";
/// `observability.exporter` env override (#4858) — `"https"` or `"otlp"`.
pub const EXPORTER_ENV: &str = "LOOM_OBSERVABILITY_EXPORTER";

/// Default batch size (envelopes per HTTP POST) when unset at every tier.
pub const DEFAULT_BATCH_SIZE: usize = 50;
/// Default flush interval when unset at every tier.
pub const DEFAULT_FLUSH_INTERVAL_SECS: u64 = 30;
/// Default queue capacity when unset at every tier.
pub const DEFAULT_QUEUE_CAPACITY: usize = 2000;
/// How often the collector samples `tokens.snapshot` / `host.health` — not
/// operator-tunable in this issue's config surface (only the six documented
/// `observability.*` keys are), so this is a fixed, generous cadence.
pub const SNAPSHOT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// The `.loom/config.json` `observability` block, read but not yet resolved
/// against env/defaults (see the `resolve_*` functions below, mirroring
/// [`crate::idle_exit::IdleExitConfig`]'s split).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObservabilityConfig {
    pub enabled: Option<bool>,
    pub endpoint: Option<String>,
    pub ingest_key_file: Option<String>,
    pub batch_size: Option<usize>,
    pub flush_interval_secs: Option<u64>,
    pub queue_capacity: Option<usize>,
    /// Raw `observability.exporter` string, resolved by [`resolve_exporters`]
    /// (#4858; since #8756 the singular key is one input to the exporter
    /// list) — not parsed to [`ExporterKind`] at read time so an unknown
    /// value can be logged (and degraded to the default) at the single call
    /// site that already owns exporter selection.
    pub exporter: Option<String>,
    /// Raw `observability.exporters` entries (Issue #8756) — each either a
    /// bare kind string (`"https"`) or an object (`{"kind": "otlp",
    /// "endpoint": "…"}`) carrying a per-exporter endpoint override. Kinds
    /// are kept as raw strings for the same reason as [`Self::exporter`]:
    /// [`resolve_exporters`] owns the warn-and-skip of an unknown kind.
    pub exporters: Option<Vec<RawExporterEntry>>,
}

/// One raw `observability.exporters` entry as parsed from config (Issue
/// #8756): the kind string kept verbatim (validated at resolve time so an
/// unknown kind is logged and skipped, mirroring the singular key's degrade
/// posture) plus an optional per-exporter endpoint override.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawExporterEntry {
    pub kind: String,
    /// Per-exporter endpoint; `None` ⇒ this entry uses the shared
    /// `observability.endpoint`.
    pub endpoint: Option<String>,
}

/// One resolved entry of the exporter list (Issue #8756): kind classified,
/// per-entry endpoint override carried through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExporterEntry {
    pub kind: ExporterKind,
    /// Per-exporter endpoint override; `None` ⇒ use the shared
    /// `observability.endpoint`.
    pub endpoint: Option<String>,
}

/// Which [`exporter::Exporter`] implementation [`spawn_task`] selects.
/// Resolved by [`resolve_exporters`] — **env > config > default**
/// ([`ExporterKind::Https`]), matching every other `observability.*` knob's
/// precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExporterKind {
    /// Native JSON-over-HTTPS push ([`exporter::HttpsExporter`]) — the
    /// default, for backward compatibility with every deployment predating
    /// this issue.
    Https,
    /// OTLP/HTTP+JSON push (`otlp::OtlpExporter`, only compiled in behind the
    /// `otlp` Cargo feature) — an escape hatch for an existing OpenTelemetry
    /// stack.
    Otlp,
}

impl ExporterKind {
    /// The exporter's config/status/queue-file name (Issue #8756): the key
    /// `observability.exporters` entries are deduped by, the
    /// `observability-queue.<name>.jsonl` suffix, and the
    /// `observability_exports` status-map key.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ExporterKind::Https => "https",
            ExporterKind::Otlp => "otlp",
        }
    }

    /// Case-insensitive classification; `None` for an unrecognized value (the
    /// caller logs and skips/degrades, never a hard error).
    #[must_use]
    pub fn parse_lenient(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "https" => Some(ExporterKind::Https),
            "otlp" => Some(ExporterKind::Otlp),
            _ => None,
        }
    }
}

/// Read the `observability` block from `root`'s resolved config
/// (`config_resolver::resolve_effective_config`), same pattern as
/// [`crate::idle_exit::read_config`].
#[must_use]
pub fn read_config(root: &Path) -> ObservabilityConfig {
    let config = crate::config_resolver::resolve_effective_config(root);
    let Some(block) = crate::config_resolver::get_path(&config, "observability") else {
        return ObservabilityConfig::default();
    };
    ObservabilityConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        endpoint: block
            .get("endpoint")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        ingest_key_file: block
            .get("ingestKeyFile")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        batch_size: block
            .get("batchSize")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v > 0),
        flush_interval_secs: block
            .get("flushIntervalSecs")
            .and_then(serde_json::Value::as_u64)
            .filter(|v| *v > 0),
        queue_capacity: block
            .get("queueCapacity")
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .filter(|v| *v > 0),
        exporter: block
            .get("exporter")
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        exporters: block.get("exporters").and_then(parse_raw_exporters),
    }
}

/// Parse the `observability.exporters` array (Issue #8756). Each element is
/// either a bare kind string (`"https"`) or an object `{"kind": "otlp",
/// "endpoint": "…"}`; anything else (or an object without a string `kind`) is
/// dropped here — malformed *shapes* cannot be named in a warn line, while a
/// well-formed-but-unknown kind string survives to [`resolve_exporters`],
/// which logs it.
fn parse_raw_exporters(value: &serde_json::Value) -> Option<Vec<RawExporterEntry>> {
    let entries = value.as_array()?;
    let parsed: Vec<RawExporterEntry> = entries
        .iter()
        .filter_map(|entry| match entry {
            serde_json::Value::String(kind) => Some(RawExporterEntry {
                kind: kind.clone(),
                endpoint: None,
            }),
            serde_json::Value::Object(fields) => {
                let kind = fields.get("kind")?.as_str()?.to_string();
                let endpoint = fields
                    .get("endpoint")
                    .and_then(serde_json::Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                Some(RawExporterEntry { kind, endpoint })
            }
            _ => None,
        })
        .collect();
    Some(parsed)
}

fn env_bool(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| {
        matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
    })
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// **env > config > default** (`false`).
#[must_use]
pub fn resolve_enabled(config: &ObservabilityConfig) -> bool {
    env_bool(ENABLED_ENV).or(config.enabled).unwrap_or(false)
}

/// **env > config**, no built-in default — a missing endpoint means "not
/// configured", handled by [`spawn_task`] as a degrade-to-disabled case.
///
/// Deliberately a *pure precedence* resolver: it answers "which tier's value
/// wins", never "is that value fit to export to". The reserved-placeholder
/// refusal lives at the point of use in [`spawn_task`]
/// ([`reserved_placeholder_host`], Issue #7815) so that this function stays
/// usable for reporting the configured value (`status`/`health`) even when
/// that value is one export must refuse.
#[must_use]
pub fn resolve_endpoint(config: &ObservabilityConfig) -> Option<String> {
    env_nonempty(ENDPOINT_ENV).or_else(|| config.endpoint.clone())
}

/// **env > config > default** (`$HOME/.loom/observability/ingest.key`).
///
/// The default keeps resolution host-relative even when no tier sets
/// `ingestKeyFile` explicitly, so no host ever *needs* a value copied
/// verbatim from a different host's `$HOME` in its config (#5336: a macOS
/// `ingestKeyFile` landed in the shared, committed `.loom/config.json` and
/// was inherited unreadable by a Linux worker via a plain `git pull` — the
/// copy path, not the value, was the defect). Provisioning a host now needs
/// only to place its key at this conventional path (or set an explicit
/// override via `.loom-local/local.json` / `$LOOM_OBSERVABILITY_INGEST_KEY_FILE`
/// for a non-default location, e.g. a system path like
/// `/etc/loom/observability-ingest.key`).
#[must_use]
pub fn resolve_ingest_key_file(config: &ObservabilityConfig) -> Option<String> {
    env_nonempty(INGEST_KEY_FILE_ENV)
        .or_else(|| config.ingest_key_file.clone())
        .or_else(default_ingest_key_file)
}

/// Join `home` with the conventional relative ingest-key-file location. Pure
/// and unit-testable independent of the real `$HOME` (see
/// [`default_ingest_key_file`] for why the `$HOME` lookup itself is not
/// unit-tested in-process).
fn ingest_key_file_under(home: &Path) -> String {
    home.join(".loom")
        .join("observability")
        .join("ingest.key")
        .to_string_lossy()
        .to_string()
}

/// This crate's single `src/` test binary links every `#[test]` together, and
/// several modules (`tokens_pool::paths::shared_tokens_dir`, `fleet::drain`,
/// `fleet::path_bootstrap`) already `set_var`/`remove_var("HOME")` for their
/// own isolation — a real ambient `$HOME` must never leak into a *different*
/// module's resolved default mid-test-run. Refusing the default under
/// `cfg(test)` closes that race structurally, mirroring
/// `shared_tokens_dir`'s `#4657` fix exactly: production behavior reads the
/// real `$HOME`, every in-process test observes `None` from this function
/// and exercises [`ingest_key_file_under`] directly instead.
#[cfg(not(test))]
fn default_ingest_key_file() -> Option<String> {
    dirs::home_dir().map(|h| ingest_key_file_under(&h))
}

#[cfg(test)]
fn default_ingest_key_file() -> Option<String> {
    None
}

/// **env > config > default** ([`DEFAULT_BATCH_SIZE`]).
#[must_use]
pub fn resolve_batch_size(config: &ObservabilityConfig) -> usize {
    std::env::var(BATCH_SIZE_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &usize| *v > 0)
        .or(config.batch_size)
        .unwrap_or(DEFAULT_BATCH_SIZE)
}

/// **env > config > default** ([`DEFAULT_FLUSH_INTERVAL_SECS`]).
#[must_use]
pub fn resolve_flush_interval_secs(config: &ObservabilityConfig) -> u64 {
    std::env::var(FLUSH_INTERVAL_SECS_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &u64| *v > 0)
        .or(config.flush_interval_secs)
        .unwrap_or(DEFAULT_FLUSH_INTERVAL_SECS)
}

/// **env > config > default** ([`DEFAULT_QUEUE_CAPACITY`]).
#[must_use]
pub fn resolve_queue_capacity(config: &ObservabilityConfig) -> usize {
    std::env::var(QUEUE_CAPACITY_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v: &usize| *v > 0)
        .or(config.queue_capacity)
        .unwrap_or(DEFAULT_QUEUE_CAPACITY)
}

// ============================================================================
// Host-identity mismatch status (Issue #4830)
// ============================================================================

/// Shared, thread-safe handle the exporter publishes a confirmed host-identity
/// mismatch to, and [`crate::ipc::build_daemon_status`] reads back — mirroring
/// [`crate::auto_update::AutoUpdateStatus`]'s process-global pattern rather than
/// threading an `Arc` through the whole IPC server.
///
/// **Write-once per process.** The first mismatch observed wins and is never
/// overwritten or cleared: the WARN is once-per-daemon-lifetime by AC, so the
/// published record has to be too, or `first_seen_at` would silently become
/// "last flush" and the two surfaces would disagree.
#[derive(Debug, Default)]
pub struct HostIdStatus {
    inner: std::sync::Mutex<Option<crate::types::ObservabilityHostIdMismatch>>,
}

// Allow expect_used: a poisoned status mutex means another thread panicked while
// holding it — unrecoverable, matching the crash-on-poison policy `auto_update`
// and `ipc` already use for their status mutexes.
#[allow(clippy::expect_used)]
impl HostIdStatus {
    /// Record the first observed mismatch. Returns `true` when this call is the
    /// one that recorded it (i.e. the caller owes exactly one WARN), `false`
    /// when a mismatch was already published this process.
    pub fn record_mismatch(&self, daemon_host_id: &str, ingest_host_id: &str) -> bool {
        let mut guard = self
            .inner
            .lock()
            .expect("observability host-id status mutex poisoned");
        if guard.is_some() {
            return false;
        }
        *guard = Some(crate::types::ObservabilityHostIdMismatch {
            daemon_host_id: daemon_host_id.to_string(),
            ingest_host_id: ingest_host_id.to_string(),
            first_seen_at: chrono::Utc::now(),
        });
        true
    }

    /// The published mismatch, or `None` when the ids have agreed on every
    /// acked batch so far (the common case).
    #[must_use]
    pub fn snapshot(&self) -> Option<crate::types::ObservabilityHostIdMismatch> {
        self.inner
            .lock()
            .expect("observability host-id status mutex poisoned")
            .clone()
    }
}

/// Process-global status handle, registered by [`spawn_task`] when — and only
/// when — the exporter actually starts. Unset (observability disabled, keyless,
/// or under-configured) reads as `None`, so a disabled exporter contributes
/// nothing to `status`/`health`.
static GLOBAL_HOST_ID_STATUS: std::sync::OnceLock<Arc<HostIdStatus>> = std::sync::OnceLock::new();

/// Register the exporter's status handle as the process-global. Idempotent:
/// only the first registration wins (there is exactly one exporter per process).
pub fn register_global_host_id_status(status: Arc<HostIdStatus>) {
    let _ = GLOBAL_HOST_ID_STATUS.set(status);
}

/// The process-global host-identity mismatch, or `None` when none has been
/// observed (or the exporter never started).
#[must_use]
pub fn global_host_id_mismatch() -> Option<crate::types::ObservabilityHostIdMismatch> {
    GLOBAL_HOST_ID_STATUS.get().and_then(|s| s.snapshot())
}

// ============================================================================
// Export liveness status (Issue #5083)
// ============================================================================

mod status;
pub use status::ExportStatus;

/// Process-global export-status handle, registered by [`spawn_task`] when the
/// exporter actually starts (a [`ExportStatus::started`] cell) **or** when it
/// fails to start due to a config problem (a [`ExportStatus::misconfigured`]
/// cell, Issue #5337). Unset only when observability is off by choice
/// (`enabled: false` / no block) — [`global_export_status`] then reports
/// [`crate::types::ObservabilityExportStatus::disabled`].
static GLOBAL_EXPORT_STATUS: std::sync::OnceLock<Arc<ExportStatus>> = std::sync::OnceLock::new();

/// Register the exporter's export-status handle as the process-global.
/// Idempotent: only the first registration wins (one exporter per process).
pub fn register_global_export_status(status: Arc<ExportStatus>) {
    let _ = GLOBAL_EXPORT_STATUS.set(status);
}

/// This process's export status — **always** an answer, never silence.
///
/// Unregistered ⇒ `disabled()` (`observability.enabled` is `false`, or the
/// block is absent — deliberately off), which is a materially different
/// report from the `None` an older daemon binary puts on the wire. An
/// `enabled: true` config that failed to resolve registers a `misconfigured()`
/// cell instead (Issue #5337), so the two are never confused. Any confirmed
/// #4830 host-identity mismatch is folded in here as `ingest_host_id`, so a
/// single field carries the whole state machine and
/// [`crate::types::ObservabilityExportStatus::classify`] needs no second input.
#[must_use]
pub fn global_export_status() -> crate::types::ObservabilityExportStatus {
    let Some(status) = GLOBAL_EXPORT_STATUS.get() else {
        return crate::types::ObservabilityExportStatus::disabled();
    };
    let mut snapshot = status.snapshot();
    if let Some(mismatch) = global_host_id_mismatch() {
        snapshot.ingest_host_id = Some(mismatch.ingest_host_id);
    }
    snapshot.state = snapshot.classify(chrono::Utc::now());
    snapshot
}

// ============================================================================
// Per-exporter status map (Issue #8756)
// ============================================================================

/// Process-global per-exporter status map (Issue #8756): one
/// [`ExportStatus`] cell per configured exporter, keyed by
/// [`ExporterKind::name`]. Registered wholesale by [`spawn_task`] after the
/// exporter set is known; read back by [`global_export_statuses`] for the
/// `observability_exports` field of `loom-daemon status --json`, so each sink
/// surfaces its own queue depth, failure counters and liveness independently.
///
/// Unlike the first-wins [`GLOBAL_EXPORT_STATUS`] OnceLock above this map is
/// **last-wins**: production calls [`spawn_task`] exactly once per process
/// (so the two are equivalent there), while replacing on each registration
/// keeps in-process `spawn_task` tests — which may run in any order —
/// observing their own registrations deterministically.
static GLOBAL_EXPORT_STATUSES: std::sync::Mutex<
    Option<std::collections::BTreeMap<String, Arc<ExportStatus>>>,
> = std::sync::Mutex::new(None);

/// Publish the per-exporter status map as the process-global (Issue #8756).
/// See [`GLOBAL_EXPORT_STATUSES`] for the last-wins rationale.
pub fn register_global_export_statuses(
    statuses: std::collections::BTreeMap<String, Arc<ExportStatus>>,
) {
    *GLOBAL_EXPORT_STATUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(statuses);
}

/// Every configured exporter's status, keyed by exporter name ("https",
/// "otlp"; Issue #8756), each with its `state` re-derived as of now. Empty
/// when observability is off by choice or no exporter reached registration.
///
/// A confirmed #4830 host-identity mismatch is folded into the `https`
/// entry's `ingest_host_id` only — the echo check is a native-ingest
/// protocol property (see [`spawn_task`]'s OTLP arm), so it never annotates
/// an OTLP entry.
#[must_use]
pub fn global_export_statuses(
) -> std::collections::BTreeMap<String, crate::types::ObservabilityExportStatus> {
    let guard = GLOBAL_EXPORT_STATUSES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(map) = guard.as_ref() else {
        return std::collections::BTreeMap::new();
    };
    let mismatch = global_host_id_mismatch();
    map.iter()
        .map(|(name, status)| {
            let mut snapshot = status.snapshot();
            if name == ExporterKind::Https.name() {
                if let Some(mismatch) = &mismatch {
                    snapshot.ingest_host_id = Some(mismatch.ingest_host_id.clone());
                }
            }
            snapshot.state = snapshot.classify(chrono::Utc::now());
            (name.clone(), snapshot)
        })
        .collect()
}

/// **env(singular) > config list > config singular > default (`[https]`)**
/// (Issue #8756). Resolves the full exporter list [`spawn_task`] fans out to:
///
/// - `$LOOM_OBSERVABILITY_EXPORTER` (the pre-fan-out singular knob) selects a
///   one-element list, overriding any configured list — same env-wins
///   precedence every other `observability.*` knob uses.
/// - `observability.exporters` (mixed string/object entries) next.
/// - The singular `observability.exporter` key after that — a one-element
///   list for back-compat.
/// - Otherwise the `[https]` default.
///
/// An unrecognized kind at any tier is logged and skipped, the same
/// "malformed input never panics/blocks startup" posture as the rest of this
/// module — never a hard error. Entries are deduped by kind (queues and
/// status are keyed by [`ExporterKind::name`], so one kind appears once —
/// first entry wins); a list whose every entry is unrecognized degrades to
/// the `[https]` default, so the return value is never empty.
#[must_use]
pub fn resolve_exporters(config: &ObservabilityConfig) -> Vec<ExporterEntry> {
    let raw: Vec<RawExporterEntry> = if let Some(env) = env_nonempty(EXPORTER_ENV) {
        vec![RawExporterEntry {
            kind: env,
            endpoint: None,
        }]
    } else if let Some(list) = config.exporters.as_ref().filter(|list| !list.is_empty()) {
        list.clone()
    } else {
        let singular = config
            .exporter
            .clone()
            .unwrap_or_else(|| "https".to_string());
        vec![RawExporterEntry {
            kind: singular,
            endpoint: None,
        }]
    };
    let mut resolved: Vec<ExporterEntry> = Vec::with_capacity(raw.len());
    for entry in raw {
        match ExporterKind::parse_lenient(&entry.kind) {
            Some(kind) => {
                if resolved.iter().any(|existing| existing.kind == kind) {
                    log::warn!(
                        "observability: duplicate exporter {:?} in observability.exporters — \
                         keeping the first entry only (queues and status are per-kind, #8756)",
                        entry.kind
                    );
                } else {
                    resolved.push(ExporterEntry {
                        kind,
                        endpoint: entry.endpoint,
                    });
                }
            }
            None => {
                log::warn!(
                    "observability: unrecognized exporter {:?} — skipping that entry",
                    entry.kind
                );
            }
        }
    }
    if resolved.is_empty() {
        resolved.push(ExporterEntry {
            kind: ExporterKind::Https,
            endpoint: None,
        });
    }
    resolved
}

/// Read `path` and return its trimmed contents as the ingest key. Every
/// failure (missing file, unreadable, empty after trimming) is logged **by
/// path only** — the key itself never reaches a log line — and yields `Err`
/// with a detail string safe to surface on `loom-daemon status` (Issue
/// #5337): the offending path and, for I/O failures, the [`std::io::Error`]'s
/// `Display` (which includes the OS errno on platforms that report one, e.g.
/// `No such file or directory (os error 2)`). [`spawn_task`] treats `Err` as
/// "not configured" — it registers the detail as a
/// [`crate::types::ObservabilityExportState::Misconfigured`] status rather
/// than dropping it.
fn read_ingest_key(path: &str) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let key = contents.trim().to_string();
            if key.is_empty() {
                let detail = format!("ingest key file {path} is empty after trimming whitespace");
                log::warn!("observability: {detail} — export disabled");
                Err(detail)
            } else {
                Ok(key)
            }
        }
        Err(error) => {
            let detail = format!("could not read ingest key file {path}: {error}");
            log::warn!("observability: {detail} — export disabled");
            Err(detail)
        }
    }
}

/// Resolve the durable-queue path for the exporter named `name` (Issue
/// #8756). A **sole** configured exporter keeps the pre-fan-out
/// [`queue::default_queue_path`] unchanged — byte-compat with every existing
/// deployment's backlog file. With N ≥ 2 exporters each gets
/// `observability-queue.<name>.jsonl` under the same directory; a
/// [`queue::QUEUE_PATH_ENV`] override gains `.<name>` before its extension
/// so a test-pinned directory still isolates per-exporter files.
fn queue_path_for(workspace_root: &Path, name: &str, sole: bool) -> PathBuf {
    if sole {
        return queue::default_queue_path(workspace_root);
    }
    if let Ok(path) = std::env::var(queue::QUEUE_PATH_ENV) {
        if !path.is_empty() {
            let mut path = PathBuf::from(path);
            if let Some(stem) = path.file_stem() {
                // "<stem>.<name>.<original-ext>" — built as one filename so
                // the appended name is never mistaken for (and replaced as)
                // the extension.
                let mut named = stem.to_os_string();
                named.push(".");
                named.push(name);
                if let Some(extension) = path.extension() {
                    named.push(".");
                    named.push(extension);
                }
                path.set_file_name(named);
                return path;
            }
            return path;
        }
    }
    workspace_root
        .join(".loom")
        .join("logs")
        .join(queue::named_queue_filename(name))
}

/// One-time upgrade step for the multi-exporter fan-out (Issue #8756): when
/// per-name queue files come into use, move a pre-fan-out backlog
/// (`observability-queue.jsonl`) into the **first** configured exporter's
/// per-name file so records queued by an older daemon are still drained.
/// Best-effort and deliberately non-merging: skipped when the per-name file
/// already exists (adoption happens exactly once, on the boot that first
/// sees the fan-out config).
fn adopt_legacy_queue_file(workspace_root: &Path, per_name: &Path) {
    if per_name.exists() {
        return;
    }
    let legacy = queue::default_queue_path(workspace_root);
    if legacy.exists() {
        if let Err(error) = std::fs::rename(&legacy, per_name) {
            log::warn!(
                "observability: could not adopt legacy queue {}: {error} — it will be \
                 adopted on a later boot",
                legacy.display()
            );
        }
    }
}

/// Spawn the observability subsystem's background tasks (the collector and
/// the sender — see the module docs) on the shared daemon runtime, or return
/// `None` when disabled or under-configured. `enabled: false` (or no block)
/// has zero side effects, matching the deliberate-off steady state; each
/// under-configured `enabled: true` path (Issue #5337) registers a
/// [`crate::types::ObservabilityExportState::Misconfigured`] status via
/// [`register_global_export_status`] before returning, so
/// [`global_export_status`] can tell a bad `ingestKeyFile` path apart from
/// telemetry being off by choice — see [`ExportStatus::misconfigured`].
///
/// **Multi-exporter fan-out (Issue #8756).** [`resolve_exporters`] yields the
/// configured exporter list; each entry gets its **own** durable queue file
/// ([`queue_path_for`]), its own sender task with an independent retry loop,
/// and its own [`ExportStatus`] entry surfaced as `observability_exports.
/// <name>` in `loom-daemon status --json`. The collector fans one
/// [`crate::telemetry::TelemetryEnvelope`] into every queue through
/// [`queue::FanoutQueue`], so a sink outage backs up only that sink's queue.
/// Policy failures (bad endpoint, placeholder host, missing Cargo feature)
/// degrade **that entry** to its own `Misconfigured` status; only when no
/// exporter survives — or the shared ingest key is unusable — does the whole
/// call return `None`. The shared `observability.endpoint` applies to every
/// entry without an explicit per-entry override.
///
/// `daemon_started_at` feeds `host.health.uptime_sec`; passing
/// `Instant::now()` at daemon startup (as [`crate::main`] does for the
/// sibling `idle_exit`/`auto_update` wiring) makes it track true daemon
/// uptime. If this task is spawned some time after the daemon's own start
/// (e.g. after a slow credential-preflight step), `uptime_sec` under-reports
/// by that startup delay — an accepted approximation for Phase 1 host-health
/// telemetry, not a correctness requirement of the exporter itself.
///
/// `workspace_pool` (Issue #4955) is the daemon's shared per-workspace
/// [`SweepRegistry`](crate::sweep_registry::SweepRegistry) pool, threaded
/// through to [`collector::spawn_task`] so `host.health`'s
/// `active_sweep_ids` reports this host's authoritative in-flight sweep-id
/// set (`collector::collect_active_sweep_ids`, private to that module).
#[must_use]
pub fn spawn_task(
    config: &ObservabilityConfig,
    workspace_root: PathBuf,
    bus: &EventBus,
    daemon_started_at: Instant,
    workspace_pool: Arc<WorkspacePool>,
) -> Option<Vec<tokio::task::JoinHandle<()>>> {
    if !resolve_enabled(config) {
        log::debug!("observability: disabled (set observability.enabled=true to opt in)");
        return None;
    }
    let entries = resolve_exporters(config);
    let sole = entries.len() == 1;
    let shared_endpoint = resolve_endpoint(config);

    // Policy pass, per entry, BEFORE the ingest key is read (Issue #7815's
    // rule — the key must never be loaded for an endpoint export will refuse,
    // and that reasoning now applies per-sink). Entries that fail a check
    // degrade to their own `Misconfigured` status; the rest are planned.
    let missing_endpoint_detail = || {
        "observability.endpoint not configured \
             (set observability.endpoint or $LOOM_OBSERVABILITY_ENDPOINT)"
            .to_string()
    };
    let mut planned: Vec<(&ExporterEntry, String)> = Vec::new();
    let mut statuses: std::collections::BTreeMap<String, Arc<ExportStatus>> =
        std::collections::BTreeMap::new();
    let reject = |statuses: &mut std::collections::BTreeMap<String, Arc<ExportStatus>>,
                  name: &str,
                  endpoint: Option<String>,
                  detail: String| {
        log::warn!(
            "observability: exporter {name} misconfigured — {detail} — export off for this sink"
        );
        statuses.insert(name.to_string(), Arc::new(ExportStatus::misconfigured(endpoint, detail)));
    };
    for entry in &entries {
        let name = entry.kind.name();
        let Some(endpoint) = entry.endpoint.clone().or_else(|| shared_endpoint.clone()) else {
            reject(&mut statuses, name, None, missing_endpoint_detail());
            continue;
        };
        if entry.kind == ExporterKind::Otlp && !endpoint_policy::valid_otlp_endpoint(&endpoint) {
            reject(
                &mut statuses,
                name,
                Some(endpoint),
                "invalid OTLP base URL: use HTTP(S) without credentials, query or fragment".into(),
            );
            continue;
        }
        // Refuse reserved placeholder domains BEFORE the ingest key is read
        // (Issue #7815) — a placeholder is "not configured", not a
        // destination, and the key must never be loaded for one, let alone
        // sent to it.
        if let Some(host) = reserved_placeholder_host(&endpoint) {
            let detail = format!(
                "observability.endpoint {endpoint} points at the reserved placeholder \
                 domain {host} (RFC 2606/6761) — refusing to export so the ingest key \
                 is never sent there; set a real endpoint via \
                 $LOOM_OBSERVABILITY_ENDPOINT or .loom-local/local.json, or leave \
                 observability.enabled=false"
            );
            reject(&mut statuses, name, Some(endpoint), detail);
            continue;
        }
        #[cfg(not(feature = "otlp"))]
        if entry.kind == ExporterKind::Otlp {
            reject(
                &mut statuses,
                name,
                Some(endpoint),
                "exporter=otlp requested but this daemon build was not compiled \
                     with the `otlp` Cargo feature"
                    .to_string(),
            );
            continue;
        }
        planned.push((entry, endpoint));
    }
    if planned.is_empty() {
        register_global_export_statuses(statuses.clone());
        register_global_export_status(primary_status_for(&entries, &statuses));
        return None;
    }

    let Some(key_file) = resolve_ingest_key_file(config) else {
        let detail = "observability.ingestKeyFile not configured \
             (set observability.ingestKeyFile or $LOOM_OBSERVABILITY_INGEST_KEY_FILE)"
            .to_string();
        log::warn!("observability: enabled but {detail} — export off");
        // The key is shared by every exporter, so every planned entry is
        // equally misconfigured (Issue #5337's detail string preserved).
        for (entry, endpoint) in &planned {
            statuses.insert(
                entry.kind.name().to_string(),
                Arc::new(ExportStatus::misconfigured(Some(endpoint.clone()), detail.clone())),
            );
        }
        register_global_export_statuses(statuses.clone());
        register_global_export_status(primary_status_for(&entries, &statuses));
        return None;
    };
    let ingest_key = match read_ingest_key(&key_file) {
        Ok(key) => key,
        Err(detail) => {
            for (entry, endpoint) in &planned {
                statuses.insert(
                    entry.kind.name().to_string(),
                    Arc::new(ExportStatus::misconfigured(Some(endpoint.clone()), detail.clone())),
                );
            }
            register_global_export_statuses(statuses.clone());
            register_global_export_status(primary_status_for(&entries, &statuses));
            return None;
        }
    };
    // Resolved ONCE and threaded into every consumer below (Issue #4830): the
    // collector stamps it on every envelope and the HTTPS exporter checks the
    // backend's echo against it. Two independent `host_identity()` calls could
    // not disagree today, but a single value makes that structurally true rather
    // than incidentally so — and the whole point of the check is that the two
    // halves being compared are the *same* identity the records are filed under.
    let host_id = crate::sweep_registry::host_identity();
    let batch_size = resolve_batch_size(config);
    let flush_interval = Duration::from_secs(resolve_flush_interval_secs(config));
    let capacity = resolve_queue_capacity(config);
    let exporter_names = entries
        .iter()
        .map(|entry| entry.kind.name())
        .collect::<Vec<_>>()
        .join(", ");
    log::info!(
        "observability: enabled (exporters=[{exporter_names}], endpoint={shared_endpoint:?}, \
         batch_size={batch_size}, flush_interval={}s, queue_capacity={capacity})",
        flush_interval.as_secs()
    );

    // Per-exporter construction (Issue #8756): each planned entry opens its
    // own durable queue at [`queue_path_for`]'s path, gets its own status
    // cell, and spawns its own sender. The `match` below constructs different
    // concrete `E: Exporter` types and each calls `sender::spawn_task` with
    // their own — no dyn/boxing needed, since every call returns the same
    // `tokio::task::JoinHandle<()>` regardless of `E` (see `exporter.rs`'s
    // module docs on why `Exporter` uses native `async fn` over
    // `async-trait`).
    let mut sender_handles = Vec::with_capacity(planned.len());
    let mut queues: Vec<Arc<DurableQueue>> = Vec::with_capacity(planned.len());
    for (index, (entry, endpoint)) in planned.iter().enumerate() {
        let name = entry.kind.name();
        let queue_path = queue_path_for(&workspace_root, name, sole);
        if !sole && index == 0 {
            adopt_legacy_queue_file(&workspace_root, &queue_path);
        }
        let queue = Arc::new(DurableQueue::open(queue_path, capacity));
        let export_status =
            Arc::new(ExportStatus::started(&host_id, endpoint, name, flush_interval.as_secs()));
        let sender_handle = match entry.kind {
            ExporterKind::Https => {
                // Host-identity mismatch detection (Issue #4830) is created and
                // registered *inside* this arm, not before the dispatch: it is a
                // property of the native ingest protocol, not of exporting in
                // general (see the OTLP arm below). Registering it only when the
                // HTTPS sink actually starts keeps `global_host_id_mismatch()`
                // reading `None` under any other sink, which is exactly the
                // "the exporter never started" semantics `status`/`health`
                // already handle.
                let host_id_status = Arc::new(HostIdStatus::default());
                register_global_host_id_status(host_id_status.clone());
                match HttpsExporter::new(
                    endpoint.clone(),
                    ingest_key.clone(),
                    host_id.clone(),
                    host_id_status,
                ) {
                    Ok(exporter) => sender::spawn_task(
                        queue.clone(),
                        exporter,
                        batch_size,
                        flush_interval,
                        export_status.clone(),
                    ),
                    Err(error) => {
                        log::warn!(
                            "observability: failed to construct HTTPS exporter for {endpoint}: {error} — export off for this sink"
                        );
                        statuses.insert(
                            name.to_string(),
                            Arc::new(ExportStatus::misconfigured(
                                Some(endpoint.clone()),
                                format!("failed to construct HTTPS exporter: {error}"),
                            )),
                        );
                        continue;
                    }
                }
            }
            ExporterKind::Otlp => {
                // No `HostIdStatus` wiring here, deliberately (Issue #4830 vs.
                // #4858). The mismatch check is not a generic exporter concern
                // that OTLP is missing out on — it is a *native-ingest
                // protocol* feature: `HttpsExporter::check_host_identity`
                // parses the Loom `/ingest` success body
                // (`{"accepted":N,"host_id":"…"}`) and compares the backend's
                // echoed `host_id` against this daemon's own, which is only
                // meaningful because the Cloudflare backend binds each ingest
                // key to exactly one host and reports that binding back.
                //
                // OTLP/HTTP has no such echo: a success response is an
                // `ExportLogsServiceResponse`/`ExportMetricsServiceResponse`
                // whose only payload is `partial_success`, and a generic OTLP
                // sink (an OpenTelemetry Collector, Grafana, Honeycomb) has no
                // concept of a per-host key binding to disagree with in the
                // first place. Threading a `HostIdStatus` in here would hand the
                // OTLP exporter a handle it could never write to — parity in the
                // signature only, while `loom-daemon status`/`health` gained a
                // field that is unconditionally `None` and indistinguishable
                // from "checked, and they agree". Better to leave the surface
                // honestly absent until an OTLP-side identity signal exists to
                // check against.
                #[cfg(feature = "otlp")]
                {
                    match otlp::OtlpExporter::new(endpoint.clone(), ingest_key.clone()) {
                        Ok(exporter) => sender::spawn_task(
                            queue.clone(),
                            exporter,
                            batch_size,
                            flush_interval,
                            export_status.clone(),
                        ),
                        Err(error) => {
                            log::warn!(
                                "observability: failed to construct OTLP exporter for {endpoint}: {error} — export off for this sink"
                            );
                            statuses.insert(
                                name.to_string(),
                                Arc::new(ExportStatus::misconfigured(
                                    Some(endpoint.clone()),
                                    format!("failed to construct OTLP exporter: {error}"),
                                )),
                            );
                            continue;
                        }
                    }
                }
                // Unreachable without the feature: the policy pass above
                // already rejected `Otlp` entries on non-`otlp` builds.
                #[cfg(not(feature = "otlp"))]
                {
                    let _ = (endpoint, ingest_key, batch_size, flush_interval);
                    unreachable!("otlp entry rejected in the policy pass without the feature")
                }
            }
        };
        statuses.insert(name.to_string(), export_status);
        queues.push(queue);
        sender_handles.push(sender_handle);
    }
    if sender_handles.is_empty() || queues.is_empty() {
        register_global_export_statuses(statuses.clone());
        register_global_export_status(primary_status_for(&entries, &statuses));
        return None;
    }
    let fanout = Arc::new(queue::FanoutQueue::new(queues));
    // `session.summary` emission (Issue #8757, fanned out by #8756): hand the
    // transcript-ingest thread the same fan-out sink every other producer
    // writes through (see `session_summary`'s module doc for why a
    // process-global, rather than a constructor argument, is the wiring) —
    // one record offered into every configured exporter's queue, drained by
    // the sender(s) spawned above, so the new record kind rides whichever
    // exporter(s) this config selected with no egress code of its own.
    session_summary::register_global_session_summary_sink(
        session_summary::SessionSummarySink::new(fanout.clone(), host_id.clone()),
    );
    // `session.analysis` emission (Issue #8760): same wiring, one slice
    // later — see `session_analysis`'s module doc.
    session_analysis::register_global_session_analysis_sink(
        session_analysis::SessionAnalysisSink::new(fanout.clone(), host_id.clone()),
    );
    // `daemon.event` collection (Issue #8760, G4): a second, independent bus
    // subscription alongside `collector::spawn_task` below — see
    // `daemon_event`'s module doc for why it is a separate subscriber rather
    // than folded into `collector`.
    let daemon_event_handle = daemon_event::spawn_task(bus, fanout.clone(), host_id.clone());
    let collector_handle = collector::spawn_task(
        bus,
        fanout,
        workspace_root,
        host_id.clone(),
        SNAPSHOT_INTERVAL,
        daemon_started_at,
        workspace_pool,
    );
    // Only reached when at least one exporter was constructed and its sender
    // spawned — every degrade-to-disabled path above returns early, so
    // `disabled` on the status wire stays truthful (Issue #5083).
    register_global_export_statuses(statuses.clone());
    register_global_export_status(primary_status_for(&entries, &statuses));
    let mut handles = Vec::with_capacity(sender_handles.len() + 2);
    handles.push(collector_handle);
    handles.push(daemon_event_handle);
    handles.extend(sender_handles);
    Some(handles)
}

/// The back-compat single-status cell for [`GLOBAL_EXPORT_STATUS`] (Issue
/// #8756): the **first configured exporter's** entry from the per-exporter
/// map — its `started` cell when it is running, its `misconfigured` cell when
/// it is the reason export is off. Byte-identical to the pre-fan-out status
/// for every single-exporter config; for N ≥ 2 it is the deterministic first
/// entry, with the full picture in [`global_export_statuses`].
fn primary_status_for(
    entries: &[ExporterEntry],
    statuses: &std::collections::BTreeMap<String, Arc<ExportStatus>>,
) -> Arc<ExportStatus> {
    let name = entries
        .first()
        .map_or_else(|| ExporterKind::Https.name(), |entry| entry.kind.name());
    statuses.get(name).cloned().unwrap_or_else(|| {
        Arc::new(ExportStatus::misconfigured(
            None,
            "observability exporter produced no status".to_string(),
        ))
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests;
