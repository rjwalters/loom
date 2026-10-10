//! Tests for the "did THIS merge close the issue?" decision (#8942).
//!
//! The `fixtures/*.json` files are verbatim `gh api graphql` responses to
//! [`query`], captured from rjwalters/loom on 2026-10-10 — one per shape the
//! forge was actually observed to produce, including the two that defeat the
//! obvious rules (a closer naming this PR on an issue closed long before it,
//! and a commit closer that is not the merge commit). Nothing in them is
//! hand-written; the synthetic inputs further down are labelled as such.

use super::*;

fn verdict(issue: u64, pr: u64, input: &str) -> Verdict {
    decide(issue, pr, &Facts::from_json(input))
}

// --- measured GitHub payloads ----------------------------------------------

#[test]
fn a_close_attributed_to_this_pr_is_this_merge() {
    // #9208, closed 2 s after PR #11362 merged; `closer` is that PullRequest.
    let v = verdict(9208, 11362, include_str!("fixtures/pr-closer.json"));
    assert_eq!(v, Verdict::ThisMerge(Evidence::CloserIsPr));
    assert_eq!(v.exit_code(), 0);
}

#[test]
fn a_close_attributed_to_one_of_the_prs_own_commits_is_this_merge() {
    // #10869 / PR #11005: a merge-commit merge. The closer is the branch
    // commit carrying the keyword, NOT `mergeCommit.oid`.
    let input = include_str!("fixtures/commit-closer.json");
    let facts = Facts::from_json(input);
    let Closer::Commit(oid) = &facts.closer else {
        panic!("expected a commit closer, got {:?}", facts.closer);
    };
    assert_ne!(Some(oid), facts.pr_commits.first(), "not the merge commit");
    assert_eq!(decide(10869, 11005, &facts), Verdict::ThisMerge(Evidence::CloserIsPrCommit));
}

#[test]
fn an_issue_closed_by_hand_before_the_merge_is_not_this_merge() {
    // The reported bug, on real data: #10264 was closed directly 954 s before
    // PR #10265 merged, yet GitHub still listed it as that PR's close target.
    let v = verdict(10264, 10265, include_str!("fixtures/closed-by-hand-before-merge.json"));
    assert_eq!(v, Verdict::ClosedBeforeMerge { secs: 954 });
    assert_eq!(v.exit_code(), 1);
}

#[test]
fn an_issue_closed_earlier_by_another_pr_is_not_this_merge() {
    // #9950 was closed by PR #9951 hours before PR #9955 merged.
    let v = verdict(9950, 9955, include_str!("fixtures/closed-earlier-by-other-pr.json"));
    assert!(matches!(v, Verdict::ClosedBeforeMerge { .. }), "{v:?}");
}

#[test]
fn a_closer_naming_this_pr_does_not_override_an_earlier_close_time() {
    // #3251 was closed 548 s before PR #3277 merged, and GitHub nevertheless
    // appended a ClosedEvent whose closer is PR #3277 while leaving closedAt
    // alone. Trusting the closer here would reopen an already-closed issue.
    let input = include_str!("fixtures/already-closed-yet-closer-is-this-pr.json");
    assert_eq!(Facts::from_json(input).closer, Closer::PullRequest(3277));
    assert_eq!(verdict(3251, 3277, input), Verdict::ClosedBeforeMerge { secs: 548 });
}

#[test]
fn another_pr_merging_the_same_second_is_not_this_merge() {
    // #8963 closed in the very second PR #9236 merged — by PR #9351. The
    // timestamp alone would tie it to #9236; the closer says otherwise.
    let v = verdict(8963, 9236, include_str!("fixtures/other-pr-same-second.json"));
    assert_eq!(v, Verdict::OtherPullRequest(9351));
    assert_eq!(v.exit_code(), 1);
}

