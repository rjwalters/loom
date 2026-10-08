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

/// The real script's exit 4: another sweep (any host) holds a fresh lease.
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

/// Above Linux's maximum `pid_max` (2^22), so never a live process: the
/// fixture's session identity is unreadable and publish keeps its own id.
const NO_SUCH_PID: u32 = 4_999_999;

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
        watch_pid: NO_SUCH_PID,
        max_age: DEFAULT_MAX_AGE_SECS,
        force: false,
        workspace: dir.path().to_string_lossy().into_owned(),
        deferred: false,
        retry_interval: 60,
    }
}

fn in_session() -> SessionEnv {
    SessionEnv {
        dispatched_issue: None,
        session_present: true,
        ..Default::default()
    }
}

/// [`SessionEnv::from_lookup`] over a single key/value pair.
fn env_with(key: &str, value: &str) -> SessionEnv {
    SessionEnv::from_lookup(|k| (k == key).then(|| value.to_string()))
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
        "start 8193 --watch-pid 4999999 --max-age 14400 --host host-abc12345 --sweep-id \
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
        ..Default::default()
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
        ..Default::default()
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

/// #9453: the pi runtime — and any other non-Claude harness — exports none of
/// the Claude-specific markers, so a hand-claim lane on it was `--force`
/// territory. `LOOM_AGENT_SESSION_PID` alone must admit the publish: a marker
/// any long-lived harness can export is self-describing and needs no flag
/// discipline on every call.
#[test]
fn a_runtime_neutral_agent_marker_alone_admits_publication() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let env = env_with("LOOM_AGENT_SESSION_PID", "4242");

    let outcome = args(&dir).ensure(&env);
    assert!(matches!(outcome, Outcome::Renewing { .. }), "unexpected outcome: {outcome:?}");
    assert!(argv(&dir, "publish-argv").is_some());
    assert!(argv(&dir, "renew-argv").is_some());
}

/// Presence means a non-empty value: a blank `LOOM_AGENT_SESSION_PID` is no
/// more a session than a blank `CLAUDE_PID` would be, so the refusal (with its
/// `--force` hint) still fires.
#[test]
fn a_blank_runtime_neutral_marker_is_not_a_session() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let env = env_with("LOOM_AGENT_SESSION_PID", "  ");

    assert_eq!(args(&dir).ensure(&env), Outcome::NoAgentSession);
    assert!(argv(&dir, "publish-argv").is_none());
}

