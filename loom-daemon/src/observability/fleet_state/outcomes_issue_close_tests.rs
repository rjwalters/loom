//! Exits of a vanished row with no resolved PR, from its issue's close
//! (#11398), beside the shared helpers of `outcomes_tests`.

use super::*;

fn held_curator() -> HeldSweep {
    HeldSweep {
        repo: REPO.to_string(),
        issue: 20,
        stage: FleetStage::SweepCurator,
        entered_at: t0() - Duration::minutes(2),
        entered_at_lower_bound: false,
        pr: None,
        overflow: false,
        start: Default::default(),
        sweep_id: None,
    }
}

/// Issue 20 closed at `t0 + 7m` as `facts` says.
fn closed_at_7(facts: fn(DateTime<Utc>) -> IssueFacts) -> (DateTime<Utc>, Forge) {
    let at = t0() + Duration::minutes(7);
    let mut forge = Forge::default();
    forge.issues.insert(20, facts(at));
    (at, forge)
}

/// A held sweep-curator row for issue 20, gone from pass 1 on.
fn vanishing(passes: usize) -> Vec<FleetInput> {
    let mut inputs = vec![input(&[], vec![held_curator()])];
    inputs.extend((0..passes).map(|_| input(&[], Vec::new())));
    inputs
}

const NUMBER: u32 = 11401;

