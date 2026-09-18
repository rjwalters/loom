//! `claude-wrapper.sh`'s retry/rotation classifiers (epic #7810, #8037).
//!
//! Six predicates — 79 code lines of the wrapper's 1,675 — decide whether a
//! sweep **retries**, **rotates to another account**, **marks a credential
//! dead**, or **dies**, and how long it waits between attempts. Everything else
//! in that script is preflight, MCP repair, output monitoring and process
//! plumbing, and stays where it is.
//!
//! A seventh joined them in #8138: [`model_class_marker`], which shapes the
//! remedy [`is_account_exhaustion`] calls for rather than choosing between
//! remedies. It landed here rather than beside the `.bad_tokens` writer it
//! feeds because the question it answers is the same kind this module already
//! answers — what does this failure's *text* prove — and the #4501 ceiling
//! phrasing it turns on is already in this file, in [`EXHAUSTION_FALLBACK`].
//!
//! `defaults/scripts/tests/test-claude-wrapper-retry.sh` (#8032) pinned all six
//! against the shell *before* this port and runs unchanged against it. That
//! suite, not review, is the equivalence evidence.
//!
//! # Every classifier has two modes, and both are ported
//!
//! `claude-wrapper.sh` sources `lib/classify-error.sh` **optionally**
//! (`if [[ -f ... ]]`). With the library present the classifiers delegate to
//! `classify_error`; without it each falls back to its own hand-rolled regex —
//! a second implementation that had no coverage at all before #8032. Porting
//! only the library path would have silently changed behaviour on exactly the
//! hosts with the least evidence, so both paths live here:
//! [`Input::classification`] is `Some` in library mode and `None` in degraded
//! mode, and the degraded arm carries the regexes verbatim.
//!
//! # Why the deny-list is NOT reimplemented here
//!
//! [`Input::classification_is_transient`] is supplied by the caller rather than
//! recomputed. `classification_is_transient` lives in `lib/classify-error.sh`
//! and is, per #4501, *the* single source of truth for every retry decision in
//! the fleet — it is not one of the six functions being ported, and other
//! scripts still source it. A second copy of that deny-list in Rust would
//! recreate the precise defect #4501 fixed: two independent verdict systems
//! that can disagree about the same failure, which on a live host printed
//!
//! ```text
//! [ERROR] Non-transient error detected - not retrying
//! [ERROR] exit_code=1 classification=RECOVERABLE
//! ```
//!
//! So the boundary is: the shell says what category this failure is and whether
//! that category is retryable; this module owns what the wrapper *does* with
//! that answer — which is the part that was duplicated, untested, and only
//! reachable through the script.

pub mod cli;

#[cfg(test)]
mod tests;

use crate::tokens_pool::bad_tokens::MODEL_CLASS_MARKER_PREFIX;
use regex::Regex;
use std::sync::LazyLock;

/// The wrapper's own sentinel, emitted by its output/startup monitors when the
/// CLI showed an interactive usage/plan-limit modal or the 100%-weekly banner.
///
/// Not a CLI phrasing, which is why both [`is_transient`] and
/// [`is_account_exhaustion`] check it *before* consulting any classification.
pub const RATE_LIMIT_ABORT: &str = "RATE_LIMIT_ABORT";

/// Reported when no classification was available — the degraded path's category.
pub const UNCLASSIFIED: &str = "UNCLASSIFIED";

/// What the caller knows about one failed invocation.
///
/// `classification` being `None` is the degraded path and is a *state*, not a
/// missing value: it means `lib/classify-error.sh` was not sourced, which each
/// predicate answers with its own fallback regex.
#[derive(Debug, Clone, Default)]
pub struct Input<'a> {
    /// The child's captured stdout+stderr.
    pub output: &'a str,
    /// The child's exit code.
    pub exit_code: i32,
    /// `classify_error`'s category, when the library was available.
    pub classification: Option<&'a str>,
    /// `classification_is_transient`'s verdict for that category. Meaningful
    /// only alongside `classification`; see the module doc for why this is
    /// passed in rather than recomputed.
    pub classification_is_transient: bool,
}

/// [`is_transient`]'s answer: the retry verdict plus the category it was
/// derived from.
///
/// The two travel together because the wrapper logs the category it *acted on*
/// (`_LAST_ERROR_CLASSIFICATION`, #4501). Returning the verdict alone would let
/// the printed classification drift from the decision again — notably for
/// [`RATE_LIMIT_ABORT`], which `classify_error` would report as the generic
/// `RECOVERABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransientVerdict {
    /// Whether the retry loop should try the same invocation again.
    pub retry: bool,
    /// The category the verdict was derived from.
    pub classification: String,
}

