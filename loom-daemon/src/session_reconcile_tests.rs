//! Issue #10453: the session reconcile pass, driven through a fake
//! [`ContainerRunner`] so no test needs Docker. The pass's snapshot is the
//! fake's containers rendered as `docker inspect` objects (one logged
//! `inspect` per snapshot).

use std::collections::{BTreeMap, HashMap};
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Result};
use serial_test::serial;

use super::*;
use crate::tokens_pool::account_lifecycle::{AccountLifecycle, ProcessCodexRunner};
use crate::tokens_pool::account_registry::account_inventory;
use crate::tokens_pool::docker_cli::DockerTimedOut;
use crate::tokens_pool::profile_root_env::ProfileRootEnv;
use crate::tokens_pool::session_hold;
use crate::tokens_pool::session_lifecycle::{
    check_mount_denials, mark_session_managed, parse_inspect_line, workspace_mount_roots,
    ExecOutput, WORKSPACE_LABEL,
};
use crate::tokens_pool::session_state::{DriftInputs, Observed};

#[path = "session_reconcile_drift_tests.rs"]
mod drift_tests;
#[path = "session_reconcile_removal_tests.rs"]
mod removal_tests;
#[path = "session_reconcile_safety_tests.rs"]
mod safety_tests;

/// Shared so a pass can snapshot it while the lifecycle owns it.
#[derive(Default, Clone)]
struct Fake(Arc<FakeState>);

impl Deref for Fake {
    type Target = FakeState;
    fn deref(&self) -> &FakeState {
        &self.0
    }
}

#[derive(Default)]
struct FakeState {
    containers: Mutex<HashMap<String, ContainerState>>,
    /// Workspace bind destinations per container (path parity).
    mounts: Mutex<HashMap<String, Vec<PathBuf>>>,
    /// The workspace registry the pass and `create` see.
    registered: Mutex<Vec<PathBuf>>,
    /// `firewall: true` paths the pass's pre-check sees (and `create`, unless
    /// `create_firewalled` says otherwise).
    firewalled: Mutex<Vec<PathBuf>>,
    /// What `create` sees instead, when the two disagree.
    create_firewalled: Mutex<Option<Vec<PathBuf>>>,
    /// Runs right after `has_active_exec` answers: a dispatch that starts in
    /// the gap `docker top` cannot see.
    after_top: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// The firewall roster cannot be read: denials are unknown and `create`
    /// (and its pre-check) fail closed.
    roster_unreadable: Mutex<bool>,
    /// Where this fake's dispatch locks live (a tempdir, made on first use).
    lock_dir: Mutex<Option<PathBuf>>,
    /// The pass sees no readable registry.
    registry_unreadable: Mutex<bool>,
    /// `create` mounts exactly these instead of what the registry implies
    /// (intended ≠ achievable).
    create_mounts: Mutex<Option<Vec<PathBuf>>>,
    busy: Mutex<HashMap<String, bool>>,
    /// Every runner call, by method name, in order.
    calls: Mutex<Vec<String>>,
    /// `(container, image, workspace)` per `create`.
    creates: Mutex<Vec<(String, String, PathBuf)>>,
    fail_create: Mutex<bool>,
    /// `docker inspect` fails with this error (Docker down / timed out).
    fail_inspect: Mutex<Option<String>>,
    /// A started/created container exits at once: `Some(true)` leaves it
    /// `Restarting`, `Some(false)` stopped.
    dies_after_start: Mutex<Option<bool>>,
    /// On the Nth `inspect` call (1-based), act like a concurrent
    /// `accounts session stop`: write the hold into this profile, then
    /// `docker stop` the container (still present, not yet `rm`ed).
    stop_on_inspect: Mutex<Option<(usize, PathBuf)>>,
    /// The next `stop_and_remove` loses the stop/reconcile race once: a
    /// reconcile `docker start` lands between its `docker stop` and `docker
    /// rm`, so `rm` fails on a running container.
    start_races_stop: Mutex<bool>,
    /// `docker start` hits its deadline (wedged engine).
    start_times_out: Mutex<bool>,
    /// `docker stop` hits its deadline (a drift recreate's teardown).
    stop_times_out: Mutex<bool>,
    /// The next N `stop_and_remove` calls stop the container, then fail the
    /// `docker rm` (it stays present, stopped).
    rm_fails: Mutex<u32>,
}

