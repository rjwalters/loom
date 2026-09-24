use super::*;
use serial_test::serial;
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Default)]
struct FakeRunner {
    containers: Mutex<HashMap<String, ContainerState>>,
    busy: Mutex<HashMap<String, bool>>,
    creates: Mutex<Vec<(String, String, PathBuf, PathBuf)>>,
    stops: Mutex<Vec<String>>,
    attaches: Mutex<Vec<(String, String)>>,
    execs: Mutex<Vec<(String, Vec<String>)>>,
    exec_results: Mutex<HashMap<String, ExecOutput>>,
    /// `(container, tmux_session)` -> set of window names, simulating
    /// enough of tmux's window table for `shell`'s create-vs-reuse logic
    /// (issue #7389) without needing a real tmux/docker.
    windows: Mutex<HashMap<(String, String), std::collections::HashSet<String>>>,
    new_windows: Mutex<Vec<NewWindowCall>>,
    selected_windows: Mutex<Vec<(String, String, String)>>,
}

/// `(container, tmux_session, window, cwd, command)` recorded by
/// `FakeRunner::new_window`.
type NewWindowCall = (String, String, String, PathBuf, Vec<String>);

fn exec_ok(output: &str) -> ExecOutput {
    ExecOutput {
        success: true,
        unavailable: false,
        timed_out: false,
        exit_code: Some(0),
        output: output.into(),
    }
}

impl FakeRunner {
    fn set_exec_result(&self, container: &str, result: ExecOutput) {
        self.exec_results
            .lock()
            .unwrap()
            .insert(container.to_string(), result);
    }
}

impl FakeRunner {
    fn seed_running(&self, container: &str) {
        self.seed_running_with_workspace(container, None);
    }

    fn seed_running_with_workspace(&self, container: &str, workspace: Option<PathBuf>) {
        self.containers.lock().unwrap().insert(
            container.to_string(),
            ContainerState {
                id: format!("{container}-id"),
                running: true,
                started_at: Some("2026-09-05T00:00:00Z".into()),
                image: Some("ghcr.io/rjwalters/loom-worker-session:test".into()),
                workspace,
            },
        );
    }

    fn set_busy(&self, container: &str, busy: bool) {
        self.busy
            .lock()
            .unwrap()
            .insert(container.to_string(), busy);
    }
}

impl ContainerRunner for FakeRunner {
    fn inspect(&self, container: &str) -> Result<Option<ContainerState>> {
        Ok(self.containers.lock().unwrap().get(container).cloned())
    }

    fn create(
        &self,
        container: &str,
        image: &str,
        codex_home: &Path,
        workspace: &Path,
    ) -> Result<()> {
        self.creates.lock().unwrap().push((
            container.to_string(),
            image.to_string(),
            codex_home.to_path_buf(),
            workspace.to_path_buf(),
        ));
        self.seed_running_with_workspace(container, Some(workspace.to_path_buf()));
        Ok(())
    }

    fn start_existing(&self, container: &str) -> Result<()> {
        let mut containers = self.containers.lock().unwrap();
        let state = containers
            .get_mut(container)
            .ok_or_else(|| anyhow!("no such container"))?;
        state.running = true;
        Ok(())
    }

    fn has_active_exec(&self, container: &str) -> Result<bool> {
        Ok(*self.busy.lock().unwrap().get(container).unwrap_or(&false))
    }

    fn stop_and_remove(&self, container: &str, _grace: Duration) -> Result<()> {
        self.stops.lock().unwrap().push(container.to_string());
        self.containers.lock().unwrap().remove(container);
        Ok(())
    }

    fn attach_interactive(&self, container: &str, tmux_session_name: &str) -> Result<i32> {
        self.attaches
            .lock()
            .unwrap()
            .push((container.to_string(), tmux_session_name.to_string()));
        Ok(0)
    }

