//! Issue #10661 items 5 and 6: an operator's Ctrl-C reaches the `docker`
//! child, and a missing image is pulled under its own, longer budget.

use super::*;
use std::path::Path;
use std::time::Instant;

fn sigint() -> Option<i32> {
    Some(libc::SIGINT)
}

/// A probe that reports SIGINT once `after` has passed.
fn sigint_after(after: Duration) -> impl Fn() -> Option<i32> {
    let start = Instant::now();
    move || (start.elapsed() >= after).then_some(libc::SIGINT)
}

#[test]
fn the_daemon_never_sees_a_pending_signal() {
    // Nothing in this test binary installs the operator handler, as nothing
    // in the daemon does: unattended callers keep the old behaviour.
    assert_eq!(operator_interrupt::pending(), None);
}

#[test]
fn a_pending_signal_starts_no_further_docker_call() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let script = format!("touch '{}'", marker.display());
    let error = run_bounded_with("sh", &["-c", &script], DOCKER_CALL_TIMEOUT, &sigint).unwrap_err();
    assert_eq!(error.downcast_ref::<Interrupted>(), Some(&Interrupted(libc::SIGINT)));
    assert!(!marker.exists());
}

#[test]
fn the_signal_is_forwarded_to_the_running_child() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("got");
    // Like `docker run`/`pull` on SIGINT: clean up and exit nonzero.
    let script = format!(
        "trap 'echo int > \"{}\"; kill $! 2>/dev/null; exit 130' INT; sleep 30 & wait",
        marker.display()
    );
    let started = Instant::now();
    let error = run_bounded_with(
        "sh",
        &["-c", &script],
        DOCKER_CALL_TIMEOUT,
        &sigint_after(Duration::from_millis(300)),
    )
    .unwrap_err();
    assert_eq!(
        error.downcast_ref::<Interrupted>(),
        Some(&Interrupted(libc::SIGINT)),
        "{error:#}"
    );
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "int", "the child got SIGINT");
}

#[test]
fn a_call_that_succeeds_despite_the_signal_stands() {
    // The child ignores SIGINT and finishes inside the grace.
    let (ok, out, _) = run_bounded_with(
        "sh",
        &["-c", "trap '' INT; sleep 0.5; echo done"],
        DOCKER_CALL_TIMEOUT,
        &sigint_after(Duration::from_millis(200)),
    )
    .unwrap();
    assert!(ok);
    assert_eq!(out.trim(), "done");
}

fn executable(path: &Path, body: &str) -> String {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(path, perms).unwrap();
    path.display().to_string()
}

/// A fake `docker`: `image inspect` succeeds iff `<dir>/present` exists;
/// `pull` records itself and creates it.
fn fake_docker(dir: &Path) -> String {
    let body = format!(
        "#!/bin/sh\nd='{}'\ncase \"$1\" in\n  image) [ -e \"$d/present\" ] ;;\n  pull) echo \"$2\" >> \"$d/pulls\"; touch \"$d/present\" ;;\n  *) exit 2 ;;\nesac\n",
        dir.display()
    );
    executable(&dir.join("docker"), &body)
}

#[test]
fn an_operator_start_pulls_a_missing_image_first_and_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let docker = fake_docker(dir.path());
    let none = || None;
    ensure_image_with(&docker, "img:1", &none, DOCKER_PULL_TIMEOUT).unwrap();
    ensure_image_with(&docker, "img:1", &none, DOCKER_PULL_TIMEOUT).unwrap();
    assert_eq!(std::fs::read_to_string(dir.path().join("pulls")).unwrap(), "img:1\n");
}

#[test]
fn a_failed_pull_fails_the_start_with_dockers_reason() {
    let dir = tempfile::tempdir().unwrap();
    let docker = executable(
        &dir.path().join("docker"),
        "#!/bin/sh\n[ \"$1\" = pull ] && echo 'manifest unknown' >&2\nexit 1\n",
    );
    let error = ensure_image_with(&docker, "img:2", &|| None, DOCKER_PULL_TIMEOUT).unwrap_err();
    assert!(format!("{error:#}").contains("manifest unknown"), "{error:#}");
}
