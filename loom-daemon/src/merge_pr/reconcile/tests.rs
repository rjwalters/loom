//! Unit tests for the post-merge stacked-child reconcile decision (#3747 item
//! 1), the merge-pr port slice in [`super`].
//!
//! The behavioural end-to-end cases (a safe child reaches `reconcile-stack.sh`,
//! an unsafe one gets a comment instead, a mixed set splits, the #8010 item-2
//! snapshot preference) live in `defaults/scripts/tests/test-merge-pr-auto-reconcile.sh`
//! and are UNCHANGED by the port — they drive `merge-pr.sh`'s real functions,
//! which now call this module. What is here is the per-predicate detail that
//! suite could not reach through a `gh` stub, plus the fidelity properties the
//! module's own docs claim.

use super::*;

// ---------------------------------------------------------------------------
// issue_from_branch — the ONE definition of the predicate the retired shell
// wrote out twice (parent gate + child derivation).
// ---------------------------------------------------------------------------

#[test]
fn a_conventional_branch_yields_its_issue_number() {
    assert_eq!(issue_from_branch("feature/issue-8191").as_deref(), Some("8191"));
    assert_eq!(issue_from_branch("feature/issue-1").as_deref(), Some("1"));
}

#[test]
fn a_nonconventional_branch_yields_nothing() {
    for branch in [
        "release-1",
        "main",
        "feature/issue-",
        "feature/issue-abc",
        "feature/issue-12a",
        "feature/issue-12.3",
        "feature/issue-+12",
        "feature/issue--12",
        "feature/issue-12 ",
        " feature/issue-12",
        "xfeature/issue-12",
        "feature/issue-12/extra",
    ] {
        assert_eq!(issue_from_branch(branch), None, "branch {branch:?}");
    }
}

/// Bash's `=~` is POSIX ERE with no `REG_NEWLINE`, so `$` anchors at end of
/// STRING: `feature/issue-12\n` did not match, and must not match here either.
/// This is the property a Rust `^…$` would satisfy today and a `(?m)` flag
/// added later would silently break, which is why the port hand-parses.
#[test]
fn a_trailing_newline_does_not_match_the_anchor() {
    assert_eq!(issue_from_branch("feature/issue-12\n"), None);
    assert_eq!(issue_from_branch("feature/issue-12\r\n"), None);
    assert_eq!(issue_from_branch("\nfeature/issue-12"), None);
}

/// The retired shell captured `[0-9]+` as TEXT and interpolated it straight
/// into a URL. A digit run far past `u64::MAX` therefore round-tripped intact;
/// parsing to an integer would turn it into a different request (or an error the
/// shell never produced). Leading zeros must survive for the same reason.
#[test]
fn the_issue_number_is_text_so_it_cannot_overflow_or_be_renormalised() {
    let huge = "9".repeat(40);
    assert_eq!(
        issue_from_branch(&format!("feature/issue-{huge}")).as_deref(),
        Some(huge.as_str())
    );
    assert_eq!(issue_from_branch("feature/issue-007").as_deref(), Some("007"));
}

/// `[0-9]` is an ASCII range. Unicode decimal digits that `char::is_numeric`
/// would accept are not matched by the retired ERE and must not be here.
#[test]
fn non_ascii_digits_are_not_digits() {
    assert_eq!(issue_from_branch("feature/issue-١٢٣"), None); // ARABIC-INDIC
    assert_eq!(issue_from_branch("feature/issue-１２"), None); // FULLWIDTH
}

// ---------------------------------------------------------------------------
// plan — the parent gate and the children-rollup parse.
// ---------------------------------------------------------------------------

#[test]
fn a_nonconventional_parent_branch_is_not_stacked() {
    assert_eq!(plan("release-1", b"[]"), Plan::NotStacked);
    // …and the gate runs BEFORE the parse, so an unreadable rollup on a
    // non-stacked parent is still NotStacked (the retired shell returned at the
    // regex, never reaching `jq`).
    assert_eq!(plan("release-1", b"not json at all"), Plan::NotStacked);
}

#[test]
fn an_empty_rollup_is_a_legitimate_no_op() {
    assert_eq!(plan("feature/issue-100", b"[]"), Plan::Rows(vec![]));
}

