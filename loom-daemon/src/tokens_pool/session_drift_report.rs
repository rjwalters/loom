//! Which host-mode Codex session containers a `workspace add` / `workspace
//! remove` just left with drifted mounts (#10364).
//!
//! A host-mode session container's workspace mounts are fixed when it is
//! created, so a registry change never reaches a running container: a newly
//! registered repo is unreachable from it (every Codex tick there fails), and
//! a deregistered one stays mounted read-write (a containment gap, since Codex
//! runs with its own sandbox off, #9979). Until the reconciler recreates idle
//! drifted containers itself (#10364 Part B, after #10453), the registry
//! command names them and prints the manual recreate.
//!
//! Best-effort by contract: nothing here returns an error, and a host with no
//! session-managed profile costs **zero** docker calls. The only docker call
//! is [`inspect_containers`]; everything else is pure over its inspect
//! objects, so it can be swapped for the daemon's container snapshot.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::session_lifecycle::SESSION_MARKER_FILE;
use super::session_state::{mount_drift, workspace_label, MountDrift};

/// Upper bound on the one `docker inspect`: a wedged engine must not hang
/// `workspace add`.
const INSPECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Session container name prefix (`session_lifecycle::container_name`).
const CONTAINER_PREFIX: &str = "loom-codex-session-";

/// One host-mode session container whose mounts no longer match the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftedSession {
    pub container: String,
    pub account: String,
    /// Its `loom.workspace` label: the `--mount-workspace` to recreate with.
    pub workspace: PathBuf,
    pub drift: MountDrift,
}

/// The session containers this host's session-managed Codex profiles were
/// adopted for, from each profile's `.session-managed.json` marker under
/// `profile_root`. Filesystem only; empty when there are none.
#[must_use]
pub fn session_containers(profile_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(profile_root) else {
        return Vec::new();
    };
    let mut containers: Vec<String> = entries
        .flatten()
        .filter_map(|entry| std::fs::read(entry.path().join(SESSION_MARKER_FILE)).ok())
        .filter_map(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .filter_map(|marker| marker["container_name"].as_str().map(str::to_string))
        .filter(|name| name.starts_with(CONTAINER_PREFIX))
        .collect();
    containers.sort();
    containers.dedup();
    containers
}

/// The one docker call: `docker inspect` of every container in `containers`
/// at once. Missing containers are simply absent from the result (docker
/// still prints the ones it found); any failure to ask yields nothing.
#[must_use]
pub fn inspect_containers(docker: &str, containers: &[String]) -> Vec<Value> {
    if containers.is_empty() {
        return Vec::new();
    }
    let Ok(mut child) = Command::new(docker)
        .args(["inspect", "--type", "container"])
        .args(containers)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Vec::new();
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Vec::new();
    };
    // Read on a thread so a large inspect cannot fill the pipe and stall the
    // child while the deadline loop waits on it.
    let reader = std::thread::spawn(move || {
        let mut text = Vec::new();
        let _ = stdout.read_to_end(&mut text);
        text
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() < INSPECT_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Vec::new();
            }
        }
    }
    let text = reader.join().unwrap_or_default();
    match serde_json::from_slice::<Value>(&text) {
        Ok(Value::Array(objects)) => objects,
        _ => Vec::new(),
    }
}

/// The drifted host-mode session containers among `objects` (inspect
/// objects) against `registered`. Pure. Private-clone containers and those
/// without a workspace label get no verdict, so never appear.
#[must_use]
pub fn drifted(objects: &[Value], registered: &[PathBuf]) -> Vec<DriftedSession> {
    objects
        .iter()
        .filter_map(|state| {
            let container = state["Name"].as_str()?.trim_start_matches('/').to_string();
            let account = container.strip_prefix(CONTAINER_PREFIX)?.to_string();
            let workspace = workspace_label(state)?.to_path_buf();
            let drift = mount_drift(state, registered);
            (!drift.is_empty()).then_some(DriftedSession {
                container,
                account,
                workspace,
                drift,
            })
        })
        .collect()
}

/// Operator-facing lines for `drifted`; empty when nothing drifted. Pure.
#[must_use]
pub fn report_lines(drifted: &[DriftedSession]) -> Vec<String> {
    if drifted.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![format!(
        "  {} Codex session container(s) no longer match the workspace registry; each must be \
         recreated when idle (#10364):",
        drifted.len()
    )];
    for session in drifted {
        lines.push(format!("    {} (account {})", session.container, session.account));
        for path in &session.drift.extra {
            lines.push(format!(
                "      still mounts {} read-write, which is no longer registered (Codex runs \
                 there with its own sandbox off)",
                path.display()
            ));
        }
        for path in &session.drift.missing {
            lines.push(format!(
                "      does not mount {}: Codex ticks there fail until it is recreated",
                path.display()
            ));
        }
    }
    lines.push(
        "  Until the daemon recreates these itself, run for each one once it has no tick in \
         flight (`stop` refuses a busy container):"
            .into(),
    );
    for session in drifted {
        lines.push(format!(
            "    loom-daemon accounts session stop {0} && loom-daemon accounts session start {0} \
             --mount-workspace {1}",
            session.account,
            session.workspace.display()
        ));
    }
    lines.push(
        "  Private-clone accounts are not listed and must not be recreated this way: re-run \
         `accounts session start <account> --private-clone <URL> --base <BRANCH>` for those."
            .into(),
    );
    lines
}

