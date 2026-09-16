//! Regression coverage for the dispatch guards' label-mutation safety:
//! non-closing PR references (#7757), and the "blocked-label removal"
//! allegation investigated in #7860.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use super::*;
use crate::sweep_registry::test_support::{
    open_pr_guard_rest_fallback_registry, park_guard_registry, running_issue_sweep_id,
    wait_until_dead, FIXTURE_CHILD_WAIT_MS,
};
use serial_test::serial;
use tempfile::tempdir;

pub(super) fn empty_graphql_registry(ws: &Path, pr: &str, status: i32) -> (SweepRegistry, PathBuf) {
    let (reg, log) = open_pr_guard_rest_fallback_registry(ws, pr, status, false);
    let fake = ws.join("fake-gh.sh");
    let script = std::fs::read_to_string(&fake).unwrap();
    let failure = "printf 'gh: rate limit exceeded\\n' >&2\nexit 1";
    assert!(script.contains(failure));
    let empty = r#"printf '%s\n' '{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[]}}}}}'
exit 0"#;
    std::fs::write(fake, script.replace(failure, empty)).unwrap();
    (reg, log)
}

#[test]
fn empty_graphql_consults_timeline_union() {
    for (pr, status, expected) in [
        ("5460", 0, OpenPrProbe::Open(5460)),
        ("", 0, OpenPrProbe::NoneOpen),
        ("", 1, OpenPrProbe::ProbeFailed),
        ("malformed", 0, OpenPrProbe::ProbeFailed),
    ] {
        let dir = tempdir().unwrap();
        let (reg, log) = empty_graphql_registry(dir.path(), pr, status);
        assert_eq!(reg.probe_open_linked_pr_transports(5240), expected);
        let calls = std::fs::read_to_string(log).unwrap();
        assert_eq!(calls.lines().filter(|s| s.contains("api graphql")).count(), 1);
        assert_eq!(
            calls
                .lines()
                .filter(|s| s.contains("/issues/5240/timeline"))
                .count(),
            1
        );
    }
}

#[test]
#[serial]
fn nonclosing_pr_refuses_dispatch_before_label_mutation() {
    let dir = tempdir().unwrap();
    let (mut reg, log) = empty_graphql_registry(dir.path(), "5460", 0);
    let err = reg
        .dispatch(&SweepKind::Issue(5240), None, None, None, None)
        .expect_err("non-closing PR must prevent duplicate dispatch");
    assert!(err.downcast_ref::<OpenPrDispatchError>().is_some(), "{err}");
    let calls = std::fs::read_to_string(log).unwrap();
    assert!(!calls.contains("issue edit"), "{calls}");
}

// --- #7860: a dispatch can never strip a deliberate `loom:blocked` park ---
//
// `kicad-tools#5481` reported the redispatch storm on `kicad-tools#5333` as a
// park-guard failure: a `loom:blocked` applied at 2026-09-16T02:05:30Z, then
// "stripped" by a dispatch that re-took the lease at 02:07:00Z. That premise
// does not survive the primary forge record — the park was first applied at
// 02:11:18Z, *after* the lease it supposedly preceded, and the storm stopped
// dead at that instant (zero lease acquisitions afterward, against 105 in the
// preceding 16 h). The #4444 park guard held, first time, with no fix. See
// `defaults/docs/token-pool.md` §"No unauthorized `loom:blocked` removal".
//
// "That report was wrong" is a claim about one incident, not a property of the
// code. These pin the property instead, and are deliberately TESTS ONLY —
// #7860 explicitly forbids adding a redundant park guard and calling it
// incident recovery.

/// Every `gh issue edit` invocation a fixture recorded, one per line.
fn issue_edit_calls(gh_log: &Path) -> Vec<String> {
    std::fs::read_to_string(gh_log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("issue edit "))
        .map(str::to_string)
        .collect()
}