impl FakeState {
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
    fn denials(&self) -> drift::Denials {
        drift::Denials {
            home: None,
            firewalled: self.firewalled.lock().unwrap().clone(),
        }
    }
    fn lock_dir(&self) -> PathBuf {
        self.lock_dir
            .lock()
            .unwrap()
            .get_or_insert_with(|| tempfile::tempdir().unwrap().keep())
            .clone()
    }
    /// What `create` accepts: its own view of the roster.
    fn create_accepts(&self, workspace: &Path, walls: &[PathBuf]) -> Result<Vec<PathBuf>> {
        if *self.roster_unreadable.lock().unwrap() {
            bail!("fleet roster (firewall input): unreadable");
        }
        let registered = self.registered.lock().unwrap().clone();
        let intended = workspace_mount_roots(workspace, &registered).unwrap_or_default();
        check_mount_denials(&intended, None, walls)?;
        Ok(intended)
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
    /// Count this read; on the configured read, act like a concurrent
    /// `accounts session stop` (hold written, container stopped, not rm'd).
    fn read(&self, containers: &mut HashMap<String, ContainerState>) {
        self.log("inspect");
        let n = self.count("inspect");
        if let Some((at, profile)) = self.stop_on_inspect.lock().unwrap().clone() {
            if n == at {
                session_hold::write_hold(&profile, session_hold::now_unix_ms()).unwrap();
                for state in containers.values_mut() {
                    state.running = false;
                    state.restarting = false;
                }
            }
        }
    }
    /// The pass's one read: every container as a `docker inspect` object.
    fn snapshot(&self, registered: &[PathBuf]) -> Snapshot {
        if let Some(error) = self.fail_inspect.lock().unwrap().clone() {
            self.log("inspect");
            return Snapshot::Unavailable(format!("docker ps failed: {error}"));
        }
        let mut containers = self.containers.lock().unwrap();
        self.read(&mut containers);
        let mounts = self.mounts.lock().unwrap();
        let map: BTreeMap<String, Observed> = containers
            .iter()
            .map(|(name, state)| {
                let inspect =
                    inspect_json(name, state, mounts.get(name).map_or(&[], Vec::as_slice));
                let observed = Observed::of(inspect, &DriftInputs::registry(registered));
                (name.clone(), observed)
            })
            .collect();
        Snapshot::Available(map)
    }
}

/// `state` (and its workspace binds) as `docker inspect` reports it.
fn inspect_json(name: &str, state: &ContainerState, mounts: &[PathBuf]) -> Value {
    let mut labels = serde_json::Map::new();
    if let Some(workspace) = &state.workspace {
        labels.insert(WORKSPACE_LABEL.into(), workspace.display().to_string().into());
        if workspace == Path::new(private_workspace::REPO) {
            labels.insert("loom.workspace-mode".into(), "private-clone".into());
        }
    }
    serde_json::json!({
        "Name": format!("/{name}"),
        "Id": state.id,
        "State": {"Running": state.running || state.restarting, "Restarting": state.restarting},
        "Config": {"Image": state.image, "Labels": labels},
        "Mounts": mounts.iter().map(|m| serde_json::json!({
            "Type": "bind", "Source": m, "Destination": m, "RW": true,
        })).collect::<Vec<_>>(),
    })
}

impl ContainerRunner for Fake {
    fn inspect(&self, container: &str) -> Result<Option<ContainerState>> {
        if let Some(error) = self.fail_inspect.lock().unwrap().clone() {
            self.log("inspect");
            bail!("docker inspect {container} failed: {error}");
        }
        let mut containers = self.containers.lock().unwrap();
        self.read(&mut containers);
        Ok(containers.get(container).cloned())
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
        // Like `docker run` behind `session start`: a firewalled root is
        // refused. (An unregistered workspace is not refused here, so the
        // #10453 tests can use paths that do not exist.)
        let walls = self.create_firewalled.lock().unwrap().clone();
        let walls = walls.unwrap_or_else(|| self.firewalled.lock().unwrap().clone());
        let intended = self.create_accepts(workspace, &walls)?;
        let dies = *self.dies_after_start.lock().unwrap();
        self.seed(container, dies.is_none(), dies == Some(true), Some(workspace));
        let mounts = self
            .create_mounts
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(intended);
        self.mounts.lock().unwrap().insert(container.into(), mounts);
        Ok(())
    }
    fn start_existing(&self, container: &str) -> Result<()> {
        self.log("start_existing");
        if *self.start_times_out.lock().unwrap() {
            return Err(DockerTimedOut {
                subcommand: "start".into(),
                secs: 60,
            }
            .into());
        }
        let dies = *self.dies_after_start.lock().unwrap();
        let mut containers = self.containers.lock().unwrap();
        let state = containers
            .get_mut(container)
            .ok_or_else(|| anyhow!("no such container"))?;
        state.running = dies.is_none();
        state.restarting = dies == Some(true);
        Ok(())
    }
    fn has_active_exec(&self, container: &str) -> Result<bool> {
        self.log("has_active_exec");
        let busy = *self.busy.lock().unwrap().get(container).unwrap_or(&false);
        if let Some(hook) = self.after_top.lock().unwrap().as_ref() {
            hook();
        }
        Ok(busy)
    }
    fn stop_and_remove(&self, container: &str, _grace: Duration) -> Result<()> {
        self.log("stop_and_remove");
        if *self.stop_times_out.lock().unwrap() {
            return Err(DockerTimedOut {
                subcommand: "stop".into(),
                secs: 120,
            }
            .into());
        }
        let mut containers = self.containers.lock().unwrap();
        let mut rm_fails = self.rm_fails.lock().unwrap();
        if *rm_fails > 0 {
            *rm_fails -= 1;
            if let Some(state) = containers.get_mut(container) {
                state.running = false;
                state.restarting = false;
            }
            bail!("docker rm {container} failed: removal of container is already in progress");
        }
        if std::mem::take(&mut *self.start_races_stop.lock().unwrap()) {
            if let Some(state) = containers.get_mut(container) {
                state.running = true;
                state.restarting = false;
            }
            bail!(
                "docker rm {container} failed: You cannot remove a running container. Stop \
                 the container before attempting removal or force remove"
            );
        }
        containers.remove(container);
        self.mounts.lock().unwrap().remove(container);
        Ok(())
    }
    fn attach_interactive(&self, _container: &str, _tmux: &str) -> Result<i32> {
        self.log("attach_interactive");
        Ok(0)
    }
    fn exec_capture(&self, _c: &str, _argv: &[&str], _t: Duration) -> Result<ExecOutput> {
        self.log("exec_capture");
        Ok(ExecOutput {
            success: true,
            unavailable: false,
            timed_out: false,
            exit_code: Some(0),
            output: "Logged in using ChatGPT".into(),
        })
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
    pass_with(lifecycle, &env.accounts, &[], private, Path::new("/srv/checkouts"), state, now)
}

/// One pass over `accounts`, with `other_roots` contributing to the
/// cross-root [`AccountIndex`] only.
fn pass_with(
    lifecycle: &mut SessionLifecycle<Fake>,
    accounts: &[AccountDescriptor],
    other_roots: &[AccountDescriptor],
    private: &dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    fallback: &Path,
    state: &mut ReconcileState,
    now: u64,
) -> Vec<Outcome> {
    pass_named(lifecycle, accounts, other_roots, private, fallback, state, now)
        .into_iter()
        .map(|o| o.outcome)
        .collect()
}

/// The inputs that do not depend on the fake's roster or registry state.
fn fake_inputs<'a>(
    fake: &Fake,
    index: &'a AccountIndex,
    private: &'a dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    fallback: &'a Path,
    registered: &'a [PathBuf],
) -> PassInputs<'a> {
    PassInputs {
        index,
        is_private_clone: private,
        fallback_workspace: fallback,
        registered: Some(registered),
        denials_for: &|_| Ok(drift::Denials::default()),
        would_create_accept: &|_| Ok(()),
        dispatch_locks: Some(Box::leak(fake.lock_dir().into_boxed_path())),
        class: None,
    }
}