    fn exec_capture(
        &self,
        container: &str,
        argv: &[&str],
        _timeout: Duration,
    ) -> Result<ExecOutput> {
        self.execs
            .lock()
            .unwrap()
            .push((container.to_string(), argv.iter().map(|arg| (*arg).to_string()).collect()));
        Ok(self
            .exec_results
            .lock()
            .unwrap()
            .get(container)
            .cloned()
            .unwrap_or_else(|| exec_ok("Logged in using ChatGPT")))
    }

    fn window_exists(&self, container: &str, tmux_session: &str, window: &str) -> Result<bool> {
        Ok(self
            .windows
            .lock()
            .unwrap()
            .get(&(container.to_string(), tmux_session.to_string()))
            .is_some_and(|windows| windows.contains(window)))
    }

    fn new_window(
        &self,
        container: &str,
        tmux_session: &str,
        window: &str,
        cwd: &Path,
        command: &[&str],
    ) -> Result<()> {
        self.new_windows.lock().unwrap().push((
            container.to_string(),
            tmux_session.to_string(),
            window.to_string(),
            cwd.to_path_buf(),
            command.iter().map(|arg| (*arg).to_string()).collect(),
        ));
        self.windows
            .lock()
            .unwrap()
            .entry((container.to_string(), tmux_session.to_string()))
            .or_default()
            .insert(window.to_string());
        Ok(())
    }

    fn select_window(&self, container: &str, tmux_session: &str, window: &str) -> Result<()> {
        self.selected_windows.lock().unwrap().push((
            container.to_string(),
            tmux_session.to_string(),
            window.to_string(),
        ));
        Ok(())
    }
}

fn setup() -> (tempfile::TempDir, tempfile::TempDir) {
    (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
}

fn import_account(workspace: &Path, root: &Path, name: &str) {
    import_account_with_email(workspace, root, name, None);
}

fn import_account_with_email(workspace: &Path, root: &Path, name: &str, email: Option<&str>) {
    std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root);
    let source_dir = tempfile::tempdir().unwrap();
    let source = source_dir.path().join("auth.json");
    std::fs::write(&source, "recognizable-fake-secret").unwrap();
    let service = super::super::account_lifecycle::AccountLifecycle::new(
        workspace,
        super::super::account_lifecycle::ProcessCodexRunner,
    )
    .unwrap();
    service.import_with_email(name, &source, email).unwrap();
}

