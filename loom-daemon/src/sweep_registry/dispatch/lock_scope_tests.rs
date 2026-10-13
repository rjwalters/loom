//! Coverage for #9573: the `Issue` begin step must not hold the registry
//! mutex across a forge `gh` call or a sleep, and releasing it must not open a
//! double-spawn window for a concurrent dispatch of the same issue.
//!
//! Sibling file (declared from `dispatch.rs`) because `dispatch/tests.rs` is
//! over the file-size ratchet threshold.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use tempfile::tempdir;

fn make_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// A registry with the real guard + flip path enabled whose fake `gh` sleeps
/// `gh_sleep_secs` on every `api` read (the guard chain's forge calls) and
/// whose fake spawn script appends one line per spawn to the returned marker.
/// Returns `(registry, gh_log, spawn_marker)`.
fn slow_gh_registry(ws: &Path, gh_sleep_secs: &str) -> (SweepRegistry, PathBuf, PathBuf) {
    touch_sweep_command(ws);
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    make_executable(
        &fake_gh,
        &format!(
            "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             sleep {sleep}\n\
             printf '%s\\n' '{open}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
            log = gh_log.display(),
            sleep = gh_sleep_secs,
            open = state_probe_json("open", false),
        ),
    );
    let spawn_marker = ws.join("spawns.log");
    let spawn = ws.join(".loom/scripts/spawn-claude.sh");
    std::fs::create_dir_all(spawn.parent().unwrap()).unwrap();
    make_executable(
        &spawn,
        &format!("#!/usr/bin/env bash\necho spawned >> '{}'\nexit 0\n", spawn_marker.display()),
    );
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log, spawn_marker)
}

fn begin_shared(
    registry: &Mutex<SweepRegistry>,
    issue: u32,
    key: Option<&str>,
) -> Result<BeginIssueDispatch> {
    SweepRegistry::begin_prepared_issue_dispatch_shared(
        registry,
        &SweepKind::Issue(issue),
        key.map(String::from),
        DispatchModel::Request(None),
        None,
        None,
        None,
        DispatchOrigin::Unknown,
    )
}

fn reap(outcome: Result<BeginIssueDispatch>) {
    if let Ok(BeginIssueDispatch::Spawned(mut prepared)) = outcome {
        let _ = prepared.child.wait();
    }
}

/// While a dispatch is parked inside a slow forge guard, another thread can
/// take the registry lock promptly (before #9573 it waited out the whole
/// chain: 20-odd `gh` calls plus the sleeps).
#[test]
#[serial]
fn registry_lock_is_free_while_a_forge_guard_is_in_flight() {
    let dir = tempdir().unwrap();
    let ws = dir.path().to_path_buf();
    std::env::remove_var("LOOM_REPO");
    let (reg, gh_log, _spawns) = slow_gh_registry(&ws, "1.5");
    let registry = Arc::new(Mutex::new(reg));

    let worker = {
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || begin_shared(&registry, 9573, None))
    };

    // Wait until the dispatch is inside a slow `gh api` read.
    assert!(
        wait_for_condition(10_000, || std::fs::read_to_string(&gh_log)
            .is_ok_and(|log| log.lines().any(|l| l.starts_with("api")))),
        "dispatch never reached a forge guard"
    );
    let started = Instant::now();
    drop(registry.lock().unwrap());
    let waited = started.elapsed();
    assert!(
        !worker.is_finished(),
        "the dispatch finished before the probe could observe it mid-guard"
    );
    assert!(
        waited < Duration::from_millis(500),
        "the registry lock was held across a forge call: waited {waited:?} (gh sleeps 1.5s)"
    );
    reap(worker.join().unwrap());
}

