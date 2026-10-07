//! W9: the memo serves refusals only, every resume decision reads the forge
//! live, and the refusal ledger counts distinct versus repeated refusals.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::forge_call_stats::counters;
use crate::sweep_registry::test_support::*;
use serial_test::serial;
use std::sync::Arc;
use tempfile::tempdir;

fn gh_calls(log: &Path) -> usize {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .count()
}

/// [`open_pr_guard_registry`] whose forge answers "no open linked PR" on
/// every leg the live probe reads: the closes-graph (empty), the
/// `issues/<n>/timeline` REST union (empty), and the #6788 known-PR backstop
/// (`pulls/<n>` is `closed`). The base fixture's catch-all `repos/*` arm
/// would otherwise answer the timeline with an issue-state body, which is
/// `ProbeFailed`, not `NoneOpen`. Both arms go before that catch-all, which
/// also matches their paths.
fn none_open_registry(ws: &Path) -> (SweepRegistry, PathBuf) {
    let (reg, log) = open_pr_guard_registry(ws, "", 0, false);
    let gh = ws.join("fake-gh.sh");
    let script = std::fs::read_to_string(&gh).unwrap();
    let catch_all = "if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]";
    let arms =
        format!("{}{}", fake_gh_timeline_rest_arm("", 0), fake_gh_pulls_state_arm("closed", 0));
    let patched = script.replacen(catch_all, &format!("{arms}{catch_all}"), 1);
    assert_ne!(patched, script, "the catch-all arm moved; update this fixture");
    std::fs::write(&gh, patched).unwrap();
    (reg, log)
}