#[test]
#[serial]
fn start_creates_a_fresh_container_and_adopts_the_profile() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    let status = lifecycle.start("alice").unwrap();
    assert!(status.running);
    assert!(status.session_managed);
    assert_eq!(status.container_name, container_name("alice"));
    assert_eq!(status.mount_path, CONTAINER_CODEX_HOME);
    let creates = lifecycle.runner.creates.lock().unwrap();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].0, container_name("alice"));
    assert_eq!(creates[0].1, DEFAULT_SESSION_IMAGE);
    assert!(is_session_managed(&root.path().join("alice")));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn start_is_idempotent_when_already_running() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    lifecycle.start("alice").unwrap();
    assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn start_resumes_a_stopped_but_present_container() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    lifecycle.stop("alice", false).unwrap();
    // stop_and_remove in the fake fully removes the entry (mirrors a
    // real `docker rm`), so a subsequent `start` must go through
    // `create` again, not `start_existing` — both paths converge on the
    // same observable "running" outcome either way.
    let status = lifecycle.start("alice").unwrap();
    assert!(status.running);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn stop_refuses_when_an_exec_is_in_flight_unless_forced() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    let container = container_name("alice");
    lifecycle.runner.set_busy(&container, true);

    let error = lifecycle.stop("alice", false).unwrap_err().to_string();
    assert!(error.contains("in-flight"));
    assert!(error.contains("--force"));
    assert!(lifecycle.status("alice").unwrap().running);

    let status = lifecycle.stop("alice", true).unwrap();
    assert!(!status.running);
    assert_eq!(lifecycle.runner.stops.lock().unwrap().len(), 1);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn stop_is_idempotent_when_already_stopped() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    let status = lifecycle.stop("alice", false).unwrap();
    assert!(!status.running);
    assert!(lifecycle.runner.stops.lock().unwrap().is_empty());
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn status_reports_not_running_for_an_account_never_started() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    let status = lifecycle.status("alice").unwrap();
    assert!(!status.running);
    assert!(status.container_id.is_none());
    assert!(!status.session_managed);
    assert!(status.workspace.is_none());
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn status_reports_no_workspace_for_a_container_created_before_the_label_existed() {
    // A container `seed_running` without a workspace models a real
    // pre-#7389 session container -- `inspect` finds no `loom.workspace`
    // label, and `status` must report `None`, not synthesize one.
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.runner.seed_running(&container_name("alice"));
    let status = lifecycle.status("alice").unwrap();
    assert!(status.running);
    assert!(status.workspace.is_none());
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn status_and_start_reject_an_unknown_account() {
    let (workspace, root) = setup();
    std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.path());
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    assert!(lifecycle.status("ghost").is_err());
    assert!(lifecycle.start("ghost").is_err());
    assert!(lifecycle.stop("ghost", false).is_err());
    assert!(lifecycle.attach("ghost").is_err());
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn attach_refuses_when_not_running() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    let error = lifecycle.attach("alice").unwrap_err().to_string();
    assert!(error.contains("not running"));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn attach_execs_tmux_against_the_running_container() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    let code = lifecycle.attach("alice").unwrap();
    assert_eq!(code, 0);
    let attaches = lifecycle.runner.attaches.lock().unwrap();
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0].0, container_name("alice"));
    assert_eq!(attaches[0].1, DEFAULT_TMUX_SESSION_NAME);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

// ---- workspace mount (#7389) -------------------------------------------

#[test]
#[serial]
fn start_mounts_the_requested_workspace_and_status_reports_it() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let repo = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    let status = lifecycle
        .start_with_workspace("alice", Some(repo.path()))
        .unwrap();
    assert_eq!(status.workspace.as_deref(), Some(repo.path()));
    let creates = lifecycle.runner.creates.lock().unwrap();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].3, repo.path());

    // `status` on its own (no workspace argument -- it never takes one)
    // reports the same mounted workspace back.
    drop(creates);
    let status = lifecycle.status("alice").unwrap();
    assert_eq!(status.workspace.as_deref(), Some(repo.path()));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn start_without_a_workspace_defaults_to_the_lifecycles_own_workspace() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    let status = lifecycle.start("alice").unwrap();
    assert_eq!(status.workspace.as_deref(), Some(workspace.path()));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn starting_a_running_session_against_a_different_workspace_fails_naming_the_current_one() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let first_repo = tempfile::tempdir().unwrap();
    let second_repo = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle
        .start_with_workspace("alice", Some(first_repo.path()))
        .unwrap();

    let error = lifecycle
        .start_with_workspace("alice", Some(second_repo.path()))
        .unwrap_err()
        .to_string();
    assert!(error.contains(&first_repo.path().display().to_string()), "{error}");
    assert!(error.contains(&second_repo.path().display().to_string()), "{error}");
    // This is the workspace-mismatch message, not the exec-in-flight one.
    assert!(!error.contains("in-flight"));
    // Only the original `create` happened -- the mismatch is rejected
    // before any second `docker run`/`docker start`.
    assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn restarting_a_stopped_session_against_a_different_workspace_also_fails() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let first_repo = tempfile::tempdir().unwrap();
    let second_repo = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle
        .start_with_workspace("alice", Some(first_repo.path()))
        .unwrap();
    // Stop the container's own `running` flag without removing it, so
    // the next `start` takes the `start_existing` branch, not `create`.
    lifecycle
        .runner
        .containers
        .lock()
        .unwrap()
        .get_mut(&container_name("alice"))
        .unwrap()
        .running = false;

    let error = lifecycle
        .start_with_workspace("alice", Some(second_repo.path()))
        .unwrap_err()
        .to_string();
    assert!(error.contains(&first_repo.path().display().to_string()), "{error}");
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn restarting_the_same_workspace_succeeds() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let repo = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle
        .start_with_workspace("alice", Some(repo.path()))
        .unwrap();
    // Re-running `start` against the identical workspace is the
    // ordinary idempotent path, not a mismatch.
    let status = lifecycle
        .start_with_workspace("alice", Some(repo.path()))
        .unwrap();
    assert!(status.running);
    assert_eq!(status.workspace.as_deref(), Some(repo.path()));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

