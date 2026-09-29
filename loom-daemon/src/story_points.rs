//! Story-point size labels (`points:1` … `points:13`, Issue #9432, epic
//! #9429) — the daemon-side reader of the Curator's size estimate.
//!
//! # Why labels, and why this is the only parser
//!
//! Points live in **labels**, not the issue body (the storage decision
//! adjudicated on #9431): labels are server-side filterable
//! (`gh issue list --label points:3`) and every forge read on the sweep
//! lifecycle path already carries a labels projection, so reading them costs
//! **zero extra forge round trips**. Both consumers — the dispatch-path park
//! guard's label read (`sweep.started`) and the terminal-transition issue
//! signal read (`sweep.outcome`) — resolve points through this one module, so
//! the vocabulary and the one-label-per-issue rule cannot drift between them.
//!
//! The numeric vocabulary itself is [`crate::points_marker::POINTS_VALUES`],
//! shared with the older `<!-- loom:points=<N> -->` body marker (#9056) so the
//! label family and the marker can never disagree about what a legal size is.
//!
//! # One `points:*` label per issue (the daemon-side guard)
//!
//! The Curator's discipline is "replace, never stack" (#9431), but discipline
//! is not a guarantee, so the *consumer* enforces it:
//!
//! | labels on the issue | resolution | telemetry |
//! |---|---|---|
//! | none | [`PointsLabels::Absent`] | attribute **omitted** |
//! | exactly one, in vocabulary | [`PointsLabels::One`] | attribute emitted |
//! | exactly one, out of vocabulary (`points:21`, `points:xl`) | [`PointsLabels::OutOfVocabulary`] | logged, attribute **omitted** |
//! | more than one | [`PointsLabels::Multiple`] | logged **loudly**, attribute **omitted** |
//!
//! **Never a guess.** A stacked pair (`points:3` + `points:8`) has no correct
//! answer — picking the first, the largest, or the newest would publish a
//! number no human assigned — so the attribute is omitted and the defect is
//! logged at `warn` where the fleet's own log scrapers can find it. Likewise
//! **absent is never `0`**: a pre-epic or operator-filed issue carries no
//! estimate at all, and a measured zero would poison every
//! estimate-vs-actual join (#9434) with a bucket nobody sized.

use crate::points_marker::POINTS_VALUES;
use std::collections::BTreeSet;

/// The label-name prefix the points family shares (`points:3` → `3`).
pub const POINTS_LABEL_PREFIX: &str = "points:";

/// Upper bound on how many labels a *new* labels-connection query needs to
/// fetch to see every possible points label (epic #9429: "cap any new
/// `labels(first:)` fetch at the points vocabulary size (~6)").
///
/// Nothing in this crate needs it today — both points consumers ride REST
/// projections that were already fetching the full label set for other guards,
/// so #9432 introduced **no** new labels connection. It is defined here so a
/// future GraphQL points reader caps its page at the vocabulary size rather
/// than re-deriving the number.
pub const POINTS_LABEL_PAGE_CAP: usize = POINTS_VALUES.len();

/// What an issue's label set says about its story-point size.
///
/// Three of the four variants resolve to **no** number — see the module doc's
/// table. Only [`PointsLabels::One`] is an estimate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PointsLabels {
    /// No `points:*` label at all — the issue was never sized (a pre-epic
    /// issue, an operator-filed issue, an uncurated issue). **Not zero.**
    Absent,
    /// Exactly one `points:*` label, and its value is in
    /// [`POINTS_VALUES`](crate::points_marker::POINTS_VALUES).
    One(u32),
    /// Exactly one `points:*` label, but its value is not in the closed
    /// vocabulary (`points:21`, `points:xl`, `points:`). A curation defect,
    /// reported rather than folded onto a neighbouring bucket.
    OutOfVocabulary(String),
    /// More than one `points:*` label. Carries every offending label name,
    /// sorted, so the log line names the actual conflict. **Never resolved to
    /// one of them.**
    Multiple(Vec<String>),
}