/// [`pass_with`], keeping each outcome's account.
fn pass_named(
    lifecycle: &mut SessionLifecycle<Fake>,
    accounts: &[AccountDescriptor],
    other_roots: &[AccountDescriptor],
    private: &dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    fallback: &Path,
    state: &mut ReconcileState,
    now: u64,
) -> Vec<AccountOutcome> {
    let index = AccountIndex::from_inventories(&[accounts, other_roots]);
    let fake = lifecycle.runner().clone();
    let registered = fake.registered.lock().unwrap().clone();
    let mut take = || fake.snapshot(&registered);
    let mut observe = PassSnapshot::new(&mut take);
    let inputs = fake_inputs(&fake, &index, private, fallback, &registered);
    let inputs = PassInputs {
        registered: (!*fake.registry_unreadable.lock().unwrap()).then_some(&registered[..]),
        denials_for: &|_| {
            if *fake.roster_unreadable.lock().unwrap() {
                bail!("fleet roster (firewall input): unreadable");
            }
            Ok(fake.denials())
        },
        // The pre-check's view of the roster; `create` may be given another.
        would_create_accept: &|workspace| {
            let walls = fake.firewalled.lock().unwrap().clone();
            fake.create_accepts(workspace, &walls).map(drop)
        },
        ..inputs
    };
    reconcile_accounts(lifecycle, accounts, &inputs, &mut observe, state, now)
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

// ---- operator hold (verdict blocker 1) --------------------------------------

fn profile(env: &Env, name: &str) -> PathBuf {
    env.accounts
        .iter()
        .find(|a| a.id.name == name)
        .unwrap()
        .credential_reference
        .clone()
}

#[test]
#[serial]
fn a_stopped_session_stays_down_and_costs_no_docker_call() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle
        .runner()
        .seed(&container_name("alice"), true, false, Some(Path::new("/w")));
    let status = lifecycle.stop("alice", false).unwrap();
    assert!(status.held && !status.running);
    let calls_after_stop = lifecycle.runner().calls().len();
    let mut state = ReconcileState::default();
    for now in [0, 60, 600, 6_000] {
        assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, now), vec![Outcome::Held]);
    }
    assert_eq!(lifecycle.runner().calls().len(), calls_after_stop, "no docker call while held");
    assert_eq!(lifecycle.runner().count("create"), 0);
    assert_eq!(lifecycle.runner().count("start_existing"), 0);
}

