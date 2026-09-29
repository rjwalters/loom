//! `loom.story_points` derivation from `points:*` issue labels (Issue #9432,
//! epic #9429 phase 3).
//!
//! The Curator assigns each curated issue a story-point estimate as a forge
//! label (`points:1`, `points:2`, `points:3`, `points:5`, `points:8`,
//! `points:13` — the closed vocabulary [`crate::points_marker::POINTS_VALUES`]
//! already pins, applied per #9431). This module turns the label snapshot a
//! dispatch already read into the numeric `loom.story_points` telemetry
//! attribute, so every backend query can join the assigned estimate to the
//! actual cost of the sweep that burned it without leaving the store.
//!
//! # The guard (contract, not convention)
//!
//! Exactly one `points:*` label per issue is a **contract** the Curator
//! upholds; this function is the daemon-side enforcement of how a violation is
//! reported rather than a silent resolution:
//!
//! - **zero** `points:*` labels → [`StoryPoints::Absent`] — the attribute is
//!   omitted entirely. Absent is not zero: a pre-epic issue, an operator-filed
//!   issue, or a sweep dispatched by a path that never read the labels all
//!   carry *no* estimate, and emitting `0` would fold them into the
//!   "estimated, trivially small" population.
//! - **one** `points:<N>` label with an in-vocabulary value →
//!   [`StoryPoints::Points`] — the attribute is emitted.
//! - **more than one** `points:*` label → [`StoryPoints::Multiple`] — the
//!   caller logs this loudly and omits the attribute; **never** a guess about
//!   which label wins.
//! - **one** label whose value is out of vocabulary or not a number →
//!   [`StoryPoints::Invalid`] — surfaced the same way (loud log, attribute
//!   omitted), mirroring `points_marker`'s own "an out-of-vocabulary value is
//!   a curation defect, not a style choice" rule.
//!
//! Pure by construction: no I/O, no clock, no logging — the loud-log half of
//! the multiple/invalid contract lives at the (single) dispatch-time call site
//! so the state machine stays unit-testable in isolation.
//!
//! `pub`, not `pub(crate)`: the module tree follows `points_marker`'s — the
//! binary crate of this package reaches the library only through its `pub`
//! surface.

use crate::points_marker::POINTS_VALUES;

/// The forge-label prefix the Curator's estimate rides on (per #9431):
/// `points:<N>`, one label per issue.
pub const POINTS_LABEL_PREFIX: &str = "points:";

/// The outcome of the one-`points:*` label-per-issue guard — see the
/// [module doc](self) for the contract each variant carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoryPoints {
    /// No `points:*` label at all: the attribute must be **absent**, never
    /// `0`. Covers pre-epic issues, operator-filed issues, and label
    /// snapshots whose read simply did not happen (fail-open paths).
    Absent,
    /// Exactly one `points:<N>` label with an in-vocabulary value: emit
    /// `loom.story_points = N`.
    Points(u8),
    /// More than one `points:*` label: ambiguous — log loudly, omit the
    /// attribute, never guess which one wins. Carries the first conflicting
    /// pair, which is all a loud log needs to name the conflict.
    Multiple(Vec<String>),
    /// Exactly one `points:*` label whose value is not in the closed
    /// vocabulary (or not a number at all): a curation defect — surfaced, not
    /// silently coerced. Carries the offending label.
    Invalid(String),
}

impl StoryPoints {
    /// The attribute value to emit, if any — `None` for every non-`Points`
    /// variant, so the "absent, not zero" rule is a single fold at the
    /// emission site rather than a convention each caller re-derives.
    #[must_use]
    pub fn value(&self) -> Option<u8> {
        match *self {
            Self::Points(value) => Some(value),
            Self::Absent | Self::Multiple(_) | Self::Invalid(_) => None,
        }
    }
}

