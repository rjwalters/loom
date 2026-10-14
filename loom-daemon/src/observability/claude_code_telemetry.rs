//! Opt-in Claude Code OTel env injection for spawned workers (Issue #9215).
//!
//! A Loom sweep child is one `claude -p "/loom:sweep N …"` process that runs
//! Curator → Builder → Judge → Doctor → Merge serially inside itself, so from
//! the daemon's side it is a single 15–20 minute span with no way to tell model
//! time from tool time. Claude Code can emit exactly that breakdown natively —
//! `claude_code.llm_request` (with `ttft_ms`) and `claude_code.tool` /
//! `claude_code.tool.execution` sub-spans — but only when its own OTel
//! environment is configured, which Loom never did.
//!
//! This module is that configuration, and nothing else: it resolves a small
//! `observability.claudeCodeTelemetry` block and stamps the resulting variables
//! onto a child [`Command`]. It opens no socket, reads no response, and emits
//! no record of its own — the spans travel from the *child's* OTel SDK straight
//! to the endpoint, never through Loom's queue.
//!
//! # Off by default, and inert when off
//!
//! Precedence is **env > config > default (`false`)**, mirroring
//! [`super::resolve_enabled`] exactly. With the block absent or disabled,
//! [`prepare_child`] only *removes* the variables in [`MANAGED_CHILD_ENV`], so
//! a spawned child's environment is byte-identical to its pre-#9215 shape
//! apart from the standard `TRACEPARENT` that
//! [`super::tracing::prepare_child`] now mirrors.
//!
//! # Loom owns the managed set
//!
//! Those six names are cleared unconditionally before anything is set, the
//! same posture [`super::tracing::prepare_child`] takes with the traceparent
//! pair: what reaches the child is decided by *this host's config*, never by
//! whatever the daemon's own supervisor environment happened to carry. Turn
//! the feature on through the config block or its `LOOM_*` overrides — not by
//! exporting `CLAUDE_CODE_ENABLE_TELEMETRY` at the daemon.
//!
//! `OTEL_EXPORTER_OTLP_HEADERS` is deliberately **not** managed here: an edge
//! that wants bearer auth (Loom's own reference Collector does) needs a
//! credential, and this module never handles one — an operator exports that
//! variable on the daemon and plain process inheritance carries it through.
//!
//! # Tool details are their own switch
//!
//! `OTEL_LOG_TOOL_DETAILS` captures Bash command lines and tool input. It has
//! an independent sub-toggle that stays off when the master switch is on, so
//! enabling span timing can never silently enable content capture.

use std::path::Path;
use std::process::Command;

/// `observability.claudeCodeTelemetry.enabled` env override.
pub const ENABLED_ENV: &str = "LOOM_CLAUDE_CODE_TELEMETRY_ENABLED";
/// `observability.claudeCodeTelemetry.endpoint` env override.
pub const ENDPOINT_ENV: &str = "LOOM_CLAUDE_CODE_TELEMETRY_ENDPOINT";
/// `observability.claudeCodeTelemetry.protocol` env override.
pub const PROTOCOL_ENV: &str = "LOOM_CLAUDE_CODE_TELEMETRY_PROTOCOL";
/// `observability.claudeCodeTelemetry.logToolDetails` env override.
pub const LOG_TOOL_DETAILS_ENV: &str = "LOOM_CLAUDE_CODE_TELEMETRY_LOG_TOOL_DETAILS";

/// The `observability` sub-block this module reads.
pub const CONFIG_PATH: &str = "observability.claudeCodeTelemetry";

/// Default OTLP protocol. `http/json` matches what Loom's own OTLP exporter
/// speaks to the same host-local edge (`observability::otlp` POSTs JSON to
/// `<endpoint>/v1/traces`), so one endpoint value is correct for both
/// producers. Claude Code's docs mark `OTEL_EXPORTER_OTLP_PROTOCOL` **required**
/// when using OTLP with no silent default, which is why it is always set
/// explicitly: omitting it is the failure where every variable a test looks for
/// is present and nothing is ever exported.
pub const DEFAULT_PROTOCOL: &str = "http/json";

/// Protocols the OTLP spec (and Claude Code) accept. An unrecognized configured
/// value is logged and degraded to [`DEFAULT_PROTOCOL`] — same warn-and-degrade
/// posture as [`super::resolve_exporters`] on an unknown exporter kind.
pub const VALID_PROTOCOLS: &[&str] = &["grpc", "http/json", "http/protobuf"];

/// Every child-facing variable this module owns: set together when the opt-in
/// resolves, removed together otherwise. Ordered as documented.
pub const MANAGED_CHILD_ENV: &[&str] = &[
    "CLAUDE_CODE_ENABLE_TELEMETRY",
    "CLAUDE_CODE_ENHANCED_TELEMETRY_BETA",
    "OTEL_TRACES_EXPORTER",
    "OTEL_EXPORTER_OTLP_PROTOCOL",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
    "OTEL_LOG_TOOL_DETAILS",
];

