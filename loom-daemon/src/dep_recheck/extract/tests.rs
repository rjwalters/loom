//! Tests for `extract-refs` (epic #7810, PR 4).

use super::*;

fn comment(login: &str, body: &str) -> Comment {
    Comment {
        author: Author {
            login: login.to_string(),
        },
        body: body.to_string(),
    }
}

fn input(body: &str, comments: Vec<Comment>) -> Input {
    Input {
        body: body.to_string(),
        comments,
    }
}

fn refs(body: &str, comments: Vec<Comment>) -> String {
    extract(&input(body, comments), DEFAULT_BOT_LOGIN)
}

// ---------------------------------------------------------------------------
// The vocabulary
// ---------------------------------------------------------------------------

#[test]
fn each_declared_phrase_yields_its_reference() {
    for phrase in [
        "Blocked by #7",
        "Depends on #7",
        "Requires #7",
        "**Epic** #7",
    ] {
        assert_eq!(refs(phrase, vec![]), "7", "{phrase}");
    }
}

#[test]
fn markup_between_the_phrase_and_the_reference_is_tolerated() {
    assert_eq!(refs("**Blocked by:** #7", vec![]), "7");
    assert_eq!(refs("_Depends on_ #7", vec![]), "7");
}

#[test]
fn a_bare_mention_with_no_phrase_is_not_a_reference() {
    // A prose `#7` is not a declaration. Counting it would park issues on
    // every number anyone happened to cite.
    assert_eq!(refs("see #7 for context", vec![]), "");
    assert_eq!(refs("`owner/repo#7`", vec![]), "");
}

#[test]
fn references_are_sorted_numerically_and_deduplicated() {
    // `sort -un` — numeric here, unlike the lexicographic line sorts that feed
    // a hash. 9 precedes 10.
    assert_eq!(refs("Blocked by #10\nDepends on #9\nRequires #9", vec![]), "9 10");
}

#[test]
fn no_references_yields_an_empty_string() {
    assert_eq!(refs("nothing here", vec![]), "");
}

// ---------------------------------------------------------------------------
// #4507: the self-perpetuating loop
// ---------------------------------------------------------------------------

#[test]
fn the_automation_identitys_own_comment_never_contributes() {
    // THE bug: the bot's prior report quotes the matched phrase back into the
    // thread, so a naive body+comments scan re-matches its own report forever.
    assert_eq!(
        refs(
            "Fixed, no dependency now.",
            vec![comment(
                DEFAULT_BOT_LOGIN,
                "Premise possibly stale: Blocked by #7"
            )]
        ),
        ""
    );
}

#[test]
fn the_login_match_tolerates_both_spellings_the_forge_uses() {
    // `gh issue view --json comments` reports a bare login; other paths report
    // `app/<login>` or `<login>[bot]`. Matching one spelling lets the loop back
    // in through the other.
    for spelling in [
        "loom-fleet-dispatch",
        "app/loom-fleet-dispatch",
        "loom-fleet-dispatch[bot]",
        "app/Loom-Fleet-Dispatch[bot]",
        "LOOM-FLEET-DISPATCH",
    ] {
        assert_eq!(refs("clean body", vec![comment(spelling, "Blocked by #7")]), "", "{spelling}");
    }
}

#[test]
fn a_comment_carrying_an_own_marker_is_excluded_whoever_wrote_it() {
    // Belt and suspenders on top of the author check: the marker identifies
    // the mechanism regardless of which login the forge attributes it to.
    for marker in [
        "<!-- curator:dep-recheck:abc -->",
        "<!-- curator:operator-premise-recheck:abc -->",
    ] {
        assert_eq!(
            refs("clean body", vec![comment("a-human", &format!("Blocked by #7\n{marker}"))]),
            "",
            "{marker}"
        );
    }
}

#[test]
fn a_genuine_human_comment_still_contributes() {
    // The loop fix must not also deafen the mechanism to new information —
    // that would be a silent regression in the safe-looking direction.
    assert_eq!(
        refs("clean body", vec![comment("a-human", "Actually this is Blocked by #7 now")]),
        "7"
    );
}

#[test]
fn the_body_is_always_scanned_even_when_every_comment_is_excluded() {
    assert_eq!(refs("Blocked by #7", vec![comment(DEFAULT_BOT_LOGIN, "Depends on #9")]), "7");
}

#[test]
fn a_custom_bot_login_is_honoured() {
    let i = input("clean", vec![comment("other-bot", "Blocked by #7")]);
    assert_eq!(extract(&i, "other-bot"), "");
    assert_eq!(extract(&i, DEFAULT_BOT_LOGIN), "7");
}

#[test]
fn an_author_field_absent_entirely_does_not_match_the_bot() {
    // `gh` omits the author on some comment shapes. An empty login must not
    // compare equal to the bot's, or an ordinary comment is silently dropped.
    let i: Input =
        serde_json::from_str(r#"{"body":"b","comments":[{"body":"Blocked by #7"}]}"#).unwrap();
    assert_eq!(extract(&i, DEFAULT_BOT_LOGIN), "7");
}

#[test]
fn a_body_only_document_decodes() {
    let i: Input = serde_json::from_str(r#"{"body":"Blocked by #7"}"#).unwrap();
    assert_eq!(extract(&i, DEFAULT_BOT_LOGIN), "7");
}

// ---------------------------------------------------------------------------
// #8011: undisclosed divergences from the pre-port shell
// ---------------------------------------------------------------------------

#[test]
fn a_phrase_and_its_reference_may_be_split_across_a_newline() {
    // The pre-port shell's `grep -oE` was line-oriented and could never see
    // this; kept intentionally (fail-safe direction — see `phrase_re`'s doc).
    assert_eq!(refs("Blocked by\n#42", vec![]), "42");
}

#[test]
fn bot_login_normalises_symmetrically_on_both_sides() {
    // The shell only normalised the comment AUTHOR's app/[bot] shape, not the
    // supplied --bot-login itself, so passing the "app/"-prefixed spelling
    // would never have matched a bare-login author. Both sides go through
    // `normalise_login` here, so it does (arguably a fix, see its doc).
    let i = input("clean body", vec![comment("loom-fleet-dispatch", "Depends on #93")]);
    // Without the flag (default bot login, already bare) the comment is
    // excluded, as it always was.
    assert_eq!(extract(&i, DEFAULT_BOT_LOGIN), "");
    // With the "app/"-prefixed spelling of the SAME identity, it is now also
    // excluded — the shell's asymmetric normalisation would have missed this.
    assert_eq!(extract(&i, "app/loom-fleet-dispatch"), "");
}
