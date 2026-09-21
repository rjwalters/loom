//! Harness-output exhaustion / rate-limit classifier for API-key providers
//! (issue #8401), the API-key analogue of the per-provider pattern tables in
//! `.loom/scripts/lib/classify-error.sh` (`spawn-codex.sh`'s classifier table,
//! built from observed CLI output).
//!
//! # Placeholder pattern table — needs a live-run capture
//!
//! The issue asks for exact strings "captured from a live run" the way the
//! Codex classifier table was built from observed 0.146.0 output. No live
//! Z.ai coding-plan exhaustion has been captured as part of this change: the
//! patterns below are transcribed from Zhipu's/Z.ai's publicly documented API
//! error shapes (HTTP 429, and the `1113`/"insufficient balance" code
//! documented for the coding-plan endpoint) so the mechanism is real and
//! independently testable, but they are **unverified against live
//! OpenCode/Pi output**. Capturing the exact strings from a real exhausted
//! run and refining this table is tracked as a follow-up (#8424) — until
//! then, treat this classifier as best-effort, not authoritative, and never
//! the sole signal an operator relies on to distinguish "exhausted" from
//! "some other failure".

/// What a harness's error text was recognised as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Classification {
    /// The provider-side allowance for this billing period is used up — no
    /// point retrying before the account's own reset instant.
    Exhausted,
    /// A transient concurrency/rate ceiling — worth a short cooldown, not a
    /// long one.
    RateLimited,
}

impl Classification {
    /// A conservative default cooldown when the caller has no better
    /// provider-reported reset instant to pass to
    /// [`super::bad_marks::mark_bad`] instead.
    #[must_use]
    pub fn default_cooldown_secs(self) -> u64 {
        match self {
            // 6h: long enough to stay out of a tight retry loop, short enough
            // that a stale mark self-heals inside a day even with no operator
            // follow-up.
            Self::Exhausted => 6 * 3600,
            Self::RateLimited => 60,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Exhausted => "exhausted",
            Self::RateLimited => "rate-limited",
        }
    }
}

const EXHAUSTED_PATTERNS: &[&str] = &[
    "insufficient balance",
    "insufficient_quota",
    "quota exceeded",
    "coding plan quota",
    // Zhipu API's documented error code for an exhausted balance/allowance.
    "\"code\":\"1113\"",
];

const RATE_LIMITED_PATTERNS: &[&str] = &[
    "rate limit",
    "rate_limit",
    "too many requests",
    "concurrency limit",
];

/// Words that make a nearby `429` an HTTP status rather than a line number, a
/// token count (`14290 tokens`) or a port. A bare `"429"` substring matched all
/// of those.
const STATUS_CONTEXT: &[&str] = &["http", "status", "code", "error"];

/// `true` when `lowered` holds `429` as a standalone number — not part of a
/// longer word or number such as `14290`, `1.429` or `429,000` — with one of
/// [`STATUS_CONTEXT`] shortly before it on the same line.
fn mentions_http_429(lowered: &str) -> bool {
    let bytes = lowered.as_bytes();
    let byte = |i: Option<usize>| i.and_then(|i| bytes.get(i)).copied();
    let digit = |b: Option<u8>| b.is_some_and(|b| b.is_ascii_digit());
    let separator = |b: Option<u8>| matches!(b, Some(b'.' | b','));
    lowered.match_indices("429").any(|(at, _)| {
        let (prev, prev2) = (byte(at.checked_sub(1)), byte(at.checked_sub(2)));
        let (next, next2) = (byte(Some(at + 3)), byte(Some(at + 4)));
        let joined_before =
            prev.is_some_and(|b| b.is_ascii_alphanumeric()) || (separator(prev) && digit(prev2));
        let joined_after =
            next.is_some_and(|b| b.is_ascii_alphanumeric()) || (separator(next) && digit(next2));
        // `get`, not slicing at a computed offset: `at - 24` need not be a
        // char boundary in non-ASCII harness output.
        let window = (at.saturating_sub(24)..=at)
            .find_map(|start| lowered.get(start..at))
            .unwrap_or("");
        let same_line = window.rsplit('\n').next().unwrap_or("");
        !joined_before
            && !joined_after
            && STATUS_CONTEXT.iter().any(|word| same_line.contains(word))
    })
}

/// Best-effort classification of a harness's combined stdout+stderr. Returns
/// `None` when nothing recognisable matched — the ordinary "this failure is
/// something else entirely" case, which must never be treated as exhaustion.
///
/// `_exit_code` is accepted but not consulted: a process exit status is 0-255,
/// so an HTTP status can never arrive through it (an earlier `== 429` check was
/// unreachable). The parameter stays so #8424 can key on a harness-specific
/// exit code without changing every caller.
#[must_use]
pub fn classify(output: &str, _exit_code: i32) -> Option<Classification> {
    let lowered = output.to_ascii_lowercase();
    if EXHAUSTED_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
    {
        return Some(Classification::Exhausted);
    }
    if RATE_LIMITED_PATTERNS
        .iter()
        .any(|pattern| lowered.contains(pattern))
        || mentions_http_429(&lowered)
    {
        return Some(Classification::RateLimited);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_the_documented_exhaustion_and_rate_limit_shapes() {
        assert_eq!(
            classify("Error: insufficient balance for this account", 1),
            Some(Classification::Exhausted)
        );
        assert_eq!(
            classify("{\"error\":{\"code\":\"1113\",\"message\":\"...\"}}", 1),
            Some(Classification::Exhausted)
        );
        assert_eq!(classify("HTTP 429 Too Many Requests", 1), Some(Classification::RateLimited));
        assert_eq!(classify("connection reset by peer", 1), None);
    }

    /// Judge nit (#8428): a bare `"429"` substring matched any output that
    /// merely contained those digits.
    #[test]
    fn a_429_counts_only_as_an_http_status() {
        for rate_limited in [
            "HTTP 429",
            "request failed with status code 429",
            "{\"error\":{\"code\":429}}",
            "Error: 429",
            "the provider answered with status 429.",
            "upstream returned HTTP/1.1 429\nretrying",
        ] {
            assert_eq!(
                classify(rate_limited, 1),
                Some(Classification::RateLimited),
                "{rate_limited:?}"
            );
        }
        for unrelated in [
            "panicked at src/main.rs:429:13",
            "used 14290 tokens",
            "error: listening on port 4290",
            "wrote 429 lines",
            "status ok\n429 files changed",
            "error code 1429",
            "error: latency 1.429s",
            "error: 429,000 rows",
            "ünïcödé ünïcödé ünïcödé 429",
        ] {
            assert_eq!(classify(unrelated, 1), None, "{unrelated:?}");
        }
    }

    #[test]
    fn is_case_insensitive_and_never_panics_on_empty_output() {
        assert_eq!(classify("", 0), None);
        assert_eq!(classify("INSUFFICIENT BALANCE", 1), Some(Classification::Exhausted));
    }

    #[test]
    fn default_cooldowns_are_ordered_rate_limit_shorter_than_exhaustion() {
        assert!(
            Classification::RateLimited.default_cooldown_secs()
                < Classification::Exhausted.default_cooldown_secs()
        );
    }
}
