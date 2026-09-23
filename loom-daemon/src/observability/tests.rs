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
    // The queue-path override (#8756): spawn_task's per-exporter path
    // derivation consults it, so a leak between serial tests would
    // redirect every later queue-path assertion.
    queue::QUEUE_PATH_ENV,
];

fn clear_env() {
    for var in ALL_ENV_VARS {
        std::env::remove_var(var);
    }
}

/// Endpoint for every `spawn_task` fixture that must reach *past* the
/// endpoint check — i.e. the ones asserting a missing/unreadable key
/// file, a missing Cargo feature, or a successful spawn. Deliberately
/// **not** an `example.com` address: `spawn_task` now refuses reserved
/// placeholder domains outright (Issue #7815), so such a fixture would
/// either make a success case fail or make a `returns_none` case pass
/// for the wrong reason. `.internal` is
/// non-resolvable private-use space, and these tests never let the
/// sender reach its first flush anyway.
const SAFE_TEST_ENDPOINT: &str = "https://ingest.test-fixture.internal/v1/telemetry";

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
    assert_eq!(
        resolve_exporters(&config),
        vec![ExporterEntry {
            kind: ExporterKind::Https,
            endpoint: None
        }],
        "https is the default exporter"
    );
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
fn resolve_exporter_unknown_value_falls_back_to_https() {
    clear_env();
    let config = ObservabilityConfig {
        exporter: Some("datadog".to_string()),
        ..Default::default()
    };
    assert_eq!(
        resolve_exporters(&config),
        vec![ExporterEntry {
            kind: ExporterKind::Https,
            endpoint: None
        }],
        "a sole unknown singular value still degrades to the https default"
    );
}

// ------------------------------------------------------------------
// resolve_exporters — the `observability.exporters` list (#8756):
// env(singular) > config list > config singular > default [https].
// Entries dedupe by kind (one queue/status per kind this slice) and an
// unknown kind is logged + skipped, never a hard error.
// ------------------------------------------------------------------
// (The four pre-#8756 `resolve_exporter` tests — env override, singular
// config, case-insensitivity, unknown-value fallback — are superseded by
// the list-level tests below, which cover each of those cases plus the
// mixed/object entry forms.)

fn raw(kind: &str, endpoint: Option<&str>) -> RawExporterEntry {
    RawExporterEntry {
        kind: kind.to_string(),
        endpoint: endpoint.map(str::to_string),
    }
}

fn entry(kind: ExporterKind, endpoint: Option<&str>) -> ExporterEntry {
    ExporterEntry {
        kind,
        endpoint: endpoint.map(str::to_string),
    }
}

#[test]
#[serial]
fn resolve_exporters_defaults_to_a_https_singleton() {
    clear_env();
    assert_eq!(
        resolve_exporters(&ObservabilityConfig::default()),
        vec![entry(ExporterKind::Https, None)]
    );
}

#[test]
#[serial]
fn resolve_exporters_parses_mixed_string_and_object_entries() {
    clear_env();
    let config = ObservabilityConfig {
        exporters: Some(vec![
            raw("https", None),
            raw("otlp", Some("http://collector.internal:4318")),
        ]),
        ..Default::default()
    };
    assert_eq!(
        resolve_exporters(&config),
        vec![
            entry(ExporterKind::Https, None),
            entry(ExporterKind::Otlp, Some("http://collector.internal:4318")),
        ]
    );
}

#[test]
#[serial]
fn resolve_exporters_env_singular_still_overrides_the_whole_list() {
    clear_env();
    let config = ObservabilityConfig {
        exporters: Some(vec![raw("https", None), raw("otlp", None)]),
        ..Default::default()
    };
    std::env::set_var(EXPORTER_ENV, "otlp");
    assert_eq!(
        resolve_exporters(&config),
        vec![entry(ExporterKind::Otlp, None)],
        "the pre-#8756 singular env knob selects a one-element list"
    );
    clear_env();
}

#[test]
#[serial]
fn resolve_exporters_singular_config_key_is_a_one_element_list() {
    clear_env();
    let config = ObservabilityConfig {
        exporter: Some("otlp".to_string()),
        ..Default::default()
    };
    assert_eq!(resolve_exporters(&config), vec![entry(ExporterKind::Otlp, None)]);
}

