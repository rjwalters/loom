//! Reaper tests for the PR-side park check on the #4256 resume decision
//! (Issue #8689) — see the sibling `pr_park` module for the rationale.
//!
//! In their own module rather than in `tests.rs`: that file is over the
//! file-size ratchet's 1000-line threshold and therefore frozen at its current
//! size (`.loom/docs/file-size-policy.md`), the same reason
//! `claim_restore_tests.rs` exists.

use super::*;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

/// Install a fake `gh` whose REST label probe answers **differently for the
/// issue and for its linked PR** — the discrimination `park_guard_registry`
/// (one label set for every number) cannot express, and the whole point of
/// #8689: the block lives on the PR while the issue is unparked.
///
/// - `api graphql` reports `pr` as the open linked PR (so the resume path
///   engages and the 2.6 bypass matches);
/// - `api repos/<owner>/<repo>/issues/<pr> --jq .labels[].name` prints
///   `pr_labels` (whitespace-separated, one name per line);
/// - the same endpoint for any OTHER number prints `issue_labels`;
/// - the `--jq '{state, is_pr: …}'` form of that endpoint reports an open,
///   non-PR node so the 2.5 closed-issue guard passes;
/// - `issue view` reports "not blocked" so `restore_label_to_ready` takes its
///   ordinary `loom:building` -> `loom:issue` path;
/// - `repo view` resolves the owner/repo.
///
/// Every invocation is logged to the returned path.
fn pr_park_registry(
    ws: &std::path::Path,
    pr: u32,
    pr_labels: &str,
    issue_labels: &str,
) -> (SweepRegistry, PathBuf) {
    let gh_log = ws.join("gh-invocations.log");
    let fake_gh = ws.join("fake-gh.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
             printf '%s\\n' \"$*\" >> \"{log}\"\n\
             if [[ \"$1\" == \"issue\" && \"$2\" == \"view\" ]]; then\n\
             printf 'false\\n'\n\
             exit 0\n\
             fi\n\
             {gql}\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* && \"$*\" == *is_pr* ]]; then\n\
             printf '%s\\n' '{state}'\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$2\" == */issues/{pr} ]]; then\n\
             printf '%s\\n' {pr_labels}\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
             printf '%s\\n' {issue_labels}\n\
             exit 0\n\
             fi\n\
             if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
             printf 'rjwalters/loom\\n'\n\
             exit 0\n\
             fi\n\
             exit 0\n",
        log = gh_log.display(),
        gql = fake_gh_graphql_arm(&pr.to_string(), 0),
        state = state_probe_json("open", false),
        pr = pr,
        pr_labels = pr_labels,
        issue_labels = issue_labels,
    );
    std::fs::write(&fake_gh, &script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }

    let scripts_dir = ws.join(".loom").join("scripts");
    std::fs::create_dir_all(&scripts_dir).unwrap();
    let spawn = scripts_dir.join("spawn-claude.sh");
    std::fs::write(&spawn, "#!/usr/bin/env bash\necho spawned\nexit 0\n").unwrap();
    let mut sperms = std::fs::metadata(&spawn).unwrap().permissions();
    sperms.set_mode(0o755);
    std::fs::set_permissions(&spawn, sperms).unwrap();
    if let Ok(f) = std::fs::File::open(&spawn) {
        let _ = f.sync_all();
    }
    touch_sweep_command(ws);

    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.spawn_bin = Some(spawn);
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false; // exercise the real guard + flip path
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), gh_log)
}

/// Collect every `SweepResumeDispatched` event the bus saw for `issue`,
/// draining up to eight events (the reap emits `SweepCrashed`,
/// `SweepGlobalCompleted` and possibly dispatch events alongside it).
async fn resume_verdicts(
    sub: &mut crate::event_bus::Subscription,
    issue: u32,
) -> Vec<(u32, bool, Option<String>)> {
    let mut out = Vec::new();
    for _ in 0..8 {
        let Ok(ev) = tokio::time::timeout(Duration::from_secs(5), sub.recv()).await else {
            break;
        };
        if let Event::SweepResumeDispatched {
            issue: ev_issue,
            pr,
            dispatched,
            checkpoint_phase,
            ..
        } = ev.unwrap()
        {
            if ev_issue == issue {
                out.push((pr, dispatched, checkpoint_phase));
            }
        }
    }
    out
}

