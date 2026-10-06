//! The state of one account's Codex session container, as the daemon's
//! visibility surfaces see it (#10455; Epic #10452).
//!
//! One small read shared by the SigNoz gauge (`loom.codex_session.state`) and
//! the WARN-on-change tracker, so "is this account's container down" has one
//! definition. [`classify_inspect`] is pure over a `docker inspect` object;
//! [`read`] asks docker.
//!
//! * `running` — the container is up and mounts every registered workspace
//!   root under the workspace it was created for.
//! * `stopped` — it exists but `State.Running` is false.
//! * `missing` — no container by that name (or docker could not be asked).
//! * `stale_mounts` — it is running but its workspace mounts differ from
//!   what `session start` would mount today ([`mount_drift`], #10364): it
//!   lacks a registered root under its own workspace label (a repository
//!   registered after it was created), **or** it still mounts one that is no
//!   longer registered (a deregistered repository, which Codex can still
//!   write with its own sandbox off, #9979).
//!
//! A private-clone container (`loom.workspace-mode=private-clone`) mounts one
//! repository volume, not the registry, so it never gets a drift verdict.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::session_lifecycle::{
    profile_control_destination, workspace_mount_roots, CONTAINER_CODEX_HOME, PROFILE_CONTROLS,
    WORKSPACE_LABEL,
};

/// What an account's session container is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SessionState {
    Running,
    Stopped,
    Missing,
    StaleMounts,
}

impl SessionState {
    /// Every state, in the order the gauge emits them.
    pub const ALL: [Self; 4] = [
        Self::Running,
        Self::Stopped,
        Self::Missing,
        Self::StaleMounts,
    ];

    /// The closed `state` label vocabulary.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Missing => "missing",
            Self::StaleMounts => "stale_mounts",
        }
    }

    /// Whether dispatch into the container can work at all.
    #[must_use]
    pub fn is_down(self) -> bool {
        matches!(self, Self::Stopped | Self::Missing)
    }
}

/// The label a private-clone session container carries (#8787).
const WORKSPACE_MODE_LABEL: &str = "loom.workspace-mode";

/// Whether `state` is a private-clone session container. Those mount one
/// repository volume rather than the registry, so they get no drift verdict
/// and must never be recreated as host-mode.
#[must_use]
pub fn is_private_clone(state: &Value) -> bool {
    state["Config"]["Labels"][WORKSPACE_MODE_LABEL] == "private-clone"
}

/// The workspace a host-mode session container was created for (its
/// `loom.workspace` label), or `None` for a container without one.
#[must_use]
pub fn workspace_label(state: &Value) -> Option<&Path> {
    state["Config"]["Labels"][WORKSPACE_LABEL]
        .as_str()
        .filter(|w| !w.is_empty())
        .map(Path::new)
}

/// How a host-mode container's workspace mounts differ from what
/// `accounts session start --mount-workspace <its label>` would mount today.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountDrift {
    /// Registered roots under the label that no mount covers: dispatch into
    /// them fails with `chdir to cwd … no such file or directory`.
    pub missing: Vec<PathBuf>,
    /// Workspace mounts no intended root accounts for: a deregistered (or
    /// never-registered) directory Codex can still read and write with its
    /// own sandbox off. A containment gap, not just stale config.
    pub extra: Vec<PathBuf>,
}

impl MountDrift {
    /// No drift in either direction.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }
}

/// Whether a bind destination is one of the daemon's GitHub App token dirs
/// (`<root>/.loom/gh-config`, `<root>/.loom/gh-config-by-owner`), which
/// `session_lifecycle::gh_credential_dirs` binds read-only and which may lie
/// inside a mounted repository.
fn is_gh_credential_dir(destination: &Path) -> bool {
    destination
        .file_name()
        .is_some_and(|name| name == "gh-config" || name == "gh-config-by-owner")
        && destination
            .parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name == ".loom")
}

