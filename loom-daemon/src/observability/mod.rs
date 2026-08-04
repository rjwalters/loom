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
//!   `observability.exporter = "otlp"` ([`resolve_exporter`]). Off by
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
pub mod exporter;
#[cfg(feature = "otlp")]
pub mod otlp;
pub mod queue;
pub mod sender;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::event_bus::EventBus;
use crate::workspace_pool::WorkspacePool;

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
    /// Raw `observability.exporter` string, resolved by [`resolve_exporter`]
    /// (#4858) — not parsed to [`ExporterKind`] at read time so an unknown
    /// value can be logged (and degraded to the default) at the single call
    /// site that already owns exporter selection.
    pub exporter: Option<String>,
}

/// Which [`exporter::Exporter`] implementation [`spawn_task`] selects.
/// Resolved by [`resolve_exporter`] — **env > config > default**
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
    }
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

/// Live record of whether telemetry is actually *reaching* the backend, written
/// by [`sender::try_flush`] on every attempt and read back by
/// [`global_export_status`] for `loom-daemon status` / `health`.
///
/// The complement to [`HostIdStatus`], which is anomaly-only by design (#4830)
/// and therefore cannot answer "is it working?" — only "is it working *wrong*
/// in this one specific way?". This cell is always readable and always has an
/// answer, including the answer #4830 could never give: *configured, running,
/// and has never successfully exported anything*.
///
/// Unlike [`HostIdStatus`] this is **not** write-once: it is a rolling record,
/// so `last_success_at` genuinely means "the last time data landed" and a watch
/// loop can alert on its age.
#[derive(Debug)]
pub struct ExportStatus {
    inner: std::sync::Mutex<crate::types::ObservabilityExportStatus>,
}

// Allow expect_used: a poisoned status mutex means another thread panicked
// while holding it — unrecoverable, matching the crash-on-poison policy
// `HostIdStatus` above (and `auto_update`/`ipc`) already use.
#[allow(clippy::expect_used)]
impl ExportStatus {
    /// A status cell for an exporter that is starting *now* under `host_id`,
    /// pushing to `endpoint` via `exporter` every `flush_interval_secs`.
    #[must_use]
    pub fn started(
        host_id: &str,
        endpoint: &str,
        exporter: &str,
        flush_interval_secs: u64,
    ) -> Self {
        ExportStatus {
            inner: std::sync::Mutex::new(crate::types::ObservabilityExportStatus {
                state: crate::types::ObservabilityExportState::Starting,
                host_id: Some(host_id.to_string()),
                ingest_host_id: None,
                endpoint: Some(endpoint.to_string()),
                exporter: Some(exporter.to_string()),
                started_at: Some(chrono::Utc::now()),
                last_success_at: None,
                last_failure_at: None,
                last_failure_detail: None,
                records_exported: 0,
                consecutive_failures: 0,
                flush_interval_secs: Some(flush_interval_secs),
            }),
        }
    }

    /// Record `count` envelopes acked by the backend. Clears the consecutive-
    /// failure run: the transport demonstrably works again.
    pub fn record_success(&self, count: usize) {
        let mut guard = self
            .inner
            .lock()
            .expect("observability export status mutex poisoned");
        guard.last_success_at = Some(chrono::Utc::now());
        guard.records_exported = guard
            .records_exported
            .saturating_add(count.try_into().unwrap_or(u64::MAX));
        guard.consecutive_failures = 0;
    }

    /// Record a failed flush attempt. `detail` is the exporter's own error
    /// text; the previous `last_success_at` is deliberately preserved so the
    /// surfaces can distinguish "used to work, broke 30s ago" from "never
    /// worked at all".
    pub fn record_failure(&self, detail: &str) {
        let mut guard = self
            .inner
            .lock()
            .expect("observability export status mutex poisoned");
        guard.last_failure_at = Some(chrono::Utc::now());
        guard.last_failure_detail = Some(detail.to_string());
        guard.consecutive_failures = guard.consecutive_failures.saturating_add(1);
    }

