//! Issue #10364 Part B: the reconcile pass recreates idle session containers
//! whose workspace mounts drifted from the registry, through the same fake
//! runner as the #10453 tests.

use super::*;
use crate::tokens_pool::session_state::MountDrift;

/// A checkout parent with real repository dirs (`workspace_mount_roots` only
/// mounts roots that exist), canonical so macOS `/var` vs `/private/var`
/// cannot split label and mounts.
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

    fn p(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn roots(&self, rels: &[&str]) -> Vec<PathBuf> {
        rels.iter().map(|r| self.p(r)).collect()
    }
}

/// A running container created for `ws` that bind-mounts `mounts`.
fn seed_running(lifecycle: &SessionLifecycle<Fake>, name: &str, ws: &Ws, mounts: &[&str]) {
    let container = container_name(name);
    let fake = lifecycle.runner();
    fake.seed(&container, true, false, Some(&ws.root));
    fake.mounts
        .lock()
        .unwrap()
        .insert(container, ws.roots(mounts));
}

fn register(lifecycle: &SessionLifecycle<Fake>, ws: &Ws, repos: &[&str]) {
    *lifecycle.runner().registered.lock().unwrap() = ws.roots(repos);
}

fn mounts_of(lifecycle: &SessionLifecycle<Fake>, name: &str) -> Vec<PathBuf> {
    lifecycle.runner().mounts.lock().unwrap()[&container_name(name)].clone()
}

fn missing(paths: Vec<PathBuf>) -> MountDrift {
    MountDrift {
        missing: paths,
        extra: Vec::new(),
    }
}

#[test]
#[serial]
fn an_idle_drifted_container_is_recreated_with_the_current_roots() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    // Created before `new` was registered.
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    let mut state = ReconcileState::default();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 0),
        vec![Outcome::DriftRecreated {
            workspace: ws.root.clone(),
            drift: missing(ws.roots(&["new"])),
        }]
    );
    assert_eq!(lifecycle.runner().mutations(), ["stop_and_remove", "create"]);
    // The pre-action re-checks ran: a fresh inspect and `docker top`.
    assert_eq!(lifecycle.runner().count("has_active_exec"), 1);
    let creates = lifecycle.runner().creates.lock().unwrap().clone();
    assert_eq!(creates[0].2, ws.root);
    assert_eq!(creates[0].1, "example/session:pinned", "the container's own image");
    assert_eq!(mounts_of(&lifecycle, "alice"), ws.roots(&["a", "new"]));
    // Confirmed on the next pass, and left alone after that.
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 60), vec![Outcome::Running]);
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 120), vec![Outcome::Running]);
    assert_eq!(lifecycle.runner().count("create"), 1);
}

#[test]
#[serial]
fn a_busy_drifted_container_is_deferred_then_recreated_once_idle() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    let container = container_name("alice");
    lifecycle
        .runner()
        .busy
        .lock()
        .unwrap()
        .insert(container.clone(), true);
    let mut state = ReconcileState::default();
    let busy = vec![Outcome::DriftDeferred {
        reason: DeferReason::Busy,
    }];
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), busy);
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 60), busy);
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new(), "never killed");
    // Re-checked every pass (no backoff), recreated on the first idle one.
    lifecycle
        .runner()
        .busy
        .lock()
        .unwrap()
        .insert(container, false);
    let out = pass(&mut lifecycle, &env, &host, &mut state, 120);
    assert!(matches!(out[..], [Outcome::DriftRecreated { .. }]), "{out:?}");
    assert_eq!(lifecycle.runner().mutations(), ["stop_and_remove", "create"]);
}

#[test]
#[serial]
fn extra_drift_is_recreated_before_missing_only_drift_and_warned_by_path() {
    let env = setup(&["alice", "bob"], &["alice", "bob"]);
    let ws = Ws::new(&["a", "new", "gone"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    // alice only lacks `new`; bob still mounts the deregistered `gone`.
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    seed_running(&lifecycle, "bob", &ws, &["a", "new", "gone"]);
    register(&lifecycle, &ws, &["a", "new"]);
    let mut state = ReconcileState::default();
    let out = pass_named(
        &mut lifecycle,
        &env.accounts,
        &[],
        &host,
        Path::new("/srv/checkouts"),
        &mut state,
        0,
    );
    let names: Vec<&str> = out.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["bob", "alice"], "{out:?}");
    assert_eq!(
        out[0].outcome,
        Outcome::DriftRecreated {
            workspace: ws.root.clone(),
            drift: MountDrift {
                missing: Vec::new(),
                extra: ws.roots(&["gone"]),
            },
        }
    );
    let creates = lifecycle.runner().creates.lock().unwrap().clone();
    let order: Vec<&str> = creates.iter().map(|c| c.0.as_str()).collect();
    assert_eq!(order, [container_name("bob"), container_name("alice")]);
    assert_eq!(mounts_of(&lifecycle, "bob"), ws.roots(&["a", "new"]));
}

#[test]
#[serial]
fn a_held_account_is_never_drift_recreated() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    session_hold::write_hold(&profile(&env, "alice"), session_hold::now_unix_ms()).unwrap();
    let mut state = ReconcileState::default();
    for now in [0, 60, 120] {
        assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, now), vec![Outcome::Held]);
    }
    assert_eq!(lifecycle.runner().calls(), Vec::<String>::new(), "no docker call at all");
}