/// AC1 (#8689): the cap-exhausted-block shape. `/loom:sweep` blocked PR
/// #8682 (`loom:blocked` on the **PR**), wrote `doctor-done` and left the
/// checkpoint in place on purpose, then exited 0 — so the checkpoint DID
/// advance (`checkpoint_progress`) and the #5614 clean-exit guard cannot see
/// this shape, while the #4444 park guard only ever reads the ISSUE's labels.
/// Before this fix that combination spawned exactly one more child per block:
/// a full agent spawn and a rotated token re-verifying identical forge state.
///
/// The reaper must perform ZERO resume dispatches and restore the issue to
/// `loom:issue` on the first reap.
#[tokio::test]
#[serial]
async fn reaper_does_not_resume_when_the_linked_pr_is_blocked() {
    use crate::event_bus::EventBus;

    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, gh_log) = pr_park_registry(ws, 8682, "loom:blocked loom:changes-requested", "");
    let bus = Arc::new(EventBus::new());
    reg.set_event_bus(bus.clone());
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    // Checkpoint written AFTER the entry's `started_at`, so this run's own
    // `doctor-done` write counts as `checkpoint_progress` — exactly what a
    // cap-exhausted Doctor cycle leaves behind. The mtime is set explicitly
    // ahead of `started_at` rather than left to the write: Linux stamps file
    // mtimes from a coarse (tick-granular) kernel clock that can trail the
    // `Utc::now()` recorded as `started_at`, which made the #5614 clean-exit
    // guard short-circuit before the PR-park check on CI.
    let sweep_id = insert_clean_exit_running(&mut reg, 8668, 1);
    write_checkpoint_with_mtime(
        &reg,
        8668,
        "doctor-done",
        std::time::SystemTime::now() + Duration::from_secs(5),
    );

    let changed = reg.reap_once();
    assert!(changed >= 1);

    let verdicts = resume_verdicts(&mut sub, 8668).await;
    assert!(
        verdicts.iter().all(|(_, dispatched, _)| !dispatched),
        "a blocked linked PR is a considered terminal state — no resume may be \
         dispatched; got: {verdicts:?}"
    );
    assert!(
        verdicts
            .iter()
            .any(|(pr, _, phase)| *pr == 8682 && phase.as_deref() == Some("doctor-done")),
        "the refusal must stay failure-visible on the bus, naming the PR and phase; \
         got: {verdicts:?}"
    );
    // The load-bearing assertion: no fresh Running entry means no resume
    // dispatch — no agent spawn, no rotated token, no prompt-prefix load.
    assert!(
        running_issue_sweep_id(&reg, 8668).is_none(),
        "a blocked PR must not produce a resume dispatch (and so no fresh Running entry)"
    );
    // Lifting the park is not a failed attempt: the #4256 runway survives for
    // the Champion-lifted resume the retained `doctor-done` checkpoint serves.
    assert_eq!(
        reg.resume_attempt_counts.get(&8668).copied(),
        None,
        "the PR-park refusal must not consume a resume attempt"
    );

    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(
        calls.contains("api repos/rjwalters/loom/issues/8682 --jq .labels[].name"),
        "the refusal must come from reading the PR's OWN labels; got: {calls:?}"
    );
    // The park is on the PR, not the issue, so the issue is restored to the
    // ready state on this very first reap (AC1's second half).
    assert!(
        calls.contains("issue edit 8668 --remove-label loom:building --add-label loom:issue"),
        "the issue itself is unparked and must be restored to loom:issue; got: {calls:?}"
    );
    // Sanity: the fixture really produced the cap-exhausted shape — a clean
    // exit whose surviving checkpoint still classifies the entry as Crashed.
    let info = reg.entries.get(&sweep_id).unwrap();
    assert!(
        matches!(info.state, SweepState::Crashed { .. }),
        "a surviving checkpoint still classifies the entry as Crashed; got: {:?}",
        info.state
    );
    std::env::remove_var("LOOM_REPO");
}

/// AC2 (#8689): narrowness. A genuine crash (no retained handle, so
/// `exit_code == None` — the signal-death / reconstructed-entry shape) at
/// `doctor-done` whose linked PR carries NO park label must resume exactly as
/// #4256 always did. Same fixture as the test above; the ONLY difference is
/// the PR's labels.
#[tokio::test]
#[serial]
async fn reaper_still_resumes_a_crash_when_the_linked_pr_is_not_blocked() {
    use crate::event_bus::EventBus;

    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _gh_log) = pr_park_registry(ws, 8683, "loom:review-requested", "");
    let bus = Arc::new(EventBus::new());
    reg.set_event_bus(bus.clone());
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    write_checkpoint(&reg, 8669, "doctor-done");
    insert_dead_running_entry(&mut reg, 8669, "sweep-issue-8669-crashed");

    let changed = reg.reap_once();
    assert!(changed >= 1);

    let verdicts = resume_verdicts(&mut sub, 8669).await;
    assert!(
        verdicts.iter().any(|(_, dispatched, _)| *dispatched),
        "an unparked PR must still resume on a genuine crash (#4256 unchanged); \
         got: {verdicts:?}"
    );
    assert!(
        running_issue_sweep_id(&reg, 8669).is_some(),
        "a genuine crash resume must still create a fresh Running entry"
    );
    assert_eq!(
        reg.resume_attempt_counts.get(&8669).copied(),
        Some(1),
        "a genuine crash resume still consumes one attempt from the runway"
    );
    std::env::remove_var("LOOM_REPO");
}

/// Variable isolation for the pair above: the PR's park label alone decides.
/// A genuine crash — the shape AC2 protects — is still NOT resumed when the
/// linked PR carries the park, because re-running the lifecycle over a blocked
/// PR can only re-observe the block.
#[tokio::test]
#[serial]
async fn reaper_does_not_resume_a_crash_when_the_linked_pr_is_blocked() {
    use crate::event_bus::EventBus;

    let tmp = tempdir().unwrap();
    let ws = tmp.path();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, _gh_log) = pr_park_registry(ws, 8684, "loom:blocked", "");
    let bus = Arc::new(EventBus::new());
    reg.set_event_bus(bus.clone());
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    write_checkpoint(&reg, 8670, "doctor-done");
    insert_dead_running_entry(&mut reg, 8670, "sweep-issue-8670-crashed");

    let changed = reg.reap_once();
    assert!(changed >= 1);

    let verdicts = resume_verdicts(&mut sub, 8670).await;
    assert!(
        verdicts.iter().all(|(_, dispatched, _)| !dispatched),
        "the park label alone must decide, independent of how the sweep died; \
         got: {verdicts:?}"
    );
    assert!(
        running_issue_sweep_id(&reg, 8670).is_none(),
        "a blocked PR must not produce a resume dispatch"
    );
    std::env::remove_var("LOOM_REPO");
}
