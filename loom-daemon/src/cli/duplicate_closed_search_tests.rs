//! Tests for the windowed closed-issue search (#9208): what reaches the
//! query string, how the answer is parsed, and every way the search can
//! decline to answer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;

use chrono::TimeZone;

use super::*;

fn terms(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

const SINCE: &str = "2026-07-12";
const FIXED: &str = "repo:owner/repo is:issue is:closed closed:>=2026-07-12";

/// The fixed qualifiers, then the terms joined by `OR` (bare terms would be
/// ANDed, and a paraphrased duplicate would then match nothing).
#[test]
fn query_is_fixed_qualifiers_plus_or_joined_terms() {
    let q = build_query("owner/repo", SINCE, &terms(&["guard", "quoting", "paradox"])).unwrap();
    assert_eq!(q, format!("{FIXED} guard OR quoting OR paradox"));
}

/// Six terms is the ceiling: GitHub allows five boolean operators.
#[test]
fn query_is_capped_at_six_terms() {
    let many = terms(&[
        "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel",
    ]);
    let q = build_query("owner/repo", SINCE, &many).unwrap();
    assert_eq!(q.matches(" OR ").count(), MAX_TERMS - 1);
    assert!(q.ends_with("alpha OR bravo OR charlie OR delta OR echo OR foxtrot"), "{q}");
    assert!(!q.contains("golf"));
}

/// The 256-character limit drops the term that would overflow, and every
/// term after it, rather than truncating a term in half.
#[test]
fn query_stays_within_the_length_cap() {
    let long: Vec<String> = (0..6).map(|i| format!("{}{i}", "x".repeat(60))).collect();
    let q = build_query("owner/repo", SINCE, &long).unwrap();
    assert!(q.len() <= MAX_QUERY_CHARS, "{} chars", q.len());
    let kept: Vec<&str> = q[FIXED.len() + 1..].split(" OR ").collect();
    assert_eq!(kept.len(), 3, "three 61-char terms fit, the fourth would overflow");
    assert!(kept.iter().all(|t| long.iter().any(|l| l == t)), "only whole terms: {kept:?}");

    // A single term too long to fit leaves nothing to search for.
    assert_eq!(build_query("owner/repo", SINCE, &["y".repeat(300)]), None);
}

/// The window's lower bound is `days` before now, as a UTC date.
#[test]
fn window_is_a_utc_date_n_days_back() {
    let now = chrono::Utc
        .with_ymd_and_hms(2026, 10, 10, 0, 30, 0)
        .unwrap();
    assert_eq!(closed_since(now, 90), "2026-07-12");
    assert_eq!(closed_since(now, 1), "2026-10-09");
    // Just after midnight UTC: the date is UTC's, not the host's.
    assert_eq!(closed_since(now, 0), "2026-10-10");
}

/// Hostile title text cannot reach the query: every qualifier, quote and
/// operator in it is either split away by keyword extraction or was never a
/// keyword at all.
#[test]
fn hostile_title_text_cannot_inject_qualifiers() {
    let title = r#"repo:other/thing is:open "quoted phrase" author:evil NOT closed:<2020-01-01 OR -label:x"#;
    let picked = search_terms(title, "user:victim in:comments archived:true");
    let q = build_query("owner/repo", SINCE, &picked).unwrap();

    assert!(q.starts_with(&format!("{FIXED} ")), "the fixed qualifiers are untouched: {q}");
    let tail = &q[FIXED.len() + 1..];
    assert!(
        tail.bytes().all(|b| b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || b == b' '
            || b == b'O'
            || b == b'R'),
        "only keyword tokens and the OR joiner follow the qualifiers: {tail}"
    );
    for word in tail.split(" OR ") {
        assert!(is_keyword_token(word), "`{word}` is a plain keyword token");
    }
    // The qualifier NAMES survive only as inert words; no `:` binds them.
    assert!(!tail.contains(':') && !tail.contains('"') && !tail.contains('-'));
    assert!(!tail.contains("NOT") && !tail.contains("AND"));
}

/// `build_query` re-checks token shape itself: a caller that hands it raw
/// text gets those entries dropped, not forwarded.
#[test]
fn build_query_drops_anything_that_is_not_a_keyword_token() {
    let raw = terms(&[
        "repo:other/thing",
        "is:open",
        "\"quoted\"",
        "NOT",
        "ab",
        "guard",
    ]);
    let q = build_query("owner/repo", SINCE, &raw).unwrap();
    assert_eq!(q, format!("{FIXED} guard"));
    assert_eq!(build_query("owner/repo", SINCE, &terms(&["is:open", "OR"])), None);
}

/// A repository slug is fixed-qualifier text too, so it must be a plain
/// `owner/name` — it comes from a git remote or `LOOM_REPO`, not from us.
#[test]
fn a_slug_that_could_carry_a_qualifier_is_refused() {
    let t = terms(&["guard"]);
    for bad in [
        "owner/repo is:open",
        "owner",
        "owner/repo/extra",
        "o wner/repo",
        "/repo",
        "a/b\"c",
    ] {
        assert_eq!(build_query(bad, SINCE, &t), None, "{bad:?}");
    }
    assert!(build_query("My-Org/some.repo_2", SINCE, &t).is_some());
}

/// Title keywords come first, in the order the title uses them, then the
/// body's; duplicates and stop words do not spend a slot.
#[test]
fn search_terms_prefer_the_title_in_reading_order() {
    let picked = search_terms(
        "Fix the Zebra guard: quoting paradox in guard",
        "apple banana cherry zebra durian elderberry",
    );
    assert_eq!(picked, terms(&["zebra", "guard", "quoting", "paradox", "apple", "banana"]));
    assert!(search_terms("the a an is", "of to it").is_empty());
}

/// The request is one page of at most 50, with the query percent-encoded.
#[test]
fn search_path_is_one_bounded_page() {
    let path = search_path("repo:owner/repo is:closed closed:>=2026-07-12 alpha OR bravo");
    assert_eq!(
        path,
        "search/issues?q=repo%3Aowner%2Frepo%20is%3Aclosed%20closed%3A%3E%3D2026-07-12%20alpha%20OR%20bravo&per_page=50"
    );
    assert!(!path.contains("page=2") && !path.contains("&page="));
}

/// `items` become candidates: pull requests are dropped, a null body reads
/// as empty, and an item with no number is skipped.
#[test]
fn items_are_parsed_with_pull_requests_excluded() {
    let answer = r#"{"total_count": 4, "incomplete_results": false, "items": [
        {"number": 6808, "title": "Guard false positive", "body": "quoting paradox", "state": "closed"},
        {"number": 6900, "title": "A pull request", "body": "x", "pull_request": {"url": "u"}},
        {"number": 6901, "title": "No body", "body": null, "pull_request": null},
        {"title": "no number", "body": "x"}
    ]}"#;
    let got = parse_items(answer).unwrap();
    let numbers: Vec<u64> = got.iter().map(|c| c.number).collect();
    assert_eq!(numbers, vec![6808, 6901]);
    assert_eq!(got[0].title, "Guard false positive");
    assert_eq!(got[0].body, "quoting paradox");
    assert_eq!(got[1].body, "");
    assert!(parse_items(r#"{"total_count": 0, "items": []}"#)
        .unwrap()
        .is_empty());
}

