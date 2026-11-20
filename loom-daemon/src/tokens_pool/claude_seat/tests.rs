use super::*;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Default)]
struct FakeRunner {
    containers: Mutex<HashMap<String, ContainerState>>,
    busy: Mutex<bool>,
    /// `(container, image, claude_home, workspace)` per create.
    creates: Mutex<Vec<(String, String, PathBuf, PathBuf)>>,
    starts: Mutex<Vec<String>>,
    stops: Mutex<Vec<String>>,
}

impl FakeRunner {
    fn seed(&self, container: &str, running: bool, workspace: &Path) {
        self.containers.lock().unwrap().insert(
            container.to_string(),
            ContainerState {
                id: format!("id-{container}"),
                running,
                restarting: false,
                started_at: Some("2026-11-19T00:00:00Z".into()),
                image: Some("img:seeded".into()),
                workspace: Some(workspace.to_path_buf()),
                extra_mounts: Vec::new(),
            },
        );
    }
}

impl session_image::BundleProbe for FakeRunner {}

impl ContainerRunner for FakeRunner {
    fn inspect(&self, container: &str) -> Result<Option<ContainerState>> {
        Ok(self.containers.lock().unwrap().get(container).cloned())
    }
    fn create(
        &self,
        _: &str,
        _: &str,
        _: &Path,
        _: &Path,
        _: &Path,
        _: &[ExtraMount],
    ) -> Result<()> {
        bail!("a Claude seat must never take the Codex create path")
    }
    fn create_claude_seat(
        &self,
        container: &str,
        image: &str,
        claude_home: &Path,
        workspace: &Path,
        _daemon_root: &Path,
        _extra_mounts: &[ExtraMount],
    ) -> Result<()> {
        self.creates.lock().unwrap().push((
            container.into(),
            image.into(),
            claude_home.into(),
            workspace.into(),
        ));
        self.containers.lock().unwrap().insert(
            container.to_string(),
            ContainerState {
                id: format!("id-{container}"),
                running: true,
                restarting: false,
                started_at: None,
                image: Some(image.to_string()),
                workspace: Some(workspace.to_path_buf()),
                extra_mounts: Vec::new(),
            },
        );
        Ok(())
    }
    fn start_existing(&self, container: &str) -> Result<()> {
        self.starts.lock().unwrap().push(container.into());
        if let Some(state) = self.containers.lock().unwrap().get_mut(container) {
            state.running = true;
        }
        Ok(())
    }
    fn has_active_exec(&self, _: &str) -> Result<bool> {
        Ok(*self.busy.lock().unwrap())
    }
    fn stop_and_remove(&self, container: &str, _: Duration) -> Result<()> {
        self.stops.lock().unwrap().push(container.into());
        self.containers.lock().unwrap().remove(container);
        Ok(())
    }
    fn attach_interactive(&self, _: &str, _: &str) -> Result<i32> {
        Ok(0)
    }
    fn exec_capture(
        &self,
        _: &str,
        _: &[&str],
        _: Duration,
    ) -> Result<super::super::session_lifecycle::ExecOutput> {
        bail!("unused")
    }
    fn window_exists(&self, _: &str, _: &str, _: &str) -> Result<bool> {
        Ok(false)
    }
    fn new_window(&self, _: &str, _: &str, _: &str, _: &Path, _: &[&str]) -> Result<()> {
        Ok(())
    }
    fn select_window(&self, _: &str, _: &str, _: &str) -> Result<()> {
        Ok(())
    }
}

/// A workspace whose per-repo token pool holds one `<name>.token` per name —
/// resolved without any environment variable, so these tests need no env guard.
fn fixture(names: &[&str]) -> (tempfile::TempDir, tempfile::TempDir) {
    let workspace = tempfile::tempdir().unwrap();
    let pool = workspace.path().join(".loom").join("tokens");
    std::fs::create_dir_all(&pool).unwrap();
    for name in names {
        std::fs::write(pool.join(format!("{name}.token")), "sk-ant-oat-FAKE-SECRET").unwrap();
    }
    (workspace, tempfile::tempdir().unwrap())
}

fn lifecycle(
    workspace: &Path,
    root: &Path,
    image: Option<&str>,
) -> ClaudeSeatLifecycle<FakeRunner> {
    ClaudeSeatLifecycle::new(workspace, root, FakeRunner::default(), image.map(str::to_string))
}

