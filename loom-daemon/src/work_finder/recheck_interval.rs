//! The issue-declared re-check interval marker (Issue #6685):
//! `<!-- loom:recheck-interval=<value> -->` in an issue body.
//!
//! Moved out of `work_finder.rs` unchanged (#9244) to make room under that
//! file's size ratchet (`.loom/docs/file-size-policy.md`).

use std::time::Duration;

use super::WorkItem;

impl WorkItem {
    /// The issue's self-declared minimum re-check interval (Issue #6685),
    /// extracted from a `<!-- loom:recheck-interval=<value> -->` marker in
    /// [`Self::body`] — in the spirit of the Curator's
    /// `<!-- loom:complexity=<tier> -->` marker (see [`Self::complexity`]),
    /// but declaring a standing polling policy rather than a cost stratum.
    ///
    /// `<value>` is a bare duration: an integer optionally followed by a
    /// single unit suffix — `s` (seconds, the default when no suffix is
    /// given), `m` (minutes), `h` (hours), or `d` (days). E.g.
    /// `<!-- loom:recheck-interval=6h -->`. `None` when no body was fetched,
    /// no marker is present, or the value is empty/zero/malformed.
    #[must_use]
    pub fn recheck_interval(&self) -> Option<Duration> {
        self.body
            .as_deref()
            .and_then(extract_recheck_interval_marker)
            .and_then(parse_recheck_interval_value)
    }

    /// True when this item declares a [`Self::recheck_interval`] AND its
    /// [`Self::updated_at`] is still within that interval of `now` — i.e. the
    /// issue told the work-finder up front it does not need re-checking yet
    /// (Issue #6685).
    ///
    /// Deliberately independent of and orthogonal to
    /// [`WorkDispatcher::noop_cooldown`] (Issue #6670): that mechanism is
    /// dispatcher-armed, only after an explicit self-report from a completed
    /// sweep pass ("no actionable delta THIS time"); this one is
    /// issue-declared, in effect from the moment the marker is added,
    /// independent of any sweep having run at all. An issue can be in neither,
    /// either, or both cooldowns at once — this check never reads
    /// `noop_cooldown` state and vice versa.
    ///
    /// `false` whenever either half is missing (no marker, or `updated_at`
    /// absent/unparseable) — a byte-for-byte no-op for every issue that does
    /// not carry the marker, which is every issue today.
    #[must_use]
    pub fn is_within_recheck_interval(&self, now: chrono::DateTime<chrono::Utc>) -> bool {
        let Some(interval) = self.recheck_interval() else {
            return false;
        };
        let Some(updated_at) = self.updated_at.as_deref() else {
            return false;
        };
        let Ok(updated_at) = chrono::DateTime::parse_from_rfc3339(updated_at) else {
            return false;
        };
        let Ok(interval) = chrono::Duration::from_std(interval) else {
            return false;
        };
        now.signed_duration_since(updated_at.with_timezone(&chrono::Utc)) < interval
    }
}

/// The marker key inside the `<!-- ... -->` comment declaring a tracker
/// issue's self-declared minimum re-check interval (Issue #6685). Mirrors
/// [`crate::script_helpers::sweep_experiment`]'s `loom:complexity=` marker
/// convention but lives here (rather than in that module) since it is a
/// work-finder-only concept, never read by dispatch-time complexity
/// stratification.
const RECHECK_INTERVAL_MARKER_KEY: &str = "loom:recheck-interval=";

/// Extract the LAST well-formed `<!-- loom:recheck-interval=<value> -->`
/// value from `body`, if any (Issue #6685).
///
/// Line-oriented and last-match-wins, mirroring
/// [`crate::script_helpers::sweep_experiment::extract_complexity_marker`]'s
/// contract — the canonical marker placement is at the end of the body, and a
/// marker split across a newline is not recognized (grep-line semantics).
/// Simpler than that function's non-overlapping multi-match-per-line scan:
/// this marker is expected to appear at most once, so a single `find` per
/// line is sufficient and avoids duplicating the general-purpose scanner for
/// a syntax with a different value vocabulary (a duration, not a closed tier
/// enum).
fn extract_recheck_interval_marker(body: &str) -> Option<&str> {
    body.lines().rev().find_map(|line| {
        let idx = line.find(RECHECK_INTERVAL_MARKER_KEY)?;
        // Anchor to the canonical `<!-- ... -->` comment form so prose that
        // merely mentions the marker key (e.g. this very doc comment, if it
        // ever ends up quoted in an issue body) does not false-fire.
        if !line[..idx].trim_end().ends_with("<!--") {
            return None;
        }
        let after = &line[idx + RECHECK_INTERVAL_MARKER_KEY.len()..];
        let end = after.find("-->")?;
        let value = after[..end].trim();
        if value.is_empty() {
            None
        } else {
            Some(value)
        }
    })
}

/// Parse a bare duration value (`<N>[s|m|h|d]`, e.g. `6h`, `45m`, `2d`, or a
/// plain integer defaulting to seconds) into a [`Duration`] (Issue #6685).
/// `None` on empty, zero, non-numeric, an unrecognized unit suffix, or
/// overflow.
fn parse_recheck_interval_value(value: &str) -> Option<Duration> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let split_at = value
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(value.len());
    let (num_part, unit) = value.split_at(split_at);
    let num: u64 = num_part.parse().ok()?;
    if num == 0 {
        return None;
    }
    let multiplier: u64 = match unit.trim() {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return None,
    };
    num.checked_mul(multiplier).map(Duration::from_secs)
}
