//! The error classifier behind `defaults/scripts/lib/classify-error.sh`
//! (#11208): one `(output, exit_code, provider)` triple in, one uppercase
//! category out — the same enum the shell library has emitted since before
//! #4190, now shipped natively so consumers stop vendoring the shell file.
//!
//! # What this is, and what it deliberately is not
//!
//! The classifier is, per #4501, the fleet's single source of truth for *what
//! a failure IS* (`classify_error`) and *whether that failure is retryable*
//! (`classification_is_transient`). Before #11208 both lived only in the
//! sourced shell library, which external consumers (2AMLogic/miniset's E2B
//! shim) had to download or vendor. This module ports both, byte-compatibly;
//! the shell library now delegates here when a daemon binary resolves and
//! keeps its in-file tables as the no-daemon fallback.
//!
//! What is NOT here: `crate::retry_classify`'s wrappers-side predicates
//! (retry vs give up, rotate, mark dead, backoff). Those keep taking the
//! classification and the transient verdict as inputs — the #4501 boundary —
//! with the difference that both halves of the answer now live in this
//! binary, pinned together by conformance tests rather than by two copies of
//! the same regexes drifting apart.
//!
//! # Conformance is enforced, not asserted
//!
//! Three suites pin this module to the shell:
//!
//! * `classify_error/tests.rs` table-drives [`classify`] over
//!   `defaults/scripts/tests/fixtures/classify_error/vectors.tsv` (the real
//!   Claude Code 2.1.295 messages from #11205, plus negatives), via
//!   `include_str!` — the same file the shell suite reads.
//! * `defaults/scripts/tests/test-classify-error.sh` runs its whole existing
//!   table through the shell library, which now delegates here when a binary
//!   resolves, and diffs the in-file shell tables against this subcommand on
//!   every vector row.
//! * `tests/test-claude-wrapper-retry.sh` and the spawn suites run the
//!   wrapper's predicates end-to-end unchanged.
//!
//! # Translation rules from the shell (why the code looks like this)
//!
//! The shell spells every pattern as `echo "$output" | grep -qiE …`, so:
//!
//! * matching is **line-by-line** — `grep` matches within a line, and a
//!   whole-string search would let `.` / `[[:space:]]` straddle a newline and
//!   match text the shell never would (same reasoning as
//!   [`crate::retry_classify::any_line_matches`]);
//! * every pattern carries `(?i)` where the shell passed `-i`, and none where
//!   it did not (`No messages returned` is a case-SENSITIVE `grep -q`);
//! * the POSIX ERE syntax (`[[:space:]]`, `\.`) transfers verbatim — the
//!   `regex` crate supports both, and `(?i)` on a negated class like
//!   `[^a-z]` excludes the upper-case range exactly as `grep -i` does.
//!
//! Any divergence between this module and the shell tables is a bug in one of
//! them; the vectors and the parity section of the shell suite are the
//! tripwires. Change the two TOGETHER or not at all.

#[cfg(test)]
mod tests;

use std::sync::LazyLock;

/// Compile one pattern that is a constant of this module. A bad pattern is a
/// programming error, not a runtime condition.
fn re(pattern: &str) -> regex::Regex {
    regex::Regex::new(pattern)
        .unwrap_or_else(|e| panic!("classify_error pattern failed to compile: {e}: {pattern}"))
}

/// Does `output` contain a line matching `re`? Line-by-line on purpose — see
/// the module docs for the `grep` semantics this preserves.
fn any_line_matches(output: &str, re: &regex::Regex) -> bool {
    output.lines().any(|line| re.is_match(line))
}

