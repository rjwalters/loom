use super::*;
use chrono::Duration;

fn fixture_claimed_at() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-08-19T08:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn comment(created_at: DateTime<Utc>, body: &str) -> PrComment {
    PrComment {
        created_at,
        body: body.to_string(),
    }
}

fn journal_entry(repo: &str, issue: u32, pid: u32) -> JournalEntry {
    JournalEntry {
        repo: repo.to_string(),
        issue,
        pid,
        started_at: Utc::now(),
    }
}

#[test]
fn claim_activity_marker_matches_claim_staleness_sh() {
    // The marker MUST be byte-identical to what
    // `defaults/scripts/claim-staleness.sh marker` prints:
    //   ACTIVITY_PREFIX='<!-- loom:claim-activity claim=' + CLAIMED_AT + ' -->'
    // with CLAIMED_AT the timeline `created_at` verbatim (which that
    // script validates as ^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$).
    assert_eq!(CLAIM_ACTIVITY_MARKER_PREFIX, "<!-- loom:claim-activity claim=");
    assert_eq!(
        claim_activity_marker(fixture_claimed_at()),
        "<!-- loom:claim-activity claim=2026-08-19T08:00:00Z -->"
    );
}

#[test]
fn most_recent_claim_activity_at_ignores_an_unrelated_comment() {
    // AC (a) / the #6513 shape reconstructed daemon-side: a routine
    // Builder post-push status note is not claimant liveness. Before
    // #6523 this comment WAS counted (it is not a stand-down note), which
    // is exactly the conflation #6514 removed on the agent side.
    let claimed_at = fixture_claimed_at();
    let comments = vec![
        comment(claimed_at + Duration::minutes(2), "Pushed the fix, CI running."),
        comment(claimed_at + Duration::minutes(9), "Champion: capped-PR notice."),
    ];
    assert_eq!(
        most_recent_claim_activity_at(&comments, claimed_at),
        None,
        "an unrelated comment must not count as claimant activity"
    );
}

#[test]
fn most_recent_claim_activity_at_counts_a_marked_claimant_heartbeat() {
    // AC (b): a comment carrying THIS claim's marker is claimant liveness.
    let claimed_at = fixture_claimed_at();
    let heartbeat_at = claimed_at + Duration::minutes(12);
    let comments = vec![
        comment(claimed_at + Duration::minutes(2), "Pushed the fix, CI running."),
        comment(
            heartbeat_at,
            &format!(
                "Doctor: still working the failing test.\n{}",
                claim_activity_marker(claimed_at)
            ),
        ),
    ];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), Some(heartbeat_at));
}

#[test]
fn most_recent_claim_activity_at_takes_the_newest_marked_heartbeat() {
    let claimed_at = fixture_claimed_at();
    let marker = claim_activity_marker(claimed_at);
    let newest = claimed_at + Duration::minutes(20);
    let comments = vec![
        comment(claimed_at + Duration::minutes(5), &marker),
        comment(newest, &marker),
        comment(claimed_at + Duration::minutes(12), &marker),
    ];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), Some(newest));
}

#[test]
fn most_recent_claim_activity_at_ignores_a_marker_for_a_different_claim() {
    // Mirrors claim-staleness.sh: the marker is matched against the
    // claim's OWN labeled-at timestamp, so a heartbeat left behind by an
    // earlier claim generation (before a reclaim + re-claim) cannot keep
    // the new claim alive.
    let claimed_at = fixture_claimed_at();
    let older_claim = claimed_at - Duration::minutes(45);
    let comments = vec![comment(
        claimed_at + Duration::minutes(3),
        &format!("Judge: reviewing.\n{}", claim_activity_marker(older_claim)),
    )];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
}

#[test]
fn most_recent_claim_activity_at_ignores_comments_at_or_before_the_claim() {
    let claimed_at = fixture_claimed_at();
    let marker = claim_activity_marker(claimed_at);
    let comments = vec![
        comment(claimed_at - Duration::minutes(1), &marker),
        comment(claimed_at, &marker),
    ];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
}