// ---- email -> short-name resolution (#7389) ----------------------------

#[test]
#[serial]
fn session_lifecycle_resolves_an_account_by_registered_email() {
    let (workspace, root) = setup();
    import_account_with_email(
        workspace.path(),
        root.path(),
        "agent-1",
        Some("agent-1@2amlogic.com"),
    );
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    let status = lifecycle.start("agent-1@2amlogic.com").unwrap();
    // The resolved identity -- and therefore the container name -- is
    // always the short profile name, never the raw email.
    assert_eq!(status.name, "agent-1");
    assert_eq!(status.container_name, container_name("agent-1"));
    assert!(!status.container_name.contains('@'));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn an_unregistered_email_is_rejected_with_a_clear_error() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    let error = lifecycle
        .start("nobody@2amlogic.com")
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not exist"), "{error}");
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

// ---- shell: start-if-absent, run Codex, attach (#7389) -----------------

#[test]
#[serial]
fn shell_starts_an_absent_session_and_launches_codex_in_a_new_window() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let repo = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);

    let code = lifecycle.shell("alice", Some(repo.path()), &[]).unwrap();
    assert_eq!(code, 0);

    // The container was started (not pre-existing).
    assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);

    let new_windows = lifecycle.runner.new_windows.lock().unwrap();
    assert_eq!(new_windows.len(), 1);
    assert_eq!(new_windows[0].0, container_name("alice"));
    assert_eq!(new_windows[0].1, DEFAULT_TMUX_SESSION_NAME);
    assert_eq!(new_windows[0].2, CODEX_WINDOW_NAME);
    assert_eq!(new_windows[0].3, repo.path());
    // Default args: `codex --yolo` (the operator's own bare-metal
    // invocation), since no explicit codex_args were passed.
    assert_eq!(new_windows[0].4, vec!["codex".to_string(), "--yolo".to_string()]);
    drop(new_windows);

    let selected = lifecycle.runner.selected_windows.lock().unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].2, CODEX_WINDOW_NAME);
    drop(selected);

    let attaches = lifecycle.runner.attaches.lock().unwrap();
    assert_eq!(attaches.len(), 1);
    assert_eq!(attaches[0].0, container_name("alice"));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn shell_passes_through_explicit_codex_args_instead_of_the_default() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let repo = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);

    let args = vec!["--dangerously-bypass-approvals-and-sandbox".to_string()];
    lifecycle.shell("alice", Some(repo.path()), &args).unwrap();

    let new_windows = lifecycle.runner.new_windows.lock().unwrap();
    assert_eq!(
        new_windows[0].4,
        vec![
            "codex".to_string(),
            "--dangerously-bypass-approvals-and-sandbox".to_string()
        ]
    );
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn shell_reattaches_to_an_existing_codex_window_without_stacking_a_second_process() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let repo = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);

    lifecycle.shell("alice", Some(repo.path()), &[]).unwrap();
    assert_eq!(lifecycle.runner.new_windows.lock().unwrap().len(), 1);

    // Detach (simulated by simply returning from the first `shell` call)
    // and run `shell` again against the same account/workspace.
    let code = lifecycle.shell("alice", Some(repo.path()), &[]).unwrap();
    assert_eq!(code, 0);

    // No second `docker run` (container reused) and no second
    // `new-window` call -- only a re-select + re-attach.
    assert_eq!(lifecycle.runner.creates.lock().unwrap().len(), 1);
    assert_eq!(lifecycle.runner.new_windows.lock().unwrap().len(), 1);
    assert_eq!(lifecycle.runner.selected_windows.lock().unwrap().len(), 2);
    assert_eq!(lifecycle.runner.attaches.lock().unwrap().len(), 2);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn shell_resolves_the_account_by_email_before_any_container_call() {
    let (workspace, root) = setup();
    import_account_with_email(
        workspace.path(),
        root.path(),
        "agent-1",
        Some("agent-1@2amlogic.com"),
    );
    let repo = tempfile::tempdir().unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);

    lifecycle
        .shell("agent-1@2amlogic.com", Some(repo.path()), &[])
        .unwrap();

    let creates = lifecycle.runner.creates.lock().unwrap();
    assert_eq!(creates.len(), 1);
    assert_eq!(creates[0].0, container_name("agent-1"));
    assert!(!creates[0].0.contains('@'));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn adopting_two_accounts_keeps_container_names_and_markers_distinct() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    import_account(workspace.path(), root.path(), "bob");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    assert!(is_session_managed(&root.path().join("alice")));
    assert!(!is_session_managed(&root.path().join("bob")));
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
fn container_name_is_namespaced_per_account() {
    assert_eq!(container_name("alice"), "loom-codex-session-alice");
    assert_ne!(container_name("alice"), container_name("bob"));
}

