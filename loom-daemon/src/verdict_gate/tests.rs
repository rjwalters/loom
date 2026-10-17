use super::*;
use chrono::TimeZone;
use serde_json::json;

const HEAD: &str = "846ed44c14e7dd4e87bf17efb554bca0d57c05b2";
const OTHER: &str = "ed058db8c0000000000000000000000000000000";

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 6, 13, 46, 32).unwrap()
}

fn comment(sha: &str, verdict: &str, at: &str) -> Value {
    json!({
        "body": format!("review text\n\n<!-- loom:verdict-sha sha={sha} verdict={verdict} -->"),
        "created_at": at,
    })
}

fn labels(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| (*s).to_string()).collect()
}

fn input<'a>(
    verdict: VerdictKind,
    comments: Option<&'a [Value]>,
    labels: Option<&'a [String]>,
    overrule: &'a str,
) -> GateInput<'a> {
    GateInput {
        verdict,
        sha: HEAD,
        comments,
        labels,
        overrule,
        now: now(),
        window_secs: DEFAULT_WINDOW_SECS,
        review_started: None,
        issues: None,
    }
}

const RATIONALE: &str =
    "file-size budget was restored in 1a2b3c and the timeout concern is moot: no change here";

#[test]
fn same_head_contradiction_is_refused() {
    // The #10578 shape: changes-requested at 13:40, approve at 13:46, same head.
    let c = [comment(HEAD, "changes-requested", "2026-10-06T13:40:31Z")];
    let l = labels(&["loom:changes-requested"]);
    let d = decide(&input(VerdictKind::Approved, Some(&c), Some(&l), ""));
    assert!(matches!(&d, Decision::Refuse(m) if m.contains("--overrules-prior")), "{d:?}");
    assert_eq!(d.render().1, EXIT_REFUSE);
}

#[test]
fn same_head_contradiction_with_overrule_is_allowed() {
    let c = [comment(HEAD, "changes-requested", "2026-10-06T10:00:00Z")];
    let l = labels(&["loom:changes-requested"]);
    let short = decide(&input(VerdictKind::Approved, Some(&c), Some(&l), "fixed"));
    assert!(matches!(short, Decision::Refuse(_)), "a token is not a rationale");
    let d = decide(&input(VerdictKind::Approved, Some(&c), Some(&l), RATIONALE));
    assert!(matches!(&d, Decision::Proceed(m) if m.contains("overruling")), "{d:?}");
}

#[test]
fn moved_head_approves_normally() {
    let c = [comment(OTHER, "changes-requested", "2026-10-06T13:40:31Z")];
    let l = labels(&["loom:review-requested"]);
    let d = decide(&input(VerdictKind::Approved, Some(&c), Some(&l), ""));
    assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
}

#[test]
fn ci_failure_label_blocks_approve_even_with_overrule() {
    let l = labels(&["loom:review-requested", "loom:ci-failure"]);
    let d = decide(&input(VerdictKind::Approved, Some(&[]), Some(&l), RATIONALE));
    assert!(matches!(&d, Decision::Refuse(m) if m.contains("loom:ci-failure")), "{d:?}");
    // A changes-requested verdict is not blocked by it.
    let d = decide(&input(VerdictKind::ChangesRequested, Some(&[]), Some(&l), ""));
    assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
}

#[test]
fn unread_state_never_passes_an_approval() {
    let l = labels(&[]);
    assert!(matches!(
        decide(&input(VerdictKind::Approved, None, Some(&l), "")),
        Decision::Refuse(_)
    ));
    assert!(matches!(
        decide(&input(VerdictKind::Approved, Some(&[]), None, "")),
        Decision::Refuse(_)
    ));
    // changes-requested is the safe direction: it still posts.
    assert!(matches!(
        decide(&input(VerdictKind::ChangesRequested, None, None, "")),
        Decision::Proceed(_)
    ));
}

#[test]
fn concurrent_same_verdict_is_deduped_inside_the_window_only() {
    let l = labels(&["loom:reviewing"]);
    for verdict in [VerdictKind::Approved, VerdictKind::ChangesRequested] {
        let token = verdict.marker_token();
        let fresh = [comment(HEAD, token, "2026-10-06T13:44:00Z")];
        let d = decide(&input(verdict, Some(&fresh), Some(&l), ""));
        assert!(matches!(d, Decision::Dedupe(_)), "{token}: {d:?}");
        assert_eq!(d.render().1, EXIT_DEDUPE);
        let old = [comment(HEAD, token, "2026-10-06T12:00:00Z")];
        let d = decide(&input(verdict, Some(&old), Some(&l), ""));
        assert!(matches!(d, Decision::Proceed(_)), "{token}: {d:?}");
    }
}