#[test]
#[serial]
fn resolve_exporters_skips_unknown_kinds_and_falls_back_when_all_invalid() {
    clear_env();
    let config = ObservabilityConfig {
        exporters: Some(vec![raw("bogus", None), raw("otlp", None)]),
        ..Default::default()
    };
    assert_eq!(
        resolve_exporters(&config),
        vec![entry(ExporterKind::Otlp, None)],
        "an unknown entry is skipped, not fatal"
    );
    let all_invalid = ObservabilityConfig {
        exporters: Some(vec![raw("bogus", None)]),
        ..Default::default()
    };
    assert_eq!(
        resolve_exporters(&all_invalid),
        vec![entry(ExporterKind::Https, None)],
        "every entry invalid degrades to the https default"
    );
}

#[test]
#[serial]
fn resolve_exporters_dedupes_by_kind_first_wins() {
    clear_env();
    let config = ObservabilityConfig {
        exporters: Some(vec![
            raw("https", None),
            raw("otlp", None),
            raw("https", Some("https://other.example.com/ingest")),
        ]),
        ..Default::default()
    };
    assert_eq!(
        resolve_exporters(&config),
        vec![
            entry(ExporterKind::Https, None),
            entry(ExporterKind::Otlp, None)
        ],
        "queue/status are keyed by kind name, so a repeated kind collapses to its first entry"
    );
}

#[test]
#[serial]
fn resolve_exporters_is_case_insensitive() {
    clear_env();
    let config = ObservabilityConfig {
        exporters: Some(vec![raw("OTLP", None)]),
        ..Default::default()
    };
    assert_eq!(resolve_exporters(&config), vec![entry(ExporterKind::Otlp, None)]);
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
// `LOOM_CONFIG_DEFAULTS_FILE` is process-global; serialize against every
// other test in the crate that mutates it via the shared `loom_config_env`
// key (#6177 — this test previously mutated the var with no `#[serial]`
// at all, racing dozens of correctly-serialized tests elsewhere).
#[serial(loom_config_env)]
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
#[serial(loom_config_env)]
fn read_config_parses_exporters_entries_from_both_forms() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"observability": {
            "enabled": true,
            "endpoint": "https://ingest.example.com/v1/telemetry",
            "exporters": ["https", {"kind": "otlp", "endpoint": "http://collector.internal:4318"}]
        }}"#,
    )
    .unwrap();
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let config = read_config(dir.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(
        config.exporters,
        Some(vec![
            raw("https", None),
            raw("otlp", Some("http://collector.internal:4318")),
        ])
    );
}

// ------------------------------------------------------------------
// Per-exporter queue paths (#8756): a sole exporter keeps the legacy
// `observability-queue.jsonl` name (byte-compat with every deployment
// predating the fan-out); N ≥ 2 exporters get per-name files, and the
// first exporter adopts any legacy backlog via rename.
// ------------------------------------------------------------------

#[test]
#[serial]
fn queue_path_for_sole_exporter_keeps_the_legacy_filename() {
    clear_env();
    let root = Path::new("/ws");
    assert_eq!(
        queue_path_for(root, "https", true),
        PathBuf::from("/ws/.loom/logs/observability-queue.jsonl")
    );
}

#[test]
#[serial]
fn queue_path_for_multiple_exporters_is_per_name() {
    clear_env();
    let root = Path::new("/ws");
    assert_eq!(
        queue_path_for(root, "https", false),
        PathBuf::from("/ws/.loom/logs/observability-queue.https.jsonl")
    );
    assert_eq!(
        queue_path_for(root, "otlp", false),
        PathBuf::from("/ws/.loom/logs/observability-queue.otlp.jsonl")
    );
}

#[test]
#[serial]
fn queue_path_env_override_gains_the_name_suffix_for_multiple_exporters() {
    clear_env();
    std::env::set_var(queue::QUEUE_PATH_ENV, "/tmp/queues/override.jsonl");
    assert_eq!(
        queue_path_for(Path::new("/ws"), "https", false),
        PathBuf::from("/tmp/queues/override.https.jsonl")
    );
    assert_eq!(
        queue_path_for(Path::new("/ws"), "https", true),
        PathBuf::from("/tmp/queues/override.jsonl"),
        "sole exporter keeps the override verbatim (pre-#8756 behavior)"
    );
    clear_env();
}

#[test]
#[serial]
fn adopt_legacy_queue_file_renames_a_backlog_into_the_first_per_name_file() {
    clear_env();
    let dir = tempdir().unwrap();
    let logs = dir.path().join(".loom/logs");
    std::fs::create_dir_all(&logs).unwrap();
    let legacy = logs.join("observability-queue.jsonl");
    let per_name = logs.join("observability-queue.https.jsonl");
    std::fs::write(&legacy, "{}\n").unwrap();
    adopt_legacy_queue_file(dir.path(), &per_name);
    assert!(per_name.exists(), "backlog moved into the per-name queue file");
    assert!(!legacy.exists(), "legacy file no longer competes for the backlog");

    // Adoption is a one-time upgrade step, not a merge: once the per-name
    // file exists, a later legacy file is left alone.
    std::fs::write(&legacy, "{}\n").unwrap();
    adopt_legacy_queue_file(dir.path(), &per_name);
    assert!(legacy.exists(), "adoption must not clobber a populated per-name file");
}