#[test]
#[serial]
fn a_private_clone_session_is_never_drift_recreated() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    register(&lifecycle, &ws, &["a", "new"]);
    let container = container_name("alice");
    // A private-clone container (labelled so) with a stray bind.
    lifecycle
        .runner()
        .seed(&container, true, false, Some(Path::new(private_workspace::REPO)));
    lifecycle
        .runner()
        .mounts
        .lock()
        .unwrap()
        .insert(container.clone(), ws.roots(&["a"]));
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Running]);
    // A host-labelled container on an account configured private-clone (or
    // whose mode cannot be ruled out).
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    let private = |_: &AccountDescriptor| -> anyhow::Result<bool> { Ok(true) };
    let unknown = |_: &AccountDescriptor| -> anyhow::Result<bool> { bail!("bad state") };
    assert_eq!(
        pass(&mut lifecycle, &env, &private, &mut state, 60),
        vec![Outcome::PrivateCloneSkipped]
    );
    assert_eq!(
        pass(&mut lifecycle, &env, &unknown, &mut state, 120),
        vec![Outcome::PrivateCloneSkipped]
    );
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
    assert_eq!(lifecycle.runner().count("has_active_exec"), 0);
}

#[test]
#[serial]
fn an_unavailable_snapshot_takes_no_action() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    *lifecycle.runner().fail_inspect.lock().unwrap() =
        Some("Cannot connect to the Docker daemon".into());
    let mut state = ReconcileState::default();
    let out = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert!(matches!(out[..], [Outcome::DockerUnavailable { .. }]), "{out:?}");
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
    assert_eq!(lifecycle.runner().count("has_active_exec"), 0);
}

#[test]
#[serial]
fn a_recreate_that_still_drifts_backs_off_instead_of_looping() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new", "other"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    // Whatever is intended, a new container only ever gets `a`.
    *lifecycle.runner().create_mounts.lock().unwrap() = Some(ws.roots(&["a"]));
    let mut state = ReconcileState::default();
    let out = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert!(matches!(out[..], [Outcome::DriftRecreated { .. }]), "{out:?}");
    for tick in 1..10 {
        assert_eq!(
            pass(&mut lifecycle, &env, &host, &mut state, tick * 60),
            vec![Outcome::DriftUnachievable],
            "tick {tick}"
        );
    }
    assert_eq!(lifecycle.runner().count("create"), 1, "one attempt, not one per pass");
    assert_eq!(lifecycle.runner().count("stop_and_remove"), 1);
    // The registry changes, so the drift does: one more attempt.
    register(&lifecycle, &ws, &["a", "new", "other"]);
    let out = pass(&mut lifecycle, &env, &host, &mut state, 600);
    assert!(matches!(out[..], [Outcome::DriftRecreated { .. }]), "{out:?}");
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 660),
        vec![Outcome::DriftUnachievable]
    );
    assert_eq!(lifecycle.runner().count("create"), 2);
}

#[test]
#[serial]
fn a_timed_out_teardown_backs_off_the_pass_not_the_account() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    *lifecycle.runner().stop_times_out.lock().unwrap() = true;
    let mut state = ReconcileState::default();
    let out = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert!(matches!(out[..], [Outcome::DockerUnavailable { retry_at: 120, .. }]), "{out:?}");
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 60),
        vec![Outcome::BackingOff { retry_at: 120 }]
    );
    // No per-account failure was counted: once Docker answers, it is recreated.
    *lifecycle.runner().stop_times_out.lock().unwrap() = false;
    let out = pass(&mut lifecycle, &env, &host, &mut state, 120);
    assert!(matches!(out[..], [Outcome::DriftRecreated { .. }]), "{out:?}");
}

#[test]
#[serial]
fn a_missing_only_drift_whose_recreate_would_be_refused_is_left_running() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    // It only lacks `new`, and the recorded operator start names `/`, which
    // no recreate accepts. Nothing it mounts is `extra`, so removing it
    // would only lose capacity: it is left running.
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    session_hold::record_operator_start(
        &profile(&env, "alice"),
        Path::new("/"),
        "example/session:recorded",
        session_hold::now_unix_ms(),
    )
    .unwrap();
    let mut state = ReconcileState::default();
    for now in [0, 60, 120] {
        assert_eq!(
            pass(&mut lifecycle, &env, &host, &mut state, now),
            vec![Outcome::DriftUnachievable]
        );
    }
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
    assert_eq!(lifecycle.runner().count("has_active_exec"), 0);
}