#[test]
#[serial]
fn an_operator_start_lifts_the_hold_and_its_workspace_survives_a_daemon_restart() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle.stop("alice", false).unwrap();
    let started = lifecycle
        .start_with_workspace("alice", Some(Path::new("/home/u/GitHub")))
        .unwrap();
    assert!(!started.held);
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Running]);
    // The container disappears and the daemon restarts (fresh in-memory state):
    // it comes back on the operator's workspace and image, not the guess.
    lifecycle.runner().containers.lock().unwrap().clear();
    let mut state = ReconcileState::default();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 60),
        vec![Outcome::Recreated {
            workspace: "/home/u/GitHub".into()
        }]
    );
    let creates = lifecycle.runner().creates.lock().unwrap().clone();
    assert_eq!(creates.len(), 2);
    assert_eq!(creates[1].1, "example/session:pinned");
    assert_eq!(creates[1].2, Path::new("/home/u/GitHub"));
}

#[test]
#[serial]
fn a_pass_inspecting_between_stops_hold_and_rm_never_starts_the_container() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle
        .runner()
        .seed(&container_name("alice"), true, false, Some(Path::new("/w")));
    // `stop` writes its hold and `docker stop`s while the pass inspects.
    *lifecycle.runner().stop_on_inspect.lock().unwrap() = Some((1, profile(&env, "alice")));
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Held]);
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
}

