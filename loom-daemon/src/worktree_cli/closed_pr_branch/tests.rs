//! Unit coverage for the closed-unmerged arm (#9083).
//!
//! [`super::decide`] is pure, so the whole decision table is pinned here
//! without a git repo or a forge. The end-to-end behaviour through the real
//! `worktree.sh` — the refusal message's text, the JSON document, and the
//! resume path — is pinned by `defaults/scripts/tests/test-worktree-stale-closed-branch.sh`.

use super::*;

fn pr(number: &str, state: &str, head: &str, merged: bool) -> Pr {
    Pr {
        number: number.to_string(),
        state: state.to_string(),
        head_sha: head.to_string(),
        url: format!("https://github.com/o/r/pull/{number}"),
        merged,
    }
}

const TIP: &str = "3a4cfd2aa28c31e85dc81761e40730ff4a198a85";
const OTHER: &str = "c356f277c8107c11f2a32a56d73829fcaae02cce";

// ---------------------------------------------------------------------------
// The headline case
// ---------------------------------------------------------------------------

/// The #8195 incident, exactly: `origin/feature/issue-8195` sat at PR #8275's
/// head, and #8275 was CLOSED without merging.
#[test]
fn closed_unmerged_head_match_refuses() {
    let probe = Probe::Answered(vec![
        pr("8275", "CLOSED", TIP, false),
        pr("8226", "MERGED", OTHER, true),
    ]);
    assert_eq!(decide(Some(TIP), &probe), Decision::Refuse(pr("8275", "CLOSED", TIP, false)));
}

/// The forge's own JSON, verbatim from
/// `loom-daemon forge pr list --head feature/issue-8195 --state all
/// --json number,state,mergedAt,headRefOid,url` on 2026-09-28 — the shape the
/// parser actually has to read, not a hand-idealised one.
#[test]
fn parses_the_real_forge_shape() {
    let text = r#"[{"headRefOid":"3a4cfd2aa28c31e85dc81761e40730ff4a198a85","mergedAt":null,"number":8275,"state":"CLOSED","url":"https://github.com/rjwalters/loom/pull/8275"},{"headRefOid":"c356f277c8107c11f2a32a56d73829fcaae02cce","mergedAt":"2026-09-18T22:34:57Z","number":8226,"state":"MERGED","url":"https://github.com/rjwalters/loom/pull/8226"}]"#;
    let Probe::Answered(prs) = parse_probe(text) else {
        panic!("real forge output must parse");
    };
    assert_eq!(prs.len(), 2);
    assert!(prs[0].is_closed_unmerged());
    assert_eq!(prs[0].number, "8275");
    assert_eq!(prs[0].head_sha, TIP);
    assert!(!prs[1].is_closed_unmerged(), "a MERGED PR is not this arm's");
    assert!(prs[1].merged);
    assert_eq!(decide(Some(TIP), &parse_probe(text)), Decision::Refuse(prs[0].clone()));
}

// ---------------------------------------------------------------------------
// Every arm that must NOT refuse
// ---------------------------------------------------------------------------

/// The #4823 in-flight case, which this guard must leave completely alone.
#[test]
fn open_pr_proceeds() {
    let probe = Probe::Answered(vec![pr("9100", "OPEN", TIP, false)]);
    assert_eq!(
        decide(Some(TIP), &probe),
        Decision::Proceed(Proceed::OpenPr("9100".to_string()))
    );
}

/// Closed #A, then reopened as a NEW PR #B on the same branch: the tip can
/// still be #A's head, and there is live work to continue. The OPEN rung wins.
#[test]
fn open_pr_wins_over_a_same_branch_closed_one() {
    let probe = Probe::Answered(vec![
        pr("8275", "CLOSED", TIP, false),
        pr("8400", "OPEN", TIP, false),
    ]);
    assert_eq!(
        decide(Some(TIP), &probe),
        Decision::Proceed(Proceed::OpenPr("8400".to_string()))
    );
}

/// A merged PR at the tip is #5657's arm, already answered by `branch_landed`
/// before this guard runs. It must not be re-answered here — and in
/// particular must not be refused, which would turn the merged case's
/// skip-to-a-fresh-branch into a hard stop.
#[test]
fn merged_pr_at_the_tip_proceeds() {
    let probe = Probe::Answered(vec![pr("8226", "MERGED", TIP, true)]);
    assert_eq!(decide(Some(TIP), &probe), Decision::Proceed(Proceed::NoClosedPr));
}