/// Resolve the story-point estimate from an issue's observed label names —
/// the pure state machine behind the `loom.story_points` attribute
/// (Issue #9432). Label order is irrelevant; a non-`points:*` label never
/// participates (so `loom:issue`, `story-points:5`-shaped neighbors, and any
/// future label cannot perturb the guard).
#[must_use]
pub fn story_points_from_labels<'a, I>(labels: I) -> StoryPoints
where
    I: IntoIterator<Item = &'a str>,
{
    let mut matched: Vec<&str> = Vec::new();
    for label in labels {
        let Some(value) = label.strip_prefix(POINTS_LABEL_PREFIX) else {
            continue;
        };
        matched.push(value);
        if matched.len() > 1 {
            // A second `points:*` label is ambiguous no matter what either
            // value says — even `points:5` twice, because the guard is about
            // the Curator's contract (one estimate per issue), not about
            // whether the duplicates happen to agree. Never a guess.
            return StoryPoints::Multiple(
                matched
                    .iter()
                    .take(2)
                    .map(|value| format!("{POINTS_LABEL_PREFIX}{value}"))
                    .collect(),
            );
        }
    }
    match matched.first() {
        None => StoryPoints::Absent,
        // In-vocabulary values are numeric by construction (POINTS_VALUES is
        // the same closed set `points_marker` validates against), so the
        // parse cannot fail for an accepted value.
        Some(&value) if POINTS_VALUES.contains(&value) => StoryPoints::Points(
            value
                .parse()
                .expect("in-vocabulary points value is numeric"),
        ),
        Some(&value) => StoryPoints::Invalid(format!("{POINTS_LABEL_PREFIX}{value}")),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    fn resolve(labels: &[&str]) -> StoryPoints {
        story_points_from_labels(labels.iter().copied())
    }

    #[test]
    fn no_points_label_is_absent_never_zero() {
        assert_eq!(resolve(&["loom:issue", "loom:curated"]), StoryPoints::Absent);
        assert_eq!(resolve(&[]), StoryPoints::Absent);
        assert_eq!(resolve(&["loom:issue"]).value(), None);
    }

    #[test]
    fn exactly_one_points_label_emits_its_value() {
        for (label, expected) in [
            ("points:1", 1),
            ("points:2", 2),
            ("points:3", 3),
            ("points:5", 5),
            ("points:8", 8),
            ("points:13", 13),
        ] {
            assert_eq!(
                resolve(&["loom:issue", label, "loom:curated"]),
                StoryPoints::Points(expected),
                "label {label} must resolve to {expected}"
            );
            assert_eq!(resolve(&[label]).value(), Some(expected));
        }
    }

    #[test]
    fn more_than_one_points_label_is_multiple_never_a_guess() {
        // The loud-log path: two different values, and the same value twice —
        // both violate the one-label contract, and neither may be resolved.
        assert_eq!(
            resolve(&["points:3", "points:8"]),
            StoryPoints::Multiple(vec!["points:3".to_string(), "points:8".to_string()])
        );
        assert_eq!(
            resolve(&["points:5", "loom:issue", "points:5"]),
            StoryPoints::Multiple(vec!["points:5".to_string(), "points:5".to_string()])
        );
        assert_eq!(
            resolve(&["points:1", "points:2", "points:3"]),
            StoryPoints::Multiple(vec!["points:1".to_string(), "points:2".to_string()])
        );
        assert_eq!(resolve(&["points:3", "points:8"]).value(), None);
    }

    #[test]
    fn an_out_of_vocabulary_value_is_surfaced_not_coerced() {
        assert_eq!(resolve(&["points:21"]), StoryPoints::Invalid("points:21".to_string()));
        assert_eq!(resolve(&["points:zero"]), StoryPoints::Invalid("points:zero".to_string()));
        assert_eq!(resolve(&["points:0"]), StoryPoints::Invalid("points:0".to_string()));
        assert_eq!(resolve(&["points:21"]).value(), None);
    }

    #[test]
    fn near_miss_labels_never_participate() {
        // The prefix is exact: a label that merely contains "points" — or
        // extends the vocabulary with a longer prefix — is not an estimate.
        assert_eq!(resolve(&["story-points:5"]), StoryPoints::Absent);
        assert_eq!(resolve(&["points-notes:5"]), StoryPoints::Absent);
        assert_eq!(resolve(&["loom:points:5"]), StoryPoints::Absent);
        assert_eq!(resolve(&["points:"]), StoryPoints::Invalid("points:".to_string()));
    }

    #[test]
    fn every_marker_vocabulary_value_resolves() {
        // The label vocabulary is the marker vocabulary verbatim: every entry
        // parses, so the two sets can never drift apart silently.
        for value in crate::points_marker::POINTS_VALUES {
            assert_eq!(
                resolve(&[&format!("{POINTS_LABEL_PREFIX}{value}")]),
                StoryPoints::Points(value.parse().unwrap())
            );
        }
    }
}