/// The refusal's one-line hint names every marker, so a foreign runtime
/// reading it learns the `LOOM_AGENT_SESSION_PID` path, not just `--force`.
#[test]
fn the_no_session_refusal_names_the_runtime_neutral_marker() {
    let line = Outcome::NoAgentSession.describe(8193);
    assert!(line.contains("LOOM_AGENT_SESSION_PID"), "hint lacks the marker: {line}");
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

/// A renew fake that records which fds above 2 it was handed. It probes each
/// number with `-e /dev/fd/N` rather than globbing `/dev/fd`, because the glob
/// opens a directory fd of its own (typically 3) and would report it.
const RENEW_RECORDS_FDS: &str = r#"#!/usr/bin/env bash
for ((f = 3; f < 1024; f++)); do [[ -e /dev/fd/$f ]] && echo "$f"; done > renew-fds
echo 424242
"#;

/// #10203: `worktree.sh` calls `lease ensure` with its caller's stdout on fd 3,
/// and an inherited copy in the detached loop held a `worktree.sh N | tail`
/// pipe open for up to 4h. A non-CLOEXEC pipe open in this process must not
/// reach the renew script — and must still be open here afterwards.
#[test]
fn the_renewal_script_inherits_no_fd_from_the_caller() {
    let mut pipe_fds = [0 as libc::c_int; 2];
    // SAFETY: `pipe` writes two fds into the 2-element array it is given.
    assert_eq!(unsafe { libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
    let dir = checkout(PUBLISH_OK, RENEW_RECORDS_FDS);

    let outcome = args(&dir).ensure(&in_session());
    assert!(matches!(outcome, Outcome::Renewing { .. }));

    let seen = argv(&dir, "renew-fds").expect("the fake renew script ran");
    for fd in pipe_fds {
        assert!(
            !seen.lines().any(|l| l.trim() == fd.to_string()),
            "renew script inherited caller fd {fd}; it saw fds: {seen:?}"
        );
        // SAFETY: F_GETFD only reads the descriptor flag; close releases an fd
        // this test opened.
        unsafe {
            assert!(libc::fcntl(fd, libc::F_GETFD) >= 0, "fd {fd} must stay open here");
            libc::close(fd);
        }
    }
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

/// #10570: `worktree.sh`'s attended call site must use the same runtime-neutral
/// pid chain as the documented claim recipe, and must not swallow stderr (the
/// one-line outcome is the only observable publication/renewal result).
#[test]
fn worktree_sh_call_site_matches_documented_pid_chain() {
    let src = include_str!("../../../../defaults/scripts/worktree.sh");
    let line = src
        .lines()
        .find(|l| l.starts_with("_wt_lease_claim()"))
        .expect("_wt_lease_claim definition");
    assert!(
        line.contains(r#"--watch-pid "${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}""#),
        "pid chain drifted: {line}"
    );
    assert!(!line.contains("2>&1"), "stderr outcome must stay visible: {line}");
}

// ------------------------------------------------------------------
// #10570: a declined publish is retried by a bounded deferred publisher
// ------------------------------------------------------------------

/// Declines (exit 4, a peer's lease is still inside its TTL) on its first two
/// calls, then publishes — the #10161 timeline: a released sweep's leftover
/// lease blocks the attended claim until it ages out.
const PUBLISH_DECLINES_TWICE: &str = r#"#!/usr/bin/env bash
n=$(cat publish-attempts 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > publish-attempts
printf '%s\n' "$*" > publish-argv
if [ "$n" -le 2 ]; then echo "SKIP: fresh peer lease" >&2; exit 4; fi
echo "host-abc12345 sweep-insession-attended"
"#;

/// Publish exit 2: the `gh` write failed (transient forge trouble).
const PUBLISH_GH_FAILS: &str = r#"#!/usr/bin/env bash
n=$(cat publish-attempts 2>/dev/null || echo 0); echo "$((n + 1))" > publish-attempts
exit 2
"#;

fn attempts(dir: &TempDir) -> u32 {
    argv(dir, "publish-attempts").map_or(0, |s| s.trim().parse().unwrap())
}

/// A fake clock driven by the loop's own sleeps.
struct FakeTime {
    now: std::cell::Cell<u64>,
    sleeps: std::cell::RefCell<Vec<u64>>,
}

impl FakeTime {
    fn new() -> Self {
        Self {
            now: std::cell::Cell::new(0),
            sleeps: std::cell::RefCell::new(Vec::new()),
        }
    }
    fn elapsed(&self) -> u64 {
        self.now.get()
    }
    fn sleep(&self, secs: u64) {
        self.now.set(self.now.get() + secs);
        self.sleeps.borrow_mut().push(secs);
    }
}

fn run_loop(a: &LeaseEnsureArgs, alive: impl FnMut() -> bool) -> (deferred::DeferredEnd, FakeTime) {
    let t = FakeTime::new();
    let end = deferred::retry_loop(a, &in_session(), || t.elapsed(), |s| t.sleep(s), alive, |_| {});
    (end, t)
}

#[test]
fn only_a_peer_or_gh_failure_decline_is_retryable() {
    assert!(Outcome::PublishDeclined(Some(4)).is_retryable());
    assert!(Outcome::PublishDeclined(Some(2)).is_retryable());
    for settled in [
        Outcome::PublishDeclined(None),
        Outcome::PublishDeclined(Some(1)),
        Outcome::NoAgentSession,
        Outcome::AlreadyDispatched,
        Outcome::RenewalFailed(Some(1)),
        Outcome::PublishUnparseable(String::new()),
    ] {
        assert!(!settled.is_retryable(), "{settled:?} must not be retried");
    }
}

/// The #10161 shape end to end: declined while the leftover lease is fresh,
/// published once it ages out, and the renewer gets only what remains of the
/// original cap.
#[test]
fn a_deferred_publisher_publishes_once_the_peer_lease_ages_out() {
    let dir = checkout(PUBLISH_DECLINES_TWICE, RENEW_OK);
    let a = args(&dir);
    let (end, t) = run_loop(&a, || true);

    assert_eq!(
        end,
        deferred::DeferredEnd::Settled(Outcome::Renewing {
            host: "host-abc12345".to_string(),
            sweep_id: "sweep-insession-attended".to_string(),
            loop_pid: Some("424242".to_string()),
        })
    );
    assert_eq!(attempts(&dir), 3);
    assert_eq!(*t.sleeps.borrow(), vec![60, 60, 60]);
    let renew = argv(&dir, "renew-argv").unwrap();
    assert!(
        renew.contains(&format!("--max-age {}", DEFAULT_MAX_AGE_SECS - 180)),
        "renewal must inherit the remaining cap, not a fresh one: {renew}"
    );
}

/// A dead session stops the publisher before another attempt: it must never
/// publish a lease for a claim nobody is working.
#[test]
fn a_deferred_publisher_stops_when_the_session_dies() {
    let dir = checkout(PUBLISH_PEER_HOLDS, RENEW_OK);
    let mut calls = 0;
    let (end, _) = run_loop(&args(&dir), || {
        calls += 1;
        calls < 2
    });

    assert_eq!(end, deferred::DeferredEnd::SessionEnded);
    assert!(argv(&dir, "publish-argv").is_some(), "one attempt while alive");
    assert!(argv(&dir, "renew-argv").is_none());
}

/// A live peer that keeps its lease fresh is never superseded, and the
/// publisher gives up at the cap rather than polling forever.
#[test]
fn a_deferred_publisher_is_bounded_by_the_age_cap() {
    let dir = checkout(PUBLISH_GH_FAILS, RENEW_OK);
    let mut a = args(&dir);
    a.max_age = 150;
    let (end, t) = run_loop(&a, || true);

    assert_eq!(end, deferred::DeferredEnd::CapReached);
    assert_eq!(attempts(&dir), 2, "attempts at 60s and 120s, none at 180s");
    assert_eq!(t.elapsed(), 180);
    assert!(argv(&dir, "renew-argv").is_none());
}

/// `--max-age 0` means unbounded for a renewer, never for this publisher.
#[test]
fn a_deferred_publisher_is_bounded_even_when_max_age_is_zero() {
    let dir = checkout(PUBLISH_GH_FAILS, RENEW_OK);
    let mut a = args(&dir);
    a.max_age = 0;
    a.retry_interval = 3_600;
    let (end, t) = run_loop(&a, || true);

    assert_eq!(end, deferred::DeferredEnd::CapReached);
    assert!(t.elapsed() >= DEFAULT_MAX_AGE_SECS);
}

/// Past the lease TTL a still-fresh peer is being renewed, i.e. live: slow
/// down to the renewer's own cadence instead of polling every minute.
#[test]
fn a_deferred_publisher_backs_off_once_the_peer_is_provably_live() {
    let dir = checkout(PUBLISH_PEER_HOLDS, RENEW_OK);
    let ttl = (loom_daemon::claim_reconciliation::resolve_lease_ttl_minutes() * 60.0) as u64;
    let t = FakeTime::new();
    let end = deferred::retry_loop(
        &args(&dir),
        &in_session(),
        || t.elapsed(),
        |s| t.sleep(s),
        || t.elapsed() < ttl + 1_000,
        |_| {},
    );

    assert_eq!(end, deferred::DeferredEnd::SessionEnded);
    let sleeps = t.sleeps.borrow();
    assert_eq!(sleeps.first(), Some(&60));
    assert_eq!(sleeps.last(), Some(&300));
}

/// Renewer failure after a deferred publish settles with an observable
/// outcome instead of retrying a publish that already succeeded.
#[test]
fn a_renewer_failure_after_a_deferred_publish_is_reported() {
    let dir = checkout(PUBLISH_OK, RENEW_REFUSES);
    let (end, _) = run_loop(&args(&dir), || true);
    assert_eq!(end, deferred::DeferredEnd::Settled(Outcome::RenewalFailed(Some(1))));
    assert!(end.describe(8193).contains("8193"));
}

#[test]
fn every_deferred_end_explains_itself() {
    for end in [
        deferred::DeferredEnd::SessionEnded,
        deferred::DeferredEnd::CapReached,
        deferred::DeferredEnd::Settled(Outcome::PublishDeclined(Some(4))),
    ] {
        assert!(end.describe(8193).contains("8193"), "{end:?}");
    }
}

/// One deferred publisher per issue per checkout.
#[test]
fn the_deferred_lock_admits_a_single_owner() {
    let dir = tempfile::tempdir().unwrap();
    let lock = dir.path().join("logs/issue-1.deferred.lock");
    let held = deferred::try_lock(&lock).expect("first owner takes it");
    assert!(deferred::try_lock(&lock).is_none(), "second owner refused");
    drop(held);
    assert!(deferred::try_lock(&lock).is_some(), "released on drop");
}

/// Outcomes land in a per-issue log, since `worktree.sh` callers rarely read
/// stderr and the detached publisher has none.
#[test]
fn outcomes_are_recorded_in_the_per_issue_log() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    deferred::record(&dir.path().to_string_lossy(), 8193, "first line");
    deferred::record(&dir.path().to_string_lossy(), 8193, "second line");
    let root = loom_daemon::repo_root::resolve_repo_root(&dir.path().to_string_lossy()).unwrap();
    let log = fs::read_to_string(deferred::log_path(&root, 8193)).unwrap();
    assert!(log.contains(" first line\n") && log.contains(" second line\n"), "{log}");
}

// ------------------------------------------------------------------
// #10570: a session-stable attended lease identity
// ------------------------------------------------------------------

#[test]
fn the_session_identity_is_stable_for_a_live_process_and_absent_otherwise() {
    let me = std::process::id();
    let id = session_sweep_id(me).expect("this test process has a start identity");
    assert!(id.starts_with(&format!("sweep-insession-s{me}-")), "{id}");
    assert!(!id.contains(char::is_whitespace) && !id.contains("-->"), "{id}");
    assert_eq!(session_sweep_id(me), Some(id));
    assert_eq!(session_sweep_id(NO_SUCH_PID), None);
}

/// A session's second `lease ensure` (a `worktree.sh N` reuse) must publish
/// under the same identity, so the script sees its OWN fresh lease rather
/// than a peer's and re-attaches instead of declining.
#[test]
fn a_live_session_publishes_under_the_same_identity_every_time() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let mut a = args(&dir);
    a.watch_pid = std::process::id();

    a.ensure(&in_session());
    let first = argv(&dir, "publish-argv").unwrap();
    a.ensure(&in_session());
    let second = argv(&dir, "publish-argv").unwrap();

    assert!(first.contains("--sweep-id sweep-insession-s"), "{first}");
    assert_eq!(first, second);
}

/// An in-session `/loom:sweep` run's `LOOM_SWEEP_RUN_ID` keeps precedence.
#[test]
fn a_sweep_run_id_keeps_publish_on_its_own_identity() {
    let dir = checkout(PUBLISH_OK, RENEW_OK);
    let mut a = args(&dir);
    a.watch_pid = std::process::id();
    let env = SessionEnv {
        run_id_set: true,
        ..in_session()
    };

    a.ensure(&env);
    assert_eq!(argv(&dir, "publish-argv").unwrap().trim(), "publish 8193");
}