#[test]
#[serial]
fn a_hold_written_during_the_reconcile_start_still_blocks_docker_start() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    // Crashed (stopped) container; the pass decides to resume it, and `stop`
    // lands between that decision and the start's own inspect.
    lifecycle
        .runner()
        .seed(&container_name("alice"), false, false, Some(Path::new("/w")));
    *lifecycle.runner().stop_on_inspect.lock().unwrap() = Some((2, profile(&env, "alice")));
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Held]);
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
    // The hold-respecting recreate path refuses outright, before any docker call.
    let calls = lifecycle.runner().calls().len();
    let error =
        recreate_container(&mut lifecycle, "alice", Path::new("/w"), None, &|| true).unwrap_err();
    assert!(error.downcast_ref::<OperatorHeld>().is_some(), "{error:#}");
    assert_eq!(lifecycle.runner().calls().len(), calls);
}

#[test]
#[serial]
fn a_hold_or_disable_in_any_root_holds_the_account_everywhere() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let mut state = ReconcileState::default();
    let fallback = Path::new("/srv/checkouts");
    // Another root lists alice disabled.
    let mut disabled = env.accounts.clone();
    disabled.iter_mut().for_each(|a| a.enabled = false);
    let out = pass_with(&mut lifecycle, &env.accounts, &disabled, &host, fallback, &mut state, 0);
    assert!(out.is_empty(), "{out:?}");
    // Another root resolves alice to a different profile directory holding a stop.
    let other = tempfile::tempdir().unwrap();
    session_hold::write_hold(other.path(), session_hold::now_unix_ms()).unwrap();
    let mut elsewhere = env.accounts.clone();
    elsewhere
        .iter_mut()
        .for_each(|a| a.credential_reference = other.path().to_path_buf());
    let out = pass_with(&mut lifecycle, &env.accounts, &elsewhere, &host, fallback, &mut state, 60);
    assert_eq!(out, vec![Outcome::Held]);
    assert_eq!(lifecycle.runner().calls(), Vec::<String>::new());
}

// ---- a start that does not stick (verdict blocker 2) -------------------------

#[test]
#[serial]
fn a_container_that_dies_after_every_start_is_restarted_on_the_backoff_schedule() {
    for restarting in [false, true] {
        let env = setup(&["alice"], &["alice"]);
        let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
        lifecycle
            .runner()
            .seed(&container_name("alice"), false, false, Some(Path::new("/w")));
        *lifecycle.runner().dies_after_start.lock().unwrap() = Some(restarting);
        let mut state = ReconcileState::default();
        let passes = 20;
        let mut outcomes = Vec::new();
        for tick in 0..passes {
            let now = tick * DEFAULT_SESSION_RECONCILE_INTERVAL_SECS;
            outcomes.extend(pass(&mut lifecycle, &env, &host, &mut state, now));
        }
        let starts = lifecycle.runner().count("start_existing");
        // Stopped: starts at t=0, 180, 480, 1020 (backoff 120, 240, 480 after
        // each dead start). Restarting: Docker owns the retries after the
        // first failed confirmation, so only the first start is ours.
        assert_eq!(starts, if restarting { 1 } else { 4 }, "{outcomes:?}");
        assert!(matches!(outcomes[1], Outcome::Failed { retry_at: 180, .. }), "{outcomes:?}");
        assert!(outcomes
            .iter()
            .any(|o| matches!(o, Outcome::BackingOff { .. })));
    }
}

