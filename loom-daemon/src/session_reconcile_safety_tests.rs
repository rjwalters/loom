//! Issue #10364 Part B, the safety rules: a drift teardown needs a
//! positively established reason, the pre-check is the check `create` makes,
//! a removal is never repeated, and a dispatch that is starting is never
//! stopped. Each test here reproduced a way to lose a running container.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::tokens_pool::{session_dispatch_lock, session_drift_removal};
use crate::workspace_registry::REGISTRY_PATH_ENV;

/// Set an environment variable for the guard's scope (tests here are
/// `#[serial]`).
struct EnvVar {
    key: &'static str,
    prior: Option<OsString>,
}

impl EnvVar {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prior = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prior }
    }
}

impl Drop for EnvVar {
    fn drop(&mut self) {
        match &self.prior {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

/// A real `docker` stand-in on `PATH` for `run_tick`: one running host-mode
/// container for `account`, created for `ws` and mounting `mounts`. Every
/// invocation's subcommand is appended to the returned log.
struct FakeDocker {
    dir: tempfile::TempDir,
    _path: EnvVar,
}

impl FakeDocker {
    fn new(account: &str, ws: &Path, mounts: &[PathBuf]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let container = container_name(account);
        let binds: Vec<Value> = mounts
            .iter()
            .map(|m| serde_json::json!({"Type": "bind", "Source": m, "Destination": m, "RW": true}))
            .collect();
        let inspect = serde_json::json!([{
            "Name": format!("/{container}"),
            "Id": "abc",
            "State": {"Running": true, "Restarting": false},
            "Config": {"Image": "img", "Labels": {WORKSPACE_LABEL: ws}},
            "Mounts": binds,
        }]);
        std::fs::write(dir.path().join("inspect.json"), inspect.to_string()).unwrap();
        let script = format!(
            "#!/bin/sh\necho \"$1\" >> '{dir}/calls'\ncase \"$1\" in\n  ps) echo {container} ;;\n  \
             inspect) if [ \"$2\" = --format ]; then printf 'abc\\ttrue\\tfalse\\t-\\timg\\t{ws}\\n'; \
             else cat '{dir}/inspect.json'; fi ;;\n  top) printf 'PID COMMAND\\n1 sleep infinity\\n' ;;\n\
             esac\n",
            dir = dir.path().display(),
            ws = ws.display(),
        );
        let docker = dir.path().join("docker");
        std::fs::write(&docker, script).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path =
            format!("{}:{}", dir.path().display(), std::env::var("PATH").unwrap_or_default());
        Self {
            _path: EnvVar::set("PATH", path),
            dir,
        }
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn ws_with(repos: &[&str]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    for repo in repos {
        std::fs::create_dir_all(root.join(repo)).unwrap();
    }
    (dir, root)
}

/// `run_tick` against a healthy container (it mounts exactly `a` and `b`)
/// with the registry file holding `registry` (`None`: no file).
fn tick_with_registry(registry: Option<&str>) -> (Vec<Outcome>, Vec<String>) {
    let env = setup(&["alice"], &["alice"]);
    let (_ws_dir, ws) = ws_with(&["a", "b"]);
    let registry_dir = tempfile::tempdir().unwrap();
    let registry_path = registry_dir.path().join("workspaces.json");
    if let Some(contents) = registry {
        let contents = contents.replace("WS", &ws.display().to_string());
        std::fs::write(&registry_path, contents).unwrap();
    }
    let _registry = EnvVar::set(REGISTRY_PATH_ENV, &registry_path);
    let _locks = EnvVar::set(session_dispatch_lock::LOCK_DIR_ENV, registry_dir.path());
    let docker = FakeDocker::new("alice", &ws, &[ws.join("a"), ws.join("b")]);
    let mut state = ReconcileState::default();
    let outcomes = run_tick(env.workspace.path(), &mut state, 0)
        .into_iter()
        .map(|o| o.outcome)
        .collect();
    (outcomes, docker.calls())
}

fn never_stopped(calls: &[String]) {
    assert!(
        !calls.iter().any(|c| c == "stop" || c == "rm" || c == "run"),
        "the container must be left alone: {calls:?}"
    );
}

#[test]
#[serial]
fn an_unparseable_registry_leaves_every_container_running() {
    let (outcomes, calls) = tick_with_registry(Some("{ this is not json"));
    never_stopped(&calls);
    assert_eq!(outcomes, vec![Outcome::Running], "{calls:?}");
}

#[test]
#[serial]
fn a_missing_registry_file_leaves_every_container_running() {
    let (outcomes, calls) = tick_with_registry(None);
    never_stopped(&calls);
    assert!(!outcomes.iter().any(Outcome::started), "{outcomes:?}");
}

#[test]
#[serial]
fn an_empty_valid_registry_leaves_every_container_running() {
    let (outcomes, calls) = tick_with_registry(Some(r#"{"version":1,"workspaces":[]}"#));
    never_stopped(&calls);
    assert!(!outcomes.iter().any(Outcome::started), "{outcomes:?}");
}

/// The pass's pre-check says a still-registered repo is firewalled; `create`
/// (reading something else) does not. Whatever the two say, a removal is
/// made at most once and nothing is recreated while the denial stands.
#[test]
#[serial]
fn a_denial_create_does_not_share_never_cycles_remove_and_recreate() {
    let env = setup(&["alice"], &["alice"]);
    let (_ws_dir, ws) = ws_with(&["a", "walled"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let container = container_name("alice");
    let fake = lifecycle.runner().clone();
    fake.seed(&container, true, false, Some(&ws));
    let mounts = vec![ws.join("a"), ws.join("walled")];
    fake.mounts
        .lock()
        .unwrap()
        .insert(container, mounts.clone());
    *fake.registered.lock().unwrap() = mounts;
    *fake.firewalled.lock().unwrap() = vec![ws.join("walled")];
    *fake.create_firewalled.lock().unwrap() = Some(Vec::new());
    let mut state = ReconcileState::default();
    let mut outcomes = Vec::new();
    for tick in 0..20 {
        outcomes.extend(pass(&mut lifecycle, &env, &host, &mut state, tick * 60));
    }
    assert!(fake.count("stop_and_remove") <= 1, "{outcomes:?}");
    assert_eq!(fake.count("create"), 0, "{outcomes:?}");
}

/// alice's container mounts `a` only; `a` and `new` are registered.
fn drifted_missing_new() -> (Env, tempfile::TempDir, PathBuf, SessionLifecycle<Fake>) {
    let env = setup(&["alice"], &["alice"]);
    let (ws_dir, ws) = ws_with(&["a", "new"]);
    let lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let container = container_name("alice");
    let fake = lifecycle.runner();
    fake.seed(&container, true, false, Some(&ws));
    fake.mounts
        .lock()
        .unwrap()
        .insert(container, vec![ws.join("a")]);
    *fake.registered.lock().unwrap() = vec![ws.join("a"), ws.join("new")];
    (env, ws_dir, ws, lifecycle)
}

// ---- rule 4: a dispatch is never stopped, even one that is only starting ----

/// A dispatch that starts right after `docker top` answered "idle" (the
/// preflight inspect, or the gap before its worker exec) must not be stopped.
#[test]
#[serial]
fn a_dispatch_starting_right_after_the_idle_check_is_not_stopped() {
    let (env, _ws_dir, _ws, mut lifecycle) = drifted_missing_new();
    let fake = lifecycle.runner().clone();
    // `session-exec host` takes the container's lock shared before its
    // first inspect and keeps it until its worker exec has exited.
    let dispatch = Arc::new(Mutex::new(None));
    let (held, dir, name) = (Arc::clone(&dispatch), fake.lock_dir(), container_name("alice"));
    *fake.after_top.lock().unwrap() = Some(Box::new(move || {
        *held.lock().unwrap() = session_dispatch_lock::shared(&dir, &name, Duration::ZERO);
    }));
    let mut state = ReconcileState::default();
    let busy = vec![Outcome::DriftDeferred {
        reason: DeferReason::Busy,
    }];
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), busy);
    assert!(dispatch.lock().unwrap().is_some(), "the dispatch got its lock");
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 60), busy);
    assert_eq!(fake.mutations(), Vec::<String>::new(), "never stopped under a dispatch");
    // The dispatch ends (its process exits, the kernel drops the lock).
    *fake.after_top.lock().unwrap() = None;
    *dispatch.lock().unwrap() = None;
    let out = pass(&mut lifecycle, &env, &host, &mut state, 120);
    assert!(matches!(out[..], [Outcome::DriftRecreated { .. }]), "{out:?}");
}

/// When the lock cannot be created, the state is unknown: never stop.
#[test]
#[serial]
fn an_unusable_dispatch_lock_defers_the_teardown() {
    let (env, _ws_dir, _ws, mut lifecycle) = drifted_missing_new();
    let fake = lifecycle.runner().clone();
    let blocker = tempfile::NamedTempFile::new().unwrap();
    *fake.lock_dir.lock().unwrap() = Some(blocker.path().join("under-a-file"));
    let mut state = ReconcileState::default();
    for now in [0, 60] {
        assert_eq!(
            pass(&mut lifecycle, &env, &host, &mut state, now),
            vec![Outcome::DriftDeferred {
                reason: DeferReason::LockUnknown
            }]
        );
    }
    assert_eq!(fake.mutations(), Vec::<String>::new());
}

// ---- rule 1: missing information means no action ---------------------------

#[test]
#[serial]
fn an_unreadable_registry_makes_no_drift_decision() {
    let (env, _ws_dir, _ws, mut lifecycle) = drifted_missing_new();
    let fake = lifecycle.runner().clone();
    *fake.registry_unreadable.lock().unwrap() = true;
    // What an unreadable registry looks like to the pass: nothing registered.
    fake.registered.lock().unwrap().clear();
    let mut state = ReconcileState::default();
    for now in [0, 60, 120] {
        assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, now), vec![Outcome::Running]);
    }
    assert_eq!(fake.mutations(), Vec::<String>::new());
    assert_eq!(fake.count("has_active_exec"), 0);
}

#[test]
#[serial]
fn a_registry_with_nothing_under_the_workspace_never_removes() {
    let (env, _ws_dir, _ws, mut lifecycle) = drifted_missing_new();
    let fake = lifecycle.runner().clone();
    // Readable, but empty: every mount is `extra`, and nothing is intended.
    fake.registered.lock().unwrap().clear();
    let mut state = ReconcileState::default();
    for now in [0, 60, 120] {
        assert_eq!(
            pass(&mut lifecycle, &env, &host, &mut state, now),
            vec![Outcome::DriftDeferred {
                reason: DeferReason::NothingIntended
            }]
        );
    }
    assert_eq!(fake.mutations(), Vec::<String>::new());
}

#[test]
#[serial]
fn a_registered_root_whose_directory_is_missing_right_now_is_not_extra() {
    let (env, _ws_dir, ws, mut lifecycle) = drifted_missing_new();
    let fake = lifecycle.runner().clone();
    // The container mounts both; `new`'s volume is briefly unavailable.
    let both = vec![ws.join("a"), ws.join("new")];
    fake.mounts
        .lock()
        .unwrap()
        .insert(container_name("alice"), both);
    std::fs::remove_dir(ws.join("new")).unwrap();
    let mut state = ReconcileState::default();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 0),
        vec![Outcome::DriftDeferred {
            reason: DeferReason::RootUnavailable
        }]
    );
    assert_eq!(fake.mutations(), Vec::<String>::new());
    std::fs::create_dir(ws.join("new")).unwrap();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 60), vec![Outcome::Running]);
}