/// Does `output` contain a line matching `re`?
///
/// Line-by-line on purpose: the shell spells every one of these as
/// `echo "$output" | grep -qE ...`, and `grep` matches within a line. A
/// whole-string search would let `.` / `[[:space:]]` straddle a newline and
/// match text the shell never would — a silent widening of every pattern below.
fn any_line_matches(output: &str, re: &Regex) -> bool {
    output.lines().any(|line| re.is_match(line))
}

/// `is_transient_error` — retry, or give up.
///
/// Order is load-bearing:
/// 1. [`RATE_LIMIT_ABORT`] is **not** transient. The CLI hit a usage/plan limit
///    and is showing an interactive prompt, so retrying hits the same limit.
///    Rotation consumes this sentinel first; reaching here means rotation was
///    capped or the pool was empty.
/// 2. No classification available → **retry-by-default** on any non-zero exit,
///    bounded by the caller's `MAX_RETRIES`. This is the fail-safe direction of
///    the same deny-list policy, not an oversight: an allow-list of known
///    transient phrasings has twice turned a new CLI wording into an instant
///    permanent death (#4255, #4501).
/// 3. Otherwise the library's verdict for the category stands.
#[must_use]
pub fn is_transient(input: &Input<'_>) -> TransientVerdict {
    if input.output.contains(RATE_LIMIT_ABORT) {
        return TransientVerdict {
            retry: false,
            classification: RATE_LIMIT_ABORT.to_string(),
        };
    }

    match input.classification {
        None => TransientVerdict {
            retry: input.exit_code != 0,
            classification: UNCLASSIFIED.to_string(),
        },
        Some(category) => TransientVerdict {
            retry: input.classification_is_transient,
            classification: category.to_string(),
        },
    }
}

/// The degraded-path exhaustion regex, verbatim from `claude-wrapper.sh`.
///
/// Kept in lockstep with `lib/classify-error.sh`'s `TOKEN_EXHAUSTED` +
/// `MODEL_CREDITS_EXHAUSTED` patterns (#4501's per-model ceiling and #5687's
/// credits family are both here).
static EXHAUSTION_FALLBACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)",
        r"hit your ([^[:space:]]+[[:space:]]+){0,3}limit",
        r"|hit\.your\.limit",
        r"|monthly usage limit",
        r"|out of extra usage",
        r"|reached your ([^[:space:]]+[[:space:]]+){0,3}limit",
        r"|(ran |run )?out of (usage |extra |plan )?credits",
        r"|no (usage |extra |plan )?credits (remaining|left)",
        r"|insufficient (usage |plan )?credits",
    ))
    .expect("exhaustion fallback pattern is a compile-time constant")
});

/// The degraded-path auth-death regex, verbatim from `claude-wrapper.sh`.
///
/// Kept in lockstep with `lib/classify-error.sh`'s `TOKEN_EXPIRED` pattern,
/// including #6614's JSON-envelope and revoked-token phrasings.
static AUTH_DEAD_FALLBACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)",
        r"401[^a-z]*authentication_error",
        "|\"type\"[[:space:]]*:[[:space:]]*\"?authentication_error",
        r"|token (has been|was) revoked",
        r"|invalid bearer token",
        r"|OAuth token has expired",
        r"|token has expired",
    ))
    .expect("auth-dead fallback pattern is a compile-time constant")
});

/// The degraded-path concurrent-session regex, verbatim from
/// `claude-wrapper.sh`.
static SESSION_LIMIT_FALLBACK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?i)",
        r"concurrent (session|sessions|request)",
        r"|maximum number of concurrent",
        r"|too many concurrent",
        r"|simultaneous session",
        r"|another session is (already )?(active|running)",
    ))
    .expect("session-limit fallback pattern is a compile-time constant")
});

/// The #4501 per-model ceiling phrase, verbatim from
/// `lib/classify-error.sh`'s `loom_model_class_marker` (#8058).
///
/// `{1,3}`, not [`EXHAUSTION_FALLBACK`]'s `{0,3}`: a bare "reached your limit"
/// names no class at all, so there is nothing for a class-scoped mark to be
/// scoped *to*.
static CEILING_PHRASE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)reached your ([^[:space:]]+[[:space:]]+){1,3}limit")
        .expect("ceiling phrase is a compile-time constant")
});