    /// The current record, with [`crate::types::ObservabilityExportStatus::state`]
    /// re-derived as of now. `ingest_host_id` is folded in from
    /// [`global_host_id_mismatch`] by [`global_export_status`], not here — this
    /// cell knows nothing about identity.
    #[must_use]
    pub fn snapshot(&self) -> crate::types::ObservabilityExportStatus {
        let mut snapshot = self
            .inner
            .lock()
            .expect("observability export status mutex poisoned")
            .clone();
        snapshot.state = snapshot.classify(chrono::Utc::now());
        snapshot
    }
}

/// Process-global export-status handle, registered by [`spawn_task`] when — and
/// only when — the exporter actually starts. Unset (observability disabled,
/// keyless, or under-configured) makes [`global_export_status`] report
/// [`crate::types::ObservabilityExportStatus::disabled`].
static GLOBAL_EXPORT_STATUS: std::sync::OnceLock<Arc<ExportStatus>> = std::sync::OnceLock::new();

/// Register the exporter's export-status handle as the process-global.
/// Idempotent: only the first registration wins (one exporter per process).
pub fn register_global_export_status(status: Arc<ExportStatus>) {
    let _ = GLOBAL_EXPORT_STATUS.set(status);
}

/// This process's export status — **always** an answer, never silence.
///
/// Unregistered ⇒ `disabled()` (the exporter never started), which is a
/// materially different report from the `None` an older daemon binary puts on
/// the wire. Any confirmed #4830 host-identity mismatch is folded in here as
/// `ingest_host_id`, so a single field carries the whole state machine and
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

/// **env > config > default** ([`ExporterKind::Https`]). An unrecognized
/// value at either tier (anything but a case-insensitive `"https"` or
/// `"otlp"`) is logged and degrades to the default, the same "malformed
/// input never panics/blocks startup" posture as the rest of this module —
/// it is never a hard error.
#[must_use]
pub fn resolve_exporter(config: &ObservabilityConfig) -> ExporterKind {
    let raw = env_nonempty(EXPORTER_ENV).or_else(|| config.exporter.clone());
    match raw.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("https") => ExporterKind::Https,
        Some("otlp") => ExporterKind::Otlp,
        Some(other) => {
            log::warn!("observability: unrecognized exporter {other:?} — falling back to https");
            ExporterKind::Https
        }
    }
}

/// Read `path` and return its trimmed contents as the ingest key. Every
/// failure (missing file, unreadable, empty after trimming) is logged
/// **by path only** — the key itself never reaches a log line — and yields
/// `None`, which [`spawn_task`] treats as "not configured".
fn read_ingest_key(path: &str) -> Option<String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => {
            let key = contents.trim().to_string();
            if key.is_empty() {
                log::warn!("observability: ingest key file {path} is empty — export disabled");
                None
            } else {
                Some(key)
            }
        }
        Err(error) => {
            log::warn!(
                "observability: could not read ingest key file {path}: {error} — export disabled"
            );
            None
        }
    }
}

