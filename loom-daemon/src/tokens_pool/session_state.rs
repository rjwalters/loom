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
//! * `stale_mounts` — it is running but lacks the mount of a registered
//!   workspace root that lies under its own workspace label (a repository
//!   registered after the container was created, #10364).

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::session_lifecycle::WORKSPACE_LABEL;
use crate::session_exec::posture::mounted;

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
    let workspace = state["Config"]["Labels"][WORKSPACE_LABEL]
        .as_str()
        .filter(|w| !w.is_empty())
        .map(Path::new);
    let Some(workspace) = workspace else {
        return SessionState::Running;
    };
    let lacking = registered
        .iter()
        .filter(|root| root.starts_with(workspace))
        .any(|root| !mounted(state, &root.to_string_lossy()));
    if lacking {
        SessionState::StaleMounts
    } else {
        SessionState::Running
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

    fn inspect(running: bool, workspace: Option<&str>, mounts: &[&str]) -> Value {
        let mut labels = serde_json::Map::new();
        if let Some(w) = workspace {
            labels.insert(WORKSPACE_LABEL.into(), json!(w));
        }
        json!({
            "State": {"Running": running},
            "Config": {"Labels": labels},
            "Mounts": mounts.iter().map(|d| json!({"Destination": d})).collect::<Vec<_>>(),
        })
    }

    fn roots() -> Vec<PathBuf> {
        vec![
            PathBuf::from("/ws/a"),
            PathBuf::from("/ws/b"),
            PathBuf::from("/other/c"),
        ]
    }

    #[test]
    fn running_stopped_and_missing() {
        let up = inspect(true, Some("/ws"), &["/ws/a", "/ws/b"]);
        assert_eq!(classify_inspect(Some(&up), &roots()), SessionState::Running);
        let down = inspect(false, Some("/ws"), &["/ws/a", "/ws/b"]);
        assert_eq!(classify_inspect(Some(&down), &roots()), SessionState::Stopped);
        assert_eq!(classify_inspect(None, &roots()), SessionState::Missing);
    }

    #[test]
    fn a_registered_root_without_its_mount_is_stale() {
        let stale = inspect(true, Some("/ws"), &["/ws/a"]);
        assert_eq!(classify_inspect(Some(&stale), &roots()), SessionState::StaleMounts);
    }

    #[test]
    fn roots_outside_the_workspace_label_are_not_expected() {
        // `/other/c` is registered but was never this container's business.
        let up = inspect(true, Some("/ws"), &["/ws/a", "/ws/b"]);
        assert_eq!(classify_inspect(Some(&up), &roots()), SessionState::Running);
    }

    #[test]
    fn no_workspace_label_means_no_stale_verdict() {
        let up = inspect(true, None, &[]);
        assert_eq!(classify_inspect(Some(&up), &roots()), SessionState::Running);
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
