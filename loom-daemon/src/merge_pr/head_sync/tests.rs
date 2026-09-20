//! Unit tests for the self-sync head attribution guard (#8164).
//!
//! The interesting cases are all *refusals*: one authorization shape is
//! correct and every deviation from it must be refused, so the tests are
//! organized as "the incident, then each clause broken in turn".

use super::*;

/// The SHAs of the incident, spelled out once: the approved head the merge was
/// gated on, the base tip that got synced into it, and the merge commit
/// `update-branch` landed.
const APPROVED: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BASE_TIP: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const SYNCED: &str = "cccccccccccccccccccccccccccccccccccccccc";
const FOREIGN_PUSH: &str = "dddddddddddddddddddddddddddddddddddddddd";

/// The #8164 incident exactly: we synced, GitHub 409'd on the pre-sync head,
/// and the new head is `merge(approved, base tip)`.
fn incident() -> Evidence {
    Evidence {
        response: "PR #1: {\"message\":\"Head branch was modified. Review and try the merge \
                   again.\",\"status\":\"409\"}"
            .to_string(),
        mismatch_confirmed: false,
        self_synced: true,
        retry_used: false,
        precondition_sha: APPROVED.to_string(),
        current_head_sha: SYNCED.to_string(),
        head_parents: vec![APPROVED.to_string(), BASE_TIP.to_string()],
        second_parent_in_base: Some(true),
    }
}

fn is_retry(v: &Verdict) -> bool {
    matches!(v, Verdict::SelfSyncRetry { .. })
}

fn why(v: &Verdict) -> String {
    match v {
        Verdict::Foreign(w) => w.clone(),
        Verdict::SelfSyncRetry { new_head } => {
            panic!("expected a refusal, got a retry on {new_head}")
        }
    }
}

// ---------------------------------------------------------------- the fix --

#[test]
fn the_incident_authorizes_exactly_one_retry() {
    let v = classify(&incident());
    assert_eq!(
        v,
        Verdict::SelfSyncRetry {
            new_head: SYNCED.to_string()
        },
        "a head moved solely by this run's own base-sync is the case #8164 exists to retry"
    );
}

#[test]
fn the_retry_reports_the_head_to_merge_next() {
    // The caller re-gates the retried merge on THIS sha; handing back the old
    // one would retry the identical failing call.
    let Verdict::SelfSyncRetry { new_head } = classify(&incident()) else {
        panic!("expected a retry");
    };
    assert_eq!(new_head, SYNCED);
    assert_ne!(new_head, APPROVED);
}

// ------------------------------------------------------- refusal clauses --

#[test]
fn a_response_that_is_not_a_head_mismatch_is_refused() {
    let mut ev = incident();
    ev.response = "Base branch was modified. Review and try the merge again.".to_string();
    let v = classify(&ev);
    assert!(!is_retry(&v));
    assert!(
        why(&v).contains("stale head-SHA precondition"),
        "the refusal must name the clause that failed, got: {}",
        why(&v)
    );
}

#[test]
fn an_exit_code_confirmation_substitutes_for_the_text() {
    // `loom-daemon forge auto-merge` signals a head mismatch by exit 4; its
    // stderr is not one of the three forge strings.
    let mut ev = incident();
    ev.response = "auto-merge failed: head SHA precondition rejected".to_string();
    assert!(!is_retry(&classify(&ev)), "text alone does not establish it");
    ev.mismatch_confirmed = true;
    assert!(
        is_retry(&classify(&ev)),
        "the native path's exit 4 is the same fact by another channel"
    );
}

#[test]
fn a_head_move_we_did_not_cause_is_never_retried() {
    let mut ev = incident();
    ev.self_synced = false;
    let v = classify(&ev);
    assert!(!is_retry(&v));
    assert!(why(&v).contains("never pushed to the head branch"));
}

#[test]
fn the_retry_budget_is_one() {
    let mut ev = incident();
    ev.retry_used = true;
    let v = classify(&ev);
    assert!(!is_retry(&v), "a second mismatch is a live race, not our sync");
    assert!(why(&v).contains("already spent"));
}

#[test]
fn an_unchanged_head_is_refused() {
    let mut ev = incident();
    ev.current_head_sha = APPROVED.to_string();
    ev.head_parents = vec![APPROVED.to_string(), BASE_TIP.to_string()];
    let v = classify(&ev);
    assert!(!is_retry(&v), "re-reading cannot fix a refusal the head did not cause");
    assert!(why(&v).contains("unchanged from the merge precondition"));
}

#[test]
fn an_unknown_sha_on_either_side_is_refused() {
    for (label, mutate) in [
        (
            "precondition",
            (|e: &mut Evidence| e.precondition_sha.clear()) as fn(&mut Evidence),
        ),
        ("current head", |e: &mut Evidence| e.current_head_sha.clear()),
    ] {
        let mut ev = incident();
        mutate(&mut ev);
        let v = classify(&ev);
        assert!(!is_retry(&v), "an empty {label} SHA must refuse");
        assert!(why(&v).contains("could not be determined"));
    }
}