// Issue #6504: `observability.ingestKeyFile` is a per-host filesystem
// path (#5354, #5464, #6499) and must resolve correctly when it lives
// ONLY in the gitignored `.loom-local/local.json` tier — never committed
// to the tracked `.loom/config.json` — proving a host-specific value
// routed off the tracked file still reaches the daemon unchanged.
#[test]
#[serial(loom_config_env)]
fn ingest_key_file_resolves_from_local_tier_when_absent_from_legacy() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    // The tracked, shared tier carries only team-shared policy — no
    // per-host path.
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"observability": {"enabled": true, "endpoint": "https://ingest.example.com/v1/telemetry"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join(".loom-local")).unwrap();
    std::fs::write(
        dir.path().join(".loom-local/local.json"),
        r#"{"observability": {"ingestKeyFile": "/home/ubuntu/.loom/observability/ingest.key"}}"#,
    )
    .unwrap();
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let config = read_config(dir.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(config.enabled, Some(true), "tracked-tier policy still resolves");
    assert_eq!(
        config.ingest_key_file.as_deref(),
        Some("/home/ubuntu/.loom/observability/ingest.key"),
        "host-local ingestKeyFile resolves even though the tracked tier never sets it"
    );
}

// A host-local override in `.loom-local/local.json` takes precedence over
// a (legacy, discouraged) value still committed in `.loom/config.json` —
// same precedence proof as `test-check-ingest-key-file.sh` test 7, here
// exercised through the Rust resolver directly.
#[test]
#[serial(loom_config_env)]
fn ingest_key_file_local_tier_overrides_legacy_tier() {
    let dir = tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"observability": {"ingestKeyFile": "/Users/a-different-mac-user/.loom/observability/ingest.key"}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join(".loom-local")).unwrap();
    std::fs::write(
        dir.path().join(".loom-local/local.json"),
        r#"{"observability": {"ingestKeyFile": "/home/ubuntu/.loom/observability/ingest.key"}}"#,
    )
    .unwrap();
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let config = read_config(dir.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(
        config.ingest_key_file.as_deref(),
        Some("/home/ubuntu/.loom/observability/ingest.key"),
        "the host-local override wins over a stale/foreign value left in the tracked tier"
    );
}

#[test]
#[serial(loom_config_env)]
fn missing_observability_block_is_the_documented_default() {
    let dir = tempdir().unwrap();
    std::env::set_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV, "");
    let config = read_config(dir.path());
    std::env::remove_var(crate::config_resolver::PRIVATE_DEFAULTS_ENV);
    assert_eq!(config, ObservabilityConfig::default());
}

// ------------------------------------------------------------------
// read_ingest_key — Issue #5337: failures now carry a detail string
// (offending path + underlying error) instead of only a `log::warn!`,
// so `spawn_task` can thread it into a `Misconfigured` status.
// ------------------------------------------------------------------

#[test]
fn read_ingest_key_missing_file_names_path_and_error() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("does-not-exist.key");
    let detail = read_ingest_key(&path.to_string_lossy()).unwrap_err();
    assert!(
        detail.contains(&path.to_string_lossy().to_string()),
        "detail must name the path: {detail}"
    );
    // `std::io::Error`'s `Display` includes the OS errno on every
    // platform that reports one (macOS/Linux: "(os error 2)").
    assert!(
        detail.to_ascii_lowercase().contains("no such file") || detail.contains("os error"),
        "detail must carry the underlying error: {detail}"
    );
}

#[test]
fn read_ingest_key_empty_file_names_path() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("empty.key");
    std::fs::write(&path, "   \n").unwrap(); // whitespace-only ⇒ empty after trim
    let detail = read_ingest_key(&path.to_string_lossy()).unwrap_err();
    assert!(
        detail.contains(&path.to_string_lossy().to_string()),
        "detail must name the path: {detail}"
    );
}

#[test]
fn read_ingest_key_trims_and_returns_the_key_on_success() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("ingest.key");
    std::fs::write(&path, "  s3cr3t-key  \n").unwrap();
    assert_eq!(read_ingest_key(&path.to_string_lossy()).unwrap(), "s3cr3t-key");
}