/// #10831 H4 under #9573: a roll that closes the gate while an admitted
/// `Shared` dispatch is parked in a lock-free forge read must either see that
/// dispatch in `mid_spawn_dispatches()` (and so wait for it) or the dispatch
/// must never spawn. Before the fix `enter()` ran only after the spawn, so the
/// closer read zero and the child started after the pause snapshot.
#[test]
#[serial]
fn roll_gate_close_during_a_forge_guard_counts_the_admitted_dispatch() {
    let dir = tempdir().unwrap();
    let ws = dir.path().to_path_buf();
    std::env::remove_var("LOOM_REPO");
    let (reg, gh_log, _spawns) = slow_gh_registry(&ws, "1.5");
    let registry = Arc::new(Mutex::new(reg));

    let worker = {
        let registry = Arc::clone(&registry);
        std::thread::spawn(move || begin_shared(&registry, 10_831, None))
    };
    assert!(
        wait_for_condition(10_000, || std::fs::read_to_string(&gh_log)
            .is_ok_and(|log| log.lines().any(|l| l.starts_with("api")))),
        "dispatch never reached a forge guard"
    );

    // The roll's `close_dispatch` + `pending_dispatches` shape: close the
    // gate and read the mid-spawn count in one lock hold.
    let closer_saw = {
        let mut reg = registry.lock().unwrap();
        reg.close_for_roll("roll-9573", true);
        reg.mid_spawn_dispatches()
    };
    assert!(
        !worker.is_finished(),
        "the dispatch finished before the closer could observe it mid-guard"
    );

    let outcome = worker.join().unwrap();
    let spawned = matches!(outcome, Ok(BeginIssueDispatch::Spawned(_)));
    assert!(
        closer_saw > 0 || !spawned,
        "a dispatch spawned after the roll closed the gate and read mid_spawn_dispatches() == 0"
    );
    assert_eq!(closer_saw, 1, "the admitted dispatch must be counted as mid-spawn");
    reap(outcome);
    assert_eq!(
        registry.lock().unwrap().mid_spawn_dispatches(),
        0,
        "the guard is released once the hand-back is dropped"
    );
}

/// Two concurrent dispatches of one issue and one idempotency key: exactly one
/// spawns, the other gets the graceful hand-back or a refusal.
#[test]
#[serial]
fn concurrent_same_key_dispatches_spawn_exactly_once() {
    concurrent_same_issue(Some("lock-scope-key-9573"));
}

/// Same, without an idempotency key: the claim lock / live-claim guard keeps
/// the second one out.
#[test]
#[serial]
fn concurrent_keyless_dispatches_spawn_exactly_once() {
    concurrent_same_issue(None);
}

fn concurrent_same_issue(key: Option<&'static str>) {
    let dir = tempdir().unwrap();
    let ws = dir.path().to_path_buf();
    std::env::remove_var("LOOM_REPO");
    let (reg, _gh_log, spawns) = slow_gh_registry(&ws, "0.3");
    let registry = Arc::new(Mutex::new(reg));
    let barrier = Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let registry = Arc::clone(&registry);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                begin_shared(&registry, 95_730, key)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let spawned = results
        .iter()
        .filter(|r| matches!(r, Ok(BeginIssueDispatch::Spawned(_))))
        .count();
    assert_eq!(spawned, 1, "exactly one dispatch may spawn");
    for other in results
        .iter()
        .filter(|r| !matches!(r, Ok(BeginIssueDispatch::Spawned(_))))
    {
        match other {
            Ok(BeginIssueDispatch::Done(Ok(outcome))) => {
                assert!(!outcome.was_new, "the loser must not report a new sweep");
            }
            Ok(BeginIssueDispatch::Done(Err(_))) | Err(_) => {}
            Ok(BeginIssueDispatch::Spawned(_)) => unreachable!(),
        }
    }
    for result in results {
        reap(result);
    }
    let lines = std::fs::read_to_string(&spawns)
        .unwrap_or_default()
        .lines()
        .count();
    assert_eq!(lines, 1, "the spawn script must have run exactly once");
}

/// The stagger reservation spaces consecutive spawns by the configured gap
/// without sleeping, so the sleep can run outside the lock.
#[test]
fn reserve_dispatch_stagger_spaces_consecutive_reservations() {
    let dir = tempdir().unwrap();
    let mut reg = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()));
    assert_eq!(reg.reserve_dispatch_stagger(), Duration::ZERO, "stagger disabled");

    let mut reg = SweepRegistry::new(SweepRegistryConfig::new(dir.path().to_path_buf()));
    reg.set_dispatch_stagger(Duration::from_millis(400));
    assert_eq!(reg.reserve_dispatch_stagger(), Duration::ZERO, "first spawn is free");
    let second = reg.reserve_dispatch_stagger();
    assert!(
        second > Duration::from_millis(300) && second <= Duration::from_millis(400),
        "{second:?}"
    );
    let third = reg.reserve_dispatch_stagger();
    assert!(
        third > Duration::from_millis(700) && third <= Duration::from_millis(800),
        "{third:?}"
    );
}