// ---- Docker unavailable (verdict 8) -----------------------------------------

#[test]
#[serial]
fn docker_unavailable_skips_the_whole_pass_and_backs_off_the_pass_not_the_accounts() {
    let env = setup(&["alice", "bob"], &["alice", "bob"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    for name in ["alice", "bob"] {
        lifecycle
            .runner()
            .seed(&container_name(name), false, false, Some(Path::new("/w")));
    }
    *lifecycle.runner().fail_inspect.lock().unwrap() =
        Some("Cannot connect to the Docker daemon".into());
    let mut state = ReconcileState::default();
    let mut passes = Vec::new();
    for tick in 0..10 {
        passes.push(pass(&mut lifecycle, &env, &host, &mut state, tick * 60));
    }
    // The first read fails: that pass stops there (bob is never inspected).
    assert!(
        matches!(passes[0][..], [Outcome::DockerUnavailable { retry_at: 120, .. }]),
        "{passes:?}"
    );
    assert!(passes[1]
        .iter()
        .all(|o| matches!(o, Outcome::BackingOff { retry_at: 120 })));
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
    // Pass-level backoff: reads at t=0, 120, 360 only, one per pass.
    assert_eq!(lifecycle.runner().count("inspect"), 3);
    // Docker comes back: no per-account failure was counted, so both
    // stopped containers are resumed on the very next pass.
    *lifecycle.runner().fail_inspect.lock().unwrap() = None;
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 1_000),
        vec![Outcome::Resumed, Outcome::Resumed]
    );
}

// ---- workspace guess (verdict 3) --------------------------------------------

#[test]
#[serial]
fn a_guessed_workspace_of_root_is_refused_not_mounted() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let mut state = ReconcileState::default();
    let out = pass_with(&mut lifecycle, &env.accounts, &[], &host, Path::new("/"), &mut state, 0);
    let Outcome::Failed { error, .. } = &out[0] else {
        panic!("{out:?}")
    };
    assert!(error.contains("whole filesystem"), "{error}");
    assert_eq!(lifecycle.runner().count("create"), 0);
}

// ---- after-start probe bypasses the cache (verdict 5) ------------------------

#[test]
#[serial]
fn the_after_start_probe_ignores_a_recent_cached_probe() {
    use crate::tokens_pool::health::{record_probe_at, ProbeOutcome};
    let env = setup(&["alice"], &["alice"]);
    let lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle
        .runner()
        .seed(&container_name("alice"), true, false, Some(Path::new("/w")));
    let id = &env.accounts[0].id;
    record_probe_at(env.workspace.path(), id, ProbeOutcome::LoggedIn, "test", 1_000).unwrap();
    let cached = lifecycle.refresh_health_at(&env.accounts, 1_010).unwrap();
    assert_eq!(cached[0].effect, None);
    assert_eq!(lifecycle.runner().count("exec_capture"), 0);
    let fresh = lifecycle
        .refresh_health_with_ttl(&env.accounts, 1_010, 0)
        .unwrap();
    assert!(fresh[0].effect.is_some(), "{fresh:?}");
    assert_eq!(lifecycle.runner().count("exec_capture"), 1);
}

// ---- round 2 (verdict r2) ---------------------------------------------------

#[test]
#[serial]
fn a_clock_step_back_between_start_and_stop_does_not_drop_the_hold() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    lifecycle
        .start_with_workspace("alice", Some(Path::new("/w")))
        .unwrap();
    // The recorded start is 5 s in the future relative to the stop's clock.
    let dir = profile(&env, "alice");
    let mut start = session_hold::read_last_start(&dir).unwrap();
    start.started_at_unix_ms = session_hold::now_unix_ms() + 5_000;
    std::fs::write(dir.join(session_hold::LAST_START_FILE), serde_json::to_vec(&start).unwrap())
        .unwrap();
    assert!(lifecycle.stop("alice", false).unwrap().held);
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Held]);
    assert_eq!(lifecycle.runner().creates.lock().unwrap().len(), 1, "no recreate");
    assert!(lifecycle.status("alice").unwrap().held);
}

