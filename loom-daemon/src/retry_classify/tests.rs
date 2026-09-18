//! Tests for the wrapper's retry/rotation classifiers (#8037).
//!
//! The six behaviours #8037 named as "what a port will most likely get wrong"
//! each get a test below whose comment says what it prevents. They are the same
//! behaviours `defaults/scripts/tests/test-claude-wrapper-retry.sh` asserts
//! through the shell entry points; these pin them at the unit level, where the
//! degraded arm can be selected directly instead of by unsetting a function.

use super::*;

/// Library mode: a category plus the deny-list verdict the library gave it.
fn with_library<'a>(
    output: &'a str,
    exit_code: i32,
    category: &'a str,
    transient: bool,
) -> Input<'a> {
    Input {
        output,
        exit_code,
        classification: Some(category),
        classification_is_transient: transient,
    }
}

/// Degraded mode: `lib/classify-error.sh` was not sourced, so every predicate
/// falls back to its own regex.
fn degraded(output: &str, exit_code: i32) -> Input<'_> {
    Input {
        output,
        exit_code,
        classification: None,
        classification_is_transient: false,
    }
}

// --- Behaviour 1: RATE_LIMIT_ABORT is not transient but IS exhaustion -------

#[test]
fn rate_limit_abort_is_not_transient_but_is_exhaustion() {
    // Prevents: the wrapper retrying its OWN usage-limit sentinel. The CLI hit
    // a plan limit and showed an interactive prompt, so another attempt on the
    // same account hits the same limit — the remedy is rotation, never retry.
    // The pair must move together: if only one half were ported, the sweep
    // would either spin on a limit it cannot clear (transient=true) or die
    // without ever rotating (exhaustion=false).
    let input = with_library(RATE_LIMIT_ABORT, 1, "RECOVERABLE", true);
    let verdict = is_transient(&input);
    assert!(!verdict.retry, "the sentinel must never be retried");
    assert!(is_account_exhaustion(&input), "the sentinel must rotate");

    // And it reports ITSELF as the classification, not the RECOVERABLE the
    // library would have said — the wrapper logs the verdict it acted on
    // (#4501), so the two may not drift.
    assert_eq!(verdict.classification, RATE_LIMIT_ABORT);
}

#[test]
fn the_sentinel_wins_before_the_degraded_branch_too() {
    // Prevents: the sentinel check being reordered below the degraded arm,
    // where a non-zero exit retries by default. A host missing the classifier
    // library would then retry a usage limit instead of rotating.
    let input = degraded(RATE_LIMIT_ABORT, 1);
    assert!(!is_transient(&input).retry);
    assert!(is_account_exhaustion(&input));
}

// --- Behaviour 2: degraded classification is retry-by-default --------------

#[test]
fn degraded_classification_retries_any_non_zero_exit() {
    // Prevents: "improving" the degraded arm into a deny-list of known-bad
    // phrasings. With no classifier available there is nothing to deny-list
    // against, so a host missing lib/classify-error.sh would stop retrying
    // recoverable failures entirely. Retry-by-default is bounded by
    // MAX_RETRIES; an allow-list has twice turned a new CLI wording into an
    // instant permanent death (#4255, #4501).
    let verdict = is_transient(&degraded("something nobody has a pattern for", 1));
    assert!(verdict.retry);
    assert_eq!(verdict.classification, UNCLASSIFIED);
}

#[test]
fn a_zero_exit_is_terminal_in_both_modes() {
    // Prevents: retry-by-default swallowing success. Exit 0 is SUCCESS, and
    // there is nothing to retry — degraded mode keys on the exit code alone,
    // so this is the one thing holding that arm back from retrying everything.
    assert!(!is_transient(&degraded("anything at all", 0)).retry);
    assert!(!is_transient(&with_library("", 0, "SUCCESS", false)).retry);
}

#[test]
fn library_mode_defers_to_the_supplied_deny_list_verdict() {
    // Prevents: a second copy of classification_is_transient growing here. The
    // deny-list lives in lib/classify-error.sh and is the single source of
    // truth (#4501); this module must act on its answer, whatever it is.
    assert!(is_transient(&with_library("boom", 1, "RECOVERABLE", true)).retry);
    assert!(!is_transient(&with_library("boom", 1, "TOKEN_EXPIRED", false)).retry);
}

// --- Behaviour 3: the three account predicates are mutually exclusive -------

