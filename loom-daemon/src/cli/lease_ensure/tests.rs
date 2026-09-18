//! Tests for `loom-daemon lease ensure` (#8193).
//!
//! Every test drives the real `ensure()` decision against FAKE
//! `sweep-lease-publish.sh` / `sweep-lease-renew.sh` scripts in a temp
//! checkout. Faking the two scripts rather than mocking a Rust trait is
//! deliberate: what this subcommand contributes is entirely *the argv it hands
//! those two scripts and the conditions under which it hands it over*, so a
//! test that does not observe the argv observes nothing.

use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;
use tempfile::TempDir;

/// A publish fake that succeeds and prints the documented identity line, with
/// the `OK:`/`NOTE:` chatter the real script emits on stderr alongside it.
const PUBLISH_OK: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" > publish-argv
echo "OK: published lease record for issue" >&2
echo "host-abc12345 sweep-insession-20260918-1234"
"#;

/// The real script's exit 4: a different host holds a fresh lease.
const PUBLISH_PEER_HOLDS: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" > publish-argv
echo "SKIP: a live peer holds this claim" >&2
exit 4
"#;

/// Exit 0 but an identity line nothing can be threaded out of.
const PUBLISH_GARBLED: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" > publish-argv
echo "host-only-no-sweep-id"
"#;

const RENEW_OK: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" > renew-argv
echo 424242
"#;

const RENEW_REFUSES: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" > renew-argv
echo "ERROR: --host/--sweep-id pair cannot match" >&2
exit 1
"#;

/// A renew fake shaped like the real one's detachment: it dups the caller's
/// stderr onto fd 9 (`exec 9>&2`, #6541) and forks a long-lived child that
/// inherits it, then returns immediately. If `start_renewal` ever pipes stderr
/// again, that child holds the pipe's write end and `output()` blocks for the
/// child's whole lifetime — the four-hour hang this shape exists to catch.
const RENEW_DETACHES_HOLDING_FD9: &str = r#"#!/usr/bin/env bash
printf '%s\n' "$*" > renew-argv
exec 9>&2
( sleep 20 ) < /dev/null > /dev/null 2>&1 &
loop_pid=$!
exec 9>&-
disown "$loop_pid" 2>/dev/null || true
echo "$loop_pid"
"#;

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A temp checkout that `resolve_repo_root` accepts: a `.git` directory and a
/// `.loom/` directory, with both lease scripts in place.
fn checkout(publish: &str, renew: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    let scripts = dir.path().join(".loom").join("scripts");
    fs::create_dir_all(&scripts).unwrap();
    write_script(&scripts.join("sweep-lease-publish.sh"), publish);
    write_script(&scripts.join("sweep-lease-renew.sh"), renew);
    dir
}

fn args(dir: &TempDir) -> LeaseEnsureArgs {
    LeaseEnsureArgs {
        issue: 8193,
        watch_pid: 3_606_631,
        max_age: DEFAULT_MAX_AGE_SECS,
        force: false,
        workspace: dir.path().to_string_lossy().into_owned(),
    }
}

fn in_session() -> SessionEnv {
    SessionEnv {
        dispatched_issue: None,
        session_present: true,
    }
}

fn argv(dir: &TempDir, name: &str) -> Option<String> {
    fs::read_to_string(dir.path().join(name)).ok()
}

#[test]
fn publishes_and_threads_the_identity_into_a_bounded_renewal_loop() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let outcome = args(&dir).ensure(&in_session());

    assert_eq!(
        outcome,
        Outcome::Renewing {
            host: "host-abc12345".to_string(),
            sweep_id: "sweep-insession-20260918-1234".to_string(),
            loop_pid: Some("424242".to_string()),
        }
    );
    assert_eq!(argv(&dir, "publish-argv").unwrap().trim(), "publish 8193");

    // The renewal argv is the whole contract: the identity publish resolved
    // (so the loop renews THIS lease rather than "newest wins", #7876), the
    // caller's durable pid (never `$$`, #8193 finding 1) and the 4h cap
    // (#8193 finding 3).
    let renew = argv(&dir, "renew-argv").unwrap();
    assert_eq!(
        renew.trim(),
        "start 8193 --watch-pid 3606631 --max-age 14400 --host host-abc12345 --sweep-id \
         sweep-insession-20260918-1234"
    );
}

#[test]
fn the_default_cap_is_four_hours_not_the_scripts_own_twenty_four() {
    assert_eq!(DEFAULT_MAX_AGE_SECS, 4 * 60 * 60);
}

#[test]
fn a_daemon_dispatched_issue_is_a_no_op() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let env = SessionEnv {
        dispatched_issue: Some("8193".to_string()),
        session_present: true,
    };

    assert_eq!(args(&dir).ensure(&env), Outcome::AlreadyDispatched);
    // The acceptance criterion is "no DUPLICATE lease comment", so the check
    // that matters is that publish was never even invoked.
    assert!(argv(&dir, "publish-argv").is_none());
    assert!(argv(&dir, "renew-argv").is_none());
}

