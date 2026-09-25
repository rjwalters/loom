//! The status-severity ordering every provider's ranking shares.
//!
//! Extracted from [`super::check`] when issue #8539 added the `"unknown"`
//! rung: `check.rs` is over the file-size ratchet, and
//! `.loom/docs/file-size-policy.md` names "put the new code in a new sibling
//! module and leave the dispatch behind" as the intended path rather than
//! growing a known-large file by one more arm. `check::status_rank` stays a
//! valid path (it re-exports this), so no caller moved.

/// Ranking rank for each status (lower sorts first). Mirrors `_STATUS_RANK`.
///
/// `"unsupported"` (design D6a, issue #5608) ranks worse than `"skipped"` —
/// defense-in-depth for any sort path that sees it, even though the primary
/// mechanism keeping it out of the selector's view is that
/// `check::format_ranking_lines` omits it from `.ranking` entirely.
///
/// `"unknown"` (issue #8539, produced only by
/// [`super::codex_check::assess_account`]) sits between `available` and
/// `rate_limited`: it must never outrank an account with a known-good reading
/// (that is the ranking half of #8539's first acceptance criterion), yet it is
/// not itself a refusal, so it still outranks every *known* refusal. The
/// absolute numbers are a private sort key — nothing persists them — so
/// inserting a rung only changes the relative order it was inserted to change.
#[must_use]
pub fn status_rank(status: &str) -> i32 {
    match status {
        "available" => 0,
        "unknown" => 1,
        "rate_limited" => 2,
        "exhausted" => 3,
        "blocked" => 4,
        "error" => 5,
        "skipped" => 6,
        "unsupported" => 7,
        _ => 99,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pre-#8539 ordering, unchanged: every rung still sorts exactly where
    /// it did relative to every other, so inserting `unknown` moved nothing.
    #[test]
    fn the_severity_order_is_stable_across_the_inserted_rung() {
        let order = [
            "available",
            "unknown",
            "rate_limited",
            "exhausted",
            "blocked",
            "error",
            "skipped",
            "unsupported",
        ];
        for pair in order.windows(2) {
            assert!(
                status_rank(pair[0]) < status_rank(pair[1]),
                "{} must sort ahead of {}",
                pair[0],
                pair[1]
            );
        }
        assert_eq!(status_rank("something-new"), 99);
    }
}