/// Words that make a [`CEILING_PHRASE`] match account-wide rather than
/// per-class, verbatim from `loom_model_class_marker` (#8058).
///
/// "You've reached your weekly limit" matches [`CEILING_PHRASE`] just as
/// "You've reached your Fable 5 limit" does, and is emphatically NOT scoped to
/// one model class. This list is what tells them apart.
static ACCOUNT_WIDE_QUALIFIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(weekly|monthly|daily|hourly|session|usage|plan|spend|account|api)\b")
        .expect("account-wide qualifier list is a compile-time constant")
});

/// `is_mcp_error`'s patterns, verbatim from `claude-wrapper.sh` — four
/// `grep -qi` passes, joined into one alternation because `grep -q` over a list
/// is exactly an alternation.
static MCP_PATTERNS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)MCP server failed|MCP.*failed|plugins failed|plugin.*failed to install")
        .expect("MCP pattern is a compile-time constant")
});

/// `is_account_exhaustion` — rotate to another account.
///
/// [`RATE_LIMIT_ABORT`] is exhaustion regardless of what any classifier makes
/// of the text: it is the mirror of that sentinel not being transient. Rotating
/// is the only response that can succeed, because the limit belongs to the
/// account, not to the attempt.
///
/// `MODEL_CREDITS_EXHAUSTED` (#5687) is accepted alongside `TOKEN_EXHAUSTED`.
/// It is a distinct category so the in-session sweep orchestrator can name the
/// signature it downgrades models on, but on this subprocess-supervision path
/// there is no per-call model knob, so the correct response is byte-identical:
/// rotate, and mark this account exhausted.
#[must_use]
pub fn is_account_exhaustion(input: &Input<'_>) -> bool {
    if input.output.contains(RATE_LIMIT_ABORT) {
        return true;
    }
    match input.classification {
        Some(category) => matches!(category, "TOKEN_EXHAUSTED" | "MODEL_CREDITS_EXHAUSTED"),
        // The exit code is conjoined with the regex, never dropped: a limit
        // phrase on a ZERO exit is a sweep quoting its own logs, not an
        // exhausted account.
        None => input.exit_code != 0 && any_line_matches(input.output, &EXHAUSTION_FALLBACK),
    }
}

/// `loom_model_class_marker` — how NARROW the `.bad_tokens` entry that
/// [`is_account_exhaustion`] just called for is allowed to be (#8058, ported
/// out of `lib/classify-error.sh` by #8138).
///
/// Returns the ` [model-class:<model>]` suffix to append to the mark's reason
/// when the death is provably scoped to ONE model class, and `None` otherwise.
/// This sits in the same place as every other function in this module — the
/// shell says *what category* the failure is, this owns *what the wrapper does*
/// about it — and it is the remedy-shaping half of `is_account_exhaustion`:
/// that predicate decides to rotate and mark, this decides how much of the
/// account the mark takes out.
///
/// Anthropic's subscription limits are not model-blind: a Max plan carries a
/// per-model-class ceiling alongside the all-models weekly limit. Marking the
/// whole account bad for one of those is what let a mixed-model fleet's Opus
/// arm starve its Sonnet arm out of a shared pool.
///
/// # Deliberately conservative
///
/// The two error directions are not symmetric. A class-scoped mark is NARROWER
/// than an account-wide one, so mis-scoping a genuinely account-wide death
/// leaves a dead account in rotation for every other class; failing to scope a
/// genuinely per-class death merely reproduces the pre-#8058 behaviour.
/// Widening is therefore the safe default, and only two signatures qualify:
///
/// 1. `MODEL_CREDITS_EXHAUSTED` (#5687, "You're out of usage credits") — the
///    category is per-tier by definition, per its own documentation. Available
///    only on the library path; a caller with no `classify_error` (the
///    `classification: None` degraded arm) falls through to the regex, exactly
///    as the shell's `declare -F classify_error` guard did.
/// 2. A [`CEILING_PHRASE`] match (#4501, "You've reached your Fable 5 limit")
///    whose own words are not an [`ACCOUNT_WIDE_QUALIFIER`].
///
/// The class always comes from `model` — the resolved model in flight — not
/// from the error text: a ceiling kill means "the class we were running ran
/// out", which is a property of the dispatch, not of the message. The raw value
/// is emitted verbatim; [`crate::tokens_pool::bad_tokens`] normalizes it through
/// `model_tiers::task_alias_of` on read, and an unrecognizable marker reads as
/// class-less (blocks everything) rather than as blocking nothing.
///
/// The one deliberate divergence from the shell: a whitespace-only `model` is
/// treated as absent rather than written into the marker. The shell's `[[ -n
/// ]]` would have emitted `[model-class: ]`, which
/// [`crate::tokens_pool::bad_tokens::scoped_reason`] already treats as no class
/// on the write side — so this only removes a way to produce a marker that
/// means nothing.
#[must_use]
pub fn model_class_marker(input: &Input<'_>, model: &str) -> Option<String> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    if input.classification != Some("MODEL_CREDITS_EXHAUSTED") {
        // `grep -m1 -ioE` semantics: stop at the FIRST line carrying a match,
        // then judge every match on that line. A qualifier anywhere among them
        // widens the mark, because the conservative direction is account-wide.
        let line = input.output.lines().find(|l| CEILING_PHRASE.is_match(l))?;
        if CEILING_PHRASE
            .find_iter(line)
            .any(|m| ACCOUNT_WIDE_QUALIFIER.is_match(m.as_str()))
        {
            return None;
        }
    }
    Some(format!(" {MODEL_CLASS_MARKER_PREFIX}{model}]"))
}