/// Spawn the observability subsystem's background tasks (the collector and
/// the sender — see the module docs) on the shared daemon runtime, or return
/// `None` without any side effect when disabled or under-configured.
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
    let Some(endpoint) = resolve_endpoint(config) else {
        log::warn!(
            "observability: enabled but no endpoint configured \
             (set observability.endpoint or $LOOM_OBSERVABILITY_ENDPOINT) — export off"
        );
        return None;
    };
    let Some(key_file) = resolve_ingest_key_file(config) else {
        log::warn!(
            "observability: enabled but no ingestKeyFile configured \
             (set observability.ingestKeyFile or $LOOM_OBSERVABILITY_INGEST_KEY_FILE) — export off"
        );
        return None;
    };
    let ingest_key = read_ingest_key(&key_file)?;
    // Resolved ONCE and threaded into every consumer below (Issue #4830): the
    // collector stamps it on every envelope and the HTTPS exporter checks the
    // backend's echo against it. Two independent `host_identity()` calls could
    // not disagree today, but a single value makes that structurally true rather
    // than incidentally so — and the whole point of the check is that the two
    // halves being compared are the *same* identity the records are filed under.
    let host_id = crate::sweep_registry::host_identity();
    let exporter_kind = resolve_exporter(config);

    let batch_size = resolve_batch_size(config);
    let flush_interval = Duration::from_secs(resolve_flush_interval_secs(config));
    let capacity = resolve_queue_capacity(config);
    let queue_path = queue::default_queue_path(&workspace_root);
    log::info!(
        "observability: enabled (exporter={exporter_kind:?}, endpoint={endpoint}, \
         batch_size={batch_size}, flush_interval={}s, queue_capacity={capacity})",
        flush_interval.as_secs()
    );
    let queue = Arc::new(DurableQueue::open(queue_path, capacity));

    // Export-liveness status (Issue #5083). Created here — where the endpoint,
    // host id, exporter kind and cadence are all resolved — but registered as
    // the process-global only *after* the exporter has actually been
    // constructed below, so the degrade-to-disabled paths (HTTPS client
    // construction failure, `otlp` without the feature) keep reporting
    // `disabled` rather than a phantom running exporter.
    let export_status = Arc::new(ExportStatus::started(
        &host_id,
        &endpoint,
        match exporter_kind {
            ExporterKind::Https => "https",
            ExporterKind::Otlp => "otlp",
        },
        flush_interval.as_secs(),
    ));

    let collector_handle = collector::spawn_task(
        bus,
        queue.clone(),
        workspace_root,
        host_id.clone(),
        SNAPSHOT_INTERVAL,
        daemon_started_at,
        workspace_pool,
    );
    // The two branches below construct different concrete `E: Exporter`
    // types and each call `sender::spawn_task` with their own — no
    // dyn/boxing needed, since both calls return the same
    // `tokio::task::JoinHandle<()>` regardless of `E` (see `exporter.rs`'s
    // module docs on why `Exporter` uses native `async fn` over
    // `async-trait`).
    let sender_handle = match exporter_kind {
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
            let exporter = match HttpsExporter::new(
                endpoint.clone(),
                ingest_key,
                host_id,
                host_id_status,
            ) {
                Ok(exporter) => exporter,
                Err(error) => {
                    log::warn!(
                        "observability: failed to construct HTTPS exporter for {endpoint}: {error} — export off"
                    );
                    return None;
                }
            };
            sender::spawn_task(queue, exporter, batch_size, flush_interval, export_status.clone())
        }
        ExporterKind::Otlp => {
            // No `HostIdStatus` wiring here, deliberately (Issue #4830 vs.
            // #4858). The mismatch check is not a generic exporter concern
            // that OTLP is missing out on — it is a *native-ingest protocol*
            // feature: `HttpsExporter::check_host_identity` parses the
            // Loom `/ingest` success body (`{"accepted":N,"host_id":"…"}`)
            // and compares the backend's echoed `host_id` against this
            // daemon's own, which is only meaningful because the Cloudflare
            // backend binds each ingest key to exactly one host and reports
            // that binding back.
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
                let exporter = match otlp::OtlpExporter::new(endpoint.clone(), ingest_key) {
                    Ok(exporter) => exporter,
                    Err(error) => {
                        log::warn!(
                            "observability: failed to construct OTLP exporter for {endpoint}: {error} — export off"
                        );
                        return None;
                    }
                };
                sender::spawn_task(
                    queue,
                    exporter,
                    batch_size,
                    flush_interval,
                    export_status.clone(),
                )
            }
            #[cfg(not(feature = "otlp"))]
            {
                log::warn!(
                    "observability: exporter=otlp requested but this daemon build was not \
                     compiled with the `otlp` Cargo feature — export off"
                );
                return None;
            }
        }
    };
    // Only reached when an exporter was constructed and its sender spawned —
    // every degrade-to-disabled path above returns early, so `disabled` on the
    // status wire stays truthful (Issue #5083).
    register_global_export_status(export_status);
    Some(vec![collector_handle, sender_handle])
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::tempdir;

    const ALL_ENV_VARS: &[&str] = &[
        ENABLED_ENV,
        ENDPOINT_ENV,
        INGEST_KEY_FILE_ENV,
        BATCH_SIZE_ENV,
        FLUSH_INTERVAL_SECS_ENV,
        QUEUE_CAPACITY_ENV,
        EXPORTER_ENV,
    ];

    fn clear_env() {
        for var in ALL_ENV_VARS {
            std::env::remove_var(var);
        }
    }

    /// A freshly-constructed, empty [`WorkspacePool`] — `spawn_task` now
    /// requires one (Issue #4955) to thread through to the collector.
    /// `Handle::current()` is why every call site below must run inside a
    /// Tokio runtime (`#[tokio::test]`), even the `spawn_task` calls whose
    /// config disables/under-configures export and so never reach the
    /// collector at all.
    fn test_workspace_pool() -> Arc<WorkspacePool> {
        Arc::new(WorkspacePool::new(Arc::new(EventBus::new()), tokio::runtime::Handle::current()))
    }

    #[test]
    #[serial]
    fn absent_config_resolves_to_documented_defaults() {
        // #[serial] + an explicit clear (matching every other test in this
        // module that reads these env vars): without it, this test races
        // `env_overrides_config` / `config_wins_over_default_when_no_env_set`
        // on the same process-global env vars and intermittently observes a
        // leaked `LOOM_OBSERVABILITY_*` value from a concurrently-running
        // test (#4705 flake, caught in review).
        clear_env();
        let config = ObservabilityConfig::default();
        assert!(!resolve_enabled(&config), "off by default (FLAGS-OFF posture)");
        assert_eq!(resolve_endpoint(&config), None);
        // `default_ingest_key_file` is `cfg(test)`-gated to `None` (see its
        // doc comment) so this in-process assertion cannot observe the real
        // `$HOME`-relative production default — `ingest_key_file_under`
        // below tests that default's actual path-join logic directly.
        assert_eq!(resolve_ingest_key_file(&config), None);
        assert_eq!(resolve_batch_size(&config), DEFAULT_BATCH_SIZE);
        assert_eq!(resolve_flush_interval_secs(&config), DEFAULT_FLUSH_INTERVAL_SECS);
        assert_eq!(resolve_queue_capacity(&config), DEFAULT_QUEUE_CAPACITY);
        assert_eq!(resolve_exporter(&config), ExporterKind::Https, "https is the default exporter");
    }

    // ------------------------------------------------------------------
    // resolve_ingest_key_file — env > config > $HOME-relative default (#5336).
    // ------------------------------------------------------------------

    #[test]
    fn ingest_key_file_under_joins_the_conventional_relative_path() {
        assert_eq!(
            ingest_key_file_under(Path::new("/home/ubuntu")),
            "/home/ubuntu/.loom/observability/ingest.key"
        );
        assert_eq!(
            ingest_key_file_under(Path::new("/Users/robb-studio")),
            "/Users/robb-studio/.loom/observability/ingest.key",
            "must resolve against WHATEVER home is passed in — never a value \
             baked in from a different host"
        );
    }

    #[test]
    #[serial]
    fn resolve_ingest_key_file_config_wins_over_default_when_no_env_set() {
        clear_env();
        let config = ObservabilityConfig {
            ingest_key_file: Some("/etc/loom/observability-ingest.key".to_string()),
            ..Default::default()
        };
        assert_eq!(
            resolve_ingest_key_file(&config),
            Some("/etc/loom/observability-ingest.key".to_string())
        );
    }

    #[test]
    #[serial]
    fn resolve_ingest_key_file_env_overrides_config() {
        clear_env();
        std::env::set_var(INGEST_KEY_FILE_ENV, "/run/secrets/ingest.key");
        let config = ObservabilityConfig {
            ingest_key_file: Some("/etc/loom/observability-ingest.key".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_ingest_key_file(&config), Some("/run/secrets/ingest.key".to_string()));
        clear_env();
    }

    // ------------------------------------------------------------------
    // resolve_exporter — env > config > default("https"); unknown ⇒ https.
    // ------------------------------------------------------------------

    #[test]
    #[serial]
    fn resolve_exporter_env_overrides_config() {
        clear_env();
        let config = ObservabilityConfig {
            exporter: Some("otlp".to_string()),
            ..Default::default()
        };
        std::env::set_var(EXPORTER_ENV, "https");
        assert_eq!(resolve_exporter(&config), ExporterKind::Https);
        clear_env();
    }

    #[test]
    #[serial]
    fn resolve_exporter_config_wins_over_default_when_no_env_set() {
        clear_env();
        let config = ObservabilityConfig {
            exporter: Some("otlp".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_exporter(&config), ExporterKind::Otlp);
    }

    #[test]
    #[serial]
    fn resolve_exporter_is_case_insensitive() {
        clear_env();
        let config = ObservabilityConfig {
            exporter: Some("OTLP".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_exporter(&config), ExporterKind::Otlp);
    }

    #[test]
    #[serial]
    fn resolve_exporter_unknown_value_falls_back_to_https() {
        clear_env();
        let config = ObservabilityConfig {
            exporter: Some("datadog".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_exporter(&config), ExporterKind::Https);
    }

    #[test]
    #[serial]
    fn env_overrides_config() {
        clear_env();
        let config = ObservabilityConfig {
            enabled: Some(false),
            endpoint: Some("https://config.example.com/ingest".to_string()),
            batch_size: Some(10),
            ..Default::default()
        };
        std::env::set_var(ENABLED_ENV, "true");
        std::env::set_var(ENDPOINT_ENV, "https://env.example.com/ingest");
        std::env::set_var(BATCH_SIZE_ENV, "99");
        assert!(resolve_enabled(&config));
        assert_eq!(resolve_endpoint(&config).as_deref(), Some("https://env.example.com/ingest"));
        assert_eq!(resolve_batch_size(&config), 99);
        clear_env();
    }

    #[test]
    #[serial]
    fn config_wins_over_default_when_no_env_set() {
        clear_env();
        let config = ObservabilityConfig {
            enabled: Some(true),
            flush_interval_secs: Some(120),
            queue_capacity: Some(500),
            ..Default::default()
        };
        assert!(resolve_enabled(&config));
        assert_eq!(resolve_flush_interval_secs(&config), 120);
        assert_eq!(resolve_queue_capacity(&config), 500);
    }

    #[test]
    fn read_config_parses_every_field_from_the_observability_block() {
        let dir = tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            r#"{"observability": {
                "enabled": true,
                "endpoint": "https://ingest.example.com/v1/telemetry",
                "ingestKeyFile": "/etc/loom/ingest.key",
                "batchSize": 25,
                "flushIntervalSecs": 45,
                "queueCapacity": 1000,
                "exporter": "otlp"
            }}"#,
        )
        .unwrap();
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let config = read_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(config.enabled, Some(true));
        assert_eq!(config.endpoint.as_deref(), Some("https://ingest.example.com/v1/telemetry"));
        assert_eq!(config.ingest_key_file.as_deref(), Some("/etc/loom/ingest.key"));
        assert_eq!(config.batch_size, Some(25));
        assert_eq!(config.flush_interval_secs, Some(45));
        assert_eq!(config.queue_capacity, Some(1000));
        assert_eq!(config.exporter.as_deref(), Some("otlp"));
    }

    #[test]
    fn missing_observability_block_is_the_documented_default() {
        let dir = tempdir().unwrap();
        std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
        let config = read_config(dir.path());
        std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
        assert_eq!(config, ObservabilityConfig::default());
    }

    // ------------------------------------------------------------------
    // spawn_task degrade-to-disabled paths — every one must return `None`
    // with zero side effects (no queue file created).
    // ------------------------------------------------------------------

    #[tokio::test]
    #[serial]
    async fn spawn_task_disabled_returns_none() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let config = ObservabilityConfig::default();
        let handles = spawn_task(
            &config,
            dir.path().to_path_buf(),
            &bus,
            Instant::now(),
            test_workspace_pool(),
        );
        assert!(handles.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn spawn_task_enabled_without_endpoint_returns_none() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let config = ObservabilityConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let handles = spawn_task(
            &config,
            dir.path().to_path_buf(),
            &bus,
            Instant::now(),
            test_workspace_pool(),
        );
        assert!(handles.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn spawn_task_enabled_without_ingest_key_file_returns_none() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let config = ObservabilityConfig {
            enabled: Some(true),
            endpoint: Some("https://ingest.example.com/v1/telemetry".to_string()),
            ..Default::default()
        };
        let handles = spawn_task(
            &config,
            dir.path().to_path_buf(),
            &bus,
            Instant::now(),
            test_workspace_pool(),
        );
        assert!(handles.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn spawn_task_enabled_with_unreadable_ingest_key_file_returns_none() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let config = ObservabilityConfig {
            enabled: Some(true),
            endpoint: Some("https://ingest.example.com/v1/telemetry".to_string()),
            ingest_key_file: Some(
                dir.path()
                    .join("does-not-exist.key")
                    .to_string_lossy()
                    .to_string(),
            ),
            ..Default::default()
        };
        let handles = spawn_task(
            &config,
            dir.path().to_path_buf(),
            &bus,
            Instant::now(),
            test_workspace_pool(),
        );
        assert!(handles.is_none());
    }

    #[tokio::test]
    #[serial]
    async fn spawn_task_fully_configured_spawns_two_tasks() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("ingest.key");
        std::fs::write(&key_path, "s3cr3t\n").unwrap();
        let config = ObservabilityConfig {
            enabled: Some(true),
            endpoint: Some("https://ingest.example.com/v1/telemetry".to_string()),
            ingest_key_file: Some(key_path.to_string_lossy().to_string()),
            flush_interval_secs: Some(3600), // avoid a real network attempt during the test
            ..Default::default()
        };
        let handles = spawn_task(
            &config,
            dir.path().to_path_buf(),
            &bus,
            Instant::now(),
            test_workspace_pool(),
        );
        let handles = handles.expect("fully configured ⇒ spawn_task must return Some");
        assert_eq!(handles.len(), 2, "collector + sender");
        for handle in handles {
            handle.abort();
        }
    }

    /// `exporter=otlp` in a build that does NOT have the `otlp` Cargo
    /// feature compiled in must degrade to disabled (never panic, never
    /// silently fall back to the HTTPS sink the operator did not ask for).
    /// `#[tokio::test]`, not a plain `#[test]`: `spawn_task` calls
    /// `collector::spawn_task` (which needs a Tokio reactor) *before* it
    /// branches on `exporter_kind`, so even the returns-`None` path must run
    /// inside a runtime — same as the `#[cfg(feature = "otlp")]` sibling below.
    #[cfg(not(feature = "otlp"))]
    #[tokio::test]
    #[serial]
    async fn spawn_task_otlp_requested_without_the_feature_returns_none() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("ingest.key");
        std::fs::write(&key_path, "s3cr3t\n").unwrap();
        let config = ObservabilityConfig {
            enabled: Some(true),
            endpoint: Some("https://ingest.example.com/v1/telemetry".to_string()),
            ingest_key_file: Some(key_path.to_string_lossy().to_string()),
            exporter: Some("otlp".to_string()),
            ..Default::default()
        };
        let handles = spawn_task(
            &config,
            dir.path().to_path_buf(),
            &bus,
            Instant::now(),
            test_workspace_pool(),
        );
        assert!(handles.is_none());
    }

    /// The `otlp`-feature counterpart of
    /// `spawn_task_fully_configured_spawns_two_tasks`: `exporter=otlp`
    /// with the feature compiled in spawns the same collector+sender pair,
    /// just wired to `otlp::OtlpExporter` instead of `HttpsExporter`.
    #[cfg(feature = "otlp")]
    #[tokio::test]
    #[serial]
    async fn spawn_task_otlp_exporter_spawns_two_tasks() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("ingest.key");
        std::fs::write(&key_path, "s3cr3t\n").unwrap();
        let config = ObservabilityConfig {
            enabled: Some(true),
            endpoint: Some("https://collector.example.com".to_string()),
            ingest_key_file: Some(key_path.to_string_lossy().to_string()),
            exporter: Some("otlp".to_string()),
            flush_interval_secs: Some(3600), // avoid a real network attempt during the test
            ..Default::default()
        };
        let handles = spawn_task(
            &config,
            dir.path().to_path_buf(),
            &bus,
            Instant::now(),
            test_workspace_pool(),
        );
        let handles =
            handles.expect("fully configured otlp exporter ⇒ spawn_task must return Some");
        assert_eq!(handles.len(), 2, "collector + sender");
        for handle in handles {
            handle.abort();
        }
    }
}
