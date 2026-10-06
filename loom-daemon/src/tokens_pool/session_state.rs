//! The state of every account's Codex session container, as the daemon sees
//! it (#10455; Epic #10452).
//!
//! The read behind "is this account's container usable". Today it is used by
//! the SigNoz gauge and WARN tracker (`observability::ops::codex_session`,
//! #10455), and its running rule by the spawn-time posture check
//! (`session_exec::posture::classify`) and `session-exec host`.
//!
//! It is built so the other readers can share it, but they do not yet: the
//! reconciler (#10453, `session_reconcile.rs`) inspects each account itself,
//! and liveness-aware selection (#10454, `session_lifecycle/liveness.rs`)
//! keeps its own cached `docker ps`. Moving both, and mount-drift detection
//! (#10364), onto [`snapshot`] / [`latest`] is a tracked follow-up, so there
//! are three docker read paths until then. It has two layers:
//!
//! * [`classify_inspect`] and [`container_running`] are **pure** over one
//!   `docker inspect` object, so every caller classifies the same way.
//! * [`snapshot`] is the **only I/O**: one bounded pass that reads every
//!   `loom-codex-session-*` container at once (`docker ps -a` plus a single
//!   `docker inspect` of the names it found, never one call per account) and
//!   returns a [`Snapshot`]. The daemon's watch loop publishes each snapshot
//!   with [`publish`]; [`latest`] hands it to readers that must not fork
//!   docker themselves (the dispatch-selection path).
//!
//! ## States
//!
//! * `running` — up, and mounts every registered workspace root under the
//!   workspace it was created for.
//! * `stopped` — it exists but `State.Running` is false.
//! * `restarting` — Docker reports `State.Restarting` (a crash loop backing off
//!   under `--restart unless-stopped`). Docker sets `Running=true` as well, so
//!   this is checked first; it is down, never `running` or `stale_mounts`.
//! * `missing` — a successful snapshot holds no container by that name.
//! * `stale_mounts` — running but lacks the mount of a registered workspace
//!   root under its own workspace label (registered after creation, #10364).
//!
//! ## "Not found" is not "could not ask"
//!
//! [`Snapshot::Unavailable`] means docker could not be queried at all: the CLI
//! is not on PATH, the Docker daemon is unreachable (Docker Desktop down), the
//! output did not parse, or the [`SNAPSHOT_DEADLINE`] expired (a wedged Docker
//! Desktop). It says **nothing** about any container. Every caller fails OPEN
//! on it: the visibility tracker holds its last state and emits no gauge;
//! #10454's selection must treat it as "cannot observe", i.e. Ungated, the
//! same rule `runtime_preference/availability.rs` applies to an unreadable
//! signal. Only an account absent from an [`Snapshot::Available`] map is
//! `missing`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::session_lifecycle::WORKSPACE_LABEL;
use crate::session_exec::posture::mounted;

/// The name prefix every session container carries
/// (`session_lifecycle::container_name`).
pub const CONTAINER_PREFIX: &str = "loom-codex-session-";

/// Hard deadline for one [`snapshot`] (both docker calls together). On expiry
/// the docker child is killed and the snapshot is [`Snapshot::Unavailable`].
pub const SNAPSHOT_DEADLINE: Duration = Duration::from_secs(8);

/// The recommended `max_age` ceiling for [`latest`]: two watch intervals
/// (the watch, `observability::ops::codex_session::WATCH_INTERVAL`, takes a
/// snapshot every 60 s). A reader on the dispatch-selection path (#10454)
/// should pass this, not a longer window; an older or absent snapshot is
/// "cannot observe" (Ungated).
pub const LATEST_MAX_AGE: Duration = Duration::from_secs(2 * 60);

/// What an account's session container is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SessionState {
    Running,
    Stopped,
    Restarting,
    Missing,
    StaleMounts,
}

