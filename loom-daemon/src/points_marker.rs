//! Curator points-estimate marker (`<!-- loom:points=<N> -->`, Issue #9056,
//! hardened by #11663 / `docs/design/issue-sizing.md` §5a–§5b).
//!
//! The one parser for the marker syntax. Its consumers:
//!
//! - `cli::curation_complete` (binary crate) — the points gate. It resolves
//!   the winning source across the issue body and its trusted comments with
//!   [`resolve_points`] and refuses the curation on any [`PointsError`].
//! - `cli::points_marker_check` (binary crate) — `CheckPointsMarker`, the
//!   same check over one stdin body.
//! - [`crate::sweep_registry::outcome_journal::points_signal`] — the
//!   sweep-outcome write-back comment's best-effort body read.
//!
//! # What counts as a marker
//!
//! The discipline of `telemetry::kinds::issue_estimate::parse`: a marker is a
//! **standalone line** outside fenced (```` ``` ````/`~~~`) or indented code.
//! Inline backticks, prose, fences and indented blocks never count, so text
//! that quotes the syntax cannot be mistaken for a real marker (#4840). A
//! standalone `<!-- loom:points…` line that is not exactly
//! `<!-- loom:points=<digits> -->` is **malformed** and reported, never
//! skipped. A standalone `<!-- loom:complexity=… -->` line is detected so the
//! gate can reject the retired tier marker (rjwalters/repo#623).
//!
//! `pub`, not `pub(crate)`: `main.rs`'s CLI tree is a separate binary crate
//! of the same package and reaches this library crate only through its
//! `pub` surface (`loom_daemon::points_marker::…`).

use regex::Regex;
use std::sync::OnceLock;

/// Closed vocabulary the marker MUST be one of: an out-of-vocabulary value
/// is a curation defect, not a style choice.
pub const POINTS_VALUES: &[&str] = &["1", "2", "3", "5", "8", "13"];

fn points_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^<!--\s*loom:points=([0-9]+)\s*-->$").expect("static points-marker pattern")
    })
}

/// The marker lines found in one source (the body, or one comment).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceMarkers {
    /// Well-formed `<!-- loom:points=<digits> -->` values, in order (not yet
    /// checked against [`POINTS_VALUES`]).
    pub points: Vec<String>,
    /// Standalone `<!-- loom:points…` lines that are not well formed.
    pub malformed: Vec<String>,
    /// Standalone `<!-- loom:complexity…` lines (the retired tier marker).
    pub complexity: Vec<String>,
}

impl SourceMarkers {
    /// Whether this source carries any points or complexity marker line.
    #[must_use]
    pub fn any(&self) -> bool {
        !(self.points.is_empty() && self.malformed.is_empty() && self.complexity.is_empty())
    }

    /// The single valid points value of this source, or why there is none.
    /// A retired tier marker in the same source is a refusal.
    ///
    /// # Errors
    ///
    /// The [`PointsError`] the gate reports.
    pub fn validate(&self) -> Result<u32, PointsError> {
        if let Some(line) = self.complexity.first() {
            return Err(PointsError::RetiredTier(line.clone()));
        }
        self.validate_points()
    }

    /// [`Self::validate`] without the retired-tier rejection.
    fn validate_points(&self) -> Result<u32, PointsError> {
        if let Some(line) = self.malformed.first() {
            return Err(PointsError::Malformed(line.clone()));
        }
        let mut values: Vec<&str> = self.points.iter().map(String::as_str).collect();
        values.sort_unstable();
        values.dedup();
        match values.as_slice() {
            [] => Err(PointsError::Missing),
            [one] if POINTS_VALUES.contains(one) => one
                .parse()
                .map_err(|_| PointsError::OutOfVocabulary((*one).to_string())),
            [one] => Err(PointsError::OutOfVocabulary((*one).to_string())),
            many => Err(PointsError::Conflict(many.iter().map(|v| (*v).to_string()).collect())),
        }
    }
}