#[test]
#[serial]
fn stop_wins_the_race_with_a_reconcile_start_between_its_stop_and_rm() {
    let env = setup(&["alice"], &["alice"]);
    let lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let container = container_name("alice");
    // Dead container, so `docker stop` is instant and the race window open.
    lifecycle
        .runner()
        .seed(&container, false, false, Some(Path::new("/w")));
    *lifecycle.runner().start_races_stop.lock().unwrap() = true;
    let status = lifecycle.stop("alice", false).unwrap();
    assert!(status.held && !status.running && status.container_id.is_none());
    assert!(lifecycle.runner().containers.lock().unwrap().is_empty());
    assert_eq!(lifecycle.runner().count("stop_and_remove"), 2);
    // ...and the retry still honours an in-flight exec without --force.
    lifecycle
        .runner()
        .seed(&container, false, false, Some(Path::new("/w")));
    *lifecycle.runner().start_races_stop.lock().unwrap() = true;
    lifecycle
        .runner()
        .busy
        .lock()
        .unwrap()
        .insert(container.clone(), true);
    let error = lifecycle.stop("alice", false).unwrap_err().to_string();
    assert!(error.contains("in-flight"), "{error}");
    assert!(lifecycle.status("alice").unwrap().held, "the hold stays; nothing restarts it");
    lifecycle.stop("alice", true).unwrap();
    assert!(lifecycle.runner().containers.lock().unwrap().is_empty());
}

#[test]
#[serial]
fn a_timed_out_docker_start_ends_the_pass_like_an_unavailable_docker() {
    let env = setup(&["alice", "bob"], &["alice", "bob"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    for name in ["alice", "bob"] {
        lifecycle
            .runner()
            .seed(&container_name(name), false, false, Some(Path::new("/w")));
    }
    *lifecycle.runner().start_times_out.lock().unwrap() = true;
    let mut state = ReconcileState::default();
    let first = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert!(
        matches!(first[..], [Outcome::DockerUnavailable { retry_at: 120, .. }]),
        "{first:?}"
    );
    assert_eq!(lifecycle.runner().count("start_existing"), 1, "one budget per pass");
    let second = pass(&mut lifecycle, &env, &host, &mut state, 60);
    assert!(second
        .iter()
        .all(|o| matches!(o, Outcome::BackingOff { .. })));
    // No per-account failure was counted: once Docker answers, both resume.
    *lifecycle.runner().start_times_out.lock().unwrap() = false;
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 120),
        vec![Outcome::Resumed, Outcome::Resumed]
    );
}

#[test]
#[serial]
fn an_operator_start_is_never_left_running_and_held() {
    let env = setup(&["alice"], &["alice"]);
    let lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let dir = profile(&env, "alice");
    lifecycle.stop("alice", false).unwrap();
    // The hold cannot be lifted: the start fails before touching Docker, so
    // the session stays down and held.
    std::fs::remove_file(dir.join(session_hold::HOLD_FILE)).unwrap();
    std::fs::create_dir_all(dir.join(session_hold::HOLD_FILE).join("x")).unwrap();
    assert!(lifecycle.start("alice").is_err());
    assert_eq!(lifecycle.runner().count("create"), 0);
    assert!(lifecycle.status("alice").unwrap().held);
    std::fs::remove_dir_all(dir.join(session_hold::HOLD_FILE)).unwrap();
    // The start record cannot be written: the start still succeeds, running
    // and unheld (a warning, not an error).
    std::fs::create_dir_all(dir.join(session_hold::LAST_START_FILE).join("x")).unwrap();
    let status = lifecycle.start("alice").unwrap();
    assert!(status.running && !status.held, "{status:?}");
}