/// The `claude` provider table, in the shell's exact match order — the order
/// is load-bearing and documented on each entry in
/// `defaults/scripts/lib/classify-error.sh`. `SESSION_LIMIT` must precede
/// `TOKEN_EXHAUSTED` (#3947: "concurrent session limit" contains "session
/// limit"); `MODEL_CREDITS_EXHAUSTED` must come last among the account-side
/// categories (#5687: the #4501 per-model ceiling mentions credits too); the
/// "No messages returned" case is case-sensitive because the shell used a
/// plain `grep -q` for it.
static CLAUDE_PATTERNS: LazyLock<Vec<(&'static str, regex::Regex)>> = LazyLock::new(|| {
    vec![
        // Working directory deleted (worktree cleaned up while CLI ran)
        ("CWD_DELETED", re(r"(?i)current working directory was deleted")),
        // Model refusal — matched only on a genuine failure (the engine has
        // already returned SUCCESS for exit 0), preserving #3233.
        (
            "MODEL_REFUSAL",
            re(r#"(?i)"?stop_reason"?[[:space:]]*[:=][[:space:]]*"?refusal"#),
        ),
        // Token expired / auth-dead: #6030, #6424, #6614, #10294 and #11205's
        // 401-family, verbatim from the shell table.
        (
            "TOKEN_EXPIRED",
            re(concat!(
                r"(?i)",
                r"401[^a-z]*authentication_error",
                r#"|"type"[[:space:]]*:[[:space:]]*"?authentication_error"#,
                r"|token (has been |was )?revoked",
                r"|invalid bearer token",
                r"|OAuth token has expired",
                r"|token has expired",
                r"|organization has disabled",
                r"|failed to authenticate.*socket connection was closed unexpectedly",
                r"|failed to authenticate\. api error: 401",
                r"|failed to authenticate: oauth session expired",
                r"|api error: 401 invalid api key",
            )),
        ),
        // Concurrent-session capacity fault (#3947) — BEFORE TOKEN_EXHAUSTED.
        (
            "SESSION_LIMIT",
            re(concat!(
                r"(?i)",
                r"concurrent (session|sessions|request)",
                r"|maximum number of concurrent",
                r"|too many concurrent",
                r"|simultaneous session",
                r"|another session is (already )?(active|running)",
            )),
        ),
        // Quota / session / weekly / usage / per-model limit (#3738, #4501,
        // #5631) plus #11205's budget/org/legacy-limit family.
        (
            "TOKEN_EXHAUSTED",
            re(concat!(
                r"(?i)",
                r"hit your ([^[:space:]]+[[:space:]]+){0,3}limit",
                r"|hit\.your\.limit",
                r"|monthly usage limit",
                r"|out of extra usage",
                r"|reached your ([^[:space:]]+[[:space:]]+){0,3}limit",
                r"|hit your ([^[:space:]]+[[:space:]]+){0,3}budget",
                r"|your org is out of usage",
                r"|claude ai usage limit reached",
            )),
        ),
        // Per-model-TIER credit exhaustion (#5687) — AFTER TOKEN_EXHAUSTED.
        (
            "MODEL_CREDITS_EXHAUSTED",
            re(concat!(
                r"(?i)",
                r"(ran |run )?out of (usage |extra |plan )?credits",
                r"|no (usage |extra |plan )?credits (remaining|left)",
                r"|insufficient (usage |plan )?credits",
            )),
        ),
    ]
});

/// The `No messages returned` case: the one pattern the shell deliberately
/// spells with a case-SENSITIVE `grep -q`, kept case-sensitive here.
static NO_MESSAGES_RETURNED: LazyLock<regex::Regex> = LazyLock::new(|| re(r"No messages returned"));

/// The `codex` provider table (epic #4167 Phase 2, #4468, #5499), in the
/// shell's order: FATAL config faults first (no retry can fix them), then
/// TOKEN_EXHAUSTED before TOKEN_EXPIRED (OpenAI has served quota faults with
/// a 401 status; mis-reading a quota fault as an auth fault is the expensive
/// direction — it marks the account bad with reason `auth`).
static CODEX_PATTERNS: LazyLock<Vec<(&'static str, regex::Regex)>> = LazyLock::new(|| {
    vec![
        // FATAL: configuration faults that no retry can fix (trusted-directory
        // refusal, unknown `-c` override key, sandbox construction failure).
        (
            "FATAL",
            re(concat!(
                r"(?i)",
                r"not inside a trusted directory",
                r"|unknown configuration field",
                r"|landlock_sandbox_executable_not_provided",
            )),
        ),
        // FATAL: model unsupported for this account's auth mode (#5499).
        ("FATAL", re(r"(?i)is not supported when using codex with a chatgpt account")),
        // Plan/quota exhaustion — rotate, mark `exhausted`. Verbatim binary
        // strings from codex-cli 0.146.0 plus the documented OpenAI API error
        // codes. A BARE 429 is deliberately NOT matched here (transient
        // throttle; the generic table owns it).
        (
            "TOKEN_EXHAUSTED",
            re(concat!(
                r"(?i)",
                r"hit your usage limit",
                r"|reached your usage limit",
                r"|usage limit reached",
                r"|workspace credit limit",
                r"|out of credits",
                r"|insufficient_quota",
                r"|rate_limit_exceeded",
                r"|exceeded your current quota",
                r"|quota exceeded",
            )),
        ),
        // This credential is bad — skip/re-auth this profile (#4468).
        (
            "TOKEN_EXPIRED",
            re(concat!(
                r"(?i)",
                r"401[^a-z]*unauthorized",
                r"|invalid_api_key",
                r"|invalid api key",
                r"|incorrect api key",
                r"|not signed in",
                r"|please run .?codex login",
                r"|refresh token has expired",
                r"|refresh token was (already used|revoked)",
                r"|missing a refresh token",
                r"|no refresh token available",
                r"|failed to refresh token",
                r"|session has expired",
                r"|please reauthenticate",
            )),
        ),
    ]
});