#[test]
#[serial]
fn a_still_registered_repo_that_became_firewalled_is_extra_and_recreated_first() {
    let env = setup(&["alice", "bob"], &["alice", "bob"]);
    let ws = Ws::new(&["a", "new", "walled"]);
    let single = Ws::new(&["repo/.git"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    // alice only lacks `new`. bob mounts exactly what the registry lists,
    // but `walled` is `firewall: true` now.
    seed_running(&lifecycle, "alice", &ws, &["a", "walled"]);
    seed_running(&lifecycle, "bob", &ws, &["a", "new", "walled"]);
    register(&lifecycle, &ws, &["a", "new", "walled"]);
    let fake = lifecycle.runner().clone();
    let snapshot = fake.snapshot(&ws.roots(&["a", "new", "walled"]));
    let bob = snapshot.inspect_of(&container_name("bob")).unwrap();
    let registered = ws.roots(&["a", "new", "walled"]);
    let assess =
        |inspect: &Value, fake: &Fake| drift::assess(inspect, &registered, Some(&fake.denials()));
    assert_eq!(assess(bob, &fake), drift::Assessed::default());
    *fake.firewalled.lock().unwrap() = ws.roots(&["walled"]);
    assert_eq!(
        assess(bob, &fake),
        drift::Assessed {
            drift: MountDrift {
                missing: Vec::new(),
                extra: ws.roots(&["walled"]),
            },
            denied: ws.roots(&["walled"]),
        }
    );
    // An unreadable roster denies nothing (and, separately, accepts nothing).
    assert_eq!(drift::assess(bob, &registered, None), drift::Assessed::default());
    // A single unregistered checkout is an explicit grant, not drift.
    let solo = inspect_json(
        "loom-codex-session-solo",
        &st(true, false, Some(single.p("repo").to_str().unwrap())),
        &single.roots(&["repo"]),
    );
    assert_eq!(assess(&solo, &fake), drift::Assessed::default());
    // The roster marks `walled` but the registry still lists it: no start is
    // accepted, so both idle containers that mount it are removed, not
    // recreated, and they stay down while that denial stands.
    let mut state = ReconcileState::default();
    let out = pass(&mut lifecycle, &env, &host, &mut state, 0);
    assert!(
        matches!(out[..], [Outcome::DriftRemoved { .. }, Outcome::DriftRemoved { .. }]),
        "{out:?}"
    );
    for removed in &out {
        let Outcome::DriftRemoved { drift } = removed else {
            unreachable!()
        };
        assert_eq!(drift.extra, ws.roots(&["walled"]), "names the denied path");
    }
    assert_eq!(lifecycle.runner().mutations(), ["stop_and_remove", "stop_and_remove"]);
    for now in [60, 120, 180] {
        assert_eq!(
            pass(&mut lifecycle, &env, &host, &mut state, now),
            vec![Outcome::DriftRemovalStands, Outcome::DriftRemovalStands]
        );
    }
    assert_eq!(lifecycle.runner().count("create"), 0, "never recreated meanwhile");
    assert!(lifecycle.runner().containers.lock().unwrap().is_empty());
    // The operator deregisters it: both come back without it.
    register(&lifecycle, &ws, &["a", "new"]);
    let out = pass(&mut lifecycle, &env, &host, &mut state, 10_000);
    assert!(out.iter().all(Outcome::started), "{out:?}");
    assert_eq!(mounts_of(&lifecycle, "bob"), ws.roots(&["a", "new"]));
}

#[test]
#[serial]
fn a_denied_mount_with_nothing_accepted_in_its_place_is_removed_only_when_idle() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "walled"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_running(&lifecycle, "alice", &ws, &["a", "walled"]);
    register(&lifecycle, &ws, &["a", "walled"]);
    *lifecycle.runner().firewalled.lock().unwrap() = ws.roots(&["walled"]);
    let container = container_name("alice");
    lifecycle
        .runner()
        .busy
        .lock()
        .unwrap()
        .insert(container.clone(), true);
    let mut state = ReconcileState::default();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 0),
        vec![Outcome::DriftDeferred {
            reason: DeferReason::Busy
        }]
    );
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new(), "never killed");
    lifecycle
        .runner()
        .busy
        .lock()
        .unwrap()
        .insert(container, false);
    // The outcome (and the WARN built from the same list) names the path.
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 60),
        vec![Outcome::DriftRemoved {
            drift: MountDrift {
                missing: Vec::new(),
                extra: ws.roots(&["walled"]),
            },
        }]
    );
    assert_eq!(lifecycle.runner().mutations(), ["stop_and_remove"]);
}