#[test]
fn changes_requested_after_same_head_approval_is_allowed() {
    // The safe direction: a later Judge may always block.
    let c = [comment(HEAD, "approved", "2026-10-06T13:45:00Z")];
    let l = labels(&["loom:pr"]);
    let d = decide(&input(VerdictKind::ChangesRequested, Some(&c), Some(&l), ""));
    assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
}

#[test]
fn newest_marker_at_this_head_wins_and_abbreviations_match() {
    let c = [
        comment(HEAD, "changes-requested", "2026-10-06T09:00:00Z"),
        comment(&HEAD[..9], "approved", "2026-10-06T10:00:00Z"),
        comment(OTHER, "changes-requested", "2026-10-06T11:00:00Z"),
    ];
    let m = latest_marker_for(&c, HEAD).expect("marker");
    assert_eq!(m.verdict, VerdictKind::Approved);
    assert_eq!(m.sha, &HEAD[..9]);
    assert!(latest_marker_for(&c, "deadbeef").is_none());
    assert!(!same_sha("846ed4", HEAD), "under 7 chars never matches");
}

#[test]
fn label_transition_is_exclusive() {
    let (add, remove) = transition(VerdictKind::Approved);
    assert_eq!(add, ["loom:pr"]);
    for l in [
        "loom:changes-requested",
        "loom:ci-failure",
        "loom:reviewing",
        "loom:review-requested",
    ] {
        assert!(remove.contains(&l), "approve must remove {l}");
    }
    let (add, remove) = transition(VerdictKind::ChangesRequested);
    assert_eq!(add, ["loom:changes-requested"]);
    assert!(remove.contains(&"loom:pr"));
    assert!(!remove.contains(&"loom:ci-failure"), "a CR keeps its CI companion");

    let after = labels(&["loom:pr", "loom:changes-requested", "loom:ci-failure"]);
    assert_eq!(
        label_problems(VerdictKind::Approved, &after),
        vec![
            "still carries loom:changes-requested",
            "still carries loom:ci-failure"
        ]
    );
    assert_eq!(
        label_problems(VerdictKind::Approved, &labels(&["loom:important"])),
        vec!["missing loom:pr"]
    );
    assert!(label_problems(VerdictKind::Approved, &labels(&["loom:pr"])).is_empty());
}

#[test]
fn repair_command_names_every_label() {
    let cmd = repair_command(10605, "o/r", VerdictKind::Approved);
    assert!(cmd.starts_with("gh pr edit 10605 --repo o/r --add-label \"loom:pr\""));
    assert!(cmd.contains("--remove-label \"loom:changes-requested\""));
}

#[test]
fn label_names_reads_rest_shapes() {
    let list = json!([{"name": "loom:pr"}, {"name": "x"}]);
    assert_eq!(label_names(&list).unwrap(), labels(&["loom:pr", "x"]));
    let issue = json!({"labels": [{"name": "loom:pr"}]});
    assert_eq!(label_names(&issue).unwrap(), labels(&["loom:pr"]));
    assert!(label_names(&json!({"message": "Not Found"})).is_none());
}

// --- cross-host arbitration (#10581) ----------------------------------------

#[test]
fn count_markers_counts_only_this_head_and_verdict() {
    let cs = vec![
        comment(HEAD, "changes-requested", "2026-10-06T13:40:00Z"),
        comment(HEAD, "changes-requested", "2026-10-06T13:41:00Z"),
        comment(HEAD, "approved", "2026-10-06T13:42:00Z"),
        comment(OTHER, "changes-requested", "2026-10-06T13:43:00Z"),
    ];
    assert_eq!(count_markers(&cs, HEAD, VerdictKind::ChangesRequested), 2);
    assert_eq!(count_markers(&cs, HEAD, VerdictKind::Approved), 1);
    assert_eq!(count_markers(&cs, &HEAD[..10], VerdictKind::Approved), 1);
}

/// Two callers both read an empty forge (gate saw 0 opposite markers) and both
/// wrote. Whichever of them re-reads, changes-requested wins.
#[test]
fn two_callers_that_both_passed_the_gate_converge_on_changes_requested() {
    let both = vec![
        comment(HEAD, "approved", "2026-10-06T13:46:00Z"),
        comment(HEAD, "changes-requested", "2026-10-06T13:46:01Z"),
    ];
    let approver = reconcile(VerdictKind::Approved, HEAD, 0, 0, "", Some(&both));
    assert!(matches!(approver, Reconciled::Superseded(_)), "{approver:?}");
    assert_eq!(approver.render().1, EXIT_SUPERSEDED);
    let rejecter = reconcile(VerdictKind::ChangesRequested, HEAD, 0, 0, "", Some(&both));
    assert!(matches!(rejecter, Reconciled::Prevails(_)), "{rejecter:?}");
    assert_eq!(rejecter.render().1, 0);
}