/// The provider-independent generic transient table — applies to every
/// provider including unknown ones, and always resolves (the catch-all).
static GENERIC_PATTERNS: LazyLock<Vec<(&'static str, regex::Regex)>> = LazyLock::new(|| {
    vec![
        // Rate limit (429) — note the unescaped dots and the bare `429`
        // substring, exactly as the shell spells them ("15004" matches; the
        // shell always did and callers depend on the exit-code-first engine
        // to keep clean exits at SUCCESS).
        ("RECOVERABLE", re(r"(?i)rate.limit|too.many.requests|429")),
        // Server errors (5xx)
        (
            "RECOVERABLE",
            re(r"(?i)500|502|503|504|internal.server.error|service.unavailable"),
        ),
        // Network errors
        ("RECOVERABLE", re(r"(?i)ECONNREFUSED|ETIMEDOUT|network.error")),
    ]
});

/// `classify_error` — classify one `(output, exit_code, provider)` triple.
///
/// Order is the shell engine's, preserved exactly (#3233, #3947, #5687):
///
/// 1. exit 124/137 (the `timeout(1)` contract) → `TIMEOUT` — a productive
///    cycle, provider-independently;
/// 2. exit 0 → `SUCCESS` regardless of output content;
/// 3. the provider's table, for `claude` and `codex` (an unknown provider
///    never errors — it has no table and falls straight through);
/// 4. the generic transient table, which always resolves.
#[must_use]
pub fn classify(output: &str, exit_code: i32, provider: &str) -> &'static str {
    if exit_code == 124 || exit_code == 137 {
        return "TIMEOUT";
    }
    if exit_code == 0 {
        return "SUCCESS";
    }

    let provider_table: &[(&'static str, regex::Regex)] = match provider {
        "claude" => &CLAUDE_PATTERNS,
        "codex" => &CODEX_PATTERNS,
        _ => &[],
    };
    for (category, pattern) in provider_table {
        if any_line_matches(output, pattern) {
            return category;
        }
    }
    if provider == "claude" && any_line_matches(output, &NO_MESSAGES_RETURNED) {
        return "RECOVERABLE";
    }

    for (category, pattern) in GENERIC_PATTERNS.iter() {
        if any_line_matches(output, pattern) {
            return category;
        }
    }
    // Catch-all: unknown non-zero exit, treat as recoverable in daemon mode.
    "RECOVERABLE"
}

/// `classification_is_transient` — the retry deny-list (#4501): exit-true
/// when a caller's retry loop SHOULD retry the same invocation.
///
/// Policy is a DENY-LIST: only categories whose fault cannot be cleared by
/// retrying are terminal (`SUCCESS`, `TIMEOUT`, `TOKEN_EXPIRED`,
/// `CWD_DELETED`, `MODEL_REFUSAL`, `FATAL`); everything else — including any
/// unrecognized category string — is retryable. Retry-by-default is the
/// fail-safe direction for unattended daemon children; see the shell
/// function's comment for the two incidents (#4255, #4501) that decided it.
#[must_use]
pub fn classification_is_transient(category: &str) -> bool {
    !matches!(
        category,
        "SUCCESS" | "TIMEOUT" | "TOKEN_EXPIRED" | "CWD_DELETED" | "MODEL_REFUSAL" | "FATAL"
    )
}

/// The provider selector's default chain, shared by the CLI: an explicit
/// `--provider` wins; then `$LOOM_RUNTIME` (the same env var
/// `spawn-worker.sh` uses for runtime dispatch); then `"claude"`. An EMPTY
/// value at either level is treated as unset, matching the shell's
/// `${3:-${LOOM_RUNTIME:-claude}}` parameter-expansion semantics.
#[must_use]
pub fn resolve_provider(explicit: Option<&str>) -> String {
    resolve_provider_with(explicit, std::env::var("LOOM_RUNTIME").ok().as_deref())
}

/// [`resolve_provider`] against an injected env value — the testable form.
#[must_use]
pub fn resolve_provider_with(explicit: Option<&str>, env: Option<&str>) -> String {
    explicit
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| env.filter(|p| !p.is_empty()).map(str::to_string))
        .unwrap_or_else(|| "claude".to_string())
}
