//! Issue #11727 Slice 2: the reconcile pass recreates an idle session
//! container whose `loom-daemon` is older than the host's, from an image
//! pinned to the host version, through the same fake runner as the drift
//! tests. Fake skewed containers: "after a host roll, no seat runs an older
//! daemon than the host's for longer than one idle window".

use super::*;
use skew::{pinned_image, refuse_downgrade, DaemonVersion};

fn v(text: &str) -> DaemonVersion {
    DaemonVersion::parse(text).unwrap()
}

fn seed(lifecycle: &SessionLifecycle<Fake>, host_ver: Option<&str>, container_ver: Option<&str>) {
    let fake = lifecycle.runner();
    fake.seed(&container_name("alice"), true, false, Some(Path::new("/srv/checkouts")));
    fake.mounts
        .lock()
        .unwrap()
        .insert(container_name("alice"), Vec::new());
    *fake.host_version.lock().unwrap() = host_ver.map(v);
    if let Some(ver) = container_ver {
        fake.daemon_versions.lock().unwrap().insert(
            container_name("alice"),
            format!("loom-daemon {ver} (commit 2d5fe17, built 2026-10-13T19:01:02Z)"),
        );
    }
}

fn skew_pass(
    lifecycle: &mut SessionLifecycle<Fake>,
    env: &Env,
    state: &mut ReconcileState,
    now: u64,
) -> Vec<Outcome> {
    pass(lifecycle, env, &host, state, now)
}

#[test]
fn versions_compare_numerically_not_as_strings() {
    assert!(v("0.19.624") < v("0.19.1151"));
    assert!(v("0.19.1151") < v("0.20.0"));
    assert!(v("0.19.9") < v("0.19.10"));
    assert_eq!(v("0.19.1"), v("0.19.1.0"));
    assert_eq!(v("0.19.1151").to_string(), "0.19.1151");
}

#[test]
fn version_output_of_the_real_flag_parses() {
    let out = "loom-daemon 0.19.1152 (commit 2d5fe17, built 2026-10-13T19:01:02Z, source \
               2d5fe17958dca799058f16bdd74a12336990bec0 clean)";
    assert_eq!(DaemonVersion::from_version_output(out), Some(v("0.19.1152")));
    assert_eq!(DaemonVersion::from_version_output("error: unrecognized option"), None);
    assert_eq!(DaemonVersion::from_version_output(""), None);
    // A bare commit hash is not a version.
    assert_eq!(DaemonVersion::from_version_output("1234567"), None);
}

#[test]
fn images_are_pinned_by_tag_and_downgrades_refused() {
    let host = v("0.19.1151");
    assert_eq!(
        pinned_image("ghcr.io/rjwalters/loom-worker-session:latest", &host),
        "ghcr.io/rjwalters/loom-worker-session:0.19.1151"
    );
    assert_eq!(pinned_image("localhost:5000/s:0.19.1", &host), "localhost:5000/s:0.19.1151");
    assert_eq!(pinned_image("r/s@sha256:abc", &host), "r/s:0.19.1151");
    let floors = [&host];
    assert!(refuse_downgrade("r/s:0.19.624", &floors).is_err());
    assert!(refuse_downgrade("r/s:0.19.1151", &floors).is_ok());
    assert!(refuse_downgrade("r/s:0.19.1152", &floors).is_ok());
    // No version in the tag: cannot be judged, allowed.
    assert!(refuse_downgrade("r/s:latest", &floors).is_ok());
    assert!(refuse_downgrade("r/s", &floors).is_ok());
    // Older than the running container's daemon, though not the host's.
    let running = v("0.19.2000");
    assert!(refuse_downgrade("r/s:0.19.1500", &[&host, &running]).is_err());
}

#[test]
#[serial]
fn an_idle_skewed_container_is_recreated_from_the_host_pinned_image() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed(&lifecycle, Some("0.19.1151"), Some("0.19.624"));
    let mut state = ReconcileState::default();
    assert_eq!(
        skew_pass(&mut lifecycle, &env, &mut state, 0),
        vec![Outcome::SkewRecreated {
            workspace: PathBuf::from("/srv/checkouts"),
            from: v("0.19.624"),
            to: v("0.19.1151"),
        }]
    );
    assert_eq!(lifecycle.runner().mutations(), ["stop_and_remove", "create"]);
    let creates = lifecycle.runner().creates.lock().unwrap().clone();
    assert_eq!(creates[0].1, "example/session:0.19.1151", "pinned to the host, not :latest");
    // The fresh container carries the host's daemon: confirmed, then left alone.
    assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 60), vec![Outcome::Running]);
    assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 120), vec![Outcome::Running]);
    assert_eq!(lifecycle.runner().count("create"), 1);
}