#[test]
fn most_recent_claim_activity_at_still_excludes_standdown_comments() {
    // #4618 regression guard, preserved: a stand-down comment is evidence
    // a LATER pass declined to reclaim, never claimant activity — even in
    // the pathological case where its body quotes an activity marker.
    let claimed_at = fixture_claimed_at();
    let comments = vec![comment(
            claimed_at + Duration::minutes(7),
            &format!(
                "Judge pass: standing down, not stomping.\n{}\n{STANDDOWN_MARKER_PREFIX}2026-08-19T08:00:00Z seq=2 -->",
                claim_activity_marker(claimed_at)
            ),
        )];
    assert_eq!(most_recent_claim_activity_at(&comments, claimed_at), None);
}

// ------------------------------------------------------------------
// Issue #10235: Judge-progress comments and claimant head pushes keep a
// long-running review claim alive; the deciding signal is named.
// ------------------------------------------------------------------

fn pr_with_signals(
    now: DateTime<Utc>,
    claim_age_min: i64,
    judge: Option<i64>,
    push: Option<i64>,
) -> ClaimedPr {
    ClaimedPr {
        number: 10235,
        updated_at: Some(now - Duration::minutes(1)),
        claim_labeled_at: Some(now - Duration::minutes(claim_age_min)),
        most_recent_claim_activity_at: None,
        most_recent_judge_activity_at: judge.map(|m| now - Duration::minutes(m)),
        most_recent_head_push_at: push.map(|m| now - Duration::minutes(m)),
        head_ref_name: None,
    }
}

fn tl(event: &str, at: DateTime<Utc>, actor: &str) -> TimelineEvent {
    TimelineEvent {
        event: event.to_string(),
        created_at: at,
        actor: Some(actor.to_string()),
    }
}

#[test]
fn recent_judge_comment_keeps_an_aged_claim() {
    let now = Utc::now();
    let pr = pr_with_signals(now, 35, Some(5), None);
    assert_eq!(pr_liveness(&pr).1, LivenessSignal::JudgeComment);
    assert_eq!(decide_pr(&pr, None, None, &|_| true, 30.0, now), PrReconcileAction::Keep);
}

#[test]
fn recent_claimant_head_push_keeps_an_aged_claim() {
    let now = Utc::now();
    let pr = pr_with_signals(now, 35, None, Some(4));
    assert_eq!(pr_liveness(&pr).1, LivenessSignal::HeadPush);
    assert_eq!(decide_pr(&pr, None, None, &|_| true, 30.0, now), PrReconcileAction::Keep);
}

#[test]
fn dead_claimant_with_no_signals_is_still_reclaimed() {
    let now = Utc::now();
    let pr = pr_with_signals(now, 35, None, None);
    assert_eq!(pr_liveness(&pr).1, LivenessSignal::LabelAge);
    assert!(matches!(
        decide_pr(&pr, None, None, &|_| true, 30.0, now),
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { .. })
    ));
}

#[test]
fn signals_older_than_the_ttl_do_not_keep_the_claim() {
    let now = Utc::now();
    let pr = pr_with_signals(now, 90, Some(40), Some(50));
    assert!(matches!(
        decide_pr(&pr, None, None, &|_| true, 30.0, now),
        PrReconcileAction::Reclaim(PrReclaimReason::Aged { .. })
    ));
}

#[test]
fn dead_pid_precedence_is_unchanged_with_signals() {
    let now = Utc::now();
    let pr = pr_with_signals(now, 35, Some(5), None);
    let entry = journal_entry("/repo/a", 1, 111);
    // Live pid keeps; a dead pid with a fresh signal is still Keep (age gate).
    assert_eq!(
        decide_pr(&pr, Some(&entry), None, &|_| true, 30.0, now),
        PrReconcileAction::Keep
    );
    let stale = pr_with_signals(now, 90, Some(60), None);
    assert!(matches!(
        decide_pr(&stale, Some(&entry), None, &|_| false, 30.0, now),
        PrReconcileAction::Reclaim(PrReclaimReason::DeadPid { pid: 111 })
    ));
}

#[test]
fn treating_claims_use_the_same_decision_path() {
    // decide_pr is label-agnostic: a 70m treating claim with a 3m-old push is kept at 60m.
    let now = Utc::now();
    let pr = pr_with_signals(now, 70, None, Some(3));
    assert_eq!(decide_pr(&pr, None, None, &|_| true, 60.0, now), PrReconcileAction::Keep);
}