/// An answer without an `items` array is "did not answer", never "found
/// nothing" — an error body must not read as an empty, passing pool.
#[test]
fn an_answer_without_items_is_an_error() {
    for bad in [
        "",
        "not json",
        "[]",
        r#"{"message": "API rate limit exceeded", "documentation_url": "u"}"#,
        r#"{"items": "nope"}"#,
    ] {
        assert!(parse_items(bad).is_err(), "{bad:?}");
    }
}

/// Counts calls to the injected search and returns `answer`.
fn counted<'a>(
    calls: &'a Cell<u32>,
    answer: Result<&'a str, &'a str>,
) -> impl FnOnce(&str, &str) -> Result<String, String> + 'a {
    move |_, _| {
        calls.set(calls.get() + 1);
        answer.map(str::to_string).map_err(str::to_string)
    }
}

const ONE_HIT: &str =
    r#"{"items": [{"number": 6808, "title": "Guard quoting paradox", "body": ""}]}"#;

/// The answered path: exactly one request, against the bounded path.
#[test]
fn a_search_that_answers_is_one_request() {
    let calls = Cell::new(0);
    let seen = std::cell::RefCell::new(String::new());
    let out =
        select(&terms(&["guard", "quoting"]), true, Some("owner/repo"), SINCE, |slug, path| {
            calls.set(calls.get() + 1);
            assert_eq!(slug, "owner/repo");
            seen.replace(path.to_string());
            Ok(ONE_HIT.to_string())
        });
    assert_eq!(calls.get(), 1);
    assert!(seen.borrow().starts_with("search/issues?q=repo%3Aowner%2Frepo%20is%3Aissue%20is%3Aclosed%20closed%3A%3E%3D2026-07-12%20guard%20OR%20quoting&per_page="));
    match out {
        ClosedSearch::Candidates(c) => assert_eq!(c[0].number, 6808),
        other => panic!("expected candidates, got {other:?}"),
    }
}