/// The reverse mismatch: with the roster unreadable `create` fails closed,
/// so the pre-check must refuse too, and a container that only lacks a
/// mount is not torn down into nothing. Nor is one with a deregistered
/// mount: unknown is not denied.
#[test]
#[serial]
fn an_unreadable_roster_tears_nothing_down() {
    for mounts in [vec!["a"], vec!["a", "new", "gone"]] {
        let (env, _ws_dir, ws, mut lifecycle) = drifted_missing_new();
        let fake = lifecycle.runner().clone();
        std::fs::create_dir_all(ws.join("gone")).unwrap();
        let mounts = mounts.iter().map(|m| ws.join(m)).collect();
        fake.mounts
            .lock()
            .unwrap()
            .insert(container_name("alice"), mounts);
        *fake.roster_unreadable.lock().unwrap() = true;
        let mut state = ReconcileState::default();
        for now in [0, 60, 120] {
            assert_eq!(
                pass(&mut lifecycle, &env, &host, &mut state, now),
                vec![Outcome::DriftUnachievable]
            );
        }
        assert_eq!(fake.mutations(), Vec::<String>::new());
        assert_eq!(fake.count("has_active_exec"), 0);
    }
}

// ---- rule 3: a removal is recorded, never repeated, cleared by an operator --