#[test]
fn container_name_is_the_claude_session_convention() {
    assert_eq!(container_name("alice"), "loom-claude-session-alice");
}

#[test]
fn two_seats_have_disjoint_state_directories() {
    let root = Path::new("/seats");
    let (a, b) = (home_dir(root, "alice"), home_dir(root, "bob"));
    assert_ne!(a, b);
    assert!(!a.starts_with(seat_dir(root, "bob")) && !b.starts_with(seat_dir(root, "alice")));
}

#[test]
fn run_args_mount_only_the_seat_home_and_never_the_token() {
    let args = claude_seat_run_args(
        "loom-claude-session-alice",
        "img:1",
        Path::new("/seats/alice/home"),
        Path::new("/ws"),
        &[PathBuf::from("/ws/repo")],
        &[PathBuf::from("/ws/repo/.loom/gh-config")],
        &[],
    );
    let joined = args.join("\n");
    assert!(joined.contains("/seats/alice/home:/home/loom/.claude"), "{joined}");
    assert!(joined.contains("CLAUDE_CONFIG_DIR=/home/loom/.claude"), "{joined}");
    assert!(joined.contains("loom.containment=claude-session"), "{joined}");
    assert!(joined.contains("loom.session-runtime=claude"), "{joined}");
    assert!(joined.contains("loom.workspace=/ws"), "{joined}");
    assert!(joined.contains("--cap-drop\nALL"), "{joined}");
    assert!(joined.contains("no-new-privileges"), "{joined}");
    // The token and its pool never reach the container at create time.
    for forbidden in [
        "CLAUDE_CODE_OAUTH_TOKEN",
        "sk-ant",
        ".loom/tokens",
        "docker.sock",
        "--privileged",
    ] {
        assert!(!joined.contains(forbidden), "{forbidden} in {joined}");
    }
    assert_eq!(args.last().map(String::as_str), Some("img:1"));
}

#[test]
fn start_registers_the_seat_and_launches_from_the_given_image() {
    let (workspace, root) = fixture(&["alice"]);
    let lifecycle = lifecycle(workspace.path(), root.path(), Some("img:configured"));
    assert!(!is_seat(root.path(), "alice"));
    let status = lifecycle.start("alice", None).unwrap();
    assert!(status.running && status.seat_registered, "{status:?}");
    assert_eq!(status.container_name, "loom-claude-session-alice");
    assert_eq!(status.image.as_deref(), Some("img:configured"));
    assert_eq!((status.runtime, status.containment), ("claude", "claude-session"));
    let creates = lifecycle.runner.creates.lock().unwrap();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].2, home_dir(root.path(), "alice"));
    assert!(home_dir(root.path(), "alice").is_dir());
    assert!(is_seat(root.path(), "alice"));
    // The marker sits beside the mounted home, not inside it.
    assert!(!home_dir(root.path(), "alice")
        .join(".session-managed.json")
        .exists());
}

#[test]
fn start_defaults_to_the_stock_session_image() {
    let (workspace, root) = fixture(&["alice"]);
    let lifecycle = lifecycle(workspace.path(), root.path(), None);
    lifecycle.start("alice", None).unwrap();
    assert_eq!(lifecycle.runner.creates.lock().unwrap()[0].1, DEFAULT_SESSION_IMAGE);
}

#[test]
fn two_seats_get_two_containers_with_disjoint_homes() {
    let (workspace, root) = fixture(&["alice", "bob"]);
    let lifecycle = lifecycle(workspace.path(), root.path(), None);
    lifecycle.start("alice", None).unwrap();
    lifecycle.start("bob", None).unwrap();
    let creates = lifecycle.runner.creates.lock().unwrap();
    assert_eq!(creates.len(), 2);
    assert_ne!(creates[0].0, creates[1].0);
    assert_ne!(creates[0].2, creates[1].2);
}

#[test]
fn start_rejects_an_account_that_is_not_in_the_pool() {
    let (workspace, root) = fixture(&["alice"]);
    let lifecycle = lifecycle(workspace.path(), root.path(), None);
    let error = lifecycle.start("mallory", None).unwrap_err().to_string();
    assert!(error.contains("not in the token pool"), "{error}");
    assert!(lifecycle.runner.creates.lock().unwrap().is_empty());
    assert!(!root.path().join("mallory").exists());
}