/// `is_account_auth_dead` — rotate **and** mark the credential dead.
///
/// A different failure class from [`is_account_exhaustion`] (#6030): an
/// exhausted account recovers on its own when its quota window resets, an
/// auth-dead one fails every dispatch forever until a human re-authenticates
/// it. Collapsing the two either burns healthy accounts on a cooldown that will
/// never help, or keeps re-selecting a credential that cannot work.
#[must_use]
pub fn is_account_auth_dead(input: &Input<'_>) -> bool {
    match input.classification {
        Some(category) => category == "TOKEN_EXPIRED",
        None => input.exit_code != 0 && any_line_matches(input.output, &AUTH_DEAD_FALLBACK),
    }
}

/// `is_account_session_limit` — re-select a sibling account, mark nothing bad.
///
/// A capacity signal from per-token session stacking (#3947): the account is
/// healthy, it just cannot start another *simultaneous* session right now.
/// Poisoning `.bad_tokens` for it would shrink the healthy pool over a
/// condition that clears in minutes.
#[must_use]
pub fn is_account_session_limit(input: &Input<'_>) -> bool {
    match input.classification {
        Some(category) => category == "SESSION_LIMIT",
        None => input.exit_code != 0 && any_line_matches(input.output, &SESSION_LIMIT_FALLBACK),
    }
}

/// `is_mcp_error` — attempt an MCP rebuild, and map an exhausted-retry exit to
/// code 7 so the caller can recognise an MCP failure (#2746).
///
/// **Ignores the exit code by construction.** It has no `classify_error` path
/// and never had an exit-code conjunction, unlike the three account predicates
/// above. `input.exit_code` is deliberately unread here.
#[must_use]
pub fn is_mcp_error(output: &str) -> bool {
    any_line_matches(output, &MCP_PATTERNS)
}

/// `calculate_wait_time`'s inputs — the wrapper's `INITIAL_WAIT`, `MULTIPLIER`
/// and `MAX_WAIT` globals (`LOOM_INITIAL_WAIT` / `LOOM_BACKOFF_MULTIPLIER` /
/// `LOOM_MAX_WAIT`).
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub initial_wait: i64,
    pub multiplier: i64,
    pub max_wait: i64,
}

/// `calculate_wait_time` — `INITIAL_WAIT * MULTIPLIER^(attempt-1)`, capped at
/// `MAX_WAIT`.
///
/// With the fleet defaults (60s, ×2, 1800s ceiling) that is 60, 120, 240, 480,
/// 960, 1800, 1800, … — the curve that decides how hard a rate-limited fleet
/// hammers the API, which is why it is pinned to the second rather than
/// "improved".
///
/// The one deliberate divergence: overflow saturates instead of wrapping. Bash
/// arithmetic is a wrapping `i64`, so a large enough `attempt` makes the shell
/// produce a *negative* wait that then compares below `MAX_WAIT` and is handed
/// to `sleep`. Saturating pins that case at the ceiling. It is unreachable in
/// practice (`attempt` is bounded by `MAX_RETRIES`), and the alternative is
/// reproducing an arithmetic bug for its own sake. An `attempt` below 1 — which
/// the shell answers with a raw `exponent less than 0` arithmetic error — lands
/// in the same place, at the ceiling: nonsense in, the most conservative wait
/// out.
#[must_use]
pub fn calculate_wait_time(attempt: i64, backoff: &Backoff) -> i64 {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    let wait = backoff
        .multiplier
        .checked_pow(exponent)
        .and_then(|factor| backoff.initial_wait.checked_mul(factor))
        .unwrap_or(i64::MAX);

    if wait > backoff.max_wait {
        backoff.max_wait
    } else {
        wait
    }
}