fn removed_for_a_firewalled_repo() -> (Env, tempfile::TempDir, PathBuf, SessionLifecycle<Fake>) {
    let env = setup(&["alice"], &["alice"]);
    let (ws_dir, ws) = ws_with(&["a", "walled"]);
    let lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let container = container_name("alice");
    let fake = lifecycle.runner();
    fake.seed(&container, true, false, Some(&ws));
    let mounts = vec![ws.join("a"), ws.join("walled")];
    fake.mounts
        .lock()
        .unwrap()
        .insert(container, mounts.clone());
    *fake.registered.lock().unwrap() = mounts;
    *fake.firewalled.lock().unwrap() = vec![ws.join("walled")];
    (env, ws_dir, ws, lifecycle)
}

#[test]
#[serial]
fn a_removal_survives_a_daemon_restart_and_a_reappearing_container_is_not_removed_again() {
    let (env, _ws_dir, ws, mut lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    let out = pass(&mut lifecycle, &env, &host, &mut ReconcileState::default(), 0);
    assert!(matches!(out[..], [Outcome::DriftRemoved { .. }]), "{out:?}");
    let record = session_drift_removal::read(&[profile(&env, "alice")]).unwrap();
    assert_eq!(record.denied, vec![ws.join("walled")]);
    // A fresh daemon (no memory) still does not recreate it.
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut ReconcileState::default(), 60),
        vec![Outcome::DriftRemovalStands]
    );
    // Something that does not share the verdict brings it back, denied mount
    // and all: it is reported, not removed a second time.
    let container = container_name("alice");
    fake.seed(&container, true, false, Some(&ws));
    let mounts = vec![ws.join("a"), ws.join("walled")];
    fake.mounts.lock().unwrap().insert(container, mounts);
    let mut state = ReconcileState::default();
    for now in [120, 180] {
        assert_eq!(
            pass(&mut lifecycle, &env, &host, &mut state, now),
            vec![Outcome::DriftUnachievable]
        );
    }
    assert_eq!(fake.count("stop_and_remove"), 1);
    assert_eq!(fake.count("create"), 0);
}

