//! Issue #9215. Every test here reads process-global env vars, so all of them
//! take the same default `#[serial]` group `observability`'s own config tests
//! use — a leaked `LOOM_CLAUDE_CODE_TELEMETRY_*` or `LOOM_OBSERVABILITY_*`
//! value from a concurrent test would otherwise decide the tier under test
//! (#4705/#8976).
#![allow(clippy::unwrap_used)]
use super::*;
use serial_test::serial;

const LOCAL_EDGE: &str = "http://127.0.0.1:4318";

/// Every env var any resolver in this module (or the shared `observability`
/// endpoint it falls back to) consults.
const ENV_KEYS: &[&str] = &[
    ENABLED_ENV,
    ENDPOINT_ENV,
    PROTOCOL_ENV,
    LOG_TOOL_DETAILS_ENV,
    super::super::ENABLED_ENV,
    super::super::ENDPOINT_ENV,
];

/// Clears [`ENV_KEYS`] for the duration of a test and restores exactly what
/// was there before — the pattern `observability::tracing`'s propagation test
/// uses, so a real fleet host's exported overrides cannot decide this suite.
struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl EnvGuard {
    fn clear() -> Self {
        let previous = ENV_KEYS
            .iter()
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in ENV_KEYS {
            std::env::remove_var(key);
        }
        Self(previous)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in &self.0 {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// The shared `observability.endpoint` tier, as read from config.
fn shared(endpoint: &str) -> super::super::ObservabilityConfig {
    super::super::ObservabilityConfig {
        endpoint: Some(endpoint.to_string()),
        ..Default::default()
    }
}

fn names(pairs: &[(&'static str, String)]) -> Vec<&'static str> {
    pairs.iter().map(|(name, _)| *name).collect()
}

fn value<'a>(pairs: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.as_str())
}

/// The named variables a child `Command` carries, from the explicit overrides
/// `Command::get_envs` reports (`None` ⇒ an `env_remove` tombstone).
fn command_env(command: &Command) -> Vec<(String, Option<String>)> {
    command
        .get_envs()
        .filter(|(key, _)| MANAGED_CHILD_ENV.contains(&key.to_string_lossy().as_ref()))
        .map(|(key, value)| {
            (
                key.to_string_lossy().to_string(),
                value.map(|v| v.to_string_lossy().to_string()),
            )
        })
        .collect()
}

#[test]
#[serial]
fn off_by_default_injects_nothing() {
    let _guard = EnvGuard::clear();
    let config = ClaudeCodeTelemetryConfig::default();
    assert!(!resolve_enabled(&config), "FLAGS-OFF posture");
    assert!(
        child_env(&config, &shared(LOCAL_EDGE)).is_empty(),
        "a resolvable endpoint alone must not turn the opt-in on"
    );
}

#[test]
#[serial]
fn enabled_injects_the_documented_variables_with_the_shared_endpoint() {
    let _guard = EnvGuard::clear();
    let config = ClaudeCodeTelemetryConfig {
        enabled: Some(true),
        ..Default::default()
    };
    let pairs = child_env(&config, &shared(LOCAL_EDGE));
    assert_eq!(
        names(&pairs),
        vec![
            "CLAUDE_CODE_ENABLE_TELEMETRY",
            "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA",
            "OTEL_TRACES_EXPORTER",
            "OTEL_EXPORTER_OTLP_PROTOCOL",
            "OTEL_EXPORTER_OTLP_ENDPOINT",
        ],
        "the master toggle alone never adds OTEL_LOG_TOOL_DETAILS"
    );
    assert_eq!(value(&pairs, "CLAUDE_CODE_ENABLE_TELEMETRY"), Some("1"));
    assert_eq!(
        value(&pairs, "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA"),
        Some("1"),
        "without the beta flag OTEL_TRACES_EXPORTER does nothing"
    );
    assert_eq!(value(&pairs, "OTEL_TRACES_EXPORTER"), Some("otlp"));
    assert_eq!(
        value(&pairs, "OTEL_EXPORTER_OTLP_PROTOCOL"),
        Some(DEFAULT_PROTOCOL),
        "the protocol is ALWAYS explicit: OTLP has no silent default"
    );
    assert_eq!(
        value(&pairs, "OTEL_EXPORTER_OTLP_ENDPOINT"),
        Some(LOCAL_EDGE),
        "the endpoint comes from observability.endpoint's own precedence chain"
    );
}

#[test]
#[serial]
fn log_tool_details_is_an_independent_sub_toggle() {
    let _guard = EnvGuard::clear();
    let mut config = ClaudeCodeTelemetryConfig {
        enabled: Some(true),
        ..Default::default()
    };
    assert!(!resolve_log_tool_details(&config), "off with the master on");
    assert_eq!(value(&child_env(&config, &shared(LOCAL_EDGE)), "OTEL_LOG_TOOL_DETAILS"), None);
    config.log_tool_details = Some(true);
    assert_eq!(
        value(&child_env(&config, &shared(LOCAL_EDGE)), "OTEL_LOG_TOOL_DETAILS"),
        Some("1"),
        "tool-input capture only with its own explicit opt-in"
    );
    // …and it is not itself a master switch.
    let details_only = ClaudeCodeTelemetryConfig {
        log_tool_details: Some(true),
        ..Default::default()
    };
    assert!(child_env(&details_only, &shared(LOCAL_EDGE)).is_empty());
}

#[test]
#[serial]
fn every_knob_resolves_env_over_config_over_default() {
    let _guard = EnvGuard::clear();
    // Tier 3: default.
    let empty = ClaudeCodeTelemetryConfig::default();
    assert!(!resolve_enabled(&empty));
    assert!(!resolve_log_tool_details(&empty));
    assert_eq!(resolve_protocol(&empty), DEFAULT_PROTOCOL);
    assert_eq!(resolve_endpoint(&empty, &shared(LOCAL_EDGE)).as_deref(), Some(LOCAL_EDGE));
    assert_eq!(
        resolve_endpoint(&empty, &super::super::ObservabilityConfig::default()),
        None,
        "no built-in endpoint default"
    );

    // Tier 2: config.
    let configured = ClaudeCodeTelemetryConfig {
        enabled: Some(true),
        endpoint: Some("http://127.0.0.1:4319".to_string()),
        protocol: Some("http/protobuf".to_string()),
        log_tool_details: Some(true),
    };
    assert!(resolve_enabled(&configured));
    assert!(resolve_log_tool_details(&configured));
    assert_eq!(resolve_protocol(&configured), "http/protobuf");
    assert_eq!(
        resolve_endpoint(&configured, &shared(LOCAL_EDGE)).as_deref(),
        Some("http://127.0.0.1:4319"),
        "this block's endpoint beats the shared observability.endpoint"
    );

    // Tier 1: env beats both.
    std::env::set_var(ENABLED_ENV, "0");
    std::env::set_var(LOG_TOOL_DETAILS_ENV, "false");
    std::env::set_var(PROTOCOL_ENV, "grpc");
    std::env::set_var(ENDPOINT_ENV, "http://127.0.0.1:4320/");
    assert!(!resolve_enabled(&configured), "env off beats config on");
    assert!(!resolve_log_tool_details(&configured));
    assert_eq!(resolve_protocol(&configured), "grpc");
    assert_eq!(
        resolve_endpoint(&configured, &shared(LOCAL_EDGE)).as_deref(),
        Some("http://127.0.0.1:4320"),
        "a trailing slash is trimmed: the SDK appends /v1/traces itself"
    );
    std::env::set_var(ENABLED_ENV, "on");
    assert!(resolve_enabled(&empty), "env on beats an absent config block");
}

#[test]
#[serial]
fn an_unusable_endpoint_fails_closed() {
    let _guard = EnvGuard::clear();
    let enabled = ClaudeCodeTelemetryConfig {
        enabled: Some(true),
        ..Default::default()
    };
    for endpoint in [
        "not a URL",
        "http://user:secret@127.0.0.1:4318",
        "http://127.0.0.1:4318/?token=abc",
        // A reserved placeholder domain — the committed config's own default.
        "https://dashboard.example.com/ingest",
    ] {
        assert!(
            child_env(&enabled, &shared(endpoint)).is_empty(),
            "{endpoint} must inject nothing"
        );
    }
    assert!(
        child_env(&enabled, &super::super::ObservabilityConfig::default()).is_empty(),
        "enabled with no endpoint anywhere injects nothing"
    );
}

#[test]
#[serial]
fn an_unknown_protocol_degrades_to_the_default() {
    let _guard = EnvGuard::clear();
    let config = ClaudeCodeTelemetryConfig {
        protocol: Some("HTTP/JSON".to_string()),
        ..Default::default()
    };
    assert_eq!(resolve_protocol(&config), "http/json", "case-insensitive");
    let config = ClaudeCodeTelemetryConfig {
        protocol: Some("thrift".to_string()),
        ..Default::default()
    };
    assert_eq!(resolve_protocol(&config), DEFAULT_PROTOCOL);
}

#[test]
#[serial]
fn read_config_parses_the_block_and_tolerates_junk() {
    let _guard = EnvGuard::clear();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"observability": {"claudeCodeTelemetry": {
             "enabled": true, "endpoint": " http://127.0.0.1:4318 ",
             "protocol": "http/protobuf", "logToolDetails": true}}}"#,
    )
    .unwrap();
    assert_eq!(
        read_config(dir.path()),
        ClaudeCodeTelemetryConfig {
            enabled: Some(true),
            endpoint: Some(LOCAL_EDGE.to_string()),
            protocol: Some("http/protobuf".to_string()),
            log_tool_details: Some(true),
        }
    );

    let junk = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(junk.path().join(".loom")).unwrap();
    std::fs::write(
        junk.path().join(".loom/config.json"),
        r#"{"observability": {"claudeCodeTelemetry": {
             "enabled": "yes", "endpoint": "", "protocol": 7}}}"#,
    )
    .unwrap();
    assert_eq!(
        read_config(junk.path()),
        ClaudeCodeTelemetryConfig::default(),
        "wrongly-typed keys read as unset, never as an error"
    );
    assert_eq!(
        read_config(tempfile::tempdir().unwrap().path()),
        ClaudeCodeTelemetryConfig::default(),
        "no config file at all"
    );
}