// ------------------------------------------------------------------
// ExportStatus::misconfigured — Issue #5337. Exercised directly against
// the wrapper type (not through the process-global `OnceLock`, which is
// write-once for the whole test binary and so cannot be asserted on
// deterministically from more than one test — see `spawn_task`'s
// under-configured tests below, which stick to the pre-existing
// `handles.is_none()` contract for that reason).
// ------------------------------------------------------------------

#[test]
fn export_status_misconfigured_reports_a_distinct_sticky_state() {
    let status = ExportStatus::misconfigured(
        Some("https://ingest.example.com/v1/telemetry".to_string()),
        "could not read ingest key file /etc/loom/ingest.key: No such file or directory (os error 2)"
            .to_string(),
    );
    let snapshot = status.snapshot();
    assert_eq!(snapshot.state, crate::types::ObservabilityExportState::Misconfigured);
    assert_eq!(snapshot.endpoint.as_deref(), Some("https://ingest.example.com/v1/telemetry"));
    let detail = snapshot.last_failure_detail.as_deref().unwrap();
    assert!(detail.contains("/etc/loom/ingest.key"));
    assert!(detail.contains("os error 2"));
    // Never `Disabled` — the whole point of this issue.
    assert_ne!(snapshot.state, crate::types::ObservabilityExportState::Disabled);
}

// ------------------------------------------------------------------
// spawn_task degrade-to-disabled paths — the `enabled: false` path
// returns `None` with zero side effects; the three `enabled: true`
// under-configured paths below now register a `Misconfigured` status
// (Issue #5337) before returning `None` — see
// `export_status_misconfigured_reports_a_distinct_sticky_state` above
// for coverage of that status's shape.
// ------------------------------------------------------------------

#[tokio::test]
#[serial]
async fn spawn_task_disabled_returns_none() {
    clear_env();
    let bus = EventBus::new();
    let dir = tempdir().unwrap();
    let config = ObservabilityConfig::default();
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
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
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
    assert!(handles.is_none());
}

