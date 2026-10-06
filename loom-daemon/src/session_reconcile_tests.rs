//! Issue #10453: the session reconcile pass, driven through a fake
//! [`ContainerRunner`] so no test needs Docker.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Result};
use serial_test::serial;

use super::*;
use crate::tokens_pool::account_lifecycle::{AccountLifecycle, ProcessCodexRunner};
use crate::tokens_pool::account_registry::account_inventory;
use crate::tokens_pool::profile_root_env::ProfileRootEnv;
use crate::tokens_pool::session_lifecycle::{mark_session_managed, parse_inspect_line, ExecOutput};

#[derive(Default)]
struct Fake {
    containers: Mutex<HashMap<String, ContainerState>>,
    busy: Mutex<HashMap<String, bool>>,
    /// Every runner call, by method name, in order.
    calls: Mutex<Vec<String>>,
    /// `(container, image, workspace)` per `create`.
    creates: Mutex<Vec<(String, String, PathBuf)>>,
    fail_create: Mutex<bool>,
}

impl Fake {
    fn seed(&self, container: &str, running: bool, restarting: bool, workspace: Option<&Path>) {
        self.containers.lock().unwrap().insert(
            container.into(),
            ContainerState {
                id: format!("{container}-id"),
                running,
                restarting,
                started_at: None,
                image: Some("example/session:pinned".into()),
                workspace: workspace.map(Path::to_path_buf),
            },
        );
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn count(&self, method: &str) -> usize {
        self.calls().iter().filter(|c| *c == method).count()
    }
    /// Calls that change a container (anything but `inspect`).
    fn mutations(&self) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c != "inspect" && c != "has_active_exec")
            .collect()
    }
    fn log(&self, method: &str) {
        self.calls.lock().unwrap().push(method.into());
    }
}

impl ContainerRunner for Fake {
    fn inspect(&self, container: &str) -> Result<Option<ContainerState>> {
        self.log("inspect");
        Ok(self.containers.lock().unwrap().get(container).cloned())
    }
    fn create(
        &self,
        container: &str,
        image: &str,
        _codex_home: &Path,
        workspace: &Path,
        _daemon_root: &Path,
    ) -> Result<()> {
        self.log("create");
        if *self.fail_create.lock().unwrap() {
            bail!("docker run {image} failed: Unable to find image");
        }
        self.creates.lock().unwrap().push((
            container.into(),
            image.into(),
            workspace.to_path_buf(),
        ));
        self.seed(container, true, false, Some(workspace));
        Ok(())
    }
    fn start_existing(&self, container: &str) -> Result<()> {
        self.log("start_existing");
        let mut containers = self.containers.lock().unwrap();
        let state = containers
            .get_mut(container)
            .ok_or_else(|| anyhow!("no such container"))?;
        state.running = true;
        Ok(())
    }
    fn has_active_exec(&self, container: &str) -> Result<bool> {
        self.log("has_active_exec");
        Ok(*self.busy.lock().unwrap().get(container).unwrap_or(&false))
    }
    fn stop_and_remove(&self, container: &str, _grace: Duration) -> Result<()> {
        self.log("stop_and_remove");
        self.containers.lock().unwrap().remove(container);
        Ok(())
    }
    fn attach_interactive(&self, _container: &str, _tmux: &str) -> Result<i32> {
        self.log("attach_interactive");
        Ok(0)
    }
    fn exec_capture(&self, _c: &str, _argv: &[&str], _t: Duration) -> Result<ExecOutput> {
        self.log("exec_capture");
        bail!("not used")
    }
    fn window_exists(&self, _c: &str, _s: &str, _w: &str) -> Result<bool> {
        self.log("window_exists");
        Ok(false)
    }
    fn new_window(&self, _c: &str, _s: &str, _w: &str, _cwd: &Path, _cmd: &[&str]) -> Result<()> {
        self.log("new_window");
        Ok(())
    }
    fn select_window(&self, _c: &str, _s: &str, _w: &str) -> Result<()> {
        self.log("select_window");
        Ok(())
    }
}

struct Env {
    workspace: tempfile::TempDir,
    _root: tempfile::TempDir,
    _env: ProfileRootEnv,
    accounts: Vec<AccountDescriptor>,
}