#[test]
#[serial]
fn prepare_child_removes_the_managed_set_when_off_and_sets_it_when_on() {
    let _guard = EnvGuard::clear();
    let dir = tempfile::tempdir().unwrap();

    // Off (no config block at all): every managed name is an explicit
    // REMOVAL, so an ambient value on the daemon cannot reach the child and
    // nothing else about the spawn env changes.
    let mut command = Command::new("/usr/bin/true");
    prepare_child(&mut command, dir.path());
    let observed = command_env(&command);
    assert_eq!(observed.len(), MANAGED_CHILD_ENV.len());
    for (name, value) in &observed {
        assert_eq!(value, &None, "{name} is removed, not set");
    }

    // On: the same call sets the documented values.
    std::env::set_var(ENABLED_ENV, "1");
    std::env::set_var(ENDPOINT_ENV, LOCAL_EDGE);
    let mut command = Command::new("/usr/bin/true");
    prepare_child(&mut command, dir.path());
    let observed = command_env(&command);
    assert_eq!(
        observed.iter().filter(|(_, value)| value.is_some()).count(),
        5,
        "five of six set; OTEL_LOG_TOOL_DETAILS stays a removal"
    );
    assert!(observed
        .contains(&("OTEL_EXPORTER_OTLP_ENDPOINT".to_string(), Some(LOCAL_EDGE.to_string()))));
    assert!(observed.contains(&("OTEL_LOG_TOOL_DETAILS".to_string(), None)));
}
