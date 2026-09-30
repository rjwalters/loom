//! Unit tests for the two post-merge partial-increment audit comments.
//!
//! The byte-for-byte agreement with the retired shell is
//! `tests/merge_pr_partial_comment_differential.rs`'s job. What is tested here
//! is the part a differential cannot reach: the properties that must hold for
//! *every* input, including ones the retired shell could never have been fed
//! through a `$(...)`-captured variable.

use super::{now_timestamp, partial_merged_comment, premature_close_comment};

/// The `$reopen_note` fragment is the only conditional in either body, and it
/// is a LEADING-newline bullet interpolated straight after `**Action taken**:`.
/// Getting that newline wrong is invisible in a diff and visible on GitHub.
#[test]
fn the_reopen_note_is_a_leading_newline_bullet_not_a_trailing_one() {
    let with = partial_merged_comment("999", true, "2026-09-30T00:00:00Z");
    let without = partial_merged_comment("999", false, "2026-09-30T00:00:00Z");

    assert!(
        without.contains("**Action taken**:\n- Removed `loom:building` label"),
        "without the reopen the heading must sit directly above the first real bullet:\n{without}"
    );
    assert!(
        with.contains("**Action taken**:\n- **Reopened** this issue"),
        "with the reopen the new bullet must be the FIRST one:\n{with}"
    );
    assert!(
        with.contains("see #4569)\n- Removed `loom:building` label"),
        "…and the pre-existing bullets must follow it unchanged:\n{with}"
    );
}

/// The two bodies differ ONLY by that fragment. Anything else drifting apart
/// between the two arms would mean the conditional had grown a second effect.
#[test]
fn reopened_adds_exactly_one_line_and_changes_nothing_else() {
    let with = partial_merged_comment("77", true, "t");
    let without = partial_merged_comment("77", false, "t");
    assert_eq!(
        with.lines().count(),
        without.lines().count() + 1,
        "the reopen note is one line, no more and no less"
    );
    let stripped: String = with
        .lines()
        .filter(|l| !l.starts_with("- **Reopened**"))
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(
        stripped, without,
        "removing the reopen bullet must reproduce the un-reopened body exactly"
    );
}

/// The retired shell built both bodies as `comment="…"`, which ended at the
/// closing quote. The shell wrapper captures this verb's stdout through
/// `$(...)`, which strips trailing newlines — so a trailing newline here would
/// be invisible in production and would still be a real divergence for the
/// differential and for any other consumer.
#[test]
fn neither_body_ends_with_a_newline() {
    for body in [
        partial_merged_comment("1", false, "t"),
        partial_merged_comment("1", true, "t"),
        premature_close_comment("2", "1", "t"),
    ] {
        assert!(
            !body.ends_with('\n'),
            "the retired `comment=\"…\"` ended at its closing quote:\n{body:?}"
        );
    }
}

/// Both bodies sign off with an attribution line naming the issue that
/// introduced them. Champion, Judge and operators read these by eye to tell
/// "merge-pr did this on purpose" from "something went wrong", so the
/// attribution is contract, not decoration.
#[test]
fn each_body_is_attributed_to_the_issue_that_introduced_it() {
    assert!(
        partial_merged_comment("1", false, "TS").ends_with("*Reset by merge-pr.sh (#3667) at TS*")
    );
    assert!(premature_close_comment("2", "1", "TS")
        .ends_with("*Reopened by merge-pr.sh (#4569) at TS*"));
}

/// The premature-close body is instructional: it names the offending issue
/// number in the `close #N` examples it tells the Builder to stop writing.
/// A substitution that stopped substituting would leave advice about a
/// different issue than the one it is posted on.
#[test]
fn the_premature_close_body_names_the_issue_in_every_example() {
    let body = premature_close_comment("4242", "999", "t");
    assert_eq!(
        body.matches("#4242").count(),
        7,
        "all seven occurrences of the issue number must interpolate:\n{body}"
    );
    assert!(body.contains("close issue #4242"), "the recommended form");
    assert!(body.contains("instead of `close #4242`"), "and the form to avoid");
    assert_eq!(
        body.matches("#999").count(),
        1,
        "the PR is named exactly once, in the opening sentence"
    );
}

/// Neither renderer validates or escapes its arguments, on purpose: both are
/// fed `$PR_NUMBER` / an issue number `merge-pr.sh` matched with `[0-9]+`
/// upstream, so there is no attacker-controlled path into them. What must NOT
/// happen is a renderer that silently drops an unexpected value — that would
/// turn a caller bug into a plausible-looking comment about nothing.
#[test]
fn an_empty_number_is_rendered_as_empty_not_silently_defaulted() {
    let body = premature_close_comment("", "", "t");
    assert!(
        body.contains("PR # referenced this issue"),
        "an empty PR number renders as an empty one, visibly:\n{body}"
    );
    assert!(
        body.contains("instead of `close #`"),
        "…and so does an empty issue number:\n{body}"
    );
}

/// `now_timestamp` is the one impure thing in this module. It cannot be
/// compared against a fixed string, but its SHAPE is part of the frozen text,
/// so that is what is pinned: the retired `date -u +%Y-%m-%dT%H:%M:%SZ`
/// produced exactly 20 ASCII characters, second resolution, `Z`-suffixed, with
/// no fractional part.
#[test]
fn the_timestamp_keeps_the_retired_date_format() {
    let ts = now_timestamp();
    assert_eq!(ts.len(), 20, "unexpected width: {ts}");
    assert!(ts.ends_with('Z'), "not Z-suffixed: {ts}");
    assert!(!ts.contains('.'), "no fractional seconds: {ts}");
    assert!(!ts.contains('+'), "no numeric offset: {ts}");
    let (date, time) = ts[..ts.len() - 1].split_once('T').expect("no T separator");
    assert_eq!(date.len(), 10, "unexpected date width: {date}");
    assert_eq!(time.len(), 8, "unexpected time width: {time}");
    assert!(date.chars().all(|c| c.is_ascii_digit() || c == '-'), "non-ASCII date: {date}");
    assert!(time.chars().all(|c| c.is_ascii_digit() || c == ':'), "non-ASCII time: {time}");
}