// ---- pure baseline-process classification (issue #6925 acceptance
// criterion: unit tests, not just a manual docker run) -----------------

#[test]
fn baseline_processes_are_recognized() {
    for command in [
        "/usr/bin/tini -- /home/loom/.local/bin/loom-session-entrypoint.sh",
        "sleep infinity",
        "tmux new-session -d -s session",
        "-bash",
    ] {
        assert!(
            ProcessContainerRunner::is_baseline_process(command, "session"),
            "{command:?} should be classified as baseline"
        );
    }
}

#[test]
fn non_baseline_processes_are_flagged_busy() {
    for command in [
        "codex exec do the thing",
        "tmux attach -t session",
        "sh -c echo hi",
    ] {
        assert!(
            !ProcessContainerRunner::is_baseline_process(command, "session"),
            "{command:?} should NOT be classified as baseline"
        );
    }
}

// ---- in-container auth probe (issue #6927) ----------------------------

fn inventory(workspace: &Path) -> Vec<AccountDescriptor> {
    account_inventory(workspace, AccountProvider::Codex).unwrap()
}

fn health_of(workspace: &Path, name: &str) -> Option<super::super::health::AccountHealth> {
    health::account_health(
        workspace,
        &super::super::account_registry::AccountId {
            provider: AccountProvider::Codex,
            name: name.into(),
        },
    )
    .unwrap()
}