#[test]
fn a_child_row_carries_the_pr_the_branch_and_the_derived_issue() {
    let got = plan("feature/issue-100", br#"[{"number":501,"headRefName":"feature/issue-201"}]"#);
    assert_eq!(
        got,
        Plan::Rows(vec![Row::Child {
            pr: "501".to_string(),
            branch: "feature/issue-201".to_string(),
            issue: Some("201".to_string()),
        }])
    );
}

/// A child on an ad-hoc branch has no `loom:building` claim to race — the
/// retired shell left `child_issue` empty and skipped the forge read entirely.
#[test]
fn a_child_on_a_nonconventional_branch_has_no_issue() {
    let got = plan("feature/issue-100", br#"[{"number":502,"headRefName":"hotfix/xyz"}]"#);
    assert_eq!(
        got,
        Plan::Rows(vec![Row::Child {
            pr: "502".to_string(),
            branch: "hotfix/xyz".to_string(),
            issue: None,
        }])
    );
}

#[test]
fn rows_keep_the_rollups_order() {
    let got = plan(
        "feature/issue-100",
        br#"[{"number":9,"headRefName":"b"},{"number":3,"headRefName":"a"}]"#,
    );
    let Plan::Rows(rows) = got else {
        panic!("expected rows")
    };
    let prs: Vec<&str> = rows
        .iter()
        .map(|r| match r {
            Row::Child { pr, .. } => pr.as_str(),
            Row::Malformed { .. } => "malformed",
        })
        .collect();
    assert_eq!(prs, vec!["9", "3"], "order must be the rollup's, not sorted");
}

#[test]
fn a_non_array_rollup_is_unreadable_rather_than_empty() {
    // The retired `jq 'length'` on an object returned its KEY COUNT, and
    // `jq -r '.[]'` iterated its VALUES — so a stray object could produce rows
    // nobody meant to act on. Refused by name instead.
    assert!(matches!(plan("feature/issue-100", br#"{"number":1}"#), Plan::Unreadable(_)));
    assert!(matches!(plan("feature/issue-100", b"null"), Plan::Unreadable(_)));
    assert!(matches!(plan("feature/issue-100", b"7"), Plan::Unreadable(_)));
    assert!(matches!(plan("feature/issue-100", b""), Plan::Unreadable(_)));
    assert!(matches!(
        plan("feature/issue-100", b"gh: command not found"),
        Plan::Unreadable(_)
    ));
}

/// One bad element must not discard the good ones. The whole pass is
/// best-effort cleanup: refusing three reconciles because a fourth row was
/// malformed would lose cleanup coverage the retired shell had.
#[test]
fn a_malformed_element_is_named_and_the_remaining_rows_survive() {
    let got = plan(
        "feature/issue-100",
        br#"[{"number":501,"headRefName":"feature/issue-201"},
             {"number":null,"headRefName":"feature/issue-202"},
             {"number":503,"headRefName":null},
             "not-an-object",
             {"number":504,"headRefName":"feature/issue-204"}]"#,
    );
    let Plan::Rows(rows) = got else {
        panic!("expected rows")
    };
    assert_eq!(rows.len(), 5);
    assert!(matches!(&rows[0], Row::Child { pr, .. } if pr == "501"));
    assert!(matches!(&rows[1], Row::Malformed { index: 1, .. }));
    assert!(matches!(&rows[2], Row::Malformed { index: 2, .. }));
    assert!(matches!(&rows[3], Row::Malformed { index: 3, .. }));
    assert!(matches!(&rows[4], Row::Child { pr, .. } if pr == "504"));
}

/// `jq`'s `"\(.number)"` rendered a digit STRING and an integer identically, so
/// both are accepted; a negative or fractional number is not a PR number.
#[test]
fn a_pr_number_may_arrive_as_a_digit_string_but_not_as_a_negative_or_float() {
    let one = |body: &str| {
        let Plan::Rows(rows) = plan("feature/issue-100", body.as_bytes()) else {
            panic!("expected rows")
        };
        rows.into_iter().next().unwrap()
    };
    assert!(
        matches!(one(r#"[{"number":"501","headRefName":"x"}]"#), Row::Child { ref pr, .. } if pr == "501")
    );
    assert!(matches!(one(r#"[{"number":-1,"headRefName":"x"}]"#), Row::Malformed { .. }));
    assert!(matches!(one(r#"[{"number":1.5,"headRefName":"x"}]"#), Row::Malformed { .. }));
    assert!(matches!(one(r#"[{"headRefName":"x"}]"#), Row::Malformed { .. }));
}

// ---------------------------------------------------------------------------
// child_route — the safe/unsafe gate.
// ---------------------------------------------------------------------------

#[test]
fn a_building_child_issue_defers() {
    let d = child_route(Some("202"), b"loom:building\n");
    assert_eq!(d.route, Route::Defer);
    assert_eq!(d.route.token(), "defer");
}

#[test]
fn a_building_label_anywhere_in_the_list_defers() {
    assert_eq!(
        child_route(Some("202"), b"loom:curated\nloom:building\nbug\n").route,
        Route::Defer
    );
    assert_eq!(
        child_route(Some("202"), b"loom:curated\nbug\nloom:building").route,
        Route::Defer,
        "a final line with no trailing newline still counts, as grep -x does"
    );
}

#[test]
fn a_child_issue_without_the_claim_reconciles() {
    let d = child_route(Some("201"), b"loom:issue\nloom:curated\n");
    assert_eq!(d.route, Route::Reconcile);
    assert_eq!(d.route.token(), "reconcile");
    assert!(d.why.contains("do not include"), "why was {:?}", d.why);
}

/// `grep -qx` is a WHOLE-LINE literal match. `grep -q` would have matched a
/// label that merely CONTAINS the name, which is the classic substring trap:
/// `loom:building-paused` is a different label and must reconcile.
#[test]
fn the_claim_match_is_whole_line_not_substring() {
    for label in [
        "loom:building-paused",
        "xloom:building",
        "loom:building ",
        " loom:building",
        "LOOM:BUILDING",
        "loom:build",
    ] {
        assert_eq!(
            child_route(Some("202"), format!("{label}\n").as_bytes()).route,
            Route::Reconcile,
            "label {label:?} must NOT read as the claim",
        );
    }
}

/// A child branch that follows no recognized Builder convention short-circuits
/// to safe with no label read at all — so whatever bytes are passed are
/// irrelevant.
#[test]
fn no_issue_means_no_claim_to_race() {
    let d = child_route(None, b"loom:building\n");
    assert_eq!(
        d.route,
        Route::Reconcile,
        "the retired shell never read labels for a non-conventional child branch"
    );
    assert!(d.why.contains("not a recognized Builder branch"), "why was {:?}", d.why);
}

/// The retired shell's three `|| true` / `|| echo '{}'` layers turned a failed
/// lookup into the empty string, which `grep -qx` then reported as "not
/// building". The port keeps that ROUTE (force-with-lease is the remaining
/// protection) but distinguishes the two situations in `why`.
#[test]
fn an_unreadable_label_set_reconciles_but_says_so() {
    for bytes in [b"".as_slice(), b"\n", b"  \n\t\n"] {
        let d = child_route(Some("202"), bytes);
        assert_eq!(d.route, Route::Reconcile);
        assert!(d.why.contains("no labels were supplied"), "why was {:?} for {bytes:?}", d.why);
    }
}

/// A trailing `\r` is DATA, not line-ending noise to be trimmed.
///
/// `grep -x` compares whole lines, and the `grep` a script resolves from PATH
/// (GNU on Linux, BSD on macOS) does not strip CR — so `loom:building\r` is not
/// the claim and the retired shell reconciled. An earlier draft of this port
/// trimmed the CR "so a CRLF list still matches", which reads as harmless
/// tidying but flips this input from `reconcile` to `defer`. Pinned here
/// because the trim is exactly the kind of edit a future reader would make
/// again; `ugrep` really does behave the other way, so the choice is not
/// self-evident from the name `grep` alone.
#[test]
fn a_trailing_cr_is_data_so_it_is_not_the_claim() {
    for labels in [
        b"loom:building\r\n".as_slice(),
        b"loom:building\r",
        b"loom:issue\r\nloom:building\r\nbug\r\n",
        b"\rloom:building\n",
    ] {
        assert_eq!(
            child_route(Some("202"), labels).route,
            Route::Reconcile,
            "labels {labels:?}: a CR is a byte, and grep -x compares whole lines",
        );
    }
    // …and the plain claim on either side of a CR-bearing line still defers, so
    // this is a byte comparison rather than a blanket "any CR means no claim".
    assert_eq!(child_route(Some("202"), b"loom:curated\r\nloom:building\n").route, Route::Defer);
}

/// `grep` is byte-oriented and never required valid UTF-8, so a label list that
/// is not must still be searched rather than aborting the read.
#[test]
fn a_non_utf8_label_list_is_still_searched() {
    let mut bytes = b"loom:building\n".to_vec();
    bytes.extend_from_slice(&[0xff, 0xfe, b'\n']);
    assert_eq!(child_route(Some("202"), &bytes).route, Route::Defer);

    let only_garbage = [0xff, 0xfe, b'\n'];
    assert_eq!(child_route(Some("202"), &only_garbage).route, Route::Reconcile);
}

// ---------------------------------------------------------------------------
// defer_comment — byte-frozen operator-visible text.
// ---------------------------------------------------------------------------

#[test]
fn the_defer_comment_names_the_child_the_parent_and_the_manual_command() {
    let body = defer_comment("202", "502", "feature/issue-100", "2026-09-27T18:00:00Z");
    assert!(body.starts_with("## Stacked parent merged — reconciliation deferred\n\n"));
    assert!(body.contains("Parent branch `feature/issue-100` squash-merged"));
    assert!(body.contains("this child's issue #202 is still `loom:building`"));
    assert!(body.contains("\n```\n./.loom/scripts/reconcile-stack.sh 502 feature/issue-100\n```\n"));
    assert!(body.ends_with("*Deferred by merge-pr.sh (#3747) at 2026-09-27T18:00:00Z*"));
    assert!(
        !body.ends_with('\n'),
        "the retired shell's comment=\"…\" had no trailing newline"
    );
}