#[test]
fn a_single_parent_head_is_refused() {
    // The shape of a new commit pushed on top of the approved head — the
    // #5579 case, which must stay a hard stop.
    let mut ev = incident();
    ev.current_head_sha = FOREIGN_PUSH.to_string();
    ev.head_parents = vec![APPROVED.to_string()];
    ev.second_parent_in_base = None;
    let v = classify(&ev);
    assert!(!is_retry(&v));
    assert!(why(&v).contains("1 parent(s)"), "got: {}", why(&v));
}

#[test]
fn an_octopus_merge_is_refused() {
    let mut ev = incident();
    ev.head_parents = vec![
        APPROVED.to_string(),
        BASE_TIP.to_string(),
        FOREIGN_PUSH.to_string(),
    ];
    let v = classify(&ev);
    assert!(!is_retry(&v));
    assert!(why(&v).contains("3 parent(s)"));
}

#[test]
fn a_merge_commit_pushed_on_top_of_the_sync_is_refused() {
    // First parent is the SYNC commit, not the approved head: someone pushed
    // after our sync landed, so the new head carries unreviewed content.
    let mut ev = incident();
    ev.current_head_sha = FOREIGN_PUSH.to_string();
    ev.head_parents = vec![SYNCED.to_string(), BASE_TIP.to_string()];
    let v = classify(&ev);
    assert!(!is_retry(&v));
    assert!(why(&v).contains("first parent"));
}

#[test]
fn a_merge_of_something_other_than_base_is_refused() {
    // The attack the structural test exists for: a two-parent commit whose
    // first parent IS the approved head, but whose second parent is an
    // arbitrary branch full of unreviewed commits.
    let mut ev = incident();
    ev.head_parents = vec![APPROVED.to_string(), FOREIGN_PUSH.to_string()];
    ev.second_parent_in_base = Some(false);
    let v = classify(&ev);
    assert!(!is_retry(&v));
    assert!(why(&v).contains("not contained in the base branch"));
}

#[test]
fn an_undetermined_containment_is_refused_not_assumed() {
    let mut ev = incident();
    ev.second_parent_in_base = None;
    let v = classify(&ev);
    assert!(!is_retry(&v), "a lookup failure is not evidence of our own sync");
    assert!(why(&v).contains("could not determine"));
}

#[test]
fn a_rebase_style_update_is_refused() {
    // GitHub's update-branch can be configured to rebase instead of merge. A
    // rebased branch is a rewritten branch: one parent, new SHAs throughout,
    // and nothing tying it to the approved head. Refusing is the correct
    // (conservative) outcome — the caller re-queues, the next pass re-reads.
    let mut ev = incident();
    ev.current_head_sha = FOREIGN_PUSH.to_string();
    ev.head_parents = vec![BASE_TIP.to_string()];
    ev.second_parent_in_base = None;
    assert!(!is_retry(&classify(&ev)));
}

// ------------------------------------------------- the mismatch predicate --

#[test]
fn the_three_forge_strings_are_recognized() {
    for s in [
        "Error: Head branch was modified. Review and try the merge again. (HTTP 409)",
        "{\"message\":\"head out of date\",\"url\":\"https://gitea.example.com/api/v1/...\"}",
        "could not enable auto-merge: expectedHeadOid does not match current head",
    ] {
        assert!(is_head_mismatch(s), "should have fired on {s:?}");
    }
}

#[test]
fn the_predicate_is_case_insensitive_like_the_shells_grep_ei() {
    assert!(is_head_mismatch("HEAD BRANCH WAS MODIFIED."));
    assert!(is_head_mismatch("EXPECTEDHEADOID"));
    assert!(is_head_mismatch("Head Out Of Date"));
}

#[test]
fn base_branch_was_modified_is_not_a_head_mismatch() {
    // The whole reason the two matchers are separate: this one means
    // sync-and-retry, and conflating them either retries forever against a
    // moving head or merges a diff nobody approved.
    for s in [
        "Error: Base branch was modified. Review and try the merge again. (HTTP 409)",
        "Merge already in progress",
        "Pull request Pull request is in clean status (enablePullRequestAutoMerge)",
        "Pull request Pull request is in unstable status (enablePullRequestAutoMerge)",
    ] {
        assert!(!is_head_mismatch(s), "should NOT have fired on {s:?}");
    }
}

#[test]
fn the_literal_dot_in_the_github_string_is_load_bearing() {
    // The shell pattern is `Head branch was modified\.` — the escaped dot
    // keeps prose about a head branch being modified from tripping it.
    assert!(!is_head_mismatch(
        "the head branch was modified by a later push, so we re-queued"
    ));
}

// ------------------------------------------------------------- messages ----

#[test]
fn the_retry_message_names_both_heads_and_the_issue() {
    let m = retry_message("8191", APPROVED, SYNCED);
    assert!(m.contains("PR #8191"));
    assert!(m.contains(&APPROVED[..8]));
    assert!(m.contains(&SYNCED[..8]));
    assert!(m.contains("#8164"));
}

#[test]
fn the_foreign_message_carries_the_reason_and_says_what_happens_next() {
    let m = foreign_message("8191", "some reason");
    assert!(m.contains("some reason"));
    assert!(m.contains("Re-queueing"));
}
