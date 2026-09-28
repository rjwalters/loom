//! Unit tests for the merge-response classifier.
//!
//! The differential (`tests/merge_pr_response_differential.rs`) proves these
//! agree with the retired `grep` ladder over a generated corpus. These name the
//! individual *properties* the ladder existed to protect, so a reader can see
//! each one without reconstructing it from a corpus entry — above all the
//! precedence that `merge-pr.sh` used to assert with an `awk` scan over its own
//! source text.

use super::*;

/// Verbatim bodies observed from the two forges, as `merge-pr.sh` captures
/// them: `forge_merge_pr … 2>&1`, so `Error:` prefixes and `(HTTP nnn)`
/// suffixes are part of the text. Kept identical to the table in
/// `defaults/scripts/tests/test-merge-pr-head-mismatch.sh` so the two cannot
/// drift silently.
const GITHUB_REST_HEAD: &[u8] =
    b"Error: Head branch was modified. Review and try the merge again. (HTTP 409)";
const GITEA_HEAD: &[u8] = b"Error: head out of date (HTTP 409)";
const GITHUB_GRAPHQL_HEAD: &[u8] =
    b"GraphQL: expectedHeadOid does not match the current head (enablePullRequestAutoMerge)";
const GITHUB_REST_BASE: &[u8] =
    b"Error: Base branch was modified. Review and try the merge again. (HTTP 409)";
const GITHUB_REST_405: &[u8] = b"Error: Merge already in progress (HTTP 405)";

#[test]
fn each_forge_head_mismatch_string_routes_to_head_mismatch() {
    for body in [GITHUB_REST_HEAD, GITEA_HEAD, GITHUB_GRAPHQL_HEAD] {
        assert_eq!(
            classify(body),
            MergeResponseKind::HeadMismatch,
            "{}",
            String::from_utf8_lossy(body)
        );
    }
}

#[test]
fn base_modified_and_405_keep_their_own_routes() {
    assert_eq!(classify(GITHUB_REST_BASE), MergeResponseKind::BaseModified);
    assert_eq!(classify(GITHUB_REST_405), MergeResponseKind::MergeInProgress);
}

/// The whole point of the module. A body naming BOTH branches must take the
/// head-mismatch route: routing it to `BaseModified` would answer a head that
/// moved past the approved SHA with `forge_update_branch` and another merge
/// attempt.
///
/// In the shell this was two `if` blocks five lines apart, and the only thing
/// asserting their order was an `awk` scan for which `grep` appeared first in
/// `merge-pr.sh`. Here it is one ordered `match`, and this is its test.
#[test]
fn head_mismatch_wins_over_base_modified() {
    let both: &[u8] =
        b"Error: Base branch was modified.\nError: Head branch was modified. (HTTP 409)";
    assert_eq!(classify(both), MergeResponseKind::HeadMismatch);

    // …and in the other textual order, so the answer cannot be an artifact of
    // which marker the input happens to mention first.
    let reversed: &[u8] =
        b"Error: Head branch was modified. (HTTP 409)\nError: Base branch was modified.";
    assert_eq!(classify(reversed), MergeResponseKind::HeadMismatch);
}

/// 405 outranks both SHA-shaped routes, as the shell's first `if` did.
#[test]
fn merge_in_progress_wins_over_everything() {
    let all_three: &[u8] =
        b"Merge already in progress. Head branch was modified. Base branch was modified.";
    assert_eq!(classify(all_three), MergeResponseKind::MergeInProgress);
}

/// The head-mismatch matcher was `grep -Ei`; its two siblings were a bare
/// `grep -q`. That asymmetry is upstream behaviour and is preserved.
#[test]
fn case_insensitivity_applies_to_head_mismatch_only() {
    assert_eq!(classify(b"HEAD BRANCH WAS MODIFIED."), MergeResponseKind::HeadMismatch);
    assert_eq!(classify(b"HeAd OuT oF dAtE"), MergeResponseKind::HeadMismatch);
    assert_eq!(classify(b"EXPECTEDHEADOID"), MergeResponseKind::HeadMismatch);

    // Case-SENSITIVE siblings: a lower-cased body matches neither, so it falls
    // through to the terminal hard stop exactly as `grep -q` left it.
    assert_eq!(classify(b"base branch was modified"), MergeResponseKind::Other);
    assert_eq!(classify(b"merge already in progress"), MergeResponseKind::Other);
}

