//! Conformance tests for [`crate::classify_error`].
//!
//! The core table is `defaults/scripts/tests/fixtures/classify_error/vectors.tsv`
//! — the SAME file `defaults/scripts/tests/test-classify-error.sh` reads, so
//! one edit drives both implementations. Everything else here pins the
//! engine-level behaviour the shell's own suite pins: exit-code-first
//! ordering (#3233), the SESSION_LIMIT-before-TOKEN_EXHAUSTED precedence
//! (#3947), the credits-after-exhaustion order (#5687), the codex table, the
//! generic transients, unknown providers, and the `classification_is_transient`
//! deny-list (#4501).

use super::{any_line_matches, NO_MESSAGES_RETURNED};
use crate::classify_error::{classification_is_transient, classify};

const VECTORS: &str =
    include_str!("../../../defaults/scripts/tests/fixtures/classify_error/vectors.tsv");

/// Every row of the shared vectors file, byte-for-byte the shell suite's
/// assertion: `expected  exit_code  provider  description  output`.
#[test]
fn vectors_tsv_conformance() {
    let mut rows = 0usize;
    for line in VECTORS.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.splitn(5, '\t').collect();
        let [expected, exit_code, provider, description, output] = fields.as_slice() else {
            panic!("malformed vector row (want 5 TAB-separated fields): {line}");
        };
        let exit_code: i32 = exit_code.parse().unwrap_or_else(|e| {
            panic!("vector row {description:?}: bad exit_code {exit_code:?}: {e}")
        });
        assert_eq!(
            *expected,
            classify(output, exit_code, provider),
            "vector failed: {description} (output {output:?})"
        );
        rows += 1;
    }
    // The fixture must never silently empty out: #11214 created it with 22
    // rows, and both suites read the same file.
    assert!(rows >= 22, "expected at least 22 vectors, parsed {rows}");
}

/// Exit-code-first ordering (#3233): a clean exit is SUCCESS no matter what
/// the output quotes, and the timeout contract precedes everything too.
#[test]
fn engine_exit_code_order() {
    for quoted in [
        "API Error: 429",
        "Failed to authenticate. API Error: 401 OAuth access token is invalid.",
        "You're out of usage credits",
        "500 502 503 rate limit network error",
    ] {
        assert_eq!("SUCCESS", classify(quoted, 0, "claude"));
        assert_eq!("SUCCESS", classify(quoted, 0, "codex"));
        assert_eq!("SUCCESS", classify(quoted, 0, "kimi"));
    }
    assert_eq!("TIMEOUT", classify("", 124, "claude"));
    assert_eq!("TIMEOUT", classify("some failure text", 137, "codex"));
    // 124/137 win over a pattern match, exactly as the shell checks them first.
    assert_eq!("TIMEOUT", classify("hit your weekly limit", 124, "claude"));
    // Any other non-zero exit inspects output: a genuine exhaustion phrasing
    // classifies normally…
    assert_eq!("TOKEN_EXHAUSTED", classify("hit your weekly limit", 42, "claude"));
    // …and an unrecognised failure lands in the catch-all.
    assert_eq!("RECOVERABLE", classify("some failure text", 42, "claude"));
}

/// SESSION_LIMIT precedes TOKEN_EXHAUSTED (#3947) and MODEL_CREDITS_EXHAUSTED
/// comes last among the account-side categories (#5687) — the two
/// load-bearing orderings of the claude table.
#[test]
fn claude_table_precedence() {
    assert_eq!(
        "SESSION_LIMIT",
        classify("API Error: 429 too many concurrent requests for this account", 1, "claude")
    );
    assert_eq!(
        "SESSION_LIMIT",
        classify(
            "maximum number of concurrent sessions reached; you are out of usage credits",
            1,
            "claude"
        )
    );
    // The #4501 per-model ceiling mentions /usage-credits and must stay
    // TOKEN_EXHAUSTED.
    assert_eq!(
        "TOKEN_EXHAUSTED",
        classify(
            "You've reached your Fable 5 limit. Run /usage-credits to continue or switch models with /model.",
            1,
            "claude"
        )
    );
    // A mixed message keeps the OLDER classification: order is load-bearing.
    assert_eq!(
        "TOKEN_EXHAUSTED",
        classify("You've hit your weekly limit. You are out of credits.", 1, "claude")
    );
}

/// Patterns match within a line, never across one — the `grep` semantics the
/// shell relies on. A multi-line transcript whose FIRST line ends with a
/// matching prefix must not weld its next line into a false match.
#[test]
fn matching_is_line_scoped() {
    let transcript = "hit your\nweekly limit\n";
    // "hit your\nweekly limit" would match `hit your …limit` if `.` or the
    // filler could straddle the newline; grep cannot, so neither may we.
    assert_eq!("RECOVERABLE", classify(transcript, 1, "claude"));
}

