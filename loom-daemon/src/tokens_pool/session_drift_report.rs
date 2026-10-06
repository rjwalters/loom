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
//! session-managed profile costs **zero** docker calls. The only I/O is one
//! bounded [`session_state::snapshot`] (a CLI process has no published
//! snapshot to reuse); [`report_from`] is pure over it. A snapshot docker
//! could not answer is reported as "not checked", never as drift.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::session_lifecycle::SESSION_MARKER_FILE;
use super::session_state::{
    self, mount_drift, workspace_label, MountDrift, Snapshot, CONTAINER_PREFIX,
};

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

/// The drifted host-mode session containers among `objects` (inspect
/// objects) against `registered`. Pure. Private-clone containers and those
/// without a workspace label get no verdict, so never appear.
#[must_use]
pub fn drifted<'a>(
    objects: impl IntoIterator<Item = &'a Value>,
    registered: &[PathBuf],
) -> Vec<DriftedSession> {
    objects
        .into_iter()
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
/// `private_clones` says whether the host has any private-clone session
/// container: only then is the warning not to recreate those this way printed.
#[must_use]
pub fn report_lines(drifted: &[DriftedSession], private_clones: bool) -> Vec<String> {
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
    if private_clones {
        lines.push(
            "  Private-clone accounts are not listed and must not be recreated this way: re-run \
             `accounts session start <account> --private-clone <URL> --base <BRANCH>` for those."
                .into(),
        );
    }
    lines
}

/// The report for one `snapshot` of the session containers. Pure.
#[must_use]
pub fn report_from(snapshot: &Snapshot, registered: &[PathBuf]) -> Vec<String> {
    match snapshot {
        Snapshot::Available(map) => report_lines(
            &drifted(map.values().map(|observed| &observed.inspect), registered),
            map.values()
                .any(|observed| session_state::is_private_clone(&observed.inspect)),
        ),
        Snapshot::Unavailable(reason) => vec![format!(
            "  Codex session containers were not checked for mount drift: docker could not be \
             queried ({reason}). Check them with `loom-daemon accounts session status <account>`."
        )],
    }
}

/// Everything above for one registry change. `profile_root` is the Codex
/// profile root (`None` when disabled). Zero docker calls unless a profile is
/// session-managed; otherwise one bounded snapshot.
#[must_use]
pub fn report(profile_root: Option<&Path>, docker: &str, registered: &[PathBuf]) -> Vec<String> {
    if profile_root
        .map(session_containers)
        .unwrap_or_default()
        .is_empty()
    {
        return Vec::new();
    }
    let snapshot = session_state::snapshot(docker, registered, session_state::SNAPSHOT_DEADLINE);
    report_from(&snapshot, registered)
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

    /// A fake docker that records each invocation, lists every session
    /// container for `ps` and prints `inspect` for `inspect`.
    fn fake_docker(dir: &Path, inspect: &str) -> (String, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let calls = dir.join("calls.log");
        let script = dir.join("docker");
        let payload = dir.join("inspect.json");
        std::fs::write(&payload, inspect).unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$1\" >> '{}'\ncase \"$1\" in\n  ps) printf \
                 'loom-codex-session-agent-1\\nloom-codex-session-agent-2\\n\
                 loom-codex-session-agent-3\\n' ;;\n  *) cat '{}' ;;\nesac\n",
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
    fn one_snapshot_reports_drifted_host_containers_only() {
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
        // One bounded snapshot: `ps` plus a single `inspect` of every container.
        assert_eq!(std::fs::read_to_string(&calls).unwrap(), "ps\ninspect\n");
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
        assert_eq!(found[0].account, "agent-1");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].drift.extra, vec![ws.join("gone")]);
        // No private-clone container on this host: no advice about them.
        let text = report_lines(&found, false).join("\n");
        assert!(text.contains("no longer registered"), "{text}");
        assert!(!text.contains("--private-clone"), "{text}");
        assert!(report_lines(&found, true)
            .join("\n")
            .contains("--private-clone"));
        assert!(report_lines(&[], true).is_empty(), "no drift, no report");
    }

    #[test]
    fn an_unavailable_docker_is_reported_as_unchecked_never_as_drift() {
        let (_tmp, root) = canonical_tempdir();
        let profiles = root.join("profiles");
        mark(&profiles, "1", "loom-codex-session-agent-1");
        for docker in [
            root.join("no-docker").display().to_string(),
            fake_docker(&root, "not json").0,
        ] {
            let lines = report(Some(&profiles), &docker, &[]);
            assert_eq!(lines.len(), 1, "{lines:?}");
            assert!(lines[0].contains("not checked for mount drift"), "{lines:?}");
        }
    }
}