/// Import `names` as Codex accounts; those in `managed` get the
/// session-managed marker.
fn setup(names: &[&str], managed: &[&str]) -> Env {
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let env = ProfileRootEnv::set(root.path());
    let service = AccountLifecycle::new(workspace.path(), ProcessCodexRunner).unwrap();
    for name in names {
        let source_dir = tempfile::tempdir().unwrap();
        let source = source_dir.path().join("auth.json");
        std::fs::write(&source, "fake-secret").unwrap();
        service.import_with_email(name, &source, None).unwrap();
    }
    let accounts = account_inventory(workspace.path(), AccountProvider::Codex).unwrap();
    for account in &accounts {
        if managed.contains(&account.id.name.as_str()) {
            mark_session_managed(&account.credential_reference, &container_name(&account.id.name))
                .unwrap();
        }
    }
    Env {
        workspace,
        _root: root,
        _env: env,
        accounts,
    }
}

fn host(_: &AccountDescriptor) -> anyhow::Result<bool> {
    Ok(false)
}

fn pass(
    lifecycle: &mut SessionLifecycle<Fake>,
    env: &Env,
    private: &dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    state: &mut ReconcileState,
    now: u64,
) -> Vec<Outcome> {
    reconcile_accounts(lifecycle, &env.accounts, private, Path::new("/srv/checkouts"), state, now)
        .into_iter()
        .map(|o| o.outcome)
        .collect()
}

// ---- decision function ------------------------------------------------------

fn st(running: bool, restarting: bool, workspace: Option<&str>) -> ContainerState {
    ContainerState {
        id: "id".into(),
        running,
        restarting,
        started_at: None,
        image: None,
        workspace: workspace.map(PathBuf::from),
    }
}

#[test]
fn a_private_clone_account_is_never_recreated_host_mounted() {
    let fallback = Path::new("/srv/checkouts");
    // Configured private-clone, container missing or stopped.
    assert_eq!(decide(None, true, 0, fallback), Decision::PrivateClone);
    let stopped = st(false, false, Some("/srv/checkouts"));
    assert_eq!(decide(Some(&stopped), true, 0, fallback), Decision::PrivateClone);
    // A stopped private-clone CONTAINER is recognised even if the account's
    // private state could not be found.
    let private = st(false, false, Some(private_workspace::REPO));
    assert_eq!(decide(Some(&private), false, 0, fallback), Decision::PrivateClone);
    // Host-mounted: resume against the label, recreate against the fallback.
    assert_eq!(
        decide(Some(&stopped), false, 0, fallback),
        Decision::Resume {
            workspace: Some("/srv/checkouts".into())
        }
    );
    assert_eq!(
        decide(None, false, 0, fallback),
        Decision::Recreate {
            workspace: fallback.into()
        }
    );
    assert_eq!(decide(Some(&st(true, false, None)), true, 0, fallback), Decision::Healthy);
}

#[test]
fn a_restarting_container_is_not_healthy_and_becomes_a_crash_loop() {
    let fallback = Path::new("/w");
    let restarting = st(false, true, Some("/w"));
    assert_eq!(decide(Some(&restarting), false, 0, fallback), Decision::Restarting);
    assert_eq!(decide(Some(&restarting), false, 1, fallback), Decision::CrashLoop);
    assert_eq!(decide(Some(&restarting), true, 5, fallback), Decision::CrashLoop);
}

#[test]
fn default_mount_workspace_is_the_common_parent_of_registered_roots() {
    let daemon = Path::new("/home/u/loom-daemon");
    assert_eq!(default_mount_workspace(&[], daemon), daemon);
    let one = vec![PathBuf::from("/home/u/GitHub/loom")];
    assert_eq!(default_mount_workspace(&one, daemon), Path::new("/home/u/GitHub/loom"));
    let many = vec![
        PathBuf::from("/home/u/GitHub/loom"),
        PathBuf::from("/home/u/GitHub/anvil"),
        PathBuf::from("/home/u/GitHub/nested/repo"),
    ];
    assert_eq!(default_mount_workspace(&many, daemon), Path::new("/home/u/GitHub"));
}