/// Everything above for one registry change. `profile_root` is the Codex
/// profile root (`None` when disabled). Zero docker calls unless a profile is
/// session-managed.
#[must_use]
pub fn report(profile_root: Option<&Path>, docker: &str, registered: &[PathBuf]) -> Vec<String> {
    let containers = profile_root.map(session_containers).unwrap_or_default();
    if containers.is_empty() {
        return Vec::new();
    }
    report_lines(&drifted(&inspect_containers(docker, &containers), registered))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tokens_pool::session_lifecycle::WORKSPACE_LABEL;
    use serde_json::json;

    fn canonical_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    fn container(name: &str, labels: Value, mounts: &[PathBuf]) -> Value {
        json!({
            "Name": format!("/{name}"),
            "State": {"Running": true},
            "Config": {"Labels": labels},
            "Mounts": mounts.iter().map(|m| json!({
                "Type": "bind", "Destination": m.display().to_string(), "RW": true,
            })).collect::<Vec<_>>(),
        })
    }

    fn mark(profiles: &Path, profile: &str, container: &str) {
        let dir = profiles.join(profile);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SESSION_MARKER_FILE),
            format!(r#"{{"schema_version":1,"container_name":"{container}","adopted_at_unix":0}}"#),
        )
        .unwrap();
    }

    /// A fake docker that records each invocation and prints `inspect`.
    fn fake_docker(dir: &Path, inspect: &str) -> (String, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let calls = dir.join("calls.log");
        let script = dir.join("docker");
        let payload = dir.join("inspect.json");
        std::fs::write(&payload, inspect).unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncat '{}'\n",
                calls.display(),
                payload.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        (script.display().to_string(), calls)
    }

    #[test]
    fn no_session_managed_profile_means_zero_docker_calls() {
        let (_tmp, root) = canonical_tempdir();
        let profiles = root.join("profiles");
        std::fs::create_dir_all(profiles.join("plain")).unwrap();
        let (docker, calls) = fake_docker(&root, "[]");
        assert!(report(Some(&profiles), &docker, &[]).is_empty());
        assert!(report(None, &docker, &[]).is_empty());
        assert!(!calls.exists(), "docker must not be called");
    }

    #[test]
    fn markers_name_the_containers() {
        let (_tmp, root) = canonical_tempdir();
        mark(&root, "a", "loom-codex-session-agent-1");
        mark(&root, "b", "loom-codex-session-agent-2");
        mark(&root, "c", "not-a-session-container");
        std::fs::create_dir_all(root.join("unmarked")).unwrap();
        assert_eq!(
            session_containers(&root),
            ["loom-codex-session-agent-1", "loom-codex-session-agent-2"]
        );
    }

    #[test]
    fn one_docker_call_reports_drifted_host_containers_only() {
        let (_tmp, root) = canonical_tempdir();
        let ws = root.join("ws");
        for repo in ["a", "new"] {
            std::fs::create_dir_all(ws.join(repo)).unwrap();
        }
        let profiles = root.join("profiles");
        for n in ["1", "2", "3"] {
            mark(&profiles, n, &format!("loom-codex-session-agent-{n}"));
        }
        let label = ws.display().to_string();
        let objects = json!([
            // agent-1: created before `new` was registered.
            container(
                "loom-codex-session-agent-1",
                json!({WORKSPACE_LABEL: label}),
                &[ws.join("a")]
            ),
            // agent-2: current.
            container(
                "loom-codex-session-agent-2",
                json!({WORKSPACE_LABEL: label}),
                &[ws.join("a"), ws.join("new")],
            ),
            // agent-3: private clone, never a verdict.
            container(
                "loom-codex-session-agent-3",
                json!({WORKSPACE_LABEL: "/workspace/repo", "loom.workspace-mode": "private-clone"}),
                &[],
            ),
        ]);
        let (docker, calls) = fake_docker(&root, &objects.to_string());
        let registered = vec![ws.join("a"), ws.join("new")];
        let lines = report(Some(&profiles), &docker, &registered);
        let text = lines.join("\n");
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        assert!(text.contains("loom-codex-session-agent-1"), "{text}");
        assert!(!text.contains("loom-codex-session-agent-2"), "{text}");
        assert!(!text.contains("loom-codex-session-agent-3"), "{text}");
        assert!(text.contains(&format!("does not mount {}", ws.join("new").display())));
        assert!(text.contains(&format!(
            "accounts session stop agent-1 && loom-daemon accounts session start agent-1 \
             --mount-workspace {label}"
        )));
        assert!(text.contains("--private-clone"), "{text}");
    }

    #[test]
    fn a_deregistered_root_is_reported_as_still_mounted() {
        let (_tmp, root) = canonical_tempdir();
        let ws = root.join("ws");
        for repo in ["a", "gone"] {
            std::fs::create_dir_all(ws.join(repo)).unwrap();
        }
        let label = ws.display().to_string();
        let objects = [container(
            "loom-codex-session-agent-1",
            json!({WORKSPACE_LABEL: label}),
            &[ws.join("a"), ws.join("gone")],
        )];
        let found = drifted(&objects, &[ws.join("a")]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].drift.extra, vec![ws.join("gone")]);
        let text = report_lines(&found).join("\n");
        assert!(text.contains("no longer registered"), "{text}");
    }

    #[test]
    fn a_failing_or_garbled_docker_is_best_effort() {
        let (_tmp, root) = canonical_tempdir();
        let profiles = root.join("profiles");
        mark(&profiles, "1", "loom-codex-session-agent-1");
        assert!(
            report(Some(&profiles), &root.join("no-docker").display().to_string(), &[]).is_empty()
        );
        let (docker, _calls) = fake_docker(&root, "not json");
        assert!(report(Some(&profiles), &docker, &[]).is_empty());
    }
}