#[test]
fn newest_signal_is_named() {
    let now = Utc::now();
    let mut pr = pr_with_signals(now, 35, Some(10), Some(2));
    assert_eq!(pr_liveness(&pr).1, LivenessSignal::HeadPush);
    pr.most_recent_claim_activity_at = Some(now - Duration::minutes(1));
    assert_eq!(pr_liveness(&pr).1, LivenessSignal::ClaimActivityMarker);
}

#[test]
fn judge_activity_requires_a_judge_marker_after_the_claim_and_not_standdown() {
    let claimed_at = fixture_claimed_at();
    let at = claimed_at + Duration::minutes(5);
    assert_eq!(
        most_recent_judge_activity_at(&[comment(at, "Builder: pushed a fix")], claimed_at),
        None
    );
    assert_eq!(
        most_recent_judge_activity_at(
            &[comment(at, "<!-- loom:ac-verified sha=abc -->")],
            claimed_at
        ),
        Some(at)
    );
    assert_eq!(
        most_recent_judge_activity_at(
            &[comment(
                claimed_at - Duration::minutes(1),
                "<!-- loom:ac-verified -->"
            )],
            claimed_at
        ),
        None,
        "pre-claim comment"
    );
    assert_eq!(
        most_recent_judge_activity_at(
            &[comment(
                at,
                &format!("<!-- loom:ac-verified -->\n{STANDDOWN_MARKER_PREFIX}x seq=1 -->")
            )],
            claimed_at
        ),
        None
    );
}

#[test]
fn head_push_counts_only_force_pushes_by_the_claimant() {
    let claimed_at = fixture_claimed_at();
    let p = claimed_at + Duration::minutes(10);
    let events = vec![
        tl("labeled", claimed_at, "judge-bot"),
        tl("head_ref_force_pushed", p, "judge-bot"),
        tl("head_ref_force_pushed", p + Duration::minutes(5), "builder-bot"),
        tl("committed", p + Duration::minutes(9), "judge-bot"),
    ];
    assert_eq!(most_recent_head_push_at(&events, claimed_at), Some(p));
    // A push before the claim is a previous generation.
    let old = vec![
        tl("labeled", claimed_at, "judge-bot"),
        tl("head_ref_force_pushed", claimed_at - Duration::minutes(3), "judge-bot"),
    ];
    assert_eq!(most_recent_head_push_at(&old, claimed_at), None);
    // Unknown claimant fails open (no signal).
    let no_label = vec![tl("head_ref_force_pushed", p, "judge-bot")];
    assert_eq!(most_recent_head_push_at(&no_label, claimed_at), None);
}

/// A fake `gh` that appends its argv to `log` and prints nothing.
fn recording_gh(dir: &std::path::Path, log: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let gh = dir.join("fake-gh-record.sh");
    std::fs::write(&gh, format!("#!/usr/bin/env bash\necho \"$*\" >>\"{}\"\n", log.display()))
        .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    gh
}

/// #10235: an explicit `OWNER/REPO` names the repository for both extra
/// liveness reads (Judge comments, head-push timeline) instead of `gh`'s
/// checkout/ambient `{owner}/{repo}` — the checkout's evidence about a
/// same-numbered item must never be borrowed for another repo's claim.
#[test]
#[serial_test::serial]
fn extra_liveness_reads_name_the_explicit_repo() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv.log");
    let gh = recording_gh(dir.path(), &log);
    let since = fixture_claimed_at();
    let paths = |repo: Option<&str>| {
        std::fs::write(&log, "").unwrap();
        forge::fetch_most_recent_judge_activity_at(&gh, dir.path(), 7, since, repo);
        forge::fetch_most_recent_head_push_at(&gh, dir.path(), 7, "loom:reviewing", since, repo);
        std::fs::read_to_string(&log).unwrap()
    };
    let explicit = paths(Some("target/other"));
    assert!(explicit.contains("repos/target/other/issues/7/comments"), "{explicit}");
    assert!(explicit.contains("repos/target/other/issues/7/timeline"), "{explicit}");
    assert!(!explicit.contains("{owner}"), "{explicit}");
    let ambient = paths(None);
    assert!(ambient.contains("repos/{owner}/{repo}/issues/7/comments"), "{ambient}");
    assert!(ambient.contains("repos/{owner}/{repo}/issues/7/timeline"), "{ambient}");
}
