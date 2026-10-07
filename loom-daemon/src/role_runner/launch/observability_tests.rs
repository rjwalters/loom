//! #10743: a role tick's launch command carries the opt-in Claude Code OTel
//! env, after (and alongside) the tick's own `TRACEPARENT`.
#![allow(clippy::unwrap_used)]
use super::apply_role_observability;
use crate::observability::claude_code_telemetry::{MANAGED_CHILD_ENV, RESOURCE_ATTRIBUTES_ENV};
use crate::role_runner::RoleTickOutcome;
use std::path::Path;
use std::process::Command;

const KEYS: &[&str] = &[
    crate::observability::claude_code_telemetry::ENABLED_ENV,
    crate::observability::claude_code_telemetry::ENDPOINT_ENV,
    crate::observability::claude_code_telemetry::PROTOCOL_ENV,
    crate::observability::claude_code_telemetry::LOG_TOOL_DETAILS_ENV,
    crate::observability::ENABLED_ENV,
    crate::observability::ENDPOINT_ENV,
    crate::observability::EXPORTER_ENV,
    RESOURCE_ATTRIBUTES_ENV,
    "CLAUDE_CODE_ENABLE_TELEMETRY",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
];

struct EnvGuard(Vec<(&'static str, Option<std::ffi::OsString>)>);
impl EnvGuard {
    fn clear() -> Self {
        let previous = KEYS.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for key in KEYS {
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

/// A workspace with Loom's own OTLP tracing on (so the tick opens its
/// `loom.role_attempt` root) and the Claude Code opt-in set to `telemetry`.
fn root(telemetry: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        format!(
            r#"{{"observability":{{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318","claudeCodeTelemetry":{{"enabled":{telemetry}}}}}}}"#
        ),
    )
    .unwrap();
    dir
}

/// The launch command's explicit env, built inside a real role invocation.
fn launch_env(root: &Path, role: &str) -> Vec<(String, Option<String>)> {
    let mut env = Vec::new();
    let _ = crate::observability::lifecycle::role_invocation(root, role, || {
        let mut cmd = Command::new("/bin/true");
        apply_role_observability(&mut cmd, root, role);
        env = cmd
            .get_envs()
            .map(|(k, v)| {
                (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned()))
            })
            .collect();
        RoleTickOutcome::QueueEmpty
    });
    env
}

fn get<'a>(env: &'a [(String, Option<String>)], name: &str) -> Option<&'a Option<String>> {
    env.iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

#[test]
#[serial_test::serial]
fn role_launch_carries_telemetry_env_with_its_traceparent_when_on() {
    let _guard = EnvGuard::clear();
    let dir = root(true);
    let env = launch_env(dir.path(), "judge");
    assert_eq!(get(&env, "CLAUDE_CODE_ENABLE_TELEMETRY"), Some(&Some("1".into())));
    assert_eq!(
        get(&env, "OTEL_EXPORTER_OTLP_ENDPOINT"),
        Some(&Some("http://127.0.0.1:4318".into()))
    );
    if cfg!(feature = "otlp") {
        // Loom's own tracing is live, so the tick opened its root: the
        // session's spans are parented by the SAME context Loom journals.
        assert!(
            matches!(get(&env, "TRACEPARENT"), Some(Some(_)))
                && get(&env, "TRACEPARENT") == get(&env, "LOOM_TRACEPARENT"),
            "telemetry env never ships without the tick's trace context: {env:?}"
        );
    } else {
        // Tracing compiled out: no context, and no ambient one leaks in.
        assert_eq!(get(&env, "TRACEPARENT"), Some(&None), "{env:?}");
    }
    let attrs = get(&env, RESOURCE_ATTRIBUTES_ENV)
        .cloned()
        .flatten()
        .unwrap();
    assert!(attrs.starts_with("loom.role=judge,loom.sweep_id=role-judge-"), "{attrs}");
}

#[test]
#[serial_test::serial]
fn role_launch_carries_no_managed_env_when_off() {
    let _guard = EnvGuard::clear();
    // An ambient value on the daemon must not reach the child.
    std::env::set_var("CLAUDE_CODE_ENABLE_TELEMETRY", "1");
    std::env::set_var("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:4318");
    let dir = root(false);
    let env = launch_env(dir.path(), "curator");
    for name in MANAGED_CHILD_ENV {
        assert_eq!(get(&env, name), Some(&None), "{name} is removed when off");
    }
    assert_eq!(get(&env, RESOURCE_ATTRIBUTES_ENV), None, "no attribute stamp when off");
}