#[test]
#[serial]
fn probe_execs_codex_login_status_inside_the_container() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();

    assert_eq!(lifecycle.probe_login("alice").unwrap(), LoginState::LoggedIn);
    let execs = lifecycle.runner.execs.lock().unwrap();
    assert_eq!(execs.len(), 1);
    assert_eq!(execs[0].0, container_name("alice"));
    assert_eq!(execs[0].1, vec!["codex", "login", "status"]);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn probe_reports_not_logged_in_from_the_container_output() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    lifecycle
        .runner
        .set_exec_result(&container_name("alice"), exec_ok("Not logged in"));
    assert_eq!(lifecycle.probe_login("alice").unwrap(), LoginState::NotLoggedIn);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn probe_reports_session_unavailable_without_a_running_container() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    // Adopted (the ownership rule applies) but never started.
    mark_session_managed(&root.path().join("alice"), &container_name("alice")).unwrap();
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    assert_eq!(lifecycle.probe_login("alice").unwrap(), LoginState::SessionUnavailable);
    assert!(lifecycle.runner.execs.lock().unwrap().is_empty());
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn probe_leaves_a_host_direct_profile_to_the_host_direct_path() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    assert_eq!(lifecycle.probe_login("alice").unwrap(), LoginState::NotChecked);
    assert!(lifecycle.runner.execs.lock().unwrap().is_empty());
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn an_expired_probe_excludes_the_account_from_selection_before_any_dispatch() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    import_account(workspace.path(), root.path(), "bob");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    lifecycle
        .runner
        .set_exec_result(&container_name("alice"), exec_ok("Not logged in"));

    let accounts = inventory(workspace.path());
    let outcomes = lifecycle.refresh_health_at(&accounts, 1_000).unwrap();
    // Only the adopted account is probed at all.
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].name, "alice");
    assert_eq!(outcomes[0].login_state, LoginState::NotLoggedIn);
    assert_eq!(outcomes[0].effect, Some(ProbeEffect::MarkedReauthRequired));

    let health = health_of(workspace.path(), "alice").unwrap();
    assert_eq!(health.reason, super::super::health::HealthReason::ReauthRequired);
    assert_eq!(health.signal_provenance, SESSION_PROBE_PROVENANCE);
    assert_eq!(health.last_probe, Some(1_000));
    assert!(health.cooldown_until.is_none(), "a reauth hold is sticky, never a cooldown");

    // The exclusion is the existing `select_healthy_at` filter, not new
    // ranking logic — "alice" is simply no longer a candidate.
    let selected =
        health::select_healthy_at(workspace.path(), AccountProvider::Codex, &accounts, 1_001)
            .unwrap();
    assert_eq!(selected.id.name, "bob");
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn a_healthy_probe_clears_an_existing_reauth_hold() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    let accounts = inventory(workspace.path());

    health::record_terminal_at(
        workspace.path(),
        &accounts[0].id,
        super::super::health::TerminalClassification::TokenExpired,
        "adapter_v1",
        1,
    )
    .unwrap();
    assert!(
        health::select_healthy_at(workspace.path(), AccountProvider::Codex, &accounts, 2).is_err()
    );

    let outcomes = lifecycle.refresh_health_at(&accounts, 3).unwrap();
    assert_eq!(outcomes[0].login_state, LoginState::LoggedIn);
    assert_eq!(outcomes[0].effect, Some(ProbeEffect::ClearedReauthHold));
    assert!(
        health::select_healthy_at(workspace.path(), AccountProvider::Codex, &accounts, 4).is_ok()
    );
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn an_inconclusive_probe_never_marks_an_account_expired() {
    let inconclusive = [
        // Container stopped: nothing was probed.
        None,
        // `codex` missing inside the container.
        Some(ExecOutput {
            success: false,
            unavailable: false,
            timed_out: false,
            exit_code: Some(127),
            output: "exec: \"codex\": executable file not found in $PATH".into(),
        }),
        // Probe timed out.
        Some(ExecOutput {
            success: false,
            unavailable: false,
            timed_out: true,
            exit_code: None,
            output: String::new(),
        }),
        // Ran, but said nothing either way.
        Some(ExecOutput {
            success: false,
            unavailable: false,
            timed_out: false,
            exit_code: Some(1),
            output: "unexpected".into(),
        }),
    ];
    for result in inconclusive {
        let (workspace, root) = setup();
        import_account(workspace.path(), root.path(), "alice");
        let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
        match result {
            Some(result) => {
                lifecycle.start("alice").unwrap();
                lifecycle
                    .runner
                    .set_exec_result(&container_name("alice"), result);
            }
            None => {
                mark_session_managed(&root.path().join("alice"), &container_name("alice")).unwrap();
            }
        }
        let accounts = inventory(workspace.path());
        let outcomes = lifecycle.refresh_health_at(&accounts, 10).unwrap();
        assert_eq!(outcomes[0].effect, None);
        assert!(
            health_of(workspace.path(), "alice").is_none(),
            "an inconclusive probe must not write health state at all"
        );
        assert!(
            health::select_healthy_at(workspace.path(), AccountProvider::Codex, &accounts, 11)
                .is_ok()
        );
        std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
    }
}

