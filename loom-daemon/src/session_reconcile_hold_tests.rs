//! Issue #10661 item 2: "lift succeeded, start failed". An operator `session
//! start` deletes the hold before it touches Docker; if the start then fails,
//! the account is unheld and down, and the reconcile pass restarts it on its
//! per-account backoff — the operator asked for it to run.

use super::*;

#[test]
#[serial]
fn an_operator_start_that_fails_after_lifting_the_hold_is_restarted_with_backoff() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let alice = profile(&env, "alice");
    session_hold::write_hold(&alice, session_hold::now_unix_ms()).unwrap();
    let mut state = ReconcileState::default();
    assert_eq!(pass(&mut lifecycle, &env, &host, &mut state, 0), vec![Outcome::Held]);

    // The operator's start: the hold is lifted, then `docker run` fails.
    *lifecycle.runner().fail_create.lock().unwrap() = true;
    let error = lifecycle
        .start_with_workspace("alice", Some(Path::new("/srv/checkouts")))
        .unwrap_err();
    assert!(format!("{error:#}").contains("docker run"), "{error:#}");
    assert!(!session_hold::held_across(std::slice::from_ref(&alice)), "unheld");
    assert!(lifecycle.runner().containers.lock().unwrap().is_empty(), "and down");

    // The pass now owns it: it retries, and backs off while the start fails.
    let interval = DEFAULT_SESSION_RECONCILE_INTERVAL_SECS;
    let out = pass(&mut lifecycle, &env, &host, &mut state, interval);
    let [Outcome::Failed { retry_at, .. }] = out.as_slice() else {
        panic!("{out:?}")
    };
    assert_eq!(*retry_at, interval + BACKOFF_BASE_SECS);
    let creates = lifecycle.runner().count("create");
    assert!(matches!(
        pass(&mut lifecycle, &env, &host, &mut state, interval * 2)[..],
        [Outcome::BackingOff { .. }]
    ));
    assert_eq!(lifecycle.runner().count("create"), creates, "no docker run while backing off");

    // Docker recovers: the next due pass brings it up.
    *lifecycle.runner().fail_create.lock().unwrap() = false;
    let out = pass(&mut lifecycle, &env, &host, &mut state, *retry_at);
    assert!(matches!(out[..], [Outcome::Recreated { .. }]), "{out:?}");
    assert!(lifecycle.runner().containers.lock().unwrap()[&container_name("alice")].running);
}