/// GitHub search's `state:closed` qualifier includes merged PRs, so a forge
/// that reports `state: CLOSED` *with* a `mergedAt` is a merged PR wearing the
/// wrong label. `merged` is checked first for exactly that reason.
#[test]
fn closed_state_with_a_merged_at_is_not_this_arm() {
    let probe = Probe::Answered(vec![pr("8226", "CLOSED", TIP, true)]);
    assert_eq!(decide(Some(TIP), &probe), Decision::Proceed(Proceed::NoClosedPr));
}

/// The branch moved past what was closed — it carries commits the closure
/// never saw, so the tip is not "the closed PR's head". #7872's
/// `merged-head-mismatch` discipline: an exact tip match, or no answer.
#[test]
fn tip_moved_past_the_closed_head_proceeds() {
    let probe = Probe::Answered(vec![pr("8275", "CLOSED", OTHER, false)]);
    assert_eq!(
        decide(Some(TIP), &probe),
        Decision::Proceed(Proceed::TipMovedPast("8275".to_string()))
    );
}

/// Two closed PRs, only the second at the tip: order must not matter.
#[test]
fn a_later_closed_pr_at_the_tip_still_refuses() {
    let probe = Probe::Answered(vec![
        pr("8100", "CLOSED", OTHER, false),
        pr("8275", "CLOSED", TIP, false),
    ]);
    assert_eq!(decide(Some(TIP), &probe), Decision::Refuse(pr("8275", "CLOSED", TIP, false)));
}

/// No PR at all for the name.
#[test]
fn empty_answer_proceeds() {
    assert_eq!(
        decide(Some(TIP), &Probe::Answered(vec![])),
        Decision::Proceed(Proceed::NoClosedPr)
    );
}

// ---------------------------------------------------------------------------
// The fail-open floor
// ---------------------------------------------------------------------------

/// The direction is fixed by the arm this guard sits in: a forge outage must
/// never block worktree creation, and the merged check it follows already
/// fails open to reuse.
#[test]
fn unavailable_probe_proceeds() {
    assert_eq!(decide(Some(TIP), &Probe::Unavailable), Decision::Proceed(Proceed::Undecidable));
}

#[test]
fn unresolvable_tip_proceeds() {
    let probe = Probe::Answered(vec![pr("8275", "CLOSED", TIP, false)]);
    assert_eq!(decide(None, &probe), Decision::Proceed(Proceed::Undecidable));
    assert_eq!(decide(Some(""), &probe), Decision::Proceed(Proceed::Undecidable));
}

#[test]
fn unparseable_output_is_unavailable_not_an_empty_answer() {
    assert_eq!(parse_probe("not json"), Probe::Unavailable);
    assert_eq!(parse_probe(""), Probe::Unavailable);
    // A JSON object rather than an array is equally unreadable here.
    assert_eq!(parse_probe("{\"number\": 1}"), Probe::Unavailable);
}

/// A field the forge omitted must not become a match: an absent `headRefOid`
/// reads as the empty string, and the empty string never equals a real tip.
#[test]
fn missing_head_sha_never_matches() {
    let Probe::Answered(prs) = parse_probe(r#"[{"number":1,"state":"CLOSED","mergedAt":null}]"#)
    else {
        panic!("must parse");
    };
    assert_eq!(prs[0].head_sha, "");
    assert!(prs[0].is_closed_unmerged());
    assert_eq!(
        decide(Some(TIP), &Probe::Answered(prs)),
        Decision::Proceed(Proceed::TipMovedPast("1".to_string()))
    );
}

// ---------------------------------------------------------------------------
// Exit codes, and the one probe this guard declines to spend
// ---------------------------------------------------------------------------

fn opts(repo: &Path) -> Options {
    Options {
        branch: "feature/issue-8195".to_string(),
        issue: "8195".to_string(),
        base_display: "main".to_string(),
        repo: repo.to_path_buf(),
        json_output: "false".to_string(),
    }
}

/// An unresolvable `origin/<branch>` must not cost a forge round-trip: the
/// decision is Proceed whatever the forge says.
#[test]
fn no_origin_ref_skips_the_forge_entirely() {
    let dir = std::env::temp_dir();
    let asked = std::cell::Cell::new(false);
    let rc = run_with(&opts(&dir), &|_| {
        asked.set(true);
        Probe::Answered(vec![pr("8275", "CLOSED", TIP, false)])
    });
    assert_eq!(rc, 0);
    assert!(!asked.get(), "no resolvable tip => nothing to match => no query");
}

#[test]
fn json_number_never_changes_the_field_type() {
    assert_eq!(json_number("8275"), "8275");
    assert_eq!(json_number(""), "null");
    assert_eq!(json_number("8275a"), "null");
}
