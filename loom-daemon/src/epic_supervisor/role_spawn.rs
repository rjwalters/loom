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

/// Whether the supervisor serving `root` must skip this tick because the
/// workspace is held (#10719). Logged each tick it is, like the supervisor's
/// main-health and drain skips. Here, not in `tick`, because
/// `epic_supervisor.rs` is frozen by the file-size ratchet.
pub(super) fn tick_held(root: Option<&Path>) -> bool {
    let Some((root, hold)) = root.and_then(|r| Some((r, crate::workspace_hold::hold_for(r)?)))
    else {
        return false;
    };
    log::warn!(
        "epic_supervisor: workspace {} is held ({}, {} copy): {}; skipping tick (no epic \
         dispatch) (#10719)",
        root.display(),
        hold.kind.as_str(),
        hold.copy.as_str(),
        hold.detail
    );
    true
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

    /// #10719: the singleton role dispatch runs this checkout's own spawn
    /// script and never reaches the registry, so it refuses a held workspace
    /// itself, with the same typed error, and spawns nothing. A sibling
    /// workspace on the same host is not affected.
    #[test]
    #[serial_test::serial]
    fn a_held_workspace_refuses_the_epic_role_dispatch_and_its_sibling_does_not() {
        use super::super::{forge::SpawnDispatcher, EpicDispatcher};
        use crate::workspace_hold::{
            set_for_test, HeldCopy, HoldKind, WorkspaceHeldDispatchError, WorkspaceHold,
        };
        let _guard = EnvGuard::clear();
        let dispatcher = |dir: &Path| {
            let (registry, log) = crate::sweep_registry::test_support::fixture_registry(dir);
            let bin = dir.join(".loom/scripts/spawn-claude.sh");
            let registry = std::sync::Arc::new(std::sync::Mutex::new(registry));
            (SpawnDispatcher::new(bin, registry), log)
        };
        let (held_dir, free_dir) = (root(false), root(false));
        let (mut held, held_log) = dispatcher(held_dir.path());
        let (mut free, free_log) = dispatcher(free_dir.path());
        set_for_test(
            held_dir.path(),
            Some(WorkspaceHold {
                kind: HoldKind::InstallIncompatible,
                copy: HeldCopy::Checkout,
                since: chrono::Utc::now(),
                detail: "installed 0.19.800 is too old for daemon 0.19.900".into(),
                verdict_at: chrono::Utc::now(),
            }),
        );

        let err = held.dispatch_role(42, &shape()).expect_err("held");
        let typed = err
            .downcast_ref::<WorkspaceHeldDispatchError>()
            .expect("the typed refusal the registry guard returns");
        assert_eq!(typed.kind, HoldKind::InstallIncompatible);
        assert!(err.to_string().contains("install-incompatible"), "{err}");
        assert!(!held_log.exists(), "the held checkout's spawn script never ran");

        free.dispatch_role(42, &shape())
            .expect("the sibling dispatches");
        assert!(free_log.exists(), "the sibling's spawn script ran");

        // The hold lifts: the same workspace dispatches again.
        set_for_test(held_dir.path(), None);
        held.dispatch_role(42, &shape())
            .expect("dispatches once the hold clears");
        assert!(held_log.exists());
    }

    /// #10719: the supervisor's whole tick is skipped for a held workspace,
    /// over the real spawn dispatcher: a flat epic would start an Architect
    /// through the held checkout's own spawn script. A second supervisor on
    /// the same host, for an unheld workspace, still dispatches.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_held_workspace_skips_the_supervisor_tick_and_its_sibling_does_not() {
        use super::super::{
            forge::SpawnDispatcher, EpicSnapshot, EpicSource, EpicSupervisor, IssueCreationMutex,
        };
        use crate::workspace_hold::{set_for_test, HeldCopy, HoldKind, WorkspaceHold};
        struct OneFlatEpic;
        impl EpicSource for OneFlatEpic {
            fn list_open_epics(&mut self) -> anyhow::Result<Vec<EpicSnapshot>> {
                Ok(vec![EpicSnapshot::new(1, "flat body", vec![], vec![])])
            }
        }
        let _guard = EnvGuard::clear();
        let supervisor = |dir: &Path| {
            let (registry, log) = crate::sweep_registry::test_support::fixture_registry(dir);
            let bin = dir.join(".loom/scripts/spawn-claude.sh");
            let registry = std::sync::Arc::new(std::sync::Mutex::new(registry));
            let dispatcher = SpawnDispatcher::new(bin, registry);
            let supervisor =
                EpicSupervisor::new(OneFlatEpic, dispatcher, IssueCreationMutex::new())
                    .with_hold_root(dir.to_path_buf());
            (supervisor, log)
        };
        let (held_dir, free_dir) = (root(false), root(false));
        let (mut held, held_log) = supervisor(held_dir.path());
        let (mut free, free_log) = supervisor(free_dir.path());
        set_for_test(
            held_dir.path(),
            Some(WorkspaceHold {
                kind: HoldKind::DaemonTooOld,
                copy: HeldCopy::Checkout,
                since: chrono::Utc::now(),
                detail: "requires daemon 0.19.950 > running 0.19.900".into(),
                verdict_at: chrono::Utc::now(),
            }),
        );

        let report = held.tick().await.unwrap();
        assert!(report.halted, "a hold halts the tick");
        assert_eq!((report.epics_seen, report.roles_dispatched), (0, 0), "before the forge list");
        assert!(!held_log.exists(), "the held checkout's spawn script never ran");

        let report = free.tick().await.unwrap();
        assert!(!report.halted);
        assert_eq!(report.roles_dispatched, 1, "the sibling's epic still advances");
        assert!(free_log.exists());

        // The hold lifts: the next tick dispatches.
        set_for_test(held_dir.path(), None);
        let report = held.tick().await.unwrap();
        assert_eq!((report.halted, report.roles_dispatched), (false, 1));
        assert!(held_log.exists());
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