#[test]
#[serial]
fn an_operator_start_clears_the_removal_and_the_reconciler_never_writes_a_hold() {
    let (env, _ws_dir, ws, mut lifecycle) = removed_for_a_firewalled_repo();
    let fake = lifecycle.runner().clone();
    let mut state = ReconcileState::default();
    pass(&mut lifecycle, &env, &host, &mut state, 0);
    let dir = profile(&env, "alice");
    assert!(dir.join(session_drift_removal::REMOVED_FILE).exists());
    assert!(!dir.join(session_hold::HOLD_FILE).exists(), "a removal is not an operator hold");
    assert!(!lifecycle.status("alice").unwrap().held);
    // The operator deregisters the repo and starts the session by hand.
    *fake.registered.lock().unwrap() = vec![ws.join("a")];
    lifecycle.start_with_workspace("alice", Some(&ws)).unwrap();
    assert!(!dir.join(session_drift_removal::REMOVED_FILE).exists());
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 60), vec![Outcome::Running]);
}

/// The drift classes are walked across every root: an `extra`-drift account
/// in a later root goes before a missing-only one in an earlier root.
#[test]
#[serial]
fn extra_drift_goes_first_across_roots() {
    let env = setup(&["alice", "bob"], &["alice", "bob"]);
    let (_ws_dir, ws) = ws_with(&["a", "new", "gone"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let fake = lifecycle.runner().clone();
    for (name, mounts) in [("alice", vec!["a"]), ("bob", vec!["a", "new", "gone"])] {
        let container = container_name(name);
        fake.seed(&container, true, false, Some(&ws));
        let mounts = mounts.iter().map(|m| ws.join(m)).collect();
        fake.mounts.lock().unwrap().insert(container, mounts);
    }
    let registered = vec![ws.join("a"), ws.join("new")];
    *fake.registered.lock().unwrap() = registered.clone();
    let index = AccountIndex::from_inventories(&[&env.accounts]);
    let mut take = || fake.snapshot(&registered);
    let mut observe = PassSnapshot::new(&mut take);
    let mut state = ReconcileState::default();
    // Two "roots", one account each, alice's first.
    let by_name = |name: &str| -> Vec<AccountDescriptor> {
        let named = env.accounts.iter().filter(|a| a.id.name == name);
        named.cloned().collect()
    };
    let roots = [by_name("alice"), by_name("bob")];
    let mut order = Vec::new();
    for class in 0..=drift::LAST_CLASS {
        for accounts in &roots {
            let inputs = PassInputs {
                class: Some(class),
                ..fake_inputs(&fake, &index, &host, Path::new("/srv/checkouts"), &registered)
            };
            let out =
                reconcile_accounts(&mut lifecycle, accounts, &inputs, &mut observe, &mut state, 0);
            order.extend(out.into_iter().map(|o| o.name));
        }
    }
    assert_eq!(order, ["bob", "alice"]);
    let creates = fake.creates.lock().unwrap().clone();
    assert_eq!(creates[0].0, container_name("bob"));
}