#[test]
fn a_direct_close_after_the_merge_is_not_this_merge() {
    // #11078: `closer: null` — closed by hand, hours after PR #11335 merged.
    let v = verdict(11078, 11335, include_str!("fixtures/direct-close-after-merge.json"));
    assert!(matches!(v, Verdict::OtherCloser(_)), "{v:?}");
    assert_eq!(v.token(), "NO");
}

// --- synthetic: the rows no captured payload reaches -------------------------

/// A [`query`]-shaped response assembled from parts (synthetic).
fn graphql(state: &str, closed_at: &str, merged_at: &str, closer: &str) -> String {
    format!(
        r#"{{"data":{{"repository":{{"pullRequest":{{"number":7,"mergedAt":"{merged_at}","mergeCommit":{{"oid":"aaa"}},"commits":{{"nodes":[{{"commit":{{"oid":"bbb"}}}}]}}}},"issue":{{"number":5,"state":"{state}","closedAt":{closed_at},"timelineItems":{{"nodes":{closer}}}}}}}}}}}"#
    )
}

const MERGED: &str = "2026-01-01T00:00:10Z";

#[test]
fn an_open_issue_has_nothing_to_attribute() {
    let v = verdict(5, 7, &graphql("OPEN", "null", MERGED, "[]"));
    assert_eq!(v, Verdict::NotClosed);
    assert_eq!(v.exit_code(), 1);
}