/// Case folding must be ASCII, not Unicode.
///
/// U+017F LATIN SMALL LETTER LONG S simple-case-folds to `s`, so a `(?i)`
/// regex — and `str::to_lowercase()` round-tripped through a comparison —
/// matches `waſ` against `was`. `grep -i` with an ASCII pattern under `LC_ALL=C`
/// does not, and neither may this port: widening the head-mismatch route on
/// forge-supplied text is the direction that spends the wrong irreversible
/// operation. `defaults/docs/verification-recipes.md` §6 records this exact
/// character (`Cloſes #1`) costing an earlier slice a silent divergence.
#[test]
fn folding_is_ascii_not_unicode() {
    let folded = "Head branch was modified.".replace('s', "\u{17f}");
    // Guard against a vacuous test: the substitution must actually have
    // changed the string, or the assertion below proves nothing.
    assert_ne!(folded, "Head branch was modified.");
    assert_eq!(classify(folded.as_bytes()), MergeResponseKind::Other);

    // Positive control on the SAME marker: an ASCII case change still matches,
    // so the `Other` above is the Unicode character's doing and not a broken
    // haystack.
    assert_eq!(classify(b"HEAD BRANCH WAS MODIFIED."), MergeResponseKind::HeadMismatch);
}

/// The escaped dot in `Head branch was modified\.` is load-bearing: without
/// it, GitHub's BASE-branch body ("Base branch was modified…") still does not
/// match, but a hypothetical "Head branch was modified" with no period would —
/// and dropping the period widens the head route at the retryable route's
/// expense. `grep` treated `\.` as a literal, so a different character there
/// is not a match.
#[test]
fn the_head_mismatch_period_is_literal() {
    assert_eq!(
        classify(b"Head branch was modified, review and retry"),
        MergeResponseKind::Other
    );
    assert_eq!(classify(b"Head branch was modified."), MergeResponseKind::HeadMismatch);
}

/// `Base branch was modified` carries NO trailing period, so punctuation after
/// it is irrelevant — unlike its head-side counterpart above. Asserted because
/// the two patterns look symmetric and are not.
#[test]
fn base_modified_needs_no_trailing_punctuation() {
    assert_eq!(
        classify(b"Base branch was modified, review and retry"),
        MergeResponseKind::BaseModified
    );
}

/// What the shell tolerated by construction (recipe §6, Cause 4).
#[test]
fn tolerances_are_preserved() {
    // Empty and absent: `echo "" | grep -q` matches nothing.
    assert_eq!(classify(b""), MergeResponseKind::Other);
    // Shorter than every needle — `windows()` yields nothing, no panic.
    assert_eq!(classify(b"x"), MergeResponseKind::Other);
    // Multi-line with the marker on a later line: `grep` scanned every line.
    let multi: &[u8] = b"gh: request failed\nretrying...\nError: Base branch was modified\n";
    assert_eq!(classify(multi), MergeResponseKind::BaseModified);
    // Not valid UTF-8. `grep` is byte-oriented and matches; a `read_to_string`
    // port would have failed the read, and a lossy decode would have inserted
    // U+FFFD. Neither may change the answer.
    let mut mojibake = b"Error: \xff\xfe Base branch was modified".to_vec();
    mojibake.push(0x80);
    assert_eq!(classify(&mojibake), MergeResponseKind::BaseModified);
    // An interior NUL likewise does not terminate the scan.
    assert_eq!(classify(b"Error:\0Head branch was modified."), MergeResponseKind::HeadMismatch);
}

/// A marker split across a line break must NOT match — `grep` is line-oriented,
/// and every pattern is newline-free, so neither implementation can join two
/// lines into one match. This is the equivalence the module doc claims; it is
/// asserted rather than assumed.
#[test]
fn a_marker_split_across_a_newline_does_not_match() {
    assert_eq!(classify(b"Base branch was\nmodified"), MergeResponseKind::Other);
    assert_eq!(classify(b"Head branch was modified\n."), MergeResponseKind::Other);
}

/// The wire tokens are a cross-file protocol (`merge-pr.sh` compares against
/// them literally) and two files that roll independently cannot renegotiate
/// one. Pinned so a rename shows up here rather than as a silent `Other`.
#[test]
fn wire_tokens_are_pinned() {
    assert_eq!(MergeResponseKind::MergeInProgress.token(), "merge-in-progress");
    assert_eq!(MergeResponseKind::HeadMismatch.token(), "head-mismatch");
    assert_eq!(MergeResponseKind::BaseModified.token(), "base-modified");
    assert_eq!(MergeResponseKind::Other.token(), "other");
}