/// No keywords: nothing to search for, so no request is made.
#[test]
fn no_keywords_means_no_request() {
    let calls = Cell::new(0);
    let out = select(&[], true, Some("owner/repo"), SINCE, counted(&calls, Ok(ONE_HIT)));
    assert!(matches!(out, ClosedSearch::NoKeywords));
    assert_eq!(calls.get(), 0);
}

/// Fallback triggers that never reach the forge: a non-GitHub forge, an
/// unresolvable repository, and a slug no safe query can be built from.
#[test]
fn fallback_triggers_that_make_no_request() {
    let t = terms(&["guard"]);
    let calls = Cell::new(0);
    let gitea = select(&t, false, Some("owner/repo"), SINCE, counted(&calls, Ok(ONE_HIT)));
    assert!(matches!(gitea, ClosedSearch::Unavailable(_)), "{gitea:?}");
    let no_repo = select(&t, true, None, SINCE, counted(&calls, Ok(ONE_HIT)));
    assert!(matches!(no_repo, ClosedSearch::Unavailable(_)), "{no_repo:?}");
    let bad_slug =
        select(&t, true, Some("owner/repo is:open"), SINCE, counted(&calls, Ok(ONE_HIT)));
    assert!(matches!(bad_slug, ClosedSearch::Unavailable(_)), "{bad_slug:?}");
    assert_eq!(calls.get(), 0, "none of these may spend a search request");
}

/// Fallback triggers after the one request: a refusal (403/429 from the
/// search bucket), a transport failure, and an answer that is not a search
/// result. Each is tried once and never retried.
#[test]
fn a_failed_search_is_unavailable_and_never_retried() {
    let t = terms(&["guard"]);
    for answer in [
        Err("the forge refused the search: gh: API rate limit exceeded (HTTP 403)"),
        Err(
            "the forge refused the search: gh: You have exceeded a secondary rate limit (HTTP 429)",
        ),
        Err("the search did not complete: timed out after 30s"),
        Ok(r#"{"message": "Validation Failed"}"#),
        Ok("<html>502 Bad Gateway</html>"),
    ] {
        let calls = Cell::new(0);
        let out = select(&t, true, Some("owner/repo"), SINCE, counted(&calls, answer));
        assert!(matches!(out, ClosedSearch::Unavailable(_)), "{answer:?} -> {out:?}");
        assert_eq!(calls.get(), 1, "one request, no retry: {answer:?}");
    }
}

/// The search call is the facade's, pinned to one credential: no raw process
/// spawn, and no reader-then-writer second attempt.
#[test]
fn the_request_goes_through_the_facade_once() {
    let src = include_str!("duplicate_closed_search.rs");
    assert_eq!(src.matches("Command::new(").count(), 0);
    assert_eq!(src.matches("GhInvocation::new(").count(), 1);
    assert!(src.contains(".writer_identity()"));
    assert!(src.contains("ops::ISSUE_SEARCH"));
}