#[test]
fn a_marker_naming_a_different_issue_does_not_suppress_this_one() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let env = SessionEnv {
        dispatched_issue: Some("8116".to_string()),
        session_present: true,
    };

    assert!(matches!(args(&dir).ensure(&env), Outcome::Renewing { .. }));
    assert!(argv(&dir, "publish-argv").is_some());
}

#[test]
fn nothing_is_published_outside_an_agent_session() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let outcome = args(&dir).ensure(&SessionEnv::default());

    assert_eq!(outcome, Outcome::NoAgentSession);
    assert!(argv(&dir, "publish-argv").is_none());
}

#[test]
fn force_opts_back_in_without_a_session_marker() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let mut a = args(&dir);
    a.force = true;

    assert!(matches!(a.ensure(&SessionEnv::default()), Outcome::Renewing { .. }));
}

#[test]
fn a_peer_held_lease_stops_short_of_renewal() {
    let dir = checkout(PUBLISH_PEER_HOLDS, RENEW_OK);

    assert_eq!(args(&dir).ensure(&in_session()), Outcome::PublishDeclined(Some(4)));
    // Renewing after a refused publish would keep a PEER's record fresh under
    // the script's "newest wins" fallback — so it must not run at all.
    assert!(argv(&dir, "renew-argv").is_none());
}

#[test]
fn an_unparseable_identity_line_does_not_start_a_loop_that_cannot_renew() {
    let dir = checkout(PUBLISH_GARBLED, RENEW_OK);

    assert_eq!(
        args(&dir).ensure(&in_session()),
        Outcome::PublishUnparseable("host-only-no-sweep-id".to_string())
    );
    assert!(argv(&dir, "renew-argv").is_none());
}

#[test]
fn a_refused_renewal_start_is_reported_not_fatal() {
    let dir = checkout(PUBLISH_OK, RENEW_REFUSES);

    assert_eq!(args(&dir).ensure(&in_session()), Outcome::RenewalFailed(Some(1)));
}

#[test]
fn missing_lease_scripts_skip_rather_than_fail() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".git")).unwrap();
    fs::create_dir_all(dir.path().join(".loom")).unwrap();

    let a = LeaseEnsureArgs {
        workspace: dir.path().to_string_lossy().into_owned(),
        ..args(&checkout(PUBLISH_OK, RENEW_OK))
    };
    assert!(matches!(a.ensure(&in_session()), Outcome::ScriptsMissing(_)));
}

#[test]
fn outside_a_loom_checkout_it_skips() {
    let dir = tempfile::tempdir().unwrap();
    let a = LeaseEnsureArgs {
        workspace: dir.path().to_string_lossy().into_owned(),
        ..args(&checkout(PUBLISH_OK, RENEW_OK))
    };
    assert!(matches!(a.ensure(&in_session()), Outcome::NoRepoRoot(_)));
}

/// Regression guard for the four-hour hang: `sweep-lease-renew.sh start` hands
/// its detached loop a dup of the caller's stderr on fd 9, so capturing stderr
/// into a pipe makes `output()` wait for the LOOP, not for `start`. The fake
/// here holds fd 9 for 20s; the margin below is wide enough that only a real
/// regression can trip it.
#[test]
fn starting_renewal_returns_before_the_detached_loop_exits() {
    let dir = checkout(PUBLISH_OK, RENEW_DETACHES_HOLDING_FD9);

    let started = Instant::now();
    let outcome = args(&dir).ensure(&in_session());
    let elapsed = started.elapsed();

    assert!(matches!(outcome, Outcome::Renewing { .. }));
    assert!(
        elapsed.as_secs() < 10,
        "lease ensure waited {elapsed:?} for a detached renewal loop — stderr must be \
         Stdio::null() so the loop's inherited fd 9 is /dev/null, not a pipe this process reads"
    );
}

/// `run()` never fails the caller, whatever it decides — `worktree.sh` calls it
/// unconditionally and a builder's worktree setup must not hinge on it.
#[test]
fn run_always_reports_success() {
    let dir = tempfile::tempdir().unwrap();
    let a = LeaseEnsureArgs {
        workspace: dir.path().to_string_lossy().into_owned(),
        ..args(&checkout(PUBLISH_OK, RENEW_OK))
    };
    assert!(a.run().is_ok());
}

#[test]
fn every_outcome_explains_itself() {
    let outcomes = [
        Outcome::AlreadyDispatched,
        Outcome::NoAgentSession,
        Outcome::NoRepoRoot("why".to_string()),
        Outcome::ScriptsMissing(PathBuf::from("/tmp/x")),
        Outcome::PublishUnavailable("boom".to_string()),
        Outcome::PublishDeclined(Some(4)),
        Outcome::PublishUnparseable("junk".to_string()),
        Outcome::Renewing {
            host: "h".to_string(),
            sweep_id: "s".to_string(),
            loop_pid: None,
        },
        Outcome::RenewalFailed(None),
    ];
    for o in &outcomes {
        let line = o.describe(8193);
        assert!(!line.is_empty(), "{o:?} has no description");
        assert!(line.contains("8193"), "{o:?} does not name the issue: {line}");
    }
}