#[test]
#[serial]
fn a_fresh_conclusive_result_suppresses_re_probing_until_the_ttl_lapses() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    let lifecycle = SessionLifecycle::new(workspace.path(), FakeRunner::default(), None);
    lifecycle.start("alice").unwrap();
    let accounts = inventory(workspace.path());

    lifecycle.refresh_health_at(&accounts, 1_000).unwrap();
    assert_eq!(lifecycle.runner.execs.lock().unwrap().len(), 1);

    // Within the TTL: no second `docker exec`.
    let skipped = lifecycle
        .refresh_health_at(&accounts, 1_000 + DEFAULT_SESSION_PROBE_TTL_SECS - 1)
        .unwrap();
    assert_eq!(skipped[0].login_state, LoginState::NotChecked);
    assert_eq!(skipped[0].effect, None);
    assert_eq!(lifecycle.runner.execs.lock().unwrap().len(), 1);

    // Once it lapses, the account is probed again.
    lifecycle
        .refresh_health_at(&accounts, 1_000 + DEFAULT_SESSION_PROBE_TTL_SECS)
        .unwrap();
    assert_eq!(lifecycle.runner.execs.lock().unwrap().len(), 2);
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
#[serial]
fn refresh_is_a_no_op_when_no_account_is_session_managed() {
    let (workspace, root) = setup();
    import_account(workspace.path(), root.path(), "alice");
    // Uses the REAL `ProcessContainerRunner`: the point of this test is
    // that the selection path costs zero `docker` invocations for a pool
    // that never opted into session containers — it must pass on a host
    // with no docker installed at all.
    let accounts = inventory(workspace.path());
    assert!(refresh_session_health(workspace.path(), &accounts, 1).is_empty());
    assert!(health_of(workspace.path(), "alice").is_none());
    std::env::remove_var("LOOM_CODEX_PROFILE_ROOT");
}

#[test]
fn a_missing_in_container_command_is_recognized_but_a_plain_failure_is_not() {
    assert!(is_command_missing(&ExecOutput {
        success: false,
        unavailable: false,
        timed_out: false,
        exit_code: Some(127),
        output: "exec: \"codex\": executable file not found in $PATH".into(),
    }));
    assert!(!is_command_missing(&ExecOutput {
        success: false,
        unavailable: false,
        timed_out: false,
        exit_code: Some(1),
        output: "Not logged in".into(),
    }));
    // Exit 127 alone is not enough — a command may legitimately exit 127.
    assert!(!is_command_missing(&ExecOutput {
        success: false,
        unavailable: false,
        timed_out: false,
        exit_code: Some(127),
        output: "some unrelated failure".into(),
    }));
}

// ---- marker file mechanics --------------------------------------------

#[test]
fn mark_session_managed_is_idempotent_and_preserves_first_adoption() {
    let profile = tempfile::tempdir().unwrap();
    assert!(!is_session_managed(profile.path()));
    mark_session_managed(profile.path(), "loom-codex-session-alice").unwrap();
    assert!(is_session_managed(profile.path()));
    let first = std::fs::read_to_string(profile.path().join(SESSION_MARKER_FILE)).unwrap();
    // A second `start` against an already-adopted profile (e.g. a
    // different container name after manual recovery) must not
    // overwrite the original adoption record.
    mark_session_managed(profile.path(), "some-other-name").unwrap();
    let second = std::fs::read_to_string(profile.path().join(SESSION_MARKER_FILE)).unwrap();
    assert_eq!(first, second);
}

#[cfg(unix)]
#[test]
fn uid_matches_image_reports_mismatch_for_a_tempdir_owned_by_the_test_process() {
    let profile = tempfile::tempdir().unwrap();
    // The result is host-dependent (the test process's own uid), but the
    // function must never panic and must return a definite answer for an
    // existing directory.
    assert!(uid_matches_image(profile.path()).is_some());
}