impl SessionState {
    /// Every state, in the order the gauge emits them.
    pub const ALL: [Self; 5] = [
        Self::Running,
        Self::Stopped,
        Self::Restarting,
        Self::Missing,
        Self::StaleMounts,
    ];

    /// The closed `state` label vocabulary.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Restarting => "restarting",
            Self::Missing => "missing",
            Self::StaleMounts => "stale_mounts",
        }
    }

    /// Whether dispatch into the container can work at all.
    #[must_use]
    pub fn is_down(self) -> bool {
        matches!(self, Self::Stopped | Self::Restarting | Self::Missing)
    }
}

/// Whether one `docker inspect` object is a container that can take a
/// `docker exec`: `State.Running` and not `State.Restarting`. The single
/// definition shared by [`classify_inspect`] and the spawn-time posture check.
#[must_use]
pub fn container_running(state: &Value) -> bool {
    state["State"]["Running"] == Value::Bool(true)
        && state["State"]["Restarting"] != Value::Bool(true)
}

/// Classify one `docker inspect` object (`None`: no such container) against
/// the daemon's registered workspace roots.
#[must_use]
pub fn classify_inspect(state: Option<&Value>, registered: &[PathBuf]) -> SessionState {
    let Some(state) = state else {
        return SessionState::Missing;
    };
    if state["State"]["Restarting"] == Value::Bool(true) {
        return SessionState::Restarting;
    }
    if !container_running(state) {
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

/// One container in an available snapshot: its state against the roots the
/// snapshot was taken with, and the raw inspect object, so a caller that needs
/// more (mount drift, #10364) never asks docker again.
#[derive(Debug, Clone, PartialEq)]
pub struct Observed {
    pub state: SessionState,
    pub inspect: Value,
}

/// Every session container at one moment, or why docker could not say.
#[derive(Debug, Clone, PartialEq)]
pub enum Snapshot {
    /// Docker answered. Keyed by container name; a name not in the map is
    /// [`SessionState::Missing`].
    Available(BTreeMap<String, Observed>),
    /// Docker could not be queried; carries a short reason. Fail open.
    Unavailable(String),
}

impl Snapshot {
    /// `container`'s state, or `None` when the snapshot is unavailable
    /// ("cannot observe", not "missing").
    #[must_use]
    pub fn state_of(&self, container: &str) -> Option<SessionState> {
        match self {
            Self::Available(map) => Some(
                map.get(container)
                    .map_or(SessionState::Missing, |observed| observed.state),
            ),
            Self::Unavailable(_) => None,
        }
    }

    /// `container`'s raw inspect object, when the snapshot holds it.
    #[must_use]
    pub fn inspect_of(&self, container: &str) -> Option<&Value> {
        match self {
            Self::Available(map) => map.get(container).map(|observed| &observed.inspect),
            Self::Unavailable(_) => None,
        }
    }
}

/// Session-container names in `docker ps --format {{.Names}}` output (one
/// line per container, aliases comma-separated).
#[must_use]
pub fn parse_ps_names(stdout: &str) -> Vec<String> {
    let mut names: Vec<String> = stdout
        .lines()
        .flat_map(|line| line.split(','))
        .map(|name| name.trim().trim_start_matches('/'))
        .filter(|name| name.starts_with(CONTAINER_PREFIX))
        .map(str::to_string)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Parse `docker inspect` stdout (a JSON array) into the snapshot map, or
/// `None` when it is not a JSON array.
#[must_use]
pub fn parse_inspect_array(
    stdout: &[u8],
    registered: &[PathBuf],
) -> Option<BTreeMap<String, Observed>> {
    let parsed: Value = serde_json::from_slice(stdout).ok()?;
    let objects = parsed.as_array()?;
    Some(
        objects
            .iter()
            .filter_map(|object| {
                let name = object["Name"].as_str()?.trim_start_matches('/');
                Some((
                    name.to_string(),
                    Observed {
                        state: classify_inspect(Some(object), registered),
                        inspect: object.clone(),
                    },
                ))
            })
            .collect(),
    )
}

/// Read every session container: one `docker ps -a` and, if it found any, one
/// `docker inspect` of all of them, both inside `deadline`. Blocking; run it
/// off the async runtime (`spawn_blocking`).
#[must_use]
pub fn snapshot(docker: &str, registered: &[PathBuf], deadline: Duration) -> Snapshot {
    let until = Instant::now() + deadline;
    let ps = match run_bounded(
        docker,
        &[
            "ps",
            "-a",
            "--no-trunc",
            "--filter",
            &format!("name={CONTAINER_PREFIX}"),
            "--format",
            "{{.Names}}",
        ],
        until,
    ) {
        Ok(ran) if ran.success => ran,
        Ok(ran) => {
            return Snapshot::Unavailable(format!("docker ps failed: {}", first_line(&ran.stderr)))
        }
        Err(reason) => return Snapshot::Unavailable(reason),
    };
    let names = parse_ps_names(&String::from_utf8_lossy(&ps.stdout));
    if names.is_empty() {
        return Snapshot::Available(BTreeMap::new());
    }
    let mut args = vec!["inspect", "--type", "container", "--"];
    args.extend(names.iter().map(String::as_str));
    match run_bounded(docker, &args, until) {
        // A non-zero exit is only trusted when every error is "not found": a
        // container removed between the two calls, which then reads missing
        // while the rest stay valid. Any other failure (notably the Docker
        // daemon going away between the calls, where the CLI still prints
        // `[]` on stdout and exits 1) says nothing about any container.
        Ok(ran) if !ran.success && !only_not_found(&ran.stderr) => {
            Snapshot::Unavailable(format!("docker inspect failed: {}", first_line(&ran.stderr)))
        }
        Ok(ran) => parse_inspect_array(&ran.stdout, registered).map_or_else(
            || {
                Snapshot::Unavailable(format!(
                    "docker inspect output did not parse: {}",
                    first_line(&ran.stderr)
                ))
            },
            Snapshot::Available,
        ),
        Err(reason) => Snapshot::Unavailable(reason),
    }
}

/// Whether a failed `docker inspect`'s stderr reports only names that do not
/// exist (and reports something). Pure.
#[must_use]
pub fn only_not_found(stderr: &str) -> bool {
    let mut lines = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .peekable();
    lines.peek().is_some()
        && lines.all(|line| {
            let line = line.to_ascii_lowercase();
            line.starts_with("error: no such object")
                || line.starts_with("error: no such container")
                || line.starts_with("error response from daemon: no such container")
                || line.starts_with("error response from daemon: no such object")
        })
}

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or("")
}

struct Ran {
    success: bool,
    stdout: Vec<u8>,
    stderr: String,
}

/// Run `docker args…` through the shared bounded executor
/// (`proc_exec::run_bounded`): both pipes are drained concurrently, and at
/// `until` the child's whole process group is killed and reaped.
fn run_bounded(docker: &str, args: &[&str], until: Instant) -> Result<Ran, String> {
    use crate::proc_exec::{Completion, ExecError};
    let mut command = Command::new(docker);
    command.args(args).stdin(Stdio::null());
    let remaining = until.saturating_duration_since(Instant::now());
    match crate::proc_exec::run_bounded(command, remaining) {
        Ok(Completion::Exited(output)) => Ok(Ran {
            success: output.status.success(),
            stdout: output.stdout,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }),
        Ok(Completion::TimedOut { .. }) => {
            Err(format!("docker {} timed out", args.first().unwrap_or(&"")))
        }
        Err(ExecError::Spawn(error)) => Err(format!("could not run {docker}: {error}")),
        Err(error) => Err(format!("waiting on docker: {error}")),
    }
}

static LATEST: Mutex<Option<(Instant, Arc<Snapshot>)>> = Mutex::new(None);

/// Record `snapshot` as the newest one (the daemon's watch loop does this once
/// per pass). `started` is when the snapshot began, so [`latest`]'s age is
/// never understated by the time docker took to answer.
pub fn publish(snapshot: Arc<Snapshot>, started: Instant) {
    *LATEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((started, snapshot));
}

/// The newest published snapshot, if it is younger than `max_age`. `None`
/// (none yet, or too old) means "cannot observe", the same as
/// [`Snapshot::Unavailable`]. The cache is a single slot refreshed by the
/// watch loop; a reader that needs fresher data calls [`snapshot`] itself.
/// Use [`LATEST_MAX_AGE`] unless there is a reason to be stricter.
#[must_use]
pub fn latest(max_age: Duration) -> Option<Arc<Snapshot>> {
    let guard = LATEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .as_ref()
        .filter(|(at, _)| at.elapsed() <= max_age)
        .map(|(_, snapshot)| Arc::clone(snapshot))
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
    fn a_restarting_container_is_restarting_even_though_docker_says_running() {
        // `docker inspect` during a crash-loop back-off: Running AND Restarting.
        let mut looping = inspect(true, Some("/ws"), &["/ws/a"]);
        looping["State"]["Restarting"] = json!(true);
        assert_eq!(
            classify_inspect(Some(&looping), &roots()),
            SessionState::Restarting,
            "never running, and never stale_mounts despite the missing /ws/b mount"
        );
        assert!(!container_running(&looping));
        let mut settled = inspect(true, Some("/ws"), &["/ws/a", "/ws/b"]);
        settled["State"]["Restarting"] = json!(false);
        assert!(container_running(&settled));
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
    fn stopped_restarting_and_missing_are_down() {
        let down: Vec<_> = SessionState::ALL
            .into_iter()
            .filter(|s| s.is_down())
            .collect();
        assert_eq!(
            down,
            [
                SessionState::Stopped,
                SessionState::Restarting,
                SessionState::Missing
            ]
        );
    }

    #[test]
    fn ps_names_keep_only_session_containers() {
        let out = "loom-codex-session-b\nother,loom-codex-session-a\n/loom-codex-session-a\nweb\n";
        assert_eq!(parse_ps_names(out), ["loom-codex-session-a", "loom-codex-session-b"]);
    }

    #[test]
    fn inspect_array_is_keyed_by_name_and_classified() {
        let mut a = inspect(true, None, &[]);
        a["Name"] = json!("/loom-codex-session-a");
        let mut b = inspect(false, None, &[]);
        b["Name"] = json!("/loom-codex-session-b");
        let map = parse_inspect_array(&serde_json::to_vec(&json!([a, b])).unwrap(), &[]).unwrap();
        let snapshot = Snapshot::Available(map);
        assert_eq!(snapshot.state_of("loom-codex-session-a"), Some(SessionState::Running));
        assert_eq!(snapshot.state_of("loom-codex-session-b"), Some(SessionState::Stopped));
        assert_eq!(snapshot.state_of("loom-codex-session-c"), Some(SessionState::Missing));
        assert!(snapshot.inspect_of("loom-codex-session-a").is_some());
        assert!(parse_inspect_array(b"Error: No such object", &[]).is_none());
        let gone = Snapshot::Unavailable("docker down".into());
        assert_eq!(gone.state_of("loom-codex-session-a"), None);
    }

    #[test]
    fn only_not_found_errors_are_trusted() {
        assert!(only_not_found("Error: No such object: loom-codex-session-x\n"));
        assert!(only_not_found(
            "Error response from daemon: No such container: a\nError: No such object: b"
        ));
        assert!(!only_not_found(""));
        assert!(!only_not_found("failed to connect to the docker API at unix:///x"));
        assert!(!only_not_found("Error: No such object: a\nCannot connect to the Docker daemon"));
    }

    #[test]
    fn latest_honours_max_age() {
        publish(Arc::new(Snapshot::Available(BTreeMap::new())), Instant::now());
        assert!(latest(LATEST_MAX_AGE).is_some());
        std::thread::sleep(Duration::from_millis(20));
        assert!(latest(Duration::from_millis(1)).is_none(), "too old to use");
    }

    #[cfg(unix)]
    mod with_fake_docker {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn fake(dir: &Path, body: &str) -> String {
            let path = dir.join("docker");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        }

        #[test]
        fn one_ps_and_one_inspect_for_every_container() {
            let dir = tempfile::tempdir().unwrap();
            let calls = dir.path().join("calls");
            let docker = fake(
                dir.path(),
                &format!(
                    r#"echo "$1" >> '{calls}'
case "$1" in
  ps) printf 'loom-codex-session-a\nloom-codex-session-gone\n' ;;
  inspect) printf '[{{"Name":"/loom-codex-session-a","State":{{"Running":true,"Restarting":true}}}}]'; echo 'Error: No such object: loom-codex-session-gone' >&2; exit 1 ;;
esac"#,
                    calls = calls.display()
                ),
            );
            let snap = snapshot(&docker, &[], Duration::from_secs(5));
            assert_eq!(snap.state_of("loom-codex-session-a"), Some(SessionState::Restarting));
            assert_eq!(snap.state_of("loom-codex-session-gone"), Some(SessionState::Missing));
            assert_eq!(std::fs::read_to_string(calls).unwrap(), "ps\ninspect\n");
        }

        #[test]
        fn docker_lost_between_ps_and_inspect_is_unavailable_not_all_missing() {
            // The real CLI against an unreachable daemon: `[]` on stdout, a
            // connection error on stderr, exit 1.
            let dir = tempfile::tempdir().unwrap();
            let docker = fake(
                dir.path(),
                r#"case "$1" in
  ps) echo loom-codex-session-a ;;
  inspect) echo '[]'; echo 'failed to connect to the docker API at unix:///var/run/docker.sock' >&2; exit 1 ;;
esac"#,
            );
            let snap = snapshot(&docker, &[], Duration::from_secs(5));
            assert!(
                matches!(&snap, Snapshot::Unavailable(r) if r.contains("failed to connect")),
                "{snap:?}"
            );
            assert_eq!(snap.state_of("loom-codex-session-a"), None);
        }

        #[test]
        fn no_containers_means_no_inspect_and_an_empty_available_map() {
            let dir = tempfile::tempdir().unwrap();
            let docker = fake(dir.path(), r#"[ "$1" = ps ] || exit 9"#);
            let snap = snapshot(&docker, &[], Duration::from_secs(5));
            assert_eq!(snap, Snapshot::Available(BTreeMap::new()));
            assert_eq!(snap.state_of("loom-codex-session-a"), Some(SessionState::Missing));
        }

        #[test]
        fn an_unreachable_docker_daemon_is_unavailable_not_missing() {
            let dir = tempfile::tempdir().unwrap();
            let docker = fake(dir.path(), "echo 'Cannot connect to the Docker daemon' >&2; exit 1");
            let snap = snapshot(&docker, &[], Duration::from_secs(5));
            assert!(matches!(&snap, Snapshot::Unavailable(r) if r.contains("Cannot connect")));
            assert_eq!(snap.state_of("loom-codex-session-a"), None);
            let absent = snapshot("/nonexistent/docker", &[], Duration::from_secs(5));
            assert!(matches!(absent, Snapshot::Unavailable(_)));
        }

        #[test]
        fn a_wedged_docker_is_killed_at_the_deadline() {
            let dir = tempfile::tempdir().unwrap();
            let docker = fake(dir.path(), "exec sleep 30");
            let started = Instant::now();
            let snap = snapshot(&docker, &[], Duration::from_millis(300));
            assert!(matches!(&snap, Snapshot::Unavailable(r) if r.contains("timed out")));
            assert!(started.elapsed() < Duration::from_secs(5));
        }
    }
}