#[test]
#[serial]
fn a_busy_skewed_container_is_deferred_then_recreated_once_idle() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed(&lifecycle, Some("0.19.1151"), Some("0.19.624"));
    let container = container_name("alice");
    lifecycle
        .runner()
        .busy
        .lock()
        .unwrap()
        .insert(container.clone(), true);
    let mut state = ReconcileState::default();
    let busy = vec![Outcome::SkewDeferred {
        reason: DeferReason::Busy,
    }];
    assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 0), busy);
    assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 60), busy);
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new(), "never killed");
    lifecycle
        .runner()
        .busy
        .lock()
        .unwrap()
        .insert(container, false);
    let out = skew_pass(&mut lifecycle, &env, &mut state, 120);
    assert!(matches!(out[..], [Outcome::SkewRecreated { .. }]), "{out:?}");
    assert_eq!(lifecycle.runner().mutations(), ["stop_and_remove", "create"]);
}

#[test]
#[serial]
fn an_unreadable_host_version_means_no_action_and_no_probe() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed(&lifecycle, None, Some("0.19.624"));
    let mut state = ReconcileState::default();
    assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 0), vec![Outcome::Running]);
    assert_eq!(lifecycle.runner().count("version_probe"), 0);
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
}

#[test]
#[serial]
fn an_unreadable_container_version_means_no_action() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed(&lifecycle, Some("0.19.1151"), None);
    let mut state = ReconcileState::default();
    assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 0), vec![Outcome::Running]);
    assert_eq!(lifecycle.runner().count("version_probe"), 1);
    // Output that is not a version is not a finding either.
    lifecycle
        .runner()
        .daemon_versions
        .lock()
        .unwrap()
        .insert(container_name("alice"), "garbled".into());
    assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 60), vec![Outcome::Running]);
    assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
}

#[test]
#[serial]
fn a_container_at_or_ahead_of_the_host_is_left_alone() {
    for container_ver in ["0.19.1151", "0.19.1200"] {
        let env = setup(&["alice"], &["alice"]);
        let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
        seed(&lifecycle, Some("0.19.1151"), Some(container_ver));
        let mut state = ReconcileState::default();
        assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 0), vec![Outcome::Running]);
        assert_eq!(lifecycle.runner().mutations(), Vec::<String>::new());
    }
}

#[test]
#[serial]
fn an_image_older_than_the_host_is_refused_not_started() {
    // The container's recorded image names a daemon older than the host: the
    // missing-container path refuses to start it (no downgrade).
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let fake = lifecycle.runner().clone();
    *fake.host_version.lock().unwrap() = Some(v("0.19.1151"));
    // Seen once running on an old tagged image, then it vanished.
    fake.seed(&container_name("alice"), true, false, Some(Path::new("/srv/checkouts")));
    fake.containers
        .lock()
        .unwrap()
        .get_mut(&container_name("alice"))
        .unwrap()
        .image = Some("example/session:0.19.624".into());
    fake.mounts
        .lock()
        .unwrap()
        .insert(container_name("alice"), Vec::new());
    let mut state = ReconcileState::default();
    assert_eq!(skew_pass(&mut lifecycle, &env, &mut state, 0), vec![Outcome::Running]);
    fake.containers
        .lock()
        .unwrap()
        .remove(&container_name("alice"));
    let out = skew_pass(&mut lifecycle, &env, &mut state, 60);
    assert!(matches!(out[..], [Outcome::Failed { .. }]), "{out:?}");
    assert_eq!(lifecycle.runner().count("create"), 0, "no downgrade");
}

#[test]
#[serial]
fn a_failed_pinned_recreate_is_not_retried_until_a_version_changes() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed(&lifecycle, Some("0.19.1151"), Some("0.19.624"));
    *lifecycle.runner().fail_create.lock().unwrap() = true;
    let mut state = ReconcileState::default();
    let out = skew_pass(&mut lifecycle, &env, &mut state, 0);
    assert!(matches!(out[..], [Outcome::Failed { .. }]), "{out:?}");
    // Something (here: the test) brings back a skewed container; the same
    // (container, host) pair is not attempted again.
    *lifecycle.runner().fail_create.lock().unwrap() = false;
    seed(&lifecycle, Some("0.19.1151"), Some("0.19.624"));
    let attempts = lifecycle.runner().count("stop_and_remove");
    let _ = skew_pass(&mut lifecycle, &env, &mut state, 10_000);
    assert_eq!(lifecycle.runner().count("stop_and_remove"), attempts);
}