#[test]
fn the_three_account_predicates_are_mutually_exclusive() {
    // Prevents: one phrase driving two remedies. Exhaustion waits out a quota
    // window, auth-death needs a human re-auth, a session limit clears in
    // minutes without touching .bad_tokens — an overlap sends the wrong one,
    // e.g. marking a healthy account permanently dead over a concurrency cap.
    let cases: [(&str, [bool; 3]); 3] = [
        ("You have hit your weekly limit", [true, false, false]),
        ("OAuth token has expired", [false, true, false]),
        ("maximum number of concurrent sessions", [false, false, true]),
    ];
    for (phrase, want) in cases {
        let input = degraded(phrase, 1);
        let got = [
            is_account_exhaustion(&input),
            is_account_auth_dead(&input),
            is_account_session_limit(&input),
        ];
        assert_eq!(got, want, "'{phrase}' must classify as exactly one remedy");
    }
}

#[test]
fn library_mode_keeps_the_three_categories_disjoint() {
    // Same disjointness on the library path, where the categories rather than
    // the regexes decide. MODEL_CREDITS_EXHAUSTED (#5687) is the one category
    // that is deliberately a synonym: a distinct NAME, not a distinct remedy.
    let exhausted = with_library("x", 1, "TOKEN_EXHAUSTED", true);
    let credits = with_library("x", 1, "MODEL_CREDITS_EXHAUSTED", true);
    let expired = with_library("x", 1, "TOKEN_EXPIRED", false);
    let session = with_library("x", 1, "SESSION_LIMIT", true);

    assert!(is_account_exhaustion(&exhausted) && is_account_exhaustion(&credits));
    assert!(!is_account_auth_dead(&exhausted) && !is_account_session_limit(&exhausted));
    assert!(is_account_auth_dead(&expired));
    assert!(!is_account_exhaustion(&expired) && !is_account_session_limit(&expired));
    assert!(is_account_session_limit(&session));
    assert!(!is_account_exhaustion(&session) && !is_account_auth_dead(&session));
}

// --- Behaviour 4: the exit code is conjoined with every fallback regex ------

#[test]
fn a_limit_phrase_on_a_zero_exit_is_not_an_account_fault() {
    // Prevents: dropping the `[[ exit_code -ne 0 ]] &&` conjunction when the
    // regex moves to Rust. Without it, a SUCCESSFUL sweep whose output merely
    // quotes a limit phrase — summarising its own logs, or this very issue —
    // would rotate accounts and mark a healthy credential bad.
    assert!(!is_account_exhaustion(&degraded("You have hit your weekly limit", 0)));
    assert!(!is_account_auth_dead(&degraded("401 authentication_error", 0)));
    assert!(!is_account_session_limit(&degraded("maximum number of concurrent sessions", 0)));

    // The same text on a non-zero exit is the real thing.
    assert!(is_account_exhaustion(&degraded("You have hit your weekly limit", 1)));
    assert!(is_account_auth_dead(&degraded("401 authentication_error", 1)));
    assert!(is_account_session_limit(&degraded("maximum number of concurrent sessions", 1)));
}

// --- Behaviour 5: the backoff curve, exactly -------------------------------

#[test]
fn the_backoff_curve_is_60_120_240_480_960_capped_at_1800() {
    // Prevents: a "better" curve. This is how hard a rate-limited fleet hammers
    // the API; jitter, a different base, or a missing ceiling all change the
    // load a limited account sees at exactly the moment it is limited.
    let backoff = Backoff {
        initial_wait: 60,
        multiplier: 2,
        max_wait: 1800,
    };
    let curve: Vec<i64> = (1..=6).map(|n| calculate_wait_time(n, &backoff)).collect();
    assert_eq!(curve, vec![60, 120, 240, 480, 960, 1800]);
    // Attempt 6 would be 1920s; the cap holds for every later attempt too.
    assert_eq!(calculate_wait_time(20, &backoff), 1800);
}

#[test]
fn the_backoff_ceiling_holds_instead_of_overflowing_negative() {
    // Prevents: reproducing bash's wrapping arithmetic. `60 * 2^(attempt-1)`
    // overflows i64 around attempt 58, and a wrapped NEGATIVE wait compares
    // below MAX_WAIT — so the shell would hand `sleep` a negative number and
    // retry instantly. Saturating keeps the ceiling.
    let backoff = Backoff {
        initial_wait: 60,
        multiplier: 2,
        max_wait: 1800,
    };
    assert_eq!(calculate_wait_time(1_000, &backoff), 1800);
    assert_eq!(calculate_wait_time(0, &backoff), 1800);
}

// --- Behaviour 6: is_mcp_error ignores the exit code -----------------------

