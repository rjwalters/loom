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
//! body for anything but a batch-accepted/rejected status, and no daemon
//! state is ever mutated from data received over this channel.
//!
//! # Ingest key handling
//!
//! The ingest key is read once at startup from `ingestKeyFile` (never
//! accepted inline in config) and held only in memory as an
//! [`exporter::HttpsExporter`] field, sent solely as an `Authorization:
//! Bearer` HTTP header value. Every log line and [`exporter::ExportError`]
//! variant in this module tree names the *file path*, never the key
//! contents.

pub mod collector;
pub mod exporter;
pub mod queue;
pub mod sender;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::event_bus::EventBus;

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

/// **env > config**, no built-in default — same "not configured" handling as
/// [`resolve_endpoint`].
#[must_use]
pub fn resolve_ingest_key_file(config: &ObservabilityConfig) -> Option<String> {
    env_nonempty(INGEST_KEY_FILE_ENV).or_else(|| config.ingest_key_file.clone())
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
#[must_use]
pub fn spawn_task(
    config: &ObservabilityConfig,
    workspace_root: PathBuf,
    bus: &EventBus,
    daemon_started_at: Instant,
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
    let exporter = match HttpsExporter::new(endpoint.clone(), ingest_key) {
        Ok(exporter) => exporter,
        Err(error) => {
            log::warn!("observability: failed to construct HTTPS exporter for {endpoint}: {error} — export off");
            return None;
        }
    };

    let batch_size = resolve_batch_size(config);
    let flush_interval = Duration::from_secs(resolve_flush_interval_secs(config));
    let capacity = resolve_queue_capacity(config);
    let queue_path = queue::default_queue_path(&workspace_root);
    log::info!(
        "observability: enabled (endpoint={endpoint}, batch_size={batch_size}, \
         flush_interval={}s, queue_capacity={capacity})",
        flush_interval.as_secs()
    );
    let queue = Arc::new(DurableQueue::open(queue_path, capacity));
    let host_id = crate::sweep_registry::host_identity();

    let collector_handle = collector::spawn_task(
        bus,
        queue.clone(),
        workspace_root,
        host_id,
        SNAPSHOT_INTERVAL,
        daemon_started_at,
    );
    let sender_handle = sender::spawn_task(queue, exporter, batch_size, flush_interval);
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
    ];

    fn clear_env() {
        for var in ALL_ENV_VARS {
            std::env::remove_var(var);
        }
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
        assert_eq!(resolve_ingest_key_file(&config), None);
        assert_eq!(resolve_batch_size(&config), DEFAULT_BATCH_SIZE);
        assert_eq!(resolve_flush_interval_secs(&config), DEFAULT_FLUSH_INTERVAL_SECS);
        assert_eq!(resolve_queue_capacity(&config), DEFAULT_QUEUE_CAPACITY);
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
                "queueCapacity": 1000
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

    #[test]
    #[serial]
    fn spawn_task_disabled_returns_none() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let config = ObservabilityConfig::default();
        let handles = spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now());
        assert!(handles.is_none());
    }

    #[test]
    #[serial]
    fn spawn_task_enabled_without_endpoint_returns_none() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let config = ObservabilityConfig {
            enabled: Some(true),
            ..Default::default()
        };
        let handles = spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now());
        assert!(handles.is_none());
    }

    #[test]
    #[serial]
    fn spawn_task_enabled_without_ingest_key_file_returns_none() {
        clear_env();
        let bus = EventBus::new();
        let dir = tempdir().unwrap();
        let config = ObservabilityConfig {
            enabled: Some(true),
            endpoint: Some("https://ingest.example.com/v1/telemetry".to_string()),
            ..Default::default()
        };
        let handles = spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now());
        assert!(handles.is_none());
    }

    #[test]
    #[serial]
    fn spawn_task_enabled_with_unreadable_ingest_key_file_returns_none() {
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
        let handles = spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now());
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
        let handles = spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now());
        let handles = handles.expect("fully configured ⇒ spawn_task must return Some");
        assert_eq!(handles.len(), 2, "collector + sender");
        for handle in handles {
            handle.abort();
        }
    }
}