#[test]
fn restart_of_a_running_seat_is_idempotent() {
    let (workspace, root) = fixture(&["alice"]);
    let lifecycle = lifecycle(workspace.path(), root.path(), None);
    lifecycle.start("alice", None).unwrap();
    lifecycle.start("alice", None).unwrap();
    assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);
    assert!(lifecycle.runner.starts.lock().unwrap().is_empty());
}

#[test]
fn a_stopped_seat_is_resumed_not_recreated() {
    let (workspace, root) = fixture(&["alice"]);
    let lifecycle = lifecycle(workspace.path(), root.path(), None);
    lifecycle
        .runner
        .seed("loom-claude-session-alice", false, workspace.path());
    let status = lifecycle.start("alice", None).unwrap();
    assert!(status.running, "{status:?}");
    assert!(lifecycle.runner.creates.lock().unwrap().is_empty());
    assert_eq!(lifecycle.runner.starts.lock().unwrap().len(), 1);
}

#[test]
fn a_different_workspace_than_the_running_seat_is_refused() {
    let (workspace, root) = fixture(&["alice"]);
    let lifecycle = lifecycle(workspace.path(), root.path(), None);
    lifecycle
        .runner
        .seed("loom-claude-session-alice", true, Path::new("/elsewhere"));
    let error = lifecycle.start("alice", None).unwrap_err().to_string();
    assert!(error.contains("/elsewhere"), "{error}");
}

#[test]
fn stop_refuses_an_in_flight_exec_unless_forced_and_keeps_the_seat_registered() {
    let (workspace, root) = fixture(&["alice"]);
    let lifecycle = lifecycle(workspace.path(), root.path(), None);
    lifecycle.start("alice", None).unwrap();
    *lifecycle.runner.busy.lock().unwrap() = true;
    let error = lifecycle.stop("alice", false).unwrap_err().to_string();
    assert!(error.contains("in-flight"), "{error}");
    assert!(lifecycle.runner.stops.lock().unwrap().is_empty());
    let status = lifecycle.stop("alice", true).unwrap();
    assert!(!status.running && status.container_id.is_none(), "{status:?}");
    assert!(status.seat_registered && home_dir(root.path(), "alice").is_dir());
}

#[test]
fn a_runner_without_seat_support_refuses_rather_than_building_a_codex_container() {
    struct Plain;
    impl session_image::BundleProbe for Plain {}
    impl ContainerRunner for Plain {
        fn inspect(&self, _: &str) -> Result<Option<ContainerState>> {
            Ok(None)
        }
        fn create(
            &self,
            _: &str,
            _: &str,
            _: &Path,
            _: &Path,
            _: &Path,
            _: &[ExtraMount],
        ) -> Result<()> {
            Ok(())
        }
        fn start_existing(&self, _: &str) -> Result<()> {
            Ok(())
        }
        fn has_active_exec(&self, _: &str) -> Result<bool> {
            Ok(false)
        }
        fn stop_and_remove(&self, _: &str, _: Duration) -> Result<()> {
            Ok(())
        }
        fn attach_interactive(&self, _: &str, _: &str) -> Result<i32> {
            Ok(0)
        }
        fn exec_capture(
            &self,
            _: &str,
            _: &[&str],
            _: Duration,
        ) -> Result<super::super::session_lifecycle::ExecOutput> {
            bail!("unused")
        }
        fn window_exists(&self, _: &str, _: &str, _: &str) -> Result<bool> {
            Ok(false)
        }
        fn new_window(&self, _: &str, _: &str, _: &str, _: &Path, _: &[&str]) -> Result<()> {
            Ok(())
        }
        fn select_window(&self, _: &str, _: &str, _: &str) -> Result<()> {
            Ok(())
        }
    }
    let (workspace, root) = fixture(&["alice"]);
    let lifecycle = ClaudeSeatLifecycle::new(workspace.path(), root.path(), Plain, None);
    let error = lifecycle.start("alice", None).unwrap_err().to_string();
    assert!(error.contains("cannot create a Claude seat"), "{error}");
    assert!(!is_seat(root.path(), "alice"), "a failed create must not register the seat");
}