impl PointsLabels {
    /// The resolved estimate, or `None` for every non-[`One`](Self::One)
    /// variant. Callers that also want the defect logged should use
    /// [`resolve_story_points`] instead.
    #[must_use]
    pub fn value(&self) -> Option<u32> {
        match self {
            Self::One(points) => Some(*points),
            Self::Absent | Self::OutOfVocabulary(_) | Self::Multiple(_) => None,
        }
    }
}

/// Classify `labels` against the points vocabulary — a pure function over a
/// label-name list, with no logging and no forge access, so the
/// absent/one/out-of-vocabulary/multiple contract is directly testable.
///
/// Duplicate occurrences of the *same* label name collapse (a label cannot be
/// applied twice on a forge, but a caller may concatenate two reads of the
/// same issue), so only genuinely *different* points labels are a conflict.
#[must_use]
pub fn classify_points_labels<S: AsRef<str>>(labels: &[S]) -> PointsLabels {
    let points: BTreeSet<&str> = labels
        .iter()
        .map(AsRef::as_ref)
        .filter(|label| label.starts_with(POINTS_LABEL_PREFIX))
        .collect();
    let mut iter = points.iter();
    let Some(only) = iter.next() else {
        return PointsLabels::Absent;
    };
    if iter.next().is_some() {
        return PointsLabels::Multiple(points.iter().map(|l| (*l).to_string()).collect());
    }
    let value = &only[POINTS_LABEL_PREFIX.len()..];
    if POINTS_VALUES.contains(&value) {
        // In-vocabulary values are `1`..=`13`, so this parse cannot fail; a
        // future vocabulary entry that did not parse is treated as the
        // curation defect it would be rather than silently panicking.
        match value.parse::<u32>() {
            Ok(points) => PointsLabels::One(points),
            Err(_) => PointsLabels::OutOfVocabulary((*only).to_string()),
        }
    } else {
        PointsLabels::OutOfVocabulary((*only).to_string())
    }
}