#[test]
#[serial]
fn after_a_daemon_restart_the_recorded_workspace_and_image_are_used() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    // The operator's last `session start` (on disk, so it outlives the
    // daemon) named a different workspace and image than the label.
    session_hold::record_operator_start(
        &profile(&env, "alice"),
        &ws.p("a"),
        "example/session:recorded",
        session_hold::now_unix_ms(),
    )
    .unwrap();
    // A fresh daemon: nothing in memory.
    let mut state = ReconcileState::default();
    assert_eq!(
        pass(&mut lifecycle, &env, &host, &mut state, 0),
        vec![Outcome::DriftRecreated {
            workspace: ws.p("a"),
            drift: missing(ws.roots(&["new"])),
        }]
    );
    let creates = lifecycle.runner().creates.lock().unwrap().clone();
    assert_eq!(creates[0].1, "example/session:recorded");
    assert_eq!(creates[0].2, ws.p("a"));
}

#[test]
#[serial]
fn a_container_replaced_since_the_snapshot_is_left_for_the_next_pass() {
    let env = setup(&["alice"], &["alice"]);
    let ws = Ws::new(&["a", "new"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_running(&lifecycle, "alice", &ws, &["a"]);
    register(&lifecycle, &ws, &["a", "new"]);
    // The pass's snapshot is taken, then the container is replaced (an
    // operator recreate) before the pass's fresh pre-action inspect.
    let container = container_name("alice");
    let fake = lifecycle.runner().clone();
    let snapshot = fake.snapshot(&fake.registered.lock().unwrap().clone());
    fake.containers
        .lock()
        .unwrap()
        .get_mut(&container)
        .unwrap()
        .id = "replaced".into();
    let index = AccountIndex::from_inventories(&[&env.accounts]);
    let registered = fake.registered.lock().unwrap().clone();
    let mut take = || snapshot.clone();
    let mut observe = PassSnapshot::new(&mut take);
    let inputs = fake_inputs(&fake, &index, &host, Path::new("/srv/checkouts"), &registered);
    let mut state = ReconcileState::default();
    let out =
        reconcile_accounts(&mut lifecycle, &env.accounts, &inputs, &mut observe, &mut state, 0);
    assert_eq!(
        out[0].outcome,
        Outcome::DriftDeferred {
            reason: DeferReason::Changed
        }
    );
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
}

#[test]
fn container_state_reads_an_inspect_object() {
    let restarting = serde_json::json!({
        "Id": "abc",
        "State": {"Running": true, "Restarting": true, "StartedAt": "2026-10-06T00:00:00Z"},
        "Config": {"Image": "img", "Labels": {WORKSPACE_LABEL: "/w"}},
    });
    let state = container_state(&restarting);
    assert_eq!(state.id, "abc");
    assert!(!state.running && state.restarting);
    assert_eq!(state.image.as_deref(), Some("img"));
    assert_eq!(state.workspace.as_deref(), Some(Path::new("/w")));
    let up = serde_json::json!({"Id": "x", "State": {"Running": true}, "Config": {}});
    let state = container_state(&up);
    assert!(state.running && !state.restarting);
    assert_eq!(state.workspace, None);
}

/// #10661: a removal record beside a healthy container is WARNed about on a
/// doubling cadence capped at about a day, and a fresh count (a daemon
/// restart) WARNs at once.
#[test]
fn a_record_beside_a_healthy_container_is_warned_about_at_least_daily() {
    use drift::{healthy_record_warn_due as due, HEALTHY_RECORD_WARN_MAX_SECS as DAY};
    let minute = DEFAULT_SESSION_RECONCILE_INTERVAL_SECS;
    // Pass 1 after any start, restart included: no previous WARN.
    assert!(due(1, None, 0));
    let warned: Vec<u32> = (1..=40)
        .filter(|&pass| due(pass, Some(0), u64::from(pass) * minute))
        .collect();
    assert_eq!(warned, vec![1, 2, 4, 8, 16, 32], "the backoff while it is young");
    // Simulate the real loop at one pass a minute for ten days: no gap
    // between WARNs exceeds a day (uncapped, pass 2048 is ~34 h after 1024).
    let (mut last, mut gaps) = (None, Vec::new());
    for pass in 1..=(10 * DAY / minute) as u32 {
        let now = u64::from(pass) * minute;
        if due(pass, last, now) {
            if let Some(at) = last {
                gaps.push(now - at);
            }
            last = Some(now);
        }
    }
    assert!(gaps.iter().all(|&gap| gap <= DAY), "{gaps:?}");
    assert_eq!(gaps.len(), 20, "{gaps:?}");
    // A wall clock stepping back never makes it due early, nor silences it.
    assert!(!due(3, Some(1_000), 10));
}