#[test]
fn a_direct_close_inside_the_window_is_still_not_this_merge() {
    // The forge answered the attribution question and named nobody: a person
    // closed it in the seconds after the merge. Not Step 4's to undo.
    let v = verdict(
        5,
        7,
        &graphql("CLOSED", r#""2026-01-01T00:00:11Z""#, MERGED, r#"[{"closer":null}]"#),
    );
    assert!(matches!(v, Verdict::OtherCloser(_)), "{v:?}");
}

#[test]
fn the_merge_commit_as_closer_is_this_merge() {
    let closer = r#"[{"closer":{"__typename":"Commit","oid":"aaa"}}]"#;
    assert_eq!(
        verdict(5, 7, &graphql("CLOSED", r#""2026-01-01T00:00:11Z""#, MERGED, closer)),
        Verdict::ThisMerge(Evidence::CloserIsPrCommit)
    );
}

#[test]
fn an_unlisted_commit_closer_falls_back_to_the_timestamps() {
    // A rebase merge rewrites every oid, so "not in the list" is no evidence
    // of a different closer — the window decides, in both directions.
    let closer = r#"[{"closer":{"__typename":"Commit","oid":"zzz"}}]"#;
    assert_eq!(
        verdict(5, 7, &graphql("CLOSED", r#""2026-01-01T00:00:12Z""#, MERGED, closer)),
        Verdict::ThisMerge(Evidence::TimestampTied)
    );
    assert_eq!(
        verdict(5, 7, &graphql("CLOSED", r#""2026-01-01T00:05:00Z""#, MERGED, closer)),
        Verdict::Unattributed { secs: 290 }
    );
}

#[test]
fn an_unmodelled_closer_type_is_not_this_merge() {
    let closer = r#"[{"closer":{"__typename":"ProjectV2"}}]"#;
    let v = verdict(5, 7, &graphql("CLOSED", r#""2026-01-01T00:00:11Z""#, MERGED, closer));
    assert_eq!(v, Verdict::OtherCloser("closed by a ProjectV2".into()));
}

// --- closer unknown: the explicit, tested answer (#8942 decision 1) ----------

/// REST-shaped facts — what Gitea, or GitHub with GraphQL unavailable, can
/// offer. The field names and the first pair of values are a `--jq`
/// projection of real `repos/rjwalters/loom/issues/9208` and `pulls/11362`
/// reads (2026-10-10); later tests vary only the close time.
fn rest(closed_at: &str) -> String {
    format!(
        "{{\"closed_at\":\"{closed_at}\",\"number\":9208,\"state\":\"closed\"}}\n{{\"merge_commit_sha\":\"0593af96f06e9cf720d203948bdb00549da1286a\",\"merged_at\":\"2026-10-10T21:32:35Z\",\"number\":11362}}\n"
    )
}

#[test]
fn closer_unknown_and_closed_within_the_window_is_this_merge() {
    let v = verdict(9208, 11362, &rest("2026-10-10T21:32:37Z"));
    assert_eq!(v, Verdict::ThisMerge(Evidence::TimestampTied));
    // The boundaries: the merge's own second, and the last second of the window.
    for at in ["2026-10-10T21:32:35Z", "2026-10-10T21:32:40Z"] {
        assert_eq!(
            verdict(9208, 11362, &rest(at)),
            Verdict::ThisMerge(Evidence::TimestampTied),
            "{at}"
        );
    }
}

#[test]
fn closer_unknown_and_closed_after_the_window_is_not_reopened() {
    let v = verdict(9208, 11362, &rest("2026-10-10T21:32:41Z"));
    assert_eq!(v, Verdict::Unattributed { secs: 6 });
    assert_eq!(v.exit_code(), 1);
    assert_eq!(v.token(), "UNATTRIBUTED");
}

#[test]
fn closer_unknown_and_closed_before_the_merge_is_not_this_merge() {
    assert_eq!(
        verdict(9208, 11362, &rest("2026-10-10T21:32:34Z")),
        Verdict::ClosedBeforeMerge { secs: 1 }
    );
}

#[test]
fn a_timezone_offset_does_not_fool_the_comparison() {
    // Gitea renders timestamps with the server's offset. 23:32:36+02:00 is
    // 21:32:36Z — one second after the merge, not two hours after it.
    assert_eq!(
        verdict(9208, 11362, &rest("2026-10-10T23:32:36+02:00")),
        Verdict::ThisMerge(Evidence::TimestampTied)
    );
}

#[test]
fn an_empty_timeline_is_closer_unknown_not_a_direct_close() {
    let input = graphql("CLOSED", r#""2026-01-01T00:00:11Z""#, MERGED, "[]");
    assert_eq!(Facts::from_json(&input).closer, Closer::Unavailable);
    assert_eq!(verdict(5, 7, &input), Verdict::ThisMerge(Evidence::TimestampTied));
}

// --- could not answer --------------------------------------------------------

#[test]
fn missing_empty_and_unparseable_input_are_all_unanswered() {
    for input in [
        "",
        "\n",
        "not json",
        "{}",
        "[]",
        r#"{"errors":[{"type":"RATE_LIMITED","message":"API rate limit exceeded"}]}"#,
        r#"{"data":{"repository":{"issue":null,"pullRequest":null}}}"#,
        r#"{"message":"Not Found","status":"404"}"#,
    ] {
        let v = verdict(5, 7, input);
        assert!(matches!(v, Verdict::Unanswered(_)), "{input:?} -> {v:?}");
        assert_eq!(v.exit_code(), 3, "{input:?}");
        assert_eq!(v.token(), "UNANSWERED");
    }
}

#[test]
fn a_closed_issue_with_no_merge_time_is_unanswered() {
    // An unmerged PR, or a PR read that failed: `mergedAt` is null.
    let v = verdict(
        5,
        7,
        &graphql("CLOSED", r#""2026-01-01T00:00:11Z""#, "", r#"[{"closer":null}]"#),
    );
    assert!(matches!(v, Verdict::Unanswered(_)), "{v:?}");
    let issue_only = r#"{"closed_at":"2026-01-01T00:00:11Z","number":5,"state":"closed"}"#;
    assert!(matches!(verdict(5, 7, issue_only), Verdict::Unanswered(_)));
}

#[test]
fn a_closed_issue_with_an_unreadable_close_time_is_unanswered() {
    for closed_at in ["null", r#""yesterday""#] {
        let v = verdict(5, 7, &graphql("CLOSED", closed_at, MERGED, "[]"));
        assert!(matches!(v, Verdict::Unanswered(_)), "{closed_at} -> {v:?}");
    }
}

#[test]
fn facts_about_a_different_issue_or_pr_are_unanswered() {
    // A real payload, asked about the wrong numbers: never answer from it.
    let input = include_str!("fixtures/pr-closer.json");
    assert!(matches!(verdict(9209, 11362, input), Verdict::Unanswered(_)));
    assert!(matches!(verdict(9208, 11363, input), Verdict::Unanswered(_)));
}

// --- input framing -----------------------------------------------------------

#[test]
fn a_failed_graphql_read_ahead_of_a_rest_fallback_is_skipped() {
    let input = format!(
        "{}\n{}",
        r#"{"errors":[{"type":"RATE_LIMITED"}]}"#,
        rest("2026-10-10T21:32:37Z")
    );
    assert_eq!(verdict(9208, 11362, &input), Verdict::ThisMerge(Evidence::TimestampTied));
}

#[test]
fn a_non_json_line_does_not_discard_the_documents_after_it() {
    let input = format!("<html>502 Bad Gateway</html>\n{}", rest("2026-10-10T21:32:37Z"));
    assert_eq!(verdict(9208, 11362, &input), Verdict::ThisMerge(Evidence::TimestampTied));
}

#[test]
fn the_first_document_to_state_a_fact_wins() {
    // A complete GraphQL read followed by REST documents that disagree: the
    // richer, earlier read is not overwritten.
    let input = format!(
        "{}{}",
        include_str!("fixtures/other-pr-same-second.json"),
        r#"{"closed_at":"2020-01-01T00:00:00Z","number":8963,"state":"open"}"#
    );
    assert_eq!(verdict(8963, 9236, &input), Verdict::OtherPullRequest(9351));
}

// --- output ------------------------------------------------------------------

#[test]
fn every_outcome_renders_one_line_naming_the_issue() {
    let outcomes = [
        Verdict::ThisMerge(Evidence::CloserIsPr),
        Verdict::ThisMerge(Evidence::CloserIsPrCommit),
        Verdict::ThisMerge(Evidence::TimestampTied),
        Verdict::NotClosed,
        Verdict::ClosedBeforeMerge { secs: 9 },
        Verdict::OtherPullRequest(3),
        Verdict::OtherCloser("closed directly, not through a PR".into()),
        Verdict::Unattributed { secs: 9 },
        Verdict::Unanswered("no issue state in the input".into()),
    ];
    for v in outcomes {
        let line = render(5, 7, &v);
        assert!(line.starts_with(&format!("LOOM-CLOSED-BY-MERGE {} issue #5: ", v.token())));
        assert_eq!(line.matches('\n').count(), 1, "{line:?}");
        assert!(line.ends_with('\n'));
    }
}

#[test]
fn the_exit_codes_never_collide_with_claps() {
    // 2 is what a binary predating this verb exits with; no verdict may use it.
    assert_eq!(Verdict::ThisMerge(Evidence::CloserIsPr).exit_code(), 0);
    assert_eq!(Verdict::NotClosed.exit_code(), 1);
    assert_eq!(Verdict::Unattributed { secs: 9 }.exit_code(), 1);
    assert_eq!(Verdict::Unanswered(String::new()).exit_code(), 3);
}

#[test]
fn the_query_asks_for_every_field_the_parser_reads() {
    let q = query(5, 7);
    for field in [
        "pullRequest(number:7){number mergedAt mergeCommit{oid} commits(last:100)",
        "issue(number:5){number state closedAt",
        "timelineItems(last:1,itemTypes:CLOSED_EVENT)",
        "closer{__typename",
        "on PullRequest{number}",
        "on Commit{oid}",
    ] {
        assert!(q.contains(field), "the query lost `{field}`: {q}");
    }
    // One shell-safe line: it travels through `$(...)` into `-f query=`.
    assert!(!q.contains('\n') && !q.contains('\'') && !q.contains('"'));
    assert_eq!(q.matches('{').count(), q.matches('}').count());
}