/// Issue #7815: a *placeholder* endpoint is as under-configured as an
/// unset one. The fixture is the exact committed placeholder (#6650)
/// plus a perfectly readable ingest key — the worst case, where every
/// other precondition for exporting is satisfied — and must still
/// return `None` rather than hand that key to a reserved domain.
#[tokio::test]
#[serial]
async fn spawn_task_placeholder_endpoint_returns_none() {
    clear_env();
    let bus = EventBus::new();
    let dir = tempdir().unwrap();
    let key_path = dir.path().join("ingest.key");
    std::fs::write(&key_path, "s3cr3t\n").unwrap();
    let config = ObservabilityConfig {
        enabled: Some(true),
        endpoint: Some("https://dashboard.example.com/ingest".to_string()),
        ingest_key_file: Some(key_path.to_string_lossy().to_string()),
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
    assert!(
        handles.is_none(),
        "a reserved placeholder endpoint must never start an exporter, \
         however complete the rest of the config is"
    );
}

#[tokio::test]
#[serial]
async fn spawn_task_enabled_without_ingest_key_file_returns_none() {
    clear_env();
    let bus = EventBus::new();
    let dir = tempdir().unwrap();
    let config = ObservabilityConfig {
        enabled: Some(true),
        endpoint: Some(SAFE_TEST_ENDPOINT.to_string()),
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
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
        endpoint: Some(SAFE_TEST_ENDPOINT.to_string()),
        ingest_key_file: Some(
            dir.path()
                .join("does-not-exist.key")
                .to_string_lossy()
                .to_string(),
        ),
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
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
        endpoint: Some(SAFE_TEST_ENDPOINT.to_string()),
        ingest_key_file: Some(key_path.to_string_lossy().to_string()),
        flush_interval_secs: Some(3600), // avoid a real network attempt during the test
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
    let handles = handles.expect("fully configured ⇒ spawn_task must return Some");
    assert_eq!(handles.len(), 2, "collector + sender");
    for handle in handles {
        handle.abort();
    }
}

#[cfg(feature = "otlp")]
#[tokio::test]
#[serial]
async fn spawn_task_two_exporters_spawns_collector_plus_two_senders() {
    clear_env();
    let bus = EventBus::new();
    let dir = tempdir().unwrap();
    let key_path = dir.path().join("ingest.key");
    std::fs::write(&key_path, "s3cr3t\n").unwrap();
    let config = ObservabilityConfig {
        enabled: Some(true),
        endpoint: Some(SAFE_TEST_ENDPOINT.to_string()),
        ingest_key_file: Some(key_path.to_string_lossy().to_string()),
        exporters: Some(vec![
            raw("https", None),
            raw("otlp", Some("http://collector.test-fixture.internal:4318")),
        ]),
        flush_interval_secs: Some(3600),
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool())
            .expect("fully configured ⇒ spawn_task must return Some");
    assert_eq!(handles.len(), 3, "collector + one sender per exporter");
    let statuses = global_export_statuses();
    assert_eq!(
        statuses.keys().collect::<Vec<_>>(),
        vec!["https", "otlp"],
        "each exporter surfaces its own status entry"
    );
    for handle in handles {
        handle.abort();
    }
}

/// Without the `otlp` Cargo feature the otlp ENTRY degrades to its own
/// `misconfigured` status while the https exporter still runs (#8756):
/// one bad sink must not take the healthy one down with it.
#[cfg(not(feature = "otlp"))]
#[tokio::test]
#[serial]
async fn spawn_task_two_exporters_isolate_the_unbuildable_kind() {
    clear_env();
    let bus = EventBus::new();
    let dir = tempdir().unwrap();
    let key_path = dir.path().join("ingest.key");
    std::fs::write(&key_path, "s3cr3t\n").unwrap();
    let config = ObservabilityConfig {
        enabled: Some(true),
        endpoint: Some(SAFE_TEST_ENDPOINT.to_string()),
        ingest_key_file: Some(key_path.to_string_lossy().to_string()),
        exporters: Some(vec![raw("https", None), raw("otlp", None)]),
        flush_interval_secs: Some(3600),
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool())
            .expect("the https exporter is fully configured and must still run");
    assert_eq!(handles.len(), 2, "collector + only the https sender");
    let statuses = global_export_statuses();
    assert_eq!(
        statuses["otlp"].state,
        crate::types::ObservabilityExportState::Misconfigured,
        "the otlp entry reports its own misconfiguration"
    );
    assert_eq!(
        statuses["https"].exporter.as_deref(),
        Some("https"),
        "the https entry started normally"
    );
    for handle in handles {
        handle.abort();
    }
}

/// `exporter=otlp` in a build that does NOT have the `otlp` Cargo
/// feature compiled in must register a `Misconfigured` status (Issue
/// #5337) and return `None` (never panic, never silently fall back to
/// the HTTPS sink the operator did not ask for). Sticks to the
/// pre-existing `handles.is_none()` contract — same as the other three
/// under-configured `spawn_task` tests above — for the OnceLock reason
/// documented on `export_status_misconfigured_reports_a_distinct_sticky_state`.
/// Reject before starting collectors or doing queue IO.
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
        endpoint: Some(SAFE_TEST_ENDPOINT.to_string()),
        ingest_key_file: Some(key_path.to_string_lossy().to_string()),
        exporter: Some("otlp".to_string()),
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
    assert!(handles.is_none());
    assert_eq!(bus.receiver_count(), 0);
    assert!(!dir
        .path()
        .join(".loom/logs/observability-queue.jsonl")
        .exists());
}

#[tokio::test]
#[serial]
async fn malformed_otlp_configuration_has_no_collector_or_queue_activity() {
    clear_env();
    for endpoint in [
        "http://user:secret@example.com",
        "http://localhost?key=secret",
    ] {
        let dir = tempdir().unwrap();
        let bus = EventBus::new();
        let config = ObservabilityConfig {
            enabled: Some(true),
            endpoint: Some(endpoint.into()),
            exporter: Some("otlp".into()),
            ingest_key_file: Some(
                dir.path()
                    .join("missing-key")
                    .to_string_lossy()
                    .into_owned(),
            ),
            ..Default::default()
        };
        assert!(spawn_task(
            &config,
            dir.path().into(),
            &bus,
            Instant::now(),
            test_workspace_pool()
        )
        .is_none());
        assert_eq!(bus.receiver_count(), 0);
        assert!(!dir.path().join(".loom").exists());
    }
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
        endpoint: Some("https://collector.test-fixture.internal".to_string()),
        ingest_key_file: Some(key_path.to_string_lossy().to_string()),
        exporter: Some("otlp".to_string()),
        flush_interval_secs: Some(3600), // avoid a real network attempt during the test
        ..Default::default()
    };
    let handles =
        spawn_task(&config, dir.path().to_path_buf(), &bus, Instant::now(), test_workspace_pool());
    let handles = handles.expect("fully configured otlp exporter ⇒ spawn_task must return Some");
    assert_eq!(handles.len(), 2, "collector + sender");
    for handle in handles {
        handle.abort();
    }
}