#[test]
fn backoff_doubles_and_caps() {
    assert_eq!(backoff_secs(1), BACKOFF_BASE_SECS);
    assert_eq!(backoff_secs(2), BACKOFF_BASE_SECS * 2);
    assert_eq!(backoff_secs(3), BACKOFF_BASE_SECS * 4);
    assert_eq!(backoff_secs(60), BACKOFF_MAX_SECS);
    const { assert!(BACKOFF_BASE_SECS > DEFAULT_SESSION_RECONCILE_INTERVAL_SECS) };
}

#[test]
fn opt_out_resolution() {
    let none = SessionReconcileConfig::default();
    assert!(resolve_enabled_from(None, &none));
    assert!(!resolve_enabled_from(Some("0"), &none));
    let off = SessionReconcileConfig {
        enabled: Some(false),
        interval_secs: None,
    };
    assert!(!resolve_enabled_from(None, &off));
    assert!(resolve_enabled_from(Some("1"), &off));
}

#[test]
fn config_block_is_read() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".loom")).unwrap();
    std::fs::write(
        dir.path().join(".loom/config.json"),
        r#"{"autonomous":{"sessionReconcile":{"enabled":false,"intervalSecs":30}}}"#,
    )
    .unwrap();
    assert_eq!(
        read_config(dir.path()),
        SessionReconcileConfig {
            enabled: Some(false),
            interval_secs: Some(30)
        }
    );
}

// ---- Docker's Restarting state ----------------------------------------------

#[test]
fn inspect_reads_restarting_as_not_running() {
    let restarting =
        parse_inspect_line("abc\ttrue\ttrue\t2026-10-06T00:00:00Z\timg\t/w\n").unwrap();
    assert!(!restarting.running);
    assert!(restarting.restarting);
    let running = parse_inspect_line("abc\ttrue\tfalse\t-\timg\t/w").unwrap();
    assert!(running.running && !running.restarting);
    assert_eq!(running.workspace.as_deref(), Some(Path::new("/w")));
    assert!(parse_inspect_line("").is_none());
}

#[test]
#[serial]
fn status_and_start_treat_a_restarting_container_as_not_running() {
    let env = setup(&["alice"], &["alice"]);
    let container = container_name("alice");
    let lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle
        .runner()
        .seed(&container, false, true, Some(env.workspace.path()));
    let status = lifecycle.status("alice").unwrap();
    assert!(!status.running);
    assert!(status.restarting);
    let error = lifecycle.start("alice").unwrap_err().to_string();
    assert!(error.contains("restarting"), "{error}");
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new(), "start must not reuse it");
}

// ---- the pass ---------------------------------------------------------------

#[test]
#[serial]
fn no_docker_call_without_an_enabled_session_managed_account() {
    let mut env = setup(&["alice", "bob"], &["bob"]);
    // bob is session-managed but held (enabled=false); alice is not managed.
    env.accounts
        .iter_mut()
        .filter(|a| a.id.name == "bob")
        .for_each(|a| a.enabled = false);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let mut state = ReconcileState::default();
    let outcomes = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert!(outcomes.is_empty());
    assert_eq!(lifecycle.runner().calls(), Vec::<String>::new());
}

#[test]
#[serial]
fn a_stopped_host_mounted_container_is_resumed_against_its_workspace() {
    let env = setup(&["alice"], &["alice"]);
    let container = container_name("alice");
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle
        .runner()
        .seed(&container, false, false, Some(Path::new("/srv/checkouts")));
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Resumed]);
    assert_eq!(lifecycle.runner().count("start_existing"), 1);
    assert_eq!(lifecycle.runner().count("create"), 0);
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 60), vec![Outcome::Running]);
}

#[test]
#[serial]
fn a_removed_host_mounted_container_is_recreated_with_its_last_workspace_and_image() {
    let env = setup(&["alice"], &["alice"]);
    let container = container_name("alice");
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle
        .runner()
        .seed(&container, true, false, Some(Path::new("/home/u/GitHub")));
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Running]);
    // `docker rm -f`
    lifecycle.runner().containers.lock().unwrap().clear();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 60),
        vec![Outcome::Recreated {
            workspace: "/home/u/GitHub".into()
        }]
    );
    let creates = lifecycle.runner().creates.lock().unwrap().clone();
    assert_eq!(
        creates,
        vec![(container, "example/session:pinned".into(), "/home/u/GitHub".into())]
    );
}