/// The store-then-load argument: the earlier writer may not see the later one,
/// but the later one always sees the earlier one, and CR still wins.
#[test]
fn the_earlier_writer_may_miss_its_rival_and_the_later_one_still_arbitrates() {
    let only_approval = vec![comment(HEAD, "approved", "2026-10-06T13:46:00Z")];
    assert_eq!(
        reconcile(VerdictKind::Approved, HEAD, 0, 0, "", Some(&only_approval)),
        Reconciled::Stable
    );
    assert!(matches!(
        reconcile(VerdictKind::ChangesRequested, HEAD, 0, 0, "", Some(&only_approval)),
        Reconciled::Prevails(_)
    ));
}

#[test]
fn an_opposite_marker_the_gate_already_saw_is_not_a_race() {
    // An approval that overruled an earlier changes-requested (gate saw 1).
    let cs = vec![
        comment(HEAD, "changes-requested", "2026-10-06T13:00:00Z"),
        comment(HEAD, "approved", "2026-10-06T13:46:00Z"),
    ];
    assert_eq!(reconcile(VerdictKind::Approved, HEAD, 1, 0, "", Some(&cs)), Reconciled::Stable);
    // Another head's verdicts are irrelevant.
    let other = vec![comment(OTHER, "changes-requested", "2026-10-06T13:46:00Z")];
    assert_eq!(
        reconcile(VerdictKind::Approved, HEAD, 0, 0, "", Some(&other)),
        Reconciled::Stable
    );
}

#[test]
fn an_unread_reconcile_is_never_stable() {
    let r = reconcile(VerdictKind::Approved, HEAD, 0, 0, "", None);
    assert_eq!(r, Reconciled::Unread);
    assert_ne!(r.render().1, 0);
}

fn with_id(mut c: Value, id: u64, nonce: &str) -> Value {
    c["id"] = json!(id);
    let body = format!("{}\n<!-- loom:verdict-nonce={nonce} -->", c["body"].as_str().unwrap());
    c["body"] = json!(body);
    c
}

/// Two hosts both gate against an empty comment list, both post the same
/// verdict, and both re-read: exactly one of them (the lower comment id) stands.
#[test]
fn identical_verdicts_that_both_passed_the_gate_arbitrate_by_lowest_id() {
    for verdict in ["approved", "changes-requested"] {
        let kind = parse_verdict(verdict).unwrap();
        let both = vec![
            with_id(comment(HEAD, verdict, "2026-10-06T13:46:00Z"), 100, "host-a"),
            with_id(comment(HEAD, verdict, "2026-10-06T13:46:01Z"), 101, "host-b"),
        ];
        assert_eq!(reconcile(kind, HEAD, 0, 0, "host-a", Some(&both)), Reconciled::Stable);
        let loser = reconcile(kind, HEAD, 0, 0, "host-b", Some(&both));
        assert_eq!(loser, Reconciled::Duplicate(101));
        assert_eq!(loser.render().1, EXIT_DUPLICATE);
        // The earlier host re-reading late (after the loser withdrew) is still stable.
        assert_eq!(reconcile(kind, HEAD, 0, 0, "host-a", Some(&both[..1])), Reconciled::Stable);
    }
}

/// An identical marker the gate already saw is a deliberate re-review, not a race.
#[test]
fn an_identical_marker_the_gate_already_saw_is_not_a_duplicate() {
    let cs = vec![
        with_id(comment(HEAD, "approved", "2026-10-06T12:00:00Z"), 90, "old"),
        with_id(comment(HEAD, "approved", "2026-10-06T13:46:00Z"), 100, "mine"),
    ];
    assert_eq!(
        reconcile(VerdictKind::Approved, HEAD, 0, 90, "mine", Some(&cs)),
        Reconciled::Stable
    );
}

#[test]
fn a_posted_verdict_missing_from_the_reread_is_unread_and_a_deduped_caller_is_stable() {
    let cs = vec![with_id(
        comment(HEAD, "approved", "2026-10-06T13:46:00Z"),
        100,
        "other",
    )];
    assert_eq!(
        reconcile(VerdictKind::Approved, HEAD, 0, 0, "mine", Some(&cs)),
        Reconciled::Unread
    );
    assert_eq!(reconcile(VerdictKind::Approved, HEAD, 0, 0, "", Some(&cs)), Reconciled::Stable);
}

// --- #12614: approvals against a stale read of the spec ---------------------

const STARTED: &str = "2026-10-06T13:00:00Z";

fn started() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(STARTED)
        .unwrap()
        .with_timezone(&Utc)
}

fn human(id: u64, at: &str) -> Value {
    json!({"id": id, "body": "rescoped, review against the new body", "created_at": at,
           "html_url": format!("https://example/c/{id}"),
           "user": {"login": "operator", "type": "User"}, "author_association": "OWNER"})
}