/// Scan `text` for standalone marker lines outside fenced or indented code.
#[must_use]
pub fn scan(text: &str) -> SourceMarkers {
    let mut fence: Option<(char, usize)> = None;
    let mut out = SourceMarkers::default();
    for line in text.lines() {
        let trimmed = line.trim();
        let leading = trimmed.chars().next().unwrap_or(' ');
        let count = trimmed.chars().take_while(|c| *c == leading).count();
        if matches!(leading, '`' | '~') && count >= 3 {
            match fence {
                None => fence = Some((leading, count)),
                Some((ch, n)) if ch == leading && count >= n => fence = None,
                _ => {}
            }
            continue;
        }
        if fence.is_some() || line.starts_with("    ") || line.starts_with('\t') {
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("<!--") else {
            continue;
        };
        let rest = rest.trim_start();
        let tag = rest.strip_prefix("loom").unwrap_or("");
        if tag.starts_with(":points") {
            match points_line_re().captures(trimmed) {
                Some(c) => out.points.push(c[1].to_string()),
                None => out.malformed.push(trimmed.to_string()),
            }
        } else if tag.starts_with(":complexity") {
            out.complexity.push(trimmed.to_string());
        }
    }
    out
}

/// Why a source yields no valid points value. Its `Display` is the BLOCKED
/// text both the gate and `CheckPointsMarker` print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointsError {
    /// No points marker in any counted source.
    Missing,
    /// A standalone `<!-- loom:points…` line that is not well formed.
    Malformed(String),
    /// A well-formed value outside [`POINTS_VALUES`].
    OutOfVocabulary(String),
    /// Two different values in the winning source.
    Conflict(Vec<String>),
    /// The winning source carries the retired `loom:complexity` tier marker.
    RetiredTier(String),
}

const SYNTAX: &str =
    "Add exactly one of these on its own line, in the issue body or a curation comment:\n\n  \
     <!-- loom:points=1 -->    <!-- loom:points=2 -->    <!-- loom:points=3 -->\n  \
     <!-- loom:points=5 -->    <!-- loom:points=8 -->    <!-- loom:points=13 -->\n\n\
     Size with the rubric in .loom/docs/story-points.md.";

impl std::fmt::Display for PointsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => write!(f, "BLOCKED: issue has no points estimate marker.\n\n{SYNTAX}"),
            Self::Malformed(line) => {
                write!(f, "BLOCKED: malformed points marker `{line}`.\n\n{SYNTAX}")
            }
            Self::OutOfVocabulary(v) => write!(
                f,
                "BLOCKED: invalid points value '{v}' (expected one of 1, 2, 3, 5, 8, 13).\n\n{SYNTAX}"
            ),
            Self::Conflict(values) => write!(
                f,
                "BLOCKED: conflicting points markers ({}) in one source.\n\n{SYNTAX}",
                values.join(", ")
            ),
            Self::RetiredTier(line) => write!(
                f,
                "BLOCKED: `{line}` is the retired complexity tier marker; story points replaced \
                 it (#11659). Remove it from this source and use loom:points= instead.\n\n{SYNTAX}"
            ),
        }
    }
}

impl std::error::Error for PointsError {}

/// Resolve the issue's points (design §5b). The **newest** trusted comment
/// with any points or complexity marker line wins; the body is the fallback,
/// and only when its author is trusted. The winner is validated on its own:
/// a malformed, out-of-vocabulary, conflicting or retired-tier winner is a
/// refusal, never a fall back to an older valid marker.
///
/// `trusted_comments` are the trusted comment bodies, oldest first
/// (`comment_trust::TrustPolicy::trusted_bodies`).
///
/// # Errors
///
/// The [`PointsError`] the gate reports.
pub fn resolve_points(
    body: &str,
    body_trusted: bool,
    trusted_comments: &[String],
) -> Result<u32, PointsError> {
    trusted_comments
        .iter()
        .rev()
        .map(|c| scan(c))
        .find(SourceMarkers::any)
        .or_else(|| body_trusted.then(|| scan(body)))
        .ok_or(PointsError::Missing)?
        .validate()
}

