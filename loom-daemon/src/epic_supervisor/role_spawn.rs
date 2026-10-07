//! The epic supervisor's singleton role-process command (#10743), split out of
//! [`super::forge::SpawnDispatcher::dispatch_role`] so its environment can be
//! asserted without spawning anything.
use std::path::Path;
use std::process::{Command, Stdio};

use super::DispatchShape;

/// `spawn-claude.sh -p <prompt> --model <model> --dangerously-skip-permissions`
/// with null stdio, plus — opt-in, default-off — the Claude Code OTel env every
/// sweep child gets, stamped `loom.role=<role>` and a per-dispatch
/// `loom.sweep_id`. This path exports no trace context of its own, so with the
/// opt-in on the session's spans are their own roots (`prepare_child` warns).
pub(super) fn command(
    spawn_bin: &Path,
    repo_root: &Path,
    epic: u32,
    shape: &DispatchShape,
    model: &str,
) -> Command {
    let mut cmd = Command::new(spawn_bin);
    cmd.arg("-p")
        .arg(&shape.prompt)
        .arg("--model")
        .arg(model)
        .arg("--dangerously-skip-permissions")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let role = shape.role.to_ascii_lowercase();
    let execution =
        format!("epic-{epic}-{role}-{}", crate::telemetry::trace::instant(chrono::Utc::now()));
    crate::observability::claude_code_telemetry::prepare_scheduled_child(
        &mut cmd,
        repo_root,
        &role,
        Some(&execution),
    );
    // #9473: no admission here, so only `spawn-worker.sh` keeps the LLM-gateway
    // contract (it maps it per launch and scrubs it from every legacy adapter).
    crate::worker_spawn::llm_gateway::guard_dispatch(&mut cmd, spawn_bin, None);
    cmd
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::observability::claude_code_telemetry::{MANAGED_CHILD_ENV, RESOURCE_ATTRIBUTES_ENV};

    const KEYS: &[&str] = &[
        crate::observability::claude_code_telemetry::ENABLED_ENV,
        crate::observability::claude_code_telemetry::ENDPOINT_ENV,
        crate::observability::claude_code_telemetry::PROTOCOL_ENV,
        crate::observability::claude_code_telemetry::LOG_TOOL_DETAILS_ENV,
        crate::observability::ENABLED_ENV,
        crate::observability::ENDPOINT_ENV,
        RESOURCE_ATTRIBUTES_ENV,
        "CLAUDE_CODE_ENABLE_TELEMETRY",
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

    fn shape() -> DispatchShape {
        DispatchShape {
            role: "Architect",
            prompt: "/loom:architect".into(),
            produces_pr: false,
            creates_issues: true,
        }
    }

    fn env(cmd: &Command, name: &str) -> Option<Option<String>> {
        cmd.get_envs()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    }

    fn root(telemetry: bool) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
        std::fs::write(
            dir.path().join(".loom/config.json"),
            format!(
                r#"{{"observability":{{"endpoint":"http://127.0.0.1:4318","claudeCodeTelemetry":{{"enabled":{telemetry}}}}}}}"#
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    #[serial_test::serial]
    fn epic_role_command_carries_the_opt_in_telemetry_env() {
        let _guard = EnvGuard::clear();
        let dir = root(true);
        let cmd = command(Path::new("/bin/true"), dir.path(), 42, &shape(), "sonnet");
        assert_eq!(env(&cmd, "CLAUDE_CODE_ENABLE_TELEMETRY"), Some(Some("1".into())));
        assert_eq!(
            env(&cmd, "OTEL_EXPORTER_OTLP_ENDPOINT"),
            Some(Some("http://127.0.0.1:4318".into()))
        );
        let attrs = env(&cmd, RESOURCE_ATTRIBUTES_ENV).flatten().unwrap();
        assert!(
            attrs.starts_with("loom.role=architect,loom.sweep_id=epic-42-architect-"),
            "{attrs}"
        );
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "-p",
                "/loom:architect",
                "--model",
                "sonnet",
                "--dangerously-skip-permissions"
            ]
        );
    }

    #[test]
    #[serial_test::serial]
    fn epic_role_command_carries_no_managed_env_when_off() {
        let _guard = EnvGuard::clear();
        // Ambient values on the daemon must not leak through.
        std::env::set_var("CLAUDE_CODE_ENABLE_TELEMETRY", "1");
        let dir = root(false);
        let cmd = command(Path::new("/bin/true"), dir.path(), 42, &shape(), "sonnet");
        for name in MANAGED_CHILD_ENV {
            assert_eq!(env(&cmd, name), Some(None), "{name} is removed when off");
        }
        assert_eq!(env(&cmd, RESOURCE_ATTRIBUTES_ENV), None, "no attribute stamp when off");
    }

    /// #9473: the gateway contract survives only into `spawn-worker.sh`.
    #[test]
    fn epic_role_command_withholds_the_llm_gateway_contract_from_a_non_seam_bin() {
        let dir = root(false);
        let gateway = crate::worker_spawn::llm_gateway::ENV_NAMES;
        let direct = command(Path::new("/x/spawn-claude.sh"), dir.path(), 42, &shape(), "sonnet");
        for name in gateway {
            assert_eq!(env(&direct, name), Some(None), "{name} must be removed");
        }
        let seam = command(Path::new("/x/spawn-worker.sh"), dir.path(), 42, &shape(), "sonnet");
        for name in gateway {
            assert_eq!(env(&seam, name), None, "{name} is left to spawn-worker");
        }
    }
}