/// [`classify_points_labels`], folded to the number telemetry publishes — and
/// **loudly logging** every resolution that is not a clean single label, so a
/// stacked or malformed points label is visible in the daemon log instead of
/// silently degrading to an absent attribute.
///
/// `issue` is used only to make those log lines actionable.
#[must_use]
pub fn resolve_story_points(issue: u32, labels: &[String]) -> Option<u32> {
    match classify_points_labels(labels) {
        PointsLabels::One(points) => Some(points),
        PointsLabels::Absent => None,
        PointsLabels::OutOfVocabulary(label) => {
            log::warn!(
                "issue #{issue}: `{label}` is not a legal story-point label (the closed \
                 vocabulary is {}) — omitting `loom.story_points` from this sweep's telemetry \
                 rather than guessing a bucket (#9432)",
                POINTS_VALUES.join("/")
            );
            None
        }
        PointsLabels::Multiple(labels) => {
            log::warn!(
                "issue #{issue}: {} story-point labels are applied at once ({}) — an issue must \
                 carry exactly ONE `points:*` label (#9431 'replace, never stack'). Omitting \
                 `loom.story_points` from this sweep's telemetry: there is no correct way to \
                 pick a winner, and publishing a guess would corrupt the estimate-vs-actual \
                 join (#9434). Fix the labels on the issue.",
                labels.len(),
                labels.join(", ")
            );
            None
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;

    /// AC: "a sweep of an issue without a points label produces records with
    /// no such attribute (absent, not 0)" — the classifier must never return a
    /// number for a label set that carries no points label, including one
    /// crowded with other Loom labels.
    #[test]
    fn absent_points_label_resolves_to_no_value_never_zero() {
        for labels in [
            vec![],
            vec!["loom:building"],
            vec!["loom:curated", "loom:issue", "tier:goal-advancing"],
            // Near-misses: neither is a member of the `points:` family.
            vec!["loom:points=3"],
            vec!["story-points:3"],
        ] {
            assert_eq!(classify_points_labels(&labels), PointsLabels::Absent, "{labels:?}");
            assert_eq!(classify_points_labels(&labels).value(), None, "{labels:?}");
        }
    }

    /// AC: "a sweep of an issue carrying one `points:*` label produces records
    /// with `loom.story_points`" — for every value in the closed vocabulary.
    #[test]
    fn one_points_label_resolves_to_its_numeric_value() {
        for (label, expected) in [
            ("points:1", 1_u32),
            ("points:2", 2),
            ("points:3", 3),
            ("points:5", 5),
            ("points:8", 8),
            ("points:13", 13),
        ] {
            let labels = vec!["loom:curated", label, "tier:goal-advancing"];
            assert_eq!(classify_points_labels(&labels), PointsLabels::One(expected), "{label}");
            assert_eq!(resolve_story_points(9432, &owned(&labels)), Some(expected), "{label}");
        }
    }

    /// The vocabulary is exactly the body marker's own closed set, so the
    /// label family and `<!-- loom:points=<N> -->` can never disagree.
    #[test]
    fn the_label_vocabulary_is_the_marker_vocabulary() {
        for value in POINTS_VALUES {
            assert_eq!(
                classify_points_labels(&[format!("{POINTS_LABEL_PREFIX}{value}")]).value(),
                Some(value.parse::<u32>().unwrap()),
                "{value}"
            );
        }
        assert_eq!(POINTS_LABEL_PAGE_CAP, 6);
    }

    /// A Fibonacci-looking value outside the vocabulary is a curation defect,
    /// not a size: it must not be folded onto 13 (or anything else).
    #[test]
    fn a_single_out_of_vocabulary_label_is_reported_not_folded() {
        for label in ["points:21", "points:0", "points:4", "points:xl", "points:"] {
            assert_eq!(
                classify_points_labels(&[label]),
                PointsLabels::OutOfVocabulary(label.to_string()),
                "{label}"
            );
            assert_eq!(resolve_story_points(9432, &owned(&[label])), None, "{label}");
        }
    }

    /// AC: "multiple points labels are logged and resolve to no attribute,
    /// never to a guess". The `Multiple` variant carries every offending label
    /// so the warning names the real conflict — and is deliberately NOT any of
    /// the plausible tie-breaks (first, last, smallest, largest).
    #[test]
    fn multiple_points_labels_are_surfaced_never_silently_resolved() {
        let labels = vec!["loom:building", "points:8", "points:3"];
        assert_eq!(
            classify_points_labels(&labels),
            // Sorted, so the diagnostic is deterministic regardless of the
            // order the forge listed the labels in.
            PointsLabels::Multiple(vec!["points:3".to_string(), "points:8".to_string()])
        );
        assert_eq!(classify_points_labels(&labels).value(), None);
        assert_eq!(resolve_story_points(9432, &owned(&labels)), None);

        // Three, including an out-of-vocabulary one: still a conflict, and the
        // in-vocabulary member is never promoted to the answer.
        let messy = vec!["points:13", "points:21", "points:5"];
        assert_eq!(
            classify_points_labels(&messy),
            PointsLabels::Multiple(vec![
                "points:13".to_string(),
                "points:21".to_string(),
                "points:5".to_string(),
            ])
        );
        assert_eq!(resolve_story_points(9432, &owned(&messy)), None);
    }

    /// The same label name twice is one label, not a conflict — a forge cannot
    /// apply a label twice, but a caller may concatenate two reads.
    #[test]
    fn the_same_label_repeated_is_not_a_conflict() {
        assert_eq!(
            classify_points_labels(&["points:5", "loom:building", "points:5"]),
            PointsLabels::One(5)
        );
    }

    fn owned(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|l| (*l).to_string()).collect()
    }
}