#[test]
fn the_ledger_counts_a_pair_once_per_utc_hour() {
    let mut ledger = HourLedger::default();
    let t0 = DateTime::parse_from_rfc3339("2026-10-06T10:05:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let key = |issue, pr| (PathBuf::from("/ws/a"), issue, pr);
    assert!(ledger.first_this_hour(key(1, 10), t0));
    assert!(!ledger.first_this_hour(key(1, 10), t0 + chrono::Duration::minutes(50)));
    assert!(ledger.first_this_hour(key(1, 11), t0), "a different PR is a new pair");
    assert!(ledger.first_this_hour(key(2, 10), t0), "a different issue is a new pair");
    let other_ws = (PathBuf::from("/ws/b"), 1, 10);
    assert!(ledger.first_this_hour(other_ws, t0), "workspaces are counted apart");
    assert!(
        ledger.first_this_hour(key(1, 10), t0 + chrono::Duration::minutes(56)),
        "the next UTC hour starts over"
    );
}

/// A fresh memo refuses an ordinary dispatch at zero cost, but never answers
/// for a resume: the resume's probe reads the forge, which here says the PR
/// is gone.
#[test]
#[serial]
fn a_resume_never_consults_the_memo() {
    std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
    std::env::set_var("LOOM_REPO", "acme/widget");
    let dir = tempdir().unwrap();
    // The forge answers "no open linked PR" on every leg.
    let (reg, log) = none_open_registry(dir.path());
    reg.seed_open_pr_memo(9201, 9301, Utc::now());

    let memo = reg
        .dispatch_open_pr_memo(9201, None)
        .expect("an ordinary dispatch is refused");
    assert_eq!(memo.pr, 9301);
    assert_eq!(reg.dispatch_open_pr_probe(9201, None), OpenPrProbe::Open(9301));
    assert_eq!(gh_calls(&log), 0, "both memo answers cost no forge call");
    assert_eq!(counters::get(REFUSED_MEMO), 1);
    assert_eq!(counters::get(REFUSED_PROBED), 1);
    assert_eq!(counters::get(REFUSED_DISTINCT), 1, "one pair, refused twice");

    assert!(reg.dispatch_open_pr_memo(9201, Some(9301)).is_none());
    assert_eq!(
        reg.dispatch_open_pr_probe(9201, Some(9301)),
        OpenPrProbe::NoneOpen,
        "a resume reads the forge even with a fresh memo naming its own PR"
    );
    assert_eq!(reg.live_open_pr_probe(9201), OpenPrProbe::NoneOpen);
    assert!(gh_calls(&log) > 0, "the live probe went to the forge");
    std::env::remove_var("LOOM_REPO");
}

/// A resume whose live probe names a DIFFERENT open PR is still refused at
/// 2.6, and one whose live probe names its own PR proceeds even though a
/// stale memo names another (before W9 the 2.5 memo refused it).
#[test]
#[serial]
fn a_resume_is_decided_by_the_live_answer() {
    std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
    std::env::set_var("LOOM_REPO", "acme/widget");
    let dir = tempdir().unwrap();
    let (mut reg, _log) = open_pr_guard_registry(dir.path(), "9302", 0, false);
    let refused = reg
        .begin_issue_dispatch(&SweepKind::Issue(9202), None, None, None, None, Some(9399))
        .err()
        .expect("a live Open(Q) refuses a resume for P != Q");
    let typed = refused
        .downcast_ref::<OpenPrDispatchError>()
        .expect("refused by the open-PR guard");
    assert_eq!(typed.pr, 9302);

    let dir = tempdir().unwrap();
    let (mut reg, log) = open_pr_guard_registry(dir.path(), "9303", 0, false);
    reg.seed_open_pr_memo(9203, 9398, Utc::now());
    let began =
        reg.begin_issue_dispatch(&SweepKind::Issue(9203), None, None, None, None, Some(9303));
    if let Err(e) = &began {
        assert!(
            e.downcast_ref::<OpenPrDispatchError>().is_none(),
            "the stale memo must not refuse the resume: {e:#}"
        );
    }
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(calls.contains("issue edit 9203"), "the resume reached the flip: {calls}");
    if let Some(id) = running_issue_sweep_id(&reg, 9203) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}

/// The skeptic's resume hole: a fresh memo naming P must not make a crashed
/// sweep resume-eligible when the forge says P is gone. The reaper's
/// eligibility probe reads live, the forge answers `NoneOpen` on every leg
/// (closes-graph, timeline, and `pulls/P` is `closed` for the #6788
/// backstop), so no `SweepResumeDispatched` fires.
#[tokio::test]
#[serial]
async fn a_fresh_memo_never_makes_a_crash_resume_eligible() {
    use crate::event_bus::EventBus;

    std::env::remove_var(OPEN_PR_MEMO_ENABLE_ENV);
    std::env::set_var("LOOM_REPO", "acme/widget");
    let tmp = tempdir().unwrap();
    // The forge: no open linked PR. The memo: a fresh Open(9304).
    let (mut reg, gh_log) = none_open_registry(tmp.path());
    reg.seed_open_pr_memo(9204, 9304, Utc::now());
    let bus = Arc::new(EventBus::new());
    reg.set_event_bus(bus.clone());
    let mut sub = bus.subscribe::<[&str; 0], &str>([]);

    write_checkpoint(&reg, 9204, "judge-done");
    insert_dead_running_entry(&mut reg, 9204, "sweep-issue-9204-crashed");
    assert!(reg.reap_once() >= 1);

    for _ in 0..4 {
        let Ok(ev) = tokio::time::timeout(Duration::from_secs(2), sub.recv()).await else {
            break;
        };
        assert!(
            !matches!(ev.unwrap(), Event::SweepResumeDispatched { .. }),
            "a memo answer must never permit a resume"
        );
    }
    assert!(running_issue_sweep_id(&reg, 9204).is_none());
    let calls = std::fs::read_to_string(&gh_log).unwrap_or_default();
    assert!(calls.contains("api graphql"), "eligibility was probed live: {calls}");
    std::env::remove_var("LOOM_REPO");
}

/// Resume safety the red team asked to pin: a peer's `DispatchBackoffArmed`
/// for the issue reaches only the work-finder pre-filter, never the
/// reaper-driven resume's own 2.8 check.
#[test]
#[serial]
fn a_peer_backoff_does_not_refuse_a_resume() {
    std::env::set_var("LOOM_REPO", "acme/widget");
    let dir = tempdir().unwrap();
    let (mut reg, log) = open_pr_guard_registry(dir.path(), "9305", 0, false);
    let repo = peer_claims::repo_slug(&reg.config().workspace_root);
    let view = Arc::new(Mutex::new(PeerClaimView::new("self".into(), Duration::from_secs(120))));
    view.lock().unwrap().observe_dispatch_backoff_at(
        &ClaimAd::dispatch_backoff_armed(9205, repo, "peer".into(), 1, "ts".into(), 600),
        Instant::now(),
    );
    reg.set_peer_claims(view);
    assert!(
        reg.dispatch_backoff_issues(Utc::now()).contains(&9205),
        "the peer window is live"
    );
    // This host's own window too: 2.8 refuses a fresh dispatch, never a resume.
    reg.record_open_pr_guard_backoff(9205);
    let began =
        reg.begin_issue_dispatch(&SweepKind::Issue(9205), None, None, None, None, Some(9305));
    if let Err(e) = &began {
        assert!(
            e.downcast_ref::<DispatchBackoffError>().is_none(),
            "a resume is exempt from the backoff: {e:#}"
        );
    }
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(calls.contains("issue edit 9205"), "the resume reached the flip: {calls}");
    if let Some(id) = running_issue_sweep_id(&reg, 9205) {
        let _ = reg.cancel(&id, Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}