/// #7860: on the **fail-open** park-probe path the claim must not remove
/// `loom:blocked`.
///
/// This is the worst case by construction, and the exact mechanism #5481
/// alleged: the issue IS parked, but the guard's REST probe fails, so #4444
/// is structurally blind and dispatch proceeds all the way to the claim. The
/// sibling `dispatch_fails_open_when_park_label_probe_errors` already pins
/// that dispatch *proceeds* here (a forge outage must never wedge the
/// daemon) — it asserts only that *an* `issue edit` happened, never what that
/// edit wrote. That is the gap this fills.
///
/// `flip_label_to_building` passes `--remove-label loom:issue` and no other
/// `--remove-label`, so the park survives. The observable outcome of a
/// fail-open claim over a parked issue is a *dual* `loom:blocked` +
/// `loom:building` state: wrong, but visibly wrong and self-correcting on the
/// next probe that succeeds — never a silent unpark that hands the issue back
/// to the work finder.
#[test]
#[serial]
fn fail_open_park_probe_claim_never_removes_the_blocked_label() {
    let dir = tempdir().unwrap();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    // Parked, but the REST probe exits non-zero ⇒ `None` ⇒ fail open.
    let (mut reg, log) = park_guard_registry(dir.path(), "loom:blocked", 1, "", false);

    let out = reg
        .dispatch(&SweepKind::Issue(7860), None, None, None, None)
        .expect("precondition: a failed park probe fails open (#4444)");
    assert!(wait_until_dead(out.pid, FIXTURE_CHILD_WAIT_MS));

    let edits = issue_edit_calls(&log);
    assert!(
        !edits.is_empty(),
        "precondition: the fail-open path reached the label flip; got: {edits:?}"
    );
    for edit in &edits {
        assert!(
            !edit.contains("--remove-label loom:blocked"),
            "a dispatch must NEVER remove a deliberate `loom:blocked` park — the mechanism \
             kicad-tools#5481 alleged and #7860 found unsupported; got: {edit:?}"
        );
        assert!(
            edit.contains("--remove-label loom:issue"),
            "the claim removes `loom:issue` and only `loom:issue`; got: {edit:?}"
        );
    }

    if let Some(id) = running_issue_sweep_id(&reg, 7860) {
        let _ = reg.cancel(&id, std::time::Duration::from_secs(2));
    }
    std::env::remove_var("LOOM_REPO");
}

/// #7860: the exact `kicad-tools#5333` end state — parked **and** carrying an
/// active non-closing PR — is refused, and the refusal writes no label at all.
///
/// The sibling `nonclosing_pr_refuses_dispatch_before_label_mutation` above
/// covers the unparked half of this shape; this adds the park, which is the
/// state #5333 was genuinely in from 2026-09-16T02:11:18Z onward (parked, with
/// draft PR #5336 open against it) and from which no further dispatch ever
/// occurred. Both guards want to refuse; 2.6 (open PR) runs first, so the
/// refusal is attributed to the PR. For the blocked-label question the outcome
/// is identical either way: **zero** `issue edit` calls, so no label — least of
/// all `loom:blocked` — was touched.
#[test]
#[serial]
fn parked_issue_with_an_active_non_closing_pr_is_refused_without_touching_labels() {
    let dir = tempdir().unwrap();
    std::env::set_var("LOOM_REPO", "rjwalters/loom");
    let (mut reg, log) = park_guard_registry(dir.path(), "loom:blocked", 0, "5336", false);

    let err = reg
        .dispatch(&SweepKind::Issue(5333), None, None, None, None)
        .expect_err("a parked issue with an active PR must never be dispatched");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("5336") || rendered.contains("loom:blocked"),
        "the refusal names the PR (2.6) or the park (2.7); got: {rendered}"
    );
    assert!(
        issue_edit_calls(&log).is_empty(),
        "a refused dispatch writes no labels — the park is left exactly as it was set (#7860); \
         got: {:?}",
        issue_edit_calls(&log)
    );
    std::env::remove_var("LOOM_REPO");
}

/// #7860: the daemon's ONLY `loom:blocked` -> `loom:issue` transition is the
/// startup quarantine-reconciliation pass, and it cannot fire for an issue the
/// daemon never quarantined.
///
/// `decide` short-circuits to `Keep` on `has_quarantine_comment == false`, and
/// `kicad-tools#5333` carries no `QUARANTINE_COMMENT_MARKER` comment anywhere
/// in its 1,316-comment history — so this path provably could not have
/// unparked it. It is doubly unreachable for the empty-pool death this issue
/// fixes: the #4122 carve-out means such a death never charges a quarantine
/// tally, so it can never post the marker that would make an issue eligible
/// here in the first place.
///
/// (`quarantine_reconciliation`'s own suite covers the recency rules; this
/// asserts only the entry condition, from #7860's angle.)
#[test]
fn an_issue_the_daemon_never_quarantined_is_never_auto_unparked() {
    use crate::quarantine_reconciliation::{decide, BlockedIssue, ReconcileAction};

    let never_quarantined = BlockedIssue {
        number: 5333,
        has_quarantine_comment: false,
        last_quarantine_comment_at: None,
        last_blocked_labeled_at: Some("2026-09-16T02:11:18Z".parse().expect("fixed timestamp")),
    };
    assert_eq!(
        decide(&never_quarantined),
        ReconcileAction::Keep,
        "the daemon's only unpark path requires its OWN quarantine marker; an issue it never \
         quarantined must never be auto-unparked (#7860)"
    );
}
