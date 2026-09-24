//! Turn the Codex CLI's own usage-limit refusal into a **reset horizon**
//! (issue #8539).
//!
//! # The gap this closes
//!
//! A dispatch against a walled Codex account dies with the CLI's own refusal,
//! and that refusal carries the one fact the account pool most wants:
//!
//! ```text
//! ERROR: You've hit your usage limit. Visit https://chatgpt.com/codex/settings/usage
//! to purchase more credits or try again at September 25, 2026 3:00 PM.
//! ```
//!
//! That refusal is **one physical line** on the wire — it is wrapped above only
//! to fit this comment's width. The distinction is load-bearing:
//! [`usage_limit_reset_in`] scans line by line and requires the needle phrase
//! and `try again at` on the *same* line, so a genuinely wrapped refusal would
//! parse to `None` (and degrade to the configured cooldown). The unwrapped form
//! is [`CAPTURED_USAGE_LIMIT_REFUSAL`], which is what the tests assert against.
//!
//! `classify-error.sh`'s codex table already classified that text as
//! `TOKEN_EXHAUSTED`, and both dispatch surfaces already turned that category
//! into an account-health hold — but the hold's deadline was always a blind
//! `now + LOOM_CODEX_EXHAUSTED_COOLDOWN_SECS` (5h default), because nothing
//! anywhere parsed `try again at <date>`. So `accounts check` reported no
//! `Resets at` for a walled account (#8539's third acceptance box), and the
//! hold expired on a schedule unrelated to the provider's actual reset —
//! early (re-dispatch bounces again) or late (a recovered account sits idle).
//!
//! # Why this parses the daemon-side log rather than extending the wire format
//!
//! The obvious alternative is to extract the horizon in `spawn-codex.sh`
//! beside the classification and ship it in a `v=3` `LOOM_TERMINAL_RESULT`
//! field. That was the original plan and it is not what happened, for two
//! reasons:
//!
//! 1. **Language policy.** Date parsing is new executable logic, and new
//!    executable logic goes into `loom-daemon`, not into shell
//!    (`.loom/docs/shell-language-policy.md`, ADR-0018). Both files it would
//!    have to grow — `lib/classify-error.sh` and `spawn-codex.sh` — are
//!    `contract` entries in the epic #7810 portable pool, so
//!    `loom-daemon shell-budget --check` refuses the growth by design and
//!    names this module's approach as the first-listed remedy.
//! 2. **No version skew.** A `v=3` record is silently dropped by any daemon
//!    older than the adapter that emitted it (the parser fails closed on an
//!    unknown arity), which would lose the *whole* terminal signal — the
//!    bad-mark included — on a mixed-version host. Parsing text the adapter
//!    already writes cannot regress a single existing path.
//!
//! Both consumers of the terminal record ([`crate::sweep_registry`] and
//! [`crate::role_runner`]) already read the child's whole retained log into a
//! string and scope their parse to their own dispatch region, so the horizon
//! costs them one extra call over text they are holding anyway.
//!
//! # "The agent's own words are not the provider's"
//!
//! The region this scans is a whole run's transcript: the codex CLI's stderr
//! *and* the agent's own stdout land in one log (`spawn-codex.sh` tees stderr
//! to its own stderr while stdout passes through). Exhaustion wording is
//! ordinary English an agent emits routinely — the hazard
//! [`crate::api_keys_pool::ingest`] documents as its guard 5 (#8521). Three
//! properties keep that hazard bounded here:
//!
//! 1. **This module never decides *whether* to hold an account.** That is the
//!    adapter's classification, arrived at from the CLI's stderr alone. This
//!    text decides only *when* an already-decided hold ends, so a transcript
//!    that merely quotes a refusal changes nothing at all.
//! 2. **The last match in the region wins** ([`usage_limit_reset_in`]). The
//!    CLI's fatal refusal is the last thing written before exit; anything the
//!    agent said is necessarily earlier.
//! 3. **Temporal policy lives downstream.** This module returns the instant it
//!    read and makes no judgement about it;
//!    `health::record_terminal_for_class_with_reset_at` refuses one that is in
//!    the past or implausibly far out and falls back to the configured
//!    cooldown. A bogus horizon therefore degrades to today's behaviour rather
//!    than inventing a hold.
//!
//! # Timezone: host-local, deliberately
//!
//! The CLI prints a wall-clock time with **no timezone** ("September 25, 2026
//! 3:00 PM"), so some assumption is unavoidable. It is read as host-local,
//! because the process that printed it and the daemon that reads it are the
//! same host, and the CLI is rendering for a human sitting at that host. A
//! local time that does not exist or is ambiguous (a DST transition) parses to
//! `None` rather than to a guess, which degrades to the configured cooldown.

use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};

use super::health::TerminalClassification;

/// The refusal wording, captured verbatim on the host in issue #8539 (a
/// headless `codex exec` inside an account's session container, every
/// registered account at its provider-side usage limit). Asserted by a test,
/// so an edit that breaks the shape breaks a test rather than silently
/// degrading the parser — the same discipline
/// [`crate::api_keys_pool::classify`]'s captures follow.
///
/// The date is the only part rewritten from the operator's report (which
/// carried a real account's reset instant).
pub const CAPTURED_USAGE_LIMIT_REFUSAL: &str = "ERROR: You've hit your usage limit. Visit \
     https://chatgpt.com/codex/settings/usage to purchase more credits or try again at September \
     25, 2026 3:00 PM.";