/// The `codex` table: FATAL config faults first, quota before auth, and the
/// claude vectors must not leak into it (cross-provider isolation).
#[test]
fn codex_table() {
    assert_eq!(
        "FATAL",
        classify(
            "Not inside a trusted directory and --skip-git-repo-check was not specified.",
            1,
            "codex"
        )
    );
    assert_eq!(
        "FATAL",
        classify(
            "The 'gpt-5-codex' model is not supported when using Codex with a ChatGPT account.",
            1,
            "codex"
        )
    );
    assert_eq!("TOKEN_EXHAUSTED", classify("You've hit your usage limit.", 1, "codex"));
    assert_eq!(
        "TOKEN_EXHAUSTED",
        classify("failed to connect to websocket: HTTP error: 429 insufficient_quota", 1, "codex")
    );
    assert_eq!(
        "TOKEN_EXPIRED",
        classify("failed to connect to websocket: HTTP error: 401 Unauthorized", 1, "codex")
    );
    assert_eq!(
        "TOKEN_EXPIRED",
        classify("Your session has expired. Please reauthenticate.", 1, "codex")
    );
    // A prefixed bare 429 stays transient through the generic table (#11205
    // negative vector, codex arm).
    assert_eq!("RECOVERABLE", classify("API Error: 429", 1, "codex"));
    // Claude's session-limit wording has no codex signature: no table entry,
    // so it falls through to the generic catch-all.
    assert_eq!(
        "RECOVERABLE",
        classify("maximum number of concurrent sessions reached", 1, "codex")
    );
}

/// Unknown providers never error: no table, straight to the generic
/// transients (the #8561 Kimi contract).
#[test]
fn unknown_provider_falls_through_to_generic() {
    assert_eq!(
        "RECOVERABLE",
        classify(
            "error: failed to run prompt: provider.rate_limit: 429 rate limit exceeded",
            1,
            "kimi"
        )
    );
    // No generic pattern matches an unrecognised config fault, so the
    // catch-all resolves it.
    assert_eq!(
        "RECOVERABLE",
        classify("error: failed to run prompt: No model configured.", 1, "kimi")
    );
    // TIMEOUT and SUCCESS are provider-independent.
    assert_eq!("TIMEOUT", classify("", 124, "kimi"));
    assert_eq!("SUCCESS", classify("provider.rate_limit: 429 rate limit exceeded", 0, "kimi"));
}

/// The `No messages returned` case is case-SENSITIVE, exactly as the shell's
/// plain `grep -q` spelled it. The negative is only observable on the pattern
/// itself — every non-match falls through to the same RECOVERABLE catch-all,
/// so `classify`'s public answer cannot distinguish the two.
#[test]
fn no_messages_returned_is_case_sensitive() {
    assert_eq!("RECOVERABLE", classify("No messages returned", 1, "claude"));
    assert!(!any_line_matches("no messages returned", &NO_MESSAGES_RETURNED));
    assert!(!any_line_matches("NO MESSAGES RETURNED", &NO_MESSAGES_RETURNED));
}

/// The retry deny-list (#4501): six terminal categories, everything else
/// retryable — including unrecognized category strings, which is the
/// retry-by-default fail-safe.
#[test]
fn transient_deny_list() {
    for terminal in [
        "SUCCESS",
        "TIMEOUT",
        "TOKEN_EXPIRED",
        "CWD_DELETED",
        "MODEL_REFUSAL",
        "FATAL",
    ] {
        assert!(!classification_is_transient(terminal), "{terminal} must be terminal");
    }
    for retryable in [
        "RECOVERABLE",
        "TOKEN_EXHAUSTED",
        "MODEL_CREDITS_EXHAUSTED",
        "SESSION_LIMIT",
        "UNCLASSIFIED",
        "",
        "nonsense",
    ] {
        assert!(classification_is_transient(retryable), "{retryable} must be retryable");
    }
}

/// The provider selector's default chain mirrors the shell's
/// `${3:-${LOOM_RUNTIME:-claude}}`: explicit > `$LOOM_RUNTIME` > claude, with
/// empty treated as unset at both levels. Pure function via the `_with` form,
/// so it is parallel-safe.
#[test]
fn provider_default_chain() {
    use crate::classify_error::resolve_provider_with as resolve;
    assert_eq!("claude", resolve(None, None));
    assert_eq!("claude", resolve(Some(""), None));
    assert_eq!("claude", resolve(None, Some("")));
    assert_eq!("claude", resolve(Some(""), Some("")));
    assert_eq!("codex", resolve(None, Some("codex")));
    assert_eq!("kimi", resolve(Some("kimi"), Some("codex")));
    assert_eq!("kimi", resolve(Some("kimi"), None));
}