/// The body's single valid points value, ignoring any tier marker — the
/// best-effort read of the sweep-outcome write-back. `None` for an absent,
/// malformed, out-of-vocabulary or conflicting marker.
#[must_use]
pub fn extract_points_marker(body: &str) -> Option<u32> {
    scan(body).validate_points().ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    fn comments(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn extracts_a_valid_marker() {
        assert_eq!(extract_points_marker("Body.\n\n<!-- loom:points=5 -->\n"), Some(5));
        assert_eq!(extract_points_marker("<!--loom:points=13-->"), Some(13));
    }

    #[test]
    fn absent_and_out_of_vocabulary_are_none() {
        assert_eq!(extract_points_marker("no marker here"), None);
        assert_eq!(extract_points_marker("<!-- loom:points=21 -->"), None);
        assert_eq!(
            scan("<!-- loom:points=21 -->").validate(),
            Err(PointsError::OutOfVocabulary("21".into()))
        );
    }

    #[test]
    fn quoted_fenced_and_indented_syntax_is_not_a_marker() {
        for text in [
            "Emit `<!-- loom:points=3 -->` in the body.",
            "```\n<!-- loom:points=3 -->\n```",
            "~~~md\n<!-- loom:points=3 -->\n~~~",
            "    <!-- loom:points=3 -->",
            "\t<!-- loom:points=3 -->",
            "Prose <!-- loom:points=3 --> mid-line.",
            "```\n<!-- loom:complexity=3 -->\n```",
        ] {
            assert!(!scan(text).any(), "{text:?}");
        }
        // A placeholder quoted in prose does not hide the real marker.
        assert_eq!(
            extract_points_marker("Emit `<!-- loom:points=<N> -->`.\n\n<!-- loom:points=3 -->"),
            Some(3)
        );
    }

    #[test]
    fn malformed_lines_are_reported_not_skipped() {
        for line in [
            "<!-- loom:points=x -->",
            "<!-- loom:points= -->",
            "<!-- loom:points=3 extra -->",
            "<!-- loom:points=3",
            "<!-- loom:points -->",
        ] {
            assert!(matches!(scan(line).validate(), Err(PointsError::Malformed(_))), "{line}");
        }
    }

    #[test]
    fn conflicting_values_in_one_source_are_rejected_and_repeats_are_not() {
        assert_eq!(
            scan("<!-- loom:points=3 -->\n<!-- loom:points=5 -->").validate(),
            Err(PointsError::Conflict(vec!["3".into(), "5".into()]))
        );
        assert_eq!(scan("<!-- loom:points=3 -->\n<!-- loom:points=3 -->").validate(), Ok(3));
    }

    #[test]
    fn complexity_marker_is_detected_and_rejected() {
        let err = scan("<!-- loom:points=3 -->\n<!-- loom:complexity=routine -->")
            .validate()
            .unwrap_err();
        assert!(matches!(err, PointsError::RetiredTier(_)));
        let text = err.to_string();
        assert!(text.contains("loom:complexity") && text.contains("loom:points="), "{text}");
        // The write-back read ignores the tier marker.
        assert_eq!(
            extract_points_marker("<!-- loom:points=3 -->\n<!-- loom:complexity=routine -->"),
            Some(3)
        );
    }

    #[test]
    fn newest_trusted_comment_wins_over_older_comments_and_body() {
        let body = "<!-- loom:points=1 -->";
        let c = comments(&[
            "<!-- loom:points=2 -->",
            "no marker",
            "<!-- loom:points=8 -->",
        ]);
        assert_eq!(resolve_points(body, true, &c), Ok(8));
        assert_eq!(resolve_points(body, true, &comments(&["chatter"])), Ok(1));
    }

    #[test]
    fn invalid_winner_never_falls_back_to_an_older_valid_marker() {
        let c = comments(&["<!-- loom:points=3 -->", "<!-- loom:points=21 -->"]);
        assert_eq!(
            resolve_points("<!-- loom:points=5 -->", true, &c),
            Err(PointsError::OutOfVocabulary("21".into()))
        );
        let c = comments(&["<!-- loom:points=3 -->", "<!-- loom:complexity=3 -->"]);
        assert!(matches!(resolve_points("", true, &c), Err(PointsError::RetiredTier(_))));
    }

    #[test]
    fn tier_markers_in_non_winning_sources_are_ignored() {
        let body = "<!-- loom:complexity=routine -->";
        let c = comments(&["<!-- loom:complexity=3 -->", "<!-- loom:points=5 -->"]);
        assert_eq!(resolve_points(body, true, &c), Ok(5));
    }

    #[test]
    fn untrusted_body_does_not_count() {
        assert_eq!(resolve_points("<!-- loom:points=3 -->", false, &[]), Err(PointsError::Missing));
        assert_eq!(resolve_points("<!-- loom:points=3 -->", true, &[]), Ok(3));
        assert_eq!(resolve_points("3", true, &comments(&["3"])), Err(PointsError::Missing));
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