/// The container's workspace bind mounts: path-parity binds whose
/// destination lies under `label`. The profile mount, its read-only
/// [`PROFILE_CONTROLS`] binds and the gh credential dirs are never counted.
fn workspace_binds(state: &Value, label: &Path) -> Vec<PathBuf> {
    let controls: Vec<String> = PROFILE_CONTROLS
        .iter()
        .map(|name| profile_control_destination(name))
        .collect();
    let mut binds: Vec<PathBuf> = state["Mounts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        // `docker inspect` always reports `Type`; volumes and tmpfs are not
        // host directories, so only binds count.
        .filter(|m| m["Type"].as_str().is_none_or(|t| t == "bind"))
        .filter_map(|m| m["Destination"].as_str())
        .filter(|d| !d.is_empty() && !controls.iter().any(|c| c == d))
        .map(PathBuf::from)
        .filter(|d| !d.starts_with(CONTAINER_CODEX_HOME) && !is_gh_credential_dir(d))
        .filter(|d| d.starts_with(label))
        .collect();
    binds.sort();
    binds.dedup();
    binds
}

/// Drift between a host-mode session container's workspace mounts and the
/// mounts its `loom.workspace` label would get today against `registered`.
///
/// * **intended**: [`workspace_mount_roots`] of the label (a root nested in
///   another registered root is covered by its parent). When it refuses (no
///   registered root under the label any more), nothing is intended.
/// * **actual**: the container's workspace binds under the label (see
///   [`workspace_binds`]).
/// * `missing`: intended roots no actual bind covers (equal or an ancestor).
/// * `extra`: actual binds not inside any intended root (a deregistered repo,
///   or a whole parent mounted by a container older than #9979).
///
/// No verdict (empty) for a private-clone container or one without a
/// workspace label.
#[must_use]
pub fn mount_drift(state: &Value, registered: &[PathBuf]) -> MountDrift {
    if is_private_clone(state) {
        return MountDrift::default();
    }
    let Some(label) = workspace_label(state) else {
        return MountDrift::default();
    };
    let intended = workspace_mount_roots(label, registered).unwrap_or_default();
    // Mount destinations are canonical registry roots; compare against the
    // canonical label, whatever spelling `session start` was given.
    let canonical = crate::workspace_registry::normalize_path(label);
    let mut actual = workspace_binds(state, &canonical);
    if canonical != label {
        actual.extend(workspace_binds(state, label));
        actual.sort();
        actual.dedup();
    }
    MountDrift {
        missing: intended
            .iter()
            .filter(|root| !actual.iter().any(|bind| root.starts_with(bind)))
            .cloned()
            .collect(),
        extra: actual
            .iter()
            .filter(|bind| !intended.iter().any(|root| bind.starts_with(root)))
            .cloned()
            .collect(),
    }
}

/// Marker line `session-exec host` writes (to stderr and its capture file)
/// when it refuses a dispatch because the container does not mount the
/// workdir. `spawn-codex.sh` maps it, with exit 78, to `SESSION_MOUNT_STALE`.
pub const MOUNT_STALE_MARKER: &str = "# LOOM_SESSION_MOUNT_STALE";

/// Whether dispatching into `state` with `--workdir workdir` would fail
/// because no mount of the container covers the workdir (#10364): Docker's
/// `chdir to cwd … no such file or directory`. Never for a private-clone
/// container, whose workdir is its own repository volume.
#[must_use]
pub fn workdir_unmounted(state: &Value, workdir: &str) -> bool {
    !is_private_clone(state) && !crate::session_exec::posture::mounted(state, workdir)
}

/// Classify one `docker inspect` object (`None`: no such container) against
/// the daemon's registered workspace roots.
#[must_use]
pub fn classify_inspect(state: Option<&Value>, registered: &[PathBuf]) -> SessionState {
    let Some(state) = state else {
        return SessionState::Missing;
    };
    if state["State"]["Running"] != Value::Bool(true) {
        return SessionState::Stopped;
    }
    if mount_drift(state, registered).is_empty() {
        SessionState::Running
    } else {
        SessionState::StaleMounts
    }
}

/// Ask docker about `container`. A failure to ask at all reads as `Missing`:
/// from the daemon's side the container is not usable either way.
#[must_use]
pub fn read(docker: &str, container: &str, registered: &[PathBuf]) -> SessionState {
    let Ok(output) = Command::new(docker)
        .args(["inspect", container])
        .stdin(std::process::Stdio::null())
        .output()
    else {
        return SessionState::Missing;
    };
    if !output.status.success() {
        return SessionState::Missing;
    }
    let parsed: Option<Value> = serde_json::from_slice(&output.stdout).ok();
    let object = parsed.as_ref().and_then(|v| v.get(0));
    classify_inspect(object, registered)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A workspace parent with real repository dirs (`workspace_mount_roots`
    /// only mounts roots that exist), canonical so macOS `/var` vs
    /// `/private/var` cannot split label and mounts.
    struct Ws {
        _dir: tempfile::TempDir,
        root: PathBuf,
    }

    impl Ws {
        fn new(repos: &[&str]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(dir.path()).unwrap();
            for repo in repos {
                std::fs::create_dir_all(root.join(repo)).unwrap();
            }
            Self { _dir: dir, root }
        }

        fn p(&self, rel: &str) -> String {
            self.root.join(rel).display().to_string()
        }

        fn label(&self) -> String {
            self.root.display().to_string()
        }

        fn roots(&self, rels: &[&str]) -> Vec<PathBuf> {
            rels.iter().map(|r| self.root.join(r)).collect()
        }
    }

    fn bind(destination: &str) -> Value {
        json!({"Type": "bind", "Source": destination, "Destination": destination, "RW": true})
    }

    fn inspect_with(running: bool, labels: &[(&str, &str)], mounts: Vec<Value>) -> Value {
        let labels: serde_json::Map<String, Value> = labels
            .iter()
            .map(|(k, v)| ((*k).to_string(), json!(v)))
            .collect();
        json!({
            "State": {"Running": running},
            "Config": {"Labels": labels},
            "Mounts": mounts,
        })
    }

    fn inspect(running: bool, workspace: Option<&str>, mounts: &[String]) -> Value {
        let labels: Vec<(&str, &str)> = workspace
            .map(|w| vec![(WORKSPACE_LABEL, w)])
            .unwrap_or_default();
        inspect_with(running, &labels, mounts.iter().map(|d| bind(d)).collect())
    }

    #[test]
    fn running_stopped_and_missing() {
        let ws = Ws::new(&["a", "b"]);
        let roots = ws.roots(&["a", "b"]);
        let up = inspect(true, Some(&ws.label()), &[ws.p("a"), ws.p("b")]);
        assert_eq!(classify_inspect(Some(&up), &roots), SessionState::Running);
        let down = inspect(false, Some(&ws.label()), &[ws.p("a"), ws.p("b")]);
        assert_eq!(classify_inspect(Some(&down), &roots), SessionState::Stopped);
        assert_eq!(classify_inspect(None, &roots), SessionState::Missing);
    }

    #[test]
    fn missing_only() {
        let ws = Ws::new(&["a", "b"]);
        let state = inspect(true, Some(&ws.label()), &[ws.p("a")]);
        let drift = mount_drift(&state, &ws.roots(&["a", "b"]));
        assert_eq!(drift.missing, ws.roots(&["b"]));
        assert!(drift.extra.is_empty());
        assert_eq!(
            classify_inspect(Some(&state), &ws.roots(&["a", "b"])),
            SessionState::StaleMounts
        );
    }

    #[test]
    fn extra_only_a_deregistered_root_still_mounted() {
        let ws = Ws::new(&["a", "gone"]);
        let state = inspect(true, Some(&ws.label()), &[ws.p("a"), ws.p("gone")]);
        let drift = mount_drift(&state, &ws.roots(&["a"]));
        assert!(drift.missing.is_empty());
        assert_eq!(drift.extra, ws.roots(&["gone"]));
        assert_eq!(classify_inspect(Some(&state), &ws.roots(&["a"])), SessionState::StaleMounts);
    }

    #[test]
    fn every_root_deregistered_leaves_every_mount_extra() {
        let ws = Ws::new(&["a"]);
        let state = inspect(true, Some(&ws.label()), &[ws.p("a")]);
        let drift = mount_drift(&state, &[]);
        assert!(drift.missing.is_empty());
        assert_eq!(drift.extra, ws.roots(&["a"]));
    }

    #[test]
    fn missing_and_extra_together() {
        let ws = Ws::new(&["a", "new", "gone"]);
        let state = inspect(true, Some(&ws.label()), &[ws.p("a"), ws.p("gone")]);
        let drift = mount_drift(&state, &ws.roots(&["a", "new"]));
        assert_eq!(drift.missing, ws.roots(&["new"]));
        assert_eq!(drift.extra, ws.roots(&["gone"]));
    }

    #[test]
    fn a_nested_root_is_covered_by_its_parent() {
        let ws = Ws::new(&["a/inner", "b"]);
        let state = inspect(true, Some(&ws.label()), &[ws.p("a"), ws.p("b")]);
        let drift = mount_drift(&state, &ws.roots(&["a", "a/inner", "b"]));
        assert!(drift.is_empty(), "{drift:?}");
    }

    #[test]
    fn a_whole_parent_mount_is_extra_though_it_covers_everything() {
        // A container from before #9979 mounted `~/GitHub` whole.
        let ws = Ws::new(&["a"]);
        let state = inspect(true, Some(&ws.label()), &[ws.label()]);
        let drift = mount_drift(&state, &ws.roots(&["a"]));
        assert!(drift.missing.is_empty());
        assert_eq!(drift.extra, vec![ws.root.clone()]);
    }

    #[test]
    fn credential_and_profile_control_binds_are_never_counted() {
        let ws = Ws::new(&["a/.loom/gh-config", "a/.loom/gh-config-by-owner"]);
        let mut mounts = vec![
            bind(&ws.p("a")),
            // gh credential dirs, read-only, here inside the mounted repo …
            json!({"Type": "bind", "Destination": ws.p("a/.loom/gh-config"), "RW": false}),
            json!({"Type": "bind", "Destination": ws.p("a/.loom/gh-config-by-owner"), "RW": false}),
            // … and in a daemon root outside the workspace.
            json!({"Type": "bind", "Destination": "/opt/daemon/.loom/gh-config", "RW": false}),
            json!({"Type": "bind", "Destination": CONTAINER_CODEX_HOME, "RW": true}),
            json!({"Type": "volume", "Destination": ws.p("cache"), "RW": true}),
        ];
        for name in PROFILE_CONTROLS {
            mounts.push(json!({
                "Type": "bind",
                "Destination": profile_control_destination(name),
                "RW": false,
            }));
        }
        let label = ws.label();
        let state = inspect_with(true, &[(WORKSPACE_LABEL, &label)], mounts);
        let drift = mount_drift(&state, &ws.roots(&["a"]));
        assert!(drift.is_empty(), "{drift:?}");
    }

    #[test]
    fn roots_outside_the_workspace_label_are_not_expected() {
        let ws = Ws::new(&["a", "b"]);
        let other = Ws::new(&["c"]);
        let mut roots = ws.roots(&["a", "b"]);
        roots.extend(other.roots(&["c"]));
        let up = inspect(true, Some(&ws.label()), &[ws.p("a"), ws.p("b")]);
        assert!(mount_drift(&up, &roots).is_empty());
        assert_eq!(classify_inspect(Some(&up), &roots), SessionState::Running);
    }

    #[test]
    fn no_workspace_label_means_no_verdict() {
        let ws = Ws::new(&["a"]);
        let up = inspect(true, None, &[ws.p("gone")]);
        assert!(mount_drift(&up, &ws.roots(&["a"])).is_empty());
        assert_eq!(classify_inspect(Some(&up), &ws.roots(&["a"])), SessionState::Running);
    }

    #[test]
    fn a_private_clone_container_gets_no_verdict() {
        let ws = Ws::new(&["a"]);
        let label = ws.label();
        let state = inspect_with(
            true,
            &[
                (WORKSPACE_LABEL, &label),
                (WORKSPACE_MODE_LABEL, "private-clone"),
            ],
            vec![bind(&ws.p("stray"))],
        );
        assert!(is_private_clone(&state));
        assert!(mount_drift(&state, &ws.roots(&["a"])).is_empty());
        assert_eq!(classify_inspect(Some(&state), &ws.roots(&["a"])), SessionState::Running);
    }

    #[test]
    fn a_workdir_is_unmounted_only_when_no_mount_covers_it() {
        let ws = Ws::new(&["a", "new"]);
        let state = inspect(true, Some(&ws.label()), &[ws.p("a")]);
        assert!(!workdir_unmounted(&state, &ws.p("a")));
        assert!(!workdir_unmounted(&state, &ws.p("a/.loom/worktrees/issue-1")));
        assert!(workdir_unmounted(&state, &ws.p("new")));
        // `/ws/ab` is not inside `/ws/a`: path components, not string prefix.
        assert!(workdir_unmounted(&state, &format!("{}b", ws.p("a"))));
        let label = ws.label();
        let private = inspect_with(
            true,
            &[
                (WORKSPACE_LABEL, &label),
                (WORKSPACE_MODE_LABEL, "private-clone"),
            ],
            vec![],
        );
        assert!(!workdir_unmounted(&private, "/workspace/repo"));
    }

    #[test]
    fn only_stopped_and_missing_are_down() {
        let down: Vec<_> = SessionState::ALL
            .into_iter()
            .filter(|s| s.is_down())
            .collect();
        assert_eq!(down, [SessionState::Stopped, SessionState::Missing]);
    }
}
