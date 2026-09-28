//! Curator points-estimate marker (`<!-- loom:points=<N> -->`, Issue #9056).
//!
//! Shared by two independent consumers so the extraction regex and closed
//! vocabulary are defined exactly once:
//!
//! - [`crate::sweep_registry::outcome_journal::points_signal`] — the
//!   sweep-outcome write-back comment's own best-effort forge read.
//! - `cli::points_marker_check` (binary crate) — the
//!   `require-complexity-marker.sh` validation helper. That check moved here
//!   (a `loom-daemon` subcommand) rather than staying inline shell because
//!   the `#7810` `shell-budget` CI gate ratchets the `contract` shell-line
//!   count down; new executable logic belongs in the daemon per
//!   `.loom/docs/shell-language-policy.md`.
//!
//! `pub`, not `pub(crate)`: `main.rs`'s CLI tree is a separate binary crate
//! of the same package and reaches this library crate only through its
//! `pub` surface (`loom_daemon::points_marker::…`).

use regex::Regex;
use std::sync::OnceLock;

/// Closed vocabulary the marker MUST be one of — mirrors
/// `crate::script_helpers::model_tiers::COMPLEXITY_TIERS`'s closed-enum
/// discipline: an out-of-vocabulary value is a curation defect, not a style
/// choice.
pub const POINTS_VALUES: &[&str] = &["1", "2", "3", "5", "8", "13"];

/// `<!-- loom:points=<N> -->`, anchored to the canonical HTML-comment form
/// exactly like `require-complexity-marker.sh`'s own
/// `<!--[[:space:]]*loom:complexity=...-->` pattern — so prose that merely
/// *discusses* the marker syntax cannot be mistaken for a real marker (the
/// same #4840 concern the complexity marker's own parser guards against).
fn points_marker_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"<!--\s*loom:points=([0-9]+)\s*-->").expect("static points-marker pattern")
    })
}

/// The raw captured digit string of the LAST `loom:points` marker in `body`,
/// regardless of whether it is in [`POINTS_VALUES`] — mirrors
/// `require-complexity-marker.sh`'s `grep -oE ... | tail -1` (the marker
/// nearest the end of the body wins when more than one is present). `None`
/// when no marker is present at all.
#[must_use]
pub fn extract_points_marker_raw(body: &str) -> Option<&str> {
    Some(
        points_marker_re()
            .captures_iter(body)
            .last()?
            .get(1)?
            .as_str(),
    )
}

/// [`extract_points_marker_raw`], additionally validated against
/// [`POINTS_VALUES`] — `None` for both "no marker" and "out-of-vocabulary
/// value", the same fold [`crate::script_helpers::sweep_experiment::extract_complexity_marker`]'s
/// caller applies to an absent vs. an invalid complexity tier.
#[must_use]
pub fn extract_points_marker(body: &str) -> Option<&str> {
    let value = extract_points_marker_raw(body)?;
    POINTS_VALUES.contains(&value).then_some(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_valid_marker() {
        let body = "Some issue body.\n\n<!-- loom:points=5 -->\n";
        assert_eq!(extract_points_marker(body), Some("5"));
        assert_eq!(extract_points_marker_raw(body), Some("5"));
    }

    #[test]
    fn raw_extraction_keeps_an_out_of_vocabulary_value_the_validated_one_drops() {
        let body = "Body.\n\n<!-- loom:points=21 -->\n";
        assert_eq!(extract_points_marker_raw(body), Some("21"));
        assert_eq!(extract_points_marker(body), None);
    }

    #[test]
    fn absent_marker_is_none_for_both() {
        assert_eq!(extract_points_marker("no marker here"), None);
        assert_eq!(extract_points_marker_raw("no marker here"), None);
    }

    #[test]
    fn takes_the_last_marker_when_several_are_present() {
        let body = "<!-- loom:points=1 -->\n\nDrifted.\n\n<!-- loom:points=8 -->\n";
        assert_eq!(extract_points_marker(body), Some("8"));
    }

    #[test]
    fn prose_mentioning_the_marker_syntax_does_not_block_the_real_one() {
        // A `<N>` placeholder has no digits to capture, so it simply never
        // matches the regex — the real marker later in the body is still
        // found, mirroring the #4840 fix for the complexity marker.
        let body = "Emit `<!-- loom:points=<N> -->` in the body.\n\n<!-- loom:points=3 -->\n";
        assert_eq!(extract_points_marker(body), Some("3"));
    }

    #[test]
    fn closed_vocabulary_matches_the_documented_set() {
        for v in ["1", "2", "3", "5", "8", "13"] {
            assert!(POINTS_VALUES.contains(&v));
        }
        assert!(!POINTS_VALUES.contains(&"21"));
        assert!(!POINTS_VALUES.contains(&"0"));
    }
}