/// The `observability.claudeCodeTelemetry` block as read from config, before
/// env/default resolution — same read/resolve split as
/// [`super::ObservabilityConfig`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaudeCodeTelemetryConfig {
    pub enabled: Option<bool>,
    /// Endpoint override; `None` ⇒ the shared `observability.endpoint`
    /// ([`super::resolve_endpoint`]), which is the same host-local edge the
    /// daemon's own OTLP sink targets.
    pub endpoint: Option<String>,
    /// Raw `protocol` string, validated at resolve time so an unknown value
    /// can be named in a warn line instead of silently disabling export.
    pub protocol: Option<String>,
    /// Independent sub-toggle for `OTEL_LOG_TOOL_DETAILS` (Bash command lines
    /// and tool input). Default off even with [`Self::enabled`] on.
    pub log_tool_details: Option<bool>,
}

/// Read [`CONFIG_PATH`] from `root`'s resolved config, same tolerant-parse
/// contract as [`super::read_config`]: any missing or wrongly-typed key reads
/// as `None` rather than failing a dispatch.
#[must_use]
pub fn read_config(root: &Path) -> ClaudeCodeTelemetryConfig {
    let config = crate::config_resolver::resolve_effective_config(root);
    let Some(block) = crate::config_resolver::get_path(&config, CONFIG_PATH) else {
        return ClaudeCodeTelemetryConfig::default();
    };
    ClaudeCodeTelemetryConfig {
        enabled: block.get("enabled").and_then(serde_json::Value::as_bool),
        endpoint: block
            .get("endpoint")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        protocol: block
            .get("protocol")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string),
        log_tool_details: block
            .get("logToolDetails")
            .and_then(serde_json::Value::as_bool),
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
pub fn resolve_enabled(config: &ClaudeCodeTelemetryConfig) -> bool {
    env_bool(ENABLED_ENV).or(config.enabled).unwrap_or(false)
}

/// **env > config > default** (`false`), resolved independently of
/// [`resolve_enabled`] — see the module doc.
#[must_use]
pub fn resolve_log_tool_details(config: &ClaudeCodeTelemetryConfig) -> bool {
    env_bool(LOG_TOOL_DETAILS_ENV)
        .or(config.log_tool_details)
        .unwrap_or(false)
}

/// **env > config > default** ([`DEFAULT_PROTOCOL`]), degrading an
/// unrecognized value to the default with a warning.
#[must_use]
pub fn resolve_protocol(config: &ClaudeCodeTelemetryConfig) -> String {
    let requested = env_nonempty(PROTOCOL_ENV).or_else(|| config.protocol.clone());
    match requested {
        None => DEFAULT_PROTOCOL.to_string(),
        Some(raw) => {
            let normalized = raw.to_ascii_lowercase();
            if VALID_PROTOCOLS.contains(&normalized.as_str()) {
                normalized
            } else {
                log::warn!(
                    "observability: unrecognized {CONFIG_PATH}.protocol {raw:?} — \
                     using {DEFAULT_PROTOCOL} (valid: {})",
                    VALID_PROTOCOLS.join(", ")
                );
                DEFAULT_PROTOCOL.to_string()
            }
        }
    }
}

/// **env > this block's `endpoint` > `observability.endpoint`** (itself
/// **env > config**, [`super::resolve_endpoint`]). No built-in default: an
/// unresolved endpoint means "not configured", which [`child_env`] treats as
/// off rather than exporting to nowhere.
#[must_use]
pub fn resolve_endpoint(
    config: &ClaudeCodeTelemetryConfig,
    observability: &super::ObservabilityConfig,
) -> Option<String> {
    env_nonempty(ENDPOINT_ENV)
        .or_else(|| config.endpoint.clone())
        .or_else(|| super::resolve_endpoint(observability))
        .map(|endpoint| endpoint.trim().trim_end_matches('/').to_string())
        .filter(|endpoint| !endpoint.is_empty())
}

/// The variables to stamp on a spawned worker, or an empty vector when the
/// opt-in is off or its endpoint is unfit.
///
/// The endpoint passes the same policy [`super::tracing::enabled`] applies to
/// Loom's own OTLP sink — an HTTP(S) URL with no credentials/query/fragment
/// ([`super::endpoint_policy::valid_otlp_endpoint`]) that is not a reserved
/// placeholder domain — so an `enabled: true` sitting next to the committed
/// placeholder endpoint fails closed with a warning instead of pointing a
/// worker's exporter at `example.com`.
#[must_use]
pub fn child_env(
    config: &ClaudeCodeTelemetryConfig,
    observability: &super::ObservabilityConfig,
) -> Vec<(&'static str, String)> {
    if !resolve_enabled(config) {
        return Vec::new();
    }
    let Some(endpoint) = resolve_endpoint(config, observability) else {
        let shared = super::ENDPOINT_ENV;
        log::warn!(
            "observability: {CONFIG_PATH}.enabled is true but no endpoint resolved \
             (set {CONFIG_PATH}.endpoint, observability.endpoint, ${ENDPOINT_ENV} or \
             ${shared}) — no Claude Code telemetry env injected"
        );
        return Vec::new();
    };
    if !super::endpoint_policy::valid_otlp_endpoint(&endpoint) {
        log::warn!(
            "observability: {CONFIG_PATH} endpoint {endpoint} is not a usable OTLP base URL \
             (use HTTP(S) without credentials, query or fragment) — no env injected"
        );
        return Vec::new();
    }
    if let Some(host) = super::endpoint_policy::reserved_placeholder_host(&endpoint) {
        log::warn!(
            "observability: {CONFIG_PATH} endpoint {endpoint} points at the reserved \
             placeholder domain {host} (RFC 2606/6761) — no env injected"
        );
        return Vec::new();
    }
    let mut pairs = vec![
        ("CLAUDE_CODE_ENABLE_TELEMETRY", "1".to_string()),
        ("CLAUDE_CODE_ENHANCED_TELEMETRY_BETA", "1".to_string()),
        ("OTEL_TRACES_EXPORTER", "otlp".to_string()),
        ("OTEL_EXPORTER_OTLP_PROTOCOL", resolve_protocol(config)),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
    ];
    if resolve_log_tool_details(config) {
        pairs.push(("OTEL_LOG_TOOL_DETAILS", "1".to_string()));
    }
    pairs
}

/// Apply the resolved injection to a child [`Command`].
///
/// Called from every owned dispatch surface immediately after
/// [`super::tracing::prepare_child`], whose standard `TRACEPARENT` is what
/// parents the child's own spans inside this execution's trace. Injecting
/// telemetry without that context is legal but yields orphan traces, so it is
/// warned about once per spawn rather than silently accepted.
///
/// Returns whether the telemetry env was injected.
pub fn prepare_child(command: &mut Command, root: &Path) -> bool {
    for name in MANAGED_CHILD_ENV {
        command.env_remove(name);
    }
    let config = read_config(root);
    if !resolve_enabled(&config) {
        return false;
    }
    let pairs = child_env(&config, &super::read_config(root));
    if pairs.is_empty() {
        return false;
    }
    if !super::tracing::enabled(root) {
        log::warn!(
            "observability: {CONFIG_PATH} is on but trace propagation is off, so the \
             worker's spans will be their own roots — enable observability.enabled with \
             an otlp exporter to parent them to this execution"
        );
    }
    for (name, value) in pairs {
        command.env(name, value);
    }
    true
}

/// The standard OTel resource-attribute variable [`prepare_scheduled_child`]
/// extends. Not in [`MANAGED_CHILD_ENV`]: Loom only *appends* to it, and only
/// when the opt-in injected, so an off path leaves inheritance untouched.
pub const RESOURCE_ATTRIBUTES_ENV: &str = "OTEL_RESOURCE_ATTRIBUTES";

/// [`prepare_child`] for a scheduled, non-sweep Claude session — a role-runner
/// tick or an epic-supervisor role dispatch (#10743). When the opt-in injects,
/// the session's resource is also stamped `loom.role=<role>` and, when known,
/// `loom.sweep_id=<execution>` (the id the tick's `loom.role_attempt` root
/// carries), appended to any inherited [`RESOURCE_ATTRIBUTES_ENV`] value, so a
/// tick's per-request LLM records can be grouped by role and tick.
pub fn prepare_scheduled_child(
    command: &mut Command,
    root: &Path,
    role: &str,
    execution: Option<&str>,
) -> bool {
    if !prepare_child(command, root) {
        return false;
    }
    let mut ours = format!("loom.role={}", encode_attribute_value(role));
    if let Some(execution) = execution {
        ours.push_str(",loom.sweep_id=");
        ours.push_str(&encode_attribute_value(execution));
    }
    let value = match env_nonempty(RESOURCE_ATTRIBUTES_ENV) {
        Some(inherited) => format!("{inherited},{ours}"),
        None => ours,
    };
    command.env(RESOURCE_ATTRIBUTES_ENV, value);
    true
}

/// Percent-encode every byte outside `[A-Za-z0-9._~:/-]`, so a value can never
/// break the `key=value,key=value` list (`,`, `=`, `%`, whitespace).
#[must_use]
pub fn encode_attribute_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"._~:/-".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests;