fn issue(number: u64, reviewed: &str, now_body: &str, comments: Vec<Value>) -> IssueSnapshot {
    IssueSnapshot {
        number,
        expected_body_sha: Some(body_sha(reviewed)),
        body: Some(now_body.to_string()),
        comments: Some(comments),
    }
}

fn gate(
    verdict: VerdictKind,
    pr_comments: &[Value],
    started_at: Option<DateTime<Utc>>,
    issues: Option<&[IssueSnapshot]>,
) -> Decision {
    let l = labels(&[]);
    let mut i = input(verdict, Some(pr_comments), Some(&l), "");
    i.review_started = started_at;
    i.issues = issues;
    decide(&i)
}

#[test]
fn human_pr_comment_after_review_start_refuses_and_names_it() {
    let c = [human(77, "2026-10-06T13:05:00Z")];
    let d = gate(VerdictKind::Approved, &c, Some(started()), Some(&[]));
    assert!(
        matches!(&d, Decision::Refuse(m) if m.contains("comment 77") && m.contains("operator")
            && m.contains("2026-10-06T13:05:00")),
        "{d:?}"
    );
    assert_eq!(d.render().1, EXIT_REFUSE);
}

#[test]
fn human_pr_comment_before_review_start_proceeds() {
    let c = [human(77, "2026-10-06T12:59:59Z")];
    let d = gate(VerdictKind::Approved, &c, Some(started()), Some(&[]));
    assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
}

#[test]
fn bot_and_loom_marker_comments_after_start_do_not_refuse() {
    let bot = json!({"id": 1, "body": "Judge note", "created_at": "2026-10-06T13:05:00Z",
        "user": {"login": "loom-fleet-dispatch[bot]", "type": "Bot"},
        "author_association": "NONE"});
    // A marker comment posted through a human token is still fleet traffic.
    let lease = json!({"id": 2, "body": "<!-- loom:lease host=h sweep=s -->\nlease",
        "created_at": "2026-10-06T13:06:00Z",
        "user": {"login": "operator", "type": "User"}, "author_association": "OWNER"});
    let c = [bot, lease];
    let d = gate(VerdictKind::Approved, &c, Some(started()), Some(&[]));
    assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
}

#[test]
fn linked_issue_body_change_refuses_and_names_the_issue() {
    let iss = [issue(12509, "old design", "new daemon-only design", vec![])];
    let d = gate(VerdictKind::Approved, &[], Some(started()), Some(&iss));
    assert!(
        matches!(&d, Decision::Refuse(m) if m.contains("issue #12509's body changed")),
        "{d:?}"
    );
}

#[test]
fn unchanged_issue_body_proceeds() {
    let iss = [issue(12509, "same\n", "same", vec![])];
    let d = gate(VerdictKind::Approved, &[], Some(started()), Some(&iss));
    assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
}

#[test]
fn human_issue_comment_after_start_refuses() {
    let iss = [issue(9, "b", "b", vec![human(5, "2026-10-06T13:30:00Z")])];
    let d = gate(VerdictKind::Approved, &[], Some(started()), Some(&iss));
    assert!(
        matches!(&d, Decision::Refuse(m) if m.contains("issue #9") && m.contains("comment 5")),
        "{d:?}"
    );
}

#[test]
fn unread_linked_issue_fails_closed_on_approval() {
    let unread = [IssueSnapshot {
        number: 3,
        expected_body_sha: None,
        body: None,
        comments: None,
    }];
    let d = gate(VerdictKind::Approved, &[], Some(started()), Some(&unread));
    assert!(matches!(&d, Decision::Refuse(m) if m.contains("issue #3")), "{d:?}");
    let d = gate(VerdictKind::Approved, &[], Some(started()), None);
    assert!(matches!(d, Decision::Refuse(_)), "undetermined issues never pass");
}

#[test]
fn changes_requested_is_never_refused_by_the_stale_read_check() {
    let c = [human(77, "2026-10-06T13:05:00Z")];
    let iss = [issue(1, "old", "new", vec![])];
    let d = gate(VerdictKind::ChangesRequested, &c, Some(started()), Some(&iss));
    assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
}

#[test]
fn without_review_started_behaviour_is_unchanged() {
    let c = [human(77, "2026-10-06T13:05:00Z")];
    let d = gate(VerdictKind::Approved, &c, None, None);
    assert!(matches!(d, Decision::Proceed(_)), "{d:?}");
}

#[test]
fn closing_issues_parses_keywords() {
    assert_eq!(closing_issues("Closes #12\nfixes: #3, Resolved #12, part of #99"), vec![3, 12]);
    assert!(closing_issues("Part of #5").is_empty());
}