#[test]
fn an_issue_closed_not_planned_mid_sweep_is_one_abandoned_exit_at_the_close_time() {
    let (closed, mut forge) = closed_at_7(IssueFacts::NotMerged);
    let out = run(&vanishing(2), Duration::minutes(10), &mut forge);
    let r = stages(&out[1]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!(r[0].stage, FleetStage::SweepCurator);
    assert_eq!(r[0].exit, StageExit::Abandoned);
    assert_eq!(r[0].next_stage, None);
    assert_eq!(r[0].forge_transition_at, Some(closed));
    assert_eq!(r[0].left_at, closed);
    assert_eq!(r[0].dwell_sec, Some(9 * 60));
    assert_eq!(r[0].resolution_sec, Some(0));
    assert!(stages(&out[2]).is_empty(), "no second exit");
}

/// Test plan (b): a merge closed the issue though the row never had a PR, so
/// the stay landed at the merge instant, never `abandoned`.
#[test]
fn a_merge_closed_issue_without_a_pr_row_is_one_landed_exit_at_the_merge_time() {
    let (merged, mut forge) = closed_at_7(IssueFacts::Merged);
    let out = run(&vanishing(2), Duration::minutes(10), &mut forge);
    let r = stages(&out[1]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!(r[0].exit, StageExit::Landed);
    assert_eq!(r[0].pr_number, None);
    assert_eq!(r[0].forge_transition_at, Some(merged));
    assert_eq!(r[0].left_at, merged);
    assert_eq!(r[0].dwell_sec, Some(9 * 60));
    assert!(stages(&out[2]).is_empty(), "no second exit");
}

/// A hand close as `completed` has no closing commit, so no merge closed
/// it: one `abandoned` exit at `closed_at`, with dwell and a forge key.
#[test]
fn a_hand_completed_close_is_one_abandoned_exit_at_the_close_time() {
    let at = t0() + Duration::minutes(7);
    let mut forge = Forge::default();
    let events = serde_json::json!([closed_event("2026-10-09T12:07:00Z", None)]);
    forge
        .issues
        .insert(20, IssueFacts::closed(REPO, NUMBER, at, &[events], &[]));
    let out = run(&vanishing(2), Duration::minutes(10), &mut forge);
    let r = stages(&out[1]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!((r[0].exit, r[0].forge_transition_at), (StageExit::Abandoned, Some(at)));
    assert_eq!((r[0].left_at, r[0].dwell_sec), (at, Some(9 * 60)));
    assert!(r[0].has_fact_key());
    assert!(stages(&out[2]).is_empty(), "no second exit");
}

/// A close no `closed` event explains is `unknown`, but still ends at
/// `closed_at`, so it keeps a forge key.
#[test]
fn a_close_with_no_closed_event_is_unknown_at_the_close_time() {
    let (closed, mut forge) = closed_at_7(IssueFacts::ClosedUnresolved);
    let out = run(&vanishing(2), Duration::minutes(10), &mut forge);
    let r = stages(&out[1]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!((r[0].exit, r[0].forge_transition_at), (StageExit::Unknown, Some(closed)));
    assert_eq!((r[0].left_at, r[0].dwell_sec), (closed, None));
    assert!(r[0].has_fact_key());
    assert!(stages(&out[2]).is_empty(), "no second exit");
}

#[test]
fn two_hosts_diffing_one_close_emit_the_same_forge_instant() {
    for facts in [
        IssueFacts::NotMerged as fn(_) -> _,
        IssueFacts::Merged,
        IssueFacts::ClosedUnresolved,
    ] {
        let run_host = |step: i64| {
            let (_, mut forge) = closed_at_7(facts);
            let out = run(&vanishing(1), Duration::minutes(step), &mut forge);
            let r = stages(&out[1])[0].clone();
            (r.exit, r.stage, r.next_stage, r.forge_transition_at)
        };
        assert_eq!(run_host(10), run_host(25));
    }
}

#[test]
fn an_unread_issue_emits_nothing_until_it_reads_then_once() {
    let (closed, mut forge) = closed_at_7(IssueFacts::NotMerged);
    forge.issue_failures = 1;
    let out = run(&vanishing(3), Duration::minutes(10), &mut forge);
    assert!(stages(&out[1]).is_empty(), "no timeless exit on a failed read");
    let r = stages(&out[2]);
    assert_eq!(r.len(), 1, "{out:?}");
    assert_eq!((r[0].exit, r[0].forge_transition_at), (StageExit::Abandoned, Some(closed)));
    assert!(stages(&out[3]).is_empty());
}

/// Test plan (f) and the cap: a transferred or deleted issue answers 404,
/// which is a failed read. It waits [`PENDING_ISSUE_MAX_PASSES`] failed
/// retries, then is one `unknown` stamped at the pass that first saw it gone,
/// with no forge instant (so no fact id), and is never emitted again.
#[test]
fn a_transferred_or_404_issue_is_one_unknown_after_the_cap_and_never_again() {
    let mut forge = Forge {
        issue_failures: usize::MAX,
        ..Forge::default()
    };
    let cap = PENDING_ISSUE_MAX_PASSES as usize;
    let out = run(&vanishing(cap + 5), Duration::minutes(5), &mut forge);
    // Pass 1 sees it gone; passes 2..=cap+1 are its failed retries.
    for (i, pass) in out.iter().enumerate() {
        let r = stages(pass);
        if i == cap + 1 {
            assert_eq!(r.len(), 1, "pass {i}: {out:?}");
            assert_eq!((r[0].exit, r[0].forge_transition_at), (StageExit::Unknown, None));
            assert_eq!(r[0].left_at, t0() + Duration::minutes(5), "first-seen-gone pass");
            assert_eq!(r[0].dwell_sec, None);
        } else {
            assert!(r.is_empty(), "pass {i}: {out:?}");
        }
    }
    assert_eq!(forge.issue_reads, cap + 1, "no read once it is emitted");
}

#[test]
fn a_vanished_row_whose_issue_is_still_open_stays_unknown() {
    let mut forge = Forge::default();
    let out = run(&vanishing(1), Duration::minutes(10), &mut forge);
    assert_eq!(stages(&out[1])[0].exit, StageExit::Unknown);
}

fn closed_event(at: &str, commit: Option<&str>) -> serde_json::Value {
    serde_json::json!({"event": "closed", "created_at": at, "commit_id": commit})
}

#[test]
fn a_closed_event_with_a_commit_is_a_merge() {
    let at = t0();
    let page = serde_json::json!([
        {"event": "labeled", "created_at": "2026-10-09T10:00:00Z", "label": {"name": "loom:issue"}},
        closed_event("2026-10-09T12:00:00Z", Some("abc123")),
    ]);
    assert_eq!(IssueFacts::closed(REPO, NUMBER, at, &[page], &[]), IssueFacts::Merged(at));
}

#[test]
fn a_closed_event_with_no_commit_is_unmerged() {
    let at = t0();
    let page = serde_json::json!([closed_event("2026-10-09T12:00:00Z", None)]);
    assert_eq!(IssueFacts::closed(REPO, NUMBER, at, &[page], &[]), IssueFacts::NotMerged(at));
}

#[test]
fn only_the_latest_close_says_how_it_closed() {
    let at = t0();
    // Merged once, reopened, then closed by hand.
    let pages = [
        serde_json::json!([closed_event("2026-10-09T09:00:00Z", Some("abc123"))]),
        serde_json::json!([
            {"event": "reopened", "created_at": "2026-10-09T10:00:00Z"},
            closed_event("2026-10-09T12:00:00Z", None),
        ]),
    ];
    assert_eq!(IssueFacts::closed(REPO, NUMBER, at, &pages, &[]), IssueFacts::NotMerged(at));
}

#[test]
fn no_closed_event_keeps_the_close_instant_unresolved() {
    let page = serde_json::json!([]);
    assert_eq!(
        IssueFacts::closed(REPO, NUMBER, t0(), &[page], &[]),
        IssueFacts::ClosedUnresolved(t0())
    );
}

/// A `Closes #N` merge leaves the `closed` event's `commit_id` null (the real
/// response for #11401, closed 2s after PR #11409 merged); the merged PR in
/// the timeline is the evidence.
fn pr_reference(merged_at: Option<&str>) -> serde_json::Value {
    pr_reference_with(merged_at, "Closes #11401", "https://api.github.com/repos/rjwalters/loom")
}

fn pr_reference_with(
    merged_at: Option<&str>,
    body: &str,
    repository_url: &str,
) -> serde_json::Value {
    serde_json::json!({
        "event": "cross-referenced",
        "created_at": "2026-10-09T11:00:00Z",
        "source": {"issue": {
            "number": 9,
            "body": body,
            "repository_url": repository_url,
            "pull_request": {"merged_at": merged_at},
        }},
    })
}

#[test]
fn a_merge_close_with_a_null_commit_is_a_merge_through_the_referencing_pr() {
    let at = t0();
    let events = serde_json::json!([closed_event("2026-10-09T12:00:02Z", None)]);
    let timeline = serde_json::json!([
        {"event": "labeled", "created_at": "2026-10-09T10:00:00Z"},
        pr_reference(Some("2026-10-09T11:59:59Z")),
    ]);
    assert_eq!(
        IssueFacts::closed(REPO, NUMBER, at, &[events], &[timeline]),
        IssueFacts::Merged(at)
    );
}

/// A merged PR that only mentions the issue (or closes a different one, or
/// lives in another repo) is not what closed it: a hand close right after
/// its merge stays `abandoned`.
#[test]
fn a_nearby_merged_pr_without_a_closing_reference_does_not_make_a_hand_close_a_merge() {
    let at = t0();
    let events = serde_json::json!([closed_event("2026-10-09T12:00:02Z", None)]);
    let near = Some("2026-10-09T11:59:59Z");
    for reference in [
        pr_reference_with(near, "Related to #11401", "https://api.github.com/repos/rjwalters/loom"),
        pr_reference_with(near, "Closes #7", "https://api.github.com/repos/rjwalters/loom"),
        pr_reference_with(
            near,
            "Does not close #11401",
            "https://api.github.com/repos/rjwalters/loom",
        ),
        pr_reference_with(near, "Closes #11401", "https://api.github.com/repos/rjwalters/other"),
        // A qualified reference naming a different repository closes that
        // repository's issue, never this one (#11402).
        pr_reference_with(
            near,
            "Closes other/repo#11401",
            "https://api.github.com/repos/rjwalters/loom",
        ),
        pr_reference_with(
            near,
            "Does not close rjwalters/loom#11401",
            "https://api.github.com/repos/rjwalters/loom",
        ),
    ] {
        let timeline = serde_json::json!([reference]);
        assert_eq!(
            IssueFacts::closed(REPO, NUMBER, at, std::slice::from_ref(&events), &[timeline]),
            IssueFacts::NotMerged(at),
            "{reference}"
        );
    }
}

/// A repository-qualified closing reference (`Closes rjwalters/loom#N`) is
/// evidence of the merge close just like a bare `#N`; its `closed` event has
/// a null `commit_id` (#11402).
#[test]
fn a_repository_qualified_closing_reference_with_a_null_commit_is_a_merge() {
    let at = t0();
    let events = serde_json::json!([closed_event("2026-10-09T12:00:02Z", None)]);
    let timeline = serde_json::json!([pr_reference_with(
        Some("2026-10-09T11:59:59Z"),
        "Fixes rjwalters/loom#11401.",
        "https://api.github.com/repos/rjwalters/loom",
    ),]);
    assert_eq!(
        IssueFacts::closed(REPO, NUMBER, at, &[events], &[timeline]),
        IssueFacts::Merged(at)
    );
}

/// A Development-sidebar link closes the linked same-repository issue when
/// the PR merges, with no closing keyword in the body at all and a null
/// `commit_id` on the `closed` event (#11402).
fn sidebar_link(merged_at: Option<&str>, repository_url: &str) -> serde_json::Value {
    sidebar_link_at("2026-10-09T11:00:00Z", merged_at, repository_url)
}

/// [`sidebar_link`] with the link's own instant: a `disconnected` event
/// only revokes a link that was taken out earlier.
fn sidebar_link_at(
    created_at: &str,
    merged_at: Option<&str>,
    repository_url: &str,
) -> serde_json::Value {
    serde_json::json!({
        "event": "connected",
        "created_at": created_at,
        "source_issue": {
            "number": 9,
            "body": "Implements the fix. No closing keyword here.",
            "repository_url": repository_url,
            "pull_request": {"merged_at": merged_at},
        },
    })
}

/// A `disconnected` event revoking the sidebar link to PR #9 at `at`.
fn revoked_sidebar_link(at: &str) -> serde_json::Value {
    serde_json::json!({
        "event": "disconnected",
        "created_at": at,
        "source_issue": {
            "number": 9,
            "repository_url": "https://api.github.com/repos/rjwalters/loom",
        },
    })
}

#[test]
fn a_development_sidebar_link_to_a_merged_pr_is_a_merge_with_a_null_commit() {
    let at = t0();
    let events = serde_json::json!([closed_event("2026-10-09T12:00:02Z", None)]);
    let timeline = serde_json::json!([sidebar_link(
        Some("2026-10-09T11:59:59Z"),
        "https://api.github.com/repos/rjwalters/loom",
    )]);
    assert_eq!(
        IssueFacts::closed(REPO, NUMBER, at, &[events], &[timeline]),
        IssueFacts::Merged(at)
    );
}

/// A sidebar link proves a link, not a close, unless a merge in this
/// repository sits within the close window: another repository's PR, or an
/// unmerged one, leaves a hand close `abandoned`.
#[test]
fn a_sidebar_link_to_a_pr_that_cannot_close_this_repo_is_not_a_merge() {
    let at = t0();
    let events = serde_json::json!([closed_event("2026-10-09T12:00:02Z", None)]);
    for link in [
        sidebar_link(Some("2026-10-09T11:59:59Z"), "https://api.github.com/repos/rjwalters/other"),
        sidebar_link(None, "https://api.github.com/repos/rjwalters/loom"),
    ] {
        let timeline = serde_json::json!([link]);
        assert_eq!(
            IssueFacts::closed(REPO, NUMBER, at, std::slice::from_ref(&events), &[timeline]),
            IssueFacts::NotMerged(at),
            "{link}"
        );
    }
}

/// A sidebar link a later `disconnected` event revoked closes nothing on
/// merge: the hand close stays `abandoned`. A disconnect that predates a
/// later reconnect leaves the link standing (#11402).
#[test]
fn a_revoked_sidebar_link_does_not_make_a_hand_close_a_merge() {
    let at = t0();
    let events = serde_json::json!([closed_event("2026-10-09T12:00:02Z", None)]);
    let merged = (Some("2026-10-09T11:59:59Z"), "https://api.github.com/repos/rjwalters/loom");
    // Revoked: the disconnect postdates the link.
    let timeline = serde_json::json!([
        sidebar_link(merged.0, merged.1),
        revoked_sidebar_link("2026-10-09T11:00:05Z"),
    ]);
    assert_eq!(
        IssueFacts::closed(REPO, NUMBER, at, std::slice::from_ref(&events), &[timeline]),
        IssueFacts::NotMerged(at)
    );
    // Reconnected: the disconnect predates the link that stands at merge.
    let timeline = serde_json::json!([
        revoked_sidebar_link("2026-10-09T11:00:05Z"),
        sidebar_link_at("2026-10-09T11:00:10Z", merged.0, merged.1),
    ]);
    assert_eq!(
        IssueFacts::closed(REPO, NUMBER, at, std::slice::from_ref(&events), &[timeline]),
        IssueFacts::Merged(at)
    );
}

#[test]
fn an_unmerged_or_long_past_referencing_pr_does_not_make_a_hand_close_a_merge() {
    let at = t0();
    let events = serde_json::json!([closed_event("2026-10-09T12:00:02Z", None)]);
    for reference in [
        pr_reference(None),
        pr_reference(Some("2026-10-09T09:00:00Z")),
    ] {
        let timeline = serde_json::json!([reference]);
        assert_eq!(
            IssueFacts::closed(REPO, NUMBER, at, std::slice::from_ref(&events), &[timeline]),
            IssueFacts::NotMerged(at)
        );
    }
}