#[test]
fn is_mcp_error_matches_on_output_alone() {
    // Prevents: a port "tidying up" is_mcp_error by conjoining the exit code
    // like its three neighbours. It never had one — it answers "does this text
    // describe an MCP/plugin failure", and the caller decides what to do with
    // that. Adding a conjunction changes behaviour on a zero-exit run whose
    // output mentions MCP.
    assert!(is_mcp_error("MCP server failed"));
    assert!(is_mcp_error("plugins failed"));
    assert!(is_mcp_error("plugin foo failed to install"));
    assert!(!is_mcp_error("connection reset by peer"));
    assert!(!is_mcp_error(""));
}

// --- The fallback regexes themselves ---------------------------------------

#[test]
fn the_exhaustion_fallback_covers_every_pinned_phrase() {
    // The degraded regexes had no coverage at all before #8032. Each phrase
    // here is one the shell matched; losing any of them means a limit that
    // silently stops rotating on a host without the classifier library.
    for phrase in [
        "You have hit your weekly limit",
        "monthly usage limit reached",
        "You are out of extra usage",
        "ran out of credits",
        "no plan credits remaining",
        "insufficient usage credits",
        "You've reached your Fable 5 limit. Run /usage-credits to continue",
    ] {
        assert!(is_account_exhaustion(&degraded(phrase, 1)), "'{phrase}' must be exhaustion");
    }
    assert!(!is_account_exhaustion(&degraded("connection reset by peer", 1)));
}

#[test]
fn the_auth_dead_fallback_covers_every_pinned_phrase() {
    for phrase in [
        "401 authentication_error",
        r#""type": "authentication_error""#,
        "token has been revoked",
        "invalid bearer token",
        "OAuth token has expired",
    ] {
        assert!(is_account_auth_dead(&degraded(phrase, 1)), "'{phrase}' must be auth-death");
    }
    // Exhaustion is NOT auth-death: one recovers with time, the other needs a
    // human. Marking an exhausted account dead shrinks the pool permanently.
    assert!(!is_account_auth_dead(&degraded("You have hit your weekly limit", 1)));
}

#[test]
fn the_session_limit_fallback_covers_every_pinned_phrase() {
    for phrase in [
        "maximum number of concurrent sessions",
        "too many concurrent requests",
        "another session is already active",
    ] {
        assert!(
            is_account_session_limit(&degraded(phrase, 1)),
            "'{phrase}' must be a session limit"
        );
    }
    assert!(!is_account_session_limit(&degraded("connection reset by peer", 1)));
}

#[test]
fn a_fallback_pattern_cannot_straddle_a_newline() {
    // Prevents: the silent widening a whole-string regex would introduce. The
    // shell pipes through `grep`, which matches within a LINE, so `[[:space:]]`
    // in "hit your <n> limit" can never eat a newline. A Rust `.is_match` over
    // the whole buffer would match two unrelated log lines that happen to end
    // and begin with those words.
    assert!(!is_account_exhaustion(&degraded("You have hit your\nweekly limit", 1)));
    assert!(!is_account_auth_dead(&degraded("\"type\"\n: \"authentication_error\"", 1)));
}

// --- Behaviour 7: how narrow a .bad_tokens mark may be (#8058 / #8138) ------
//
// `model_class_marker` is the one predicate whose two error directions are NOT
// symmetric, so each test below says which direction it is holding the line on.
// Over-scoping (a marker where the death was account-wide) leaves a dead
// account in rotation for every OTHER class — a live pool regression.
// Under-scoping merely reproduces the pre-#8058 behaviour.

#[test]
fn model_credits_exhaustion_scopes_the_mark_to_the_model_in_flight() {
    // Prevents: the #8058 fix being lost in the port. MODEL_CREDITS_EXHAUSTED
    // (#5687) is per-tier BY DEFINITION, so an account that hit only its Opus
    // ceiling must stay eligible for Sonnet work.
    let input = with_library("You're out of usage credits.", 1, "MODEL_CREDITS_EXHAUSTED", true);
    assert_eq!(
        model_class_marker(&input, "claude-opus-5").as_deref(),
        Some(" [model-class:claude-opus-5]")
    );
}

#[test]
fn the_marker_carries_the_raw_model_and_a_leading_space() {
    // Both halves are contract. The RAW value (not a normalized class) is what
    // `tokens_pool::bad_tokens` expects to normalize on READ, and the leading
    // space is what keeps the reason a well-formed
    // `exhausted: <phrase> [model-class:<m>]` when the caller concatenates it.
    let input = with_library("out of usage credits", 1, "MODEL_CREDITS_EXHAUSTED", true);
    let marker = model_class_marker(&input, "claude-sonnet-4-6").expect("scoped");
    assert!(marker.starts_with(' '), "{marker:?} must be appendable as a suffix");
    assert_eq!(
        format!("exhausted: out of credits{marker}"),
        "exhausted: out of credits [model-class:claude-sonnet-4-6]"
    );
}