#[test]
#[serial]
fn a_never_seen_missing_container_uses_the_fallback_workspace() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let mut state = ReconcileState::default();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 0),
        vec![Outcome::Recreated {
            workspace: "/srv/checkouts".into()
        }]
    );
    let creates = lifecycle.runner().creates.lock().unwrap().clone();
    assert_eq!(creates[0].1, crate::tokens_pool::session_lifecycle::DEFAULT_SESSION_IMAGE);
}

#[test]
#[serial]
fn a_private_clone_account_is_skipped_not_created() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let mut state = ReconcileState::default();
    let private = |_: &AccountDescriptor| -> anyhow::Result<bool> { Ok(true) };
    assert_eq!(
        pass(&mut lifecycle, &env, &private, &mut state, 0),
        vec![Outcome::PrivateCloneSkipped]
    );
    // Undecidable mode fails closed too.
    let unknown = |_: &AccountDescriptor| -> anyhow::Result<bool> { bail!("bad state") };
    assert_eq!(
        pass(&mut lifecycle, &env, &unknown, &mut state, 60),
        vec![Outcome::PrivateCloneSkipped]
    );
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
}

#[test]
#[serial]
fn a_container_with_an_in_flight_exec_is_never_touched() {
    let env = setup(&["alice", "bob"], &["alice", "bob"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let ws = Some(Path::new("/w"));
    lifecycle
        .runner()
        .seed(&container_name("alice"), true, false, ws);
    // bob is crash-looping while an exec is (as far as `docker top` knows) live.
    lifecycle
        .runner()
        .seed(&container_name("bob"), false, true, ws);
    for name in ["alice", "bob"] {
        lifecycle
            .runner()
            .busy
            .lock()
            .unwrap()
            .insert(container_name(name), true);
    }
    let mut state = ReconcileState::default();
    for now in [0, 60, 120] {
        pass(&mut lifecycle, &env, &host, &mut state, now);
    }
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
    assert_eq!(lifecycle.runner().containers.lock().unwrap().len(), 2);
}

#[test]
#[serial]
fn a_container_restarting_across_passes_is_reported_as_a_crash_loop() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle
        .runner()
        .seed(&container_name("alice"), false, true, Some(Path::new("/w")));
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Restarting]);
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 60), vec![Outcome::CrashLoop]);
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 120), vec![Outcome::CrashLoop]);
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
    // Docker's own restart succeeded: the streak resets.
    lifecycle
        .runner()
        .seed(&container_name("alice"), true, false, Some(Path::new("/w")));
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 180), vec![Outcome::Running]);
    lifecycle
        .runner()
        .seed(&container_name("alice"), false, true, Some(Path::new("/w")));
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 240), vec![Outcome::Restarting]);
}

#[test]
#[serial]
fn a_failing_start_backs_off_instead_of_rerunning_every_interval() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    *lifecycle.runner().fail_create.lock().unwrap() = true;
    let mut state = ReconcileState::default();
    let interval = DEFAULT_SESSION_RECONCILE_INTERVAL_SECS;
    let mut runs = Vec::new();
    let mut outcomes = Vec::new();
    for tick in 0..10 {
        let out = pass(&mut lifecycle, &env, &host, &mut state, tick * interval);
        outcomes.push(out[0].clone());
        runs.push(lifecycle.runner().count("create"));
    }
    // Retries at t=0, base (120s), then base+2*base (360s), then +4*base (840s).
    assert_eq!(runs, vec![1, 1, 2, 2, 2, 2, 3, 3, 3, 3]);
    assert!(matches!(outcomes[0], Outcome::Failed { retry_at: 120, .. }));
    assert!(matches!(outcomes[1], Outcome::BackingOff { retry_at: 120 }));
    // While backing off the account costs no docker call at all.
    let inspects = lifecycle.runner().count("inspect");
    pass(&mut lifecycle, &env, &host, &mut state, 10 * interval);
    assert_eq!(lifecycle.runner().count("inspect"), inspects);
    // Recovery clears the backoff.
    *lifecycle.runner().fail_create.lock().unwrap() = false;
    let out = pass(&mut lifecycle, &env, &host, &mut state, 10_000);
    assert!(out[0].started(), "{out:?}");
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 10_060), vec![Outcome::Running]);
}