/// The refusal family this module reads a horizon out of — the same wording
/// `classify-error.sh`'s `_classify_error_codex` maps to `TOKEN_EXHAUSTED`,
/// restricted to the *usage-limit* rows (the credit-limit and API-error rows
/// carry no "try again at"). Matched case-insensitively.
const USAGE_LIMIT_NEEDLES: &[&str] = &[
    "hit your usage limit",
    "reached your usage limit",
    "usage limit reached",
];

/// The phrase introducing the horizon. Everything after it is parsed by
/// [`parse_reset_instant`].
const TRY_AGAIN_AT: &str = "try again at ";

/// How many whitespace-separated tokens the captured horizon occupies:
/// `September` `25,` `2026` `3:00` `PM`.
const HORIZON_TOKENS: usize = 5;

/// Parse the horizon out of the text **following** `try again at ` — the
/// captured form `<Month> <day>, <year> <h:mm AM/PM>`, read as host-local.
///
/// Only that one form is recognised. A relative horizon ("try again in 3
/// hours"), a bare time with no date ("try again at 3:00 PM" — today or
/// tomorrow is not knowable), or any other shape returns `None`, which leaves
/// the caller on its configured cooldown. That is the fail-safe direction: a
/// missing horizon costs the pre-#8539 behaviour, while a guessed one puts a
/// wrong deadline on a real account.
#[must_use]
pub fn parse_reset_instant(after_try_again_at: &str) -> Option<DateTime<Utc>> {
    let tokens: Vec<&str> = after_try_again_at.split_whitespace().collect();
    if tokens.len() < HORIZON_TOKENS {
        return None;
    }
    // Trailing sentence punctuation belongs to the prose, not to the date.
    let candidate = tokens[..HORIZON_TOKENS]
        .join(" ")
        .trim_end_matches(['.', ',', ';', ')', '"', '\''])
        .to_string();
    let naive = NaiveDateTime::parse_from_str(&candidate, "%B %d, %Y %I:%M %p").ok()?;
    // `.single()` on purpose: a local time that is ambiguous (the repeated hour
    // of a DST fall-back) or nonexistent (the skipped hour of a spring-forward)
    // is not a horizon this module may invent an answer for.
    Local
        .from_local_datetime(&naive)
        .single()
        .map(|local| local.with_timezone(&Utc))
}

/// The reset horizon named by the **last** usage-limit refusal in `region`, or
/// `None` when it carries none.
///
/// Last, not first: see the module docs — the CLI's fatal refusal is the final
/// thing written before exit, so anything an agent said that merely looks like
/// one is necessarily earlier in the region.
#[must_use]
pub fn usage_limit_reset_in(region: &str) -> Option<DateTime<Utc>> {
    region.lines().rev().find_map(|line| {
        // `to_ascii_lowercase` is length-preserving (non-ASCII bytes pass
        // through untouched), so byte offsets found in the lowered copy index
        // the original line correctly.
        let lowered = line.to_ascii_lowercase();
        if !USAGE_LIMIT_NEEDLES
            .iter()
            .any(|needle| lowered.contains(needle))
        {
            return None;
        }
        let at = lowered.rfind(TRY_AGAIN_AT)? + TRY_AGAIN_AT.len();
        parse_reset_instant(line.get(at..)?)
    })
}

/// [`usage_limit_reset_in`], scoped to the caller's own dispatch region — the
/// text from the **last** occurrence of `anchor` onward.
///
/// The same region discipline `parse_terminal_result_after` and
/// `api_keys_pool::ingest` already apply, and for the same reason: one sweep or
/// role tick must never be attributed a horizon printed by an earlier run
/// sharing the same log file.
#[must_use]
pub fn usage_limit_reset_after(contents: &str, anchor: &str) -> Option<DateTime<Utc>> {
    usage_limit_reset_in(&contents[contents.rfind(anchor)?..])
}

/// The horizon as a Unix timestamp, ready for
/// `health::record_terminal_for_class_with_reset_at`.
#[must_use]
pub fn usage_limit_reset_epoch_after(contents: &str, anchor: &str) -> Option<u64> {
    u64::try_from(usage_limit_reset_after(contents, anchor)?.timestamp()).ok()
}

/// The one entry point a dispatch surface should call: the reset horizon for
/// `category`, or `None` when this failure is not an exhaustion at all.
///
/// **The classification gate lives here, not at each call site.** It is the
/// property that makes module-docs point 1 true — the adapter's own
/// classification (derived from the CLI's stderr) decides *whether* an account
/// is held, and the transcript is consulted only to refine *when* that hold
/// ends. Both dispatch surfaces (`sweep_registry` and `role_runner`) call this
/// rather than the raw scanner so neither can drift from that rule.
#[must_use]
pub fn exhaustion_reset_horizon(
    contents: &str,
    anchor: &str,
    category: TerminalClassification,
) -> Option<u64> {
    matches!(
        category,
        TerminalClassification::TokenExhausted | TerminalClassification::ModelCreditsExhausted
    )
    .then(|| usage_limit_reset_epoch_after(contents, anchor))
    .flatten()
}

#[cfg(test)]
#[path = "codex_reset/tests.rs"]
mod tests;