#[test]
fn a_per_model_ceiling_scopes_but_an_account_wide_one_does_not() {
    // THE asymmetry, and the reason the predicate is not the bare #4501 regex:
    // "reached your Fable 5 limit" and "reached your weekly limit" both match
    // that regex, and only the first is scoped to one class. Asserted with the
    // SAME model in flight, because the model is not what tells them apart.
    let ceiling = degraded("You've reached your Fable 5 limit.", 1);
    assert_eq!(
        model_class_marker(&ceiling, "claude-opus-5").as_deref(),
        Some(" [model-class:claude-opus-5]")
    );
    for account_wide in [
        "You've reached your weekly limit. Your limit will reset later.",
        "You've reached your monthly usage limit.",
        "You've reached your daily limit.",
        "You've reached your account limit.",
        "You've reached your plan limit.",
        "You've reached your api spend limit.",
    ] {
        assert_eq!(
            model_class_marker(&degraded(account_wide, 1), "claude-opus-5"),
            None,
            "'{account_wide}' must leave the mark ACCOUNT-WIDE"
        );
    }
}

#[test]
fn a_ceiling_that_names_nothing_does_not_scope() {
    // The `{1,3}` in CEILING_PHRASE, not EXHAUSTION_FALLBACK's `{0,3}`: a bare
    // "reached your limit" names no class, so there is nothing to scope TO and
    // the conservative answer is account-wide.
    assert_eq!(
        model_class_marker(&degraded("You've reached your limit.", 1), "claude-opus-5"),
        None
    );
}

#[test]
fn credits_exhaustion_only_scopes_through_the_library_path() {
    // Prevents: widening the #5687 arm into the degraded path. The shell gated
    // it behind `declare -F classify_error`, so a caller with no classifier saw
    // the credits phrasing and still wrote an account-wide mark. The port keeps
    // that, because the phrase alone does not say WHICH tier ran out — only the
    // category does.
    assert_eq!(
        model_class_marker(&degraded("You're out of usage credits.", 1), "claude-opus-5"),
        None
    );
    // ...and a DIFFERENT category on the library path is not the credits one.
    let exhausted = with_library("You're out of usage credits.", 1, "TOKEN_EXHAUSTED", true);
    assert_eq!(model_class_marker(&exhausted, "claude-opus-5"), None);
}

#[test]
fn no_model_in_flight_never_scopes() {
    // The session default: the wrapper does not know which class the child ran,
    // so there is no honest marker to write. Blank is treated as absent — the
    // one deliberate divergence from the shell's `[[ -n ]]`, which would have
    // written the meaningless `[model-class: ]`.
    let input = with_library("out of usage credits", 1, "MODEL_CREDITS_EXHAUSTED", true);
    assert_eq!(model_class_marker(&input, ""), None);
    assert_eq!(model_class_marker(&input, "   "), None);
}

#[test]
fn an_account_wide_qualifier_anywhere_on_the_matched_line_widens_the_mark() {
    // `grep -m1 -ioE` semantics, both halves:
    //   * ALL matches on the first matching line are judged, so a per-model
    //     ceiling quoted alongside a weekly one still widens — the safe
    //     direction when the text is ambiguous.
    //   * Only the FIRST matching line is judged, so an unrelated later line
    //     does not retroactively widen a clean one.
    let mixed = degraded("reached your Fable 5 limit and reached your weekly limit", 1);
    assert_eq!(model_class_marker(&mixed, "claude-opus-5"), None);

    let later_line = degraded("reached your Fable 5 limit\nreached your weekly limit", 1);
    assert_eq!(
        model_class_marker(&later_line, "claude-opus-5").as_deref(),
        Some(" [model-class:claude-opus-5]")
    );
}

#[test]
fn the_ceiling_phrase_cannot_straddle_a_newline() {
    // Same widening this module's other regexes are pinned against: the shell
    // fed `grep`, which matches within a LINE, so `[[:space:]]` can never eat a
    // newline. A whole-buffer match would scope a mark off two unrelated log
    // lines.
    assert_eq!(
        model_class_marker(&degraded("reached your\nFable 5 limit", 1), "claude-opus-5"),
        None
    );
}
