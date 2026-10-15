//! What Champion does with a PR's verdict and park labels (#11932): the
//! Verdict-State Janitor's exception for Champion holds, in code.
//!
//! # Why this is not prose
//!
//! After #11861 a `loom:pr` + `loom:blocked` pair is legal only as Champion's
//! own hold, keyed on a standing trusted hold marker ([`super::standing_hold`]).
//! The daemon read that marker, but the janitor's side of the rule lived only
//! in `champion-pr-merge.md`. A pair with **no** marker (hand-moved, or parked
//! by a sweep) then fell through: Champion could not release a hold it never
//! wrote, `merge-pr.sh` refused the merge, and the Error Handling section posted
//! an unguarded "Merge Failed" comment on every pass.
//!
//! [`classify`] makes the decision once, from labels plus trusted comments:
//!
//! | state | labels | Champion |
//! |---|---|---|
//! | [`PrClass::ForeignPark`] | `loom:blocked`, no standing hold marker | skips the PR: no merge attempt, no comment, no label change |
//! | [`PrClass::Contradiction`] | `loom:pr` beside `loom:changes-requested` / `loom:review-requested` | the janitor repairs it (strips `loom:pr`) |
//! | [`PrClass::ChampionHold`] | `loom:blocked` with a standing hold marker | leaves it; criteria #2/#3 decide |
//! | [`PrClass::Clear`] | none of these | the 6 criteria run |
//!
//! A foreign park is checked first: a PR another actor parked is theirs, even
//! when its verdict labels also contradict. A contradiction beside a Champion
//! hold (two Judges racing on a held PR) is still repaired, and the hold
//! survives the repair because `loom:blocked` is never touched.
//!
//! [`merge_failed_marker`] is the second half: the "Merge Failed" notice is
//! posted at most once per head.

use super::{standing_hold, HoldKind};

/// The first token of every `champion-hold classify` stdout line.
pub const CLASSIFY_SENTINEL: &str = "LOOM-CHAMPION-HOLD";

/// The verdict labels that contradict `loom:pr` for the janitor, in the order
/// it reports them (#4570, #7018).
pub const CONTRADICTING_VERDICTS: [&str; 2] = ["loom:changes-requested", "loom:review-requested"];

/// The "Merge Failed" notice's marker prefix; the full marker names the head
/// (`<!-- champion:merge-failed head=<sha> -->`).
pub const MERGE_FAILED_MARKER_PREFIX: &str = "<!-- champion:merge-failed head=";

/// What Champion does with a PR (see the module table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrClass {
    /// Nothing for the janitor: the 6 criteria run.
    Clear,
    /// Champion's own hold: leave it.
    ChampionHold(HoldKind),
    /// `loom:blocked` from another actor: skip, silently.
    ForeignPark,
    /// `loom:pr` beside the named verdict label: strip `loom:pr`.
    Contradiction(&'static str),
}

impl PrClass {
    /// The `state=` token.
    #[must_use]
    pub fn state(self) -> &'static str {
        match self {
            PrClass::Clear => "clear",
            PrClass::ChampionHold(_) => "champion-hold",
            PrClass::ForeignPark => "foreign-park",
            PrClass::Contradiction(_) => "contradiction",
        }
    }

    /// The stdout line: `LOOM-CHAMPION-HOLD state=<s> pr=<N> detail=<d>`.
    /// `detail` is the contradicting label, the hold kind, or `-`.
    #[must_use]
    pub fn line(self, pr: u64) -> String {
        let detail = match self {
            PrClass::Contradiction(label) => label,
            PrClass::ChampionHold(HoldKind::MergeRisk) => "merge-risk",
            PrClass::ChampionHold(HoldKind::CriticalFile) => "critical-file",
            PrClass::Clear | PrClass::ForeignPark => "-",
        };
        format!("{CLASSIFY_SENTINEL} state={} pr={pr} detail={detail}", self.state())
    }
}

/// Whether the trusted comments are needed to classify `labels`: only a
/// `loom:blocked` PR asks whose park it is.
#[must_use]
pub fn needs_comments<L: AsRef<str>>(labels: &[L]) -> bool {
    crate::work_finder::is_blocked_hold(labels)
}

/// Classify a PR from its labels and its **trusted** comment bodies, oldest
/// first (`comment_trust`, #9548). `trusted` is read only when
/// [`needs_comments`]; a caller that could not read it must not call this
/// for such a PR (an unknown owner is not a foreign park, nor a hold).
#[must_use]
pub fn classify<L: AsRef<str>, C: AsRef<str>>(labels: &[L], trusted: &[C]) -> PrClass {
    let has = |want: &str| labels.iter().any(|l| l.as_ref() == want);
    let hold = if needs_comments(labels) {
        match standing_hold(trusted) {
            Some(kind) => Some(kind),
            None => return PrClass::ForeignPark,
        }
    } else {
        None
    };
    if has("loom:pr") {
        if let Some(label) = CONTRADICTING_VERDICTS.iter().copied().find(|l| has(l)) {
            return PrClass::Contradiction(label);
        }
    }
    hold.map_or(PrClass::Clear, PrClass::ChampionHold)
}

/// The marker a "Merge Failed" notice for `head` opens with.
#[must_use]
pub fn merge_failed_marker(head: &str) -> String {
    format!("{MERGE_FAILED_MARKER_PREFIX}{head} -->")
}

/// The marker to post a "Merge Failed" notice under, or `None` when a trusted
/// notice for this head already stands (once per head, #11932). `startswith`,
/// not `contains`: a later comment quoting the marker is not the notice
/// (#5371). An empty or non-hex head never posts.
#[must_use]
pub fn merge_failed_notice_due<C: AsRef<str>>(trusted: &[C], head: &str) -> Option<String> {
    if head.is_empty() || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let marker = merge_failed_marker(head);
    (!trusted.iter().any(|c| c.as_ref().starts_with(&marker))).then_some(marker)
}

#[cfg(test)]
mod tests {
    use super::super::{
        CRITICAL_FILE_HOLD_MARKER, MERGE_RISK_CLEARED_MARKER, MERGE_RISK_HOLD_MARKER,
    };
    use super::*;

    const NONE: [&str; 0] = [];

    fn hold() -> String {
        format!("{MERGE_RISK_HOLD_MARKER}\n<!-- champion:hold-state head=abc -->")
    }

    #[test]
    fn a_markerless_approved_park_is_foreign() {
        assert_eq!(classify(&["loom:pr", "loom:blocked"], &NONE), PrClass::ForeignPark);
        // A quoted marker is prose, not a hold.
        let quoted = format!("see {MERGE_RISK_HOLD_MARKER}");
        assert_eq!(classify(&["loom:pr", "loom:blocked"], &[quoted]), PrClass::ForeignPark);
        // A released hold leaves the park ownerless, i.e. not Champion's.
        let released = format!("Merging.\n{MERGE_RISK_CLEARED_MARKER}");
        assert_eq!(
            classify(&["loom:pr", "loom:blocked"], &[hold(), released]),
            PrClass::ForeignPark
        );
    }

    #[test]
    fn a_foreign_park_wins_over_a_verdict_contradiction() {
        let labels = ["loom:pr", "loom:changes-requested", "loom:blocked"];
        assert_eq!(classify(&labels, &NONE), PrClass::ForeignPark);
    }

    #[test]
    fn a_standing_marker_makes_it_champions_hold() {
        assert_eq!(
            classify(&["loom:pr", "loom:blocked"], &[hold()]),
            PrClass::ChampionHold(HoldKind::MergeRisk)
        );
        let cf = format!("{CRITICAL_FILE_HOLD_MARKER}\nheld");
        assert_eq!(
            classify(&["loom:pr", "loom:blocked"], &[cf]),
            PrClass::ChampionHold(HoldKind::CriticalFile)
        );
        // The Doctor round trip of a hold is still the hold.
        assert_eq!(
            classify(&["loom:changes-requested", "loom:blocked"], &[hold()]),
            PrClass::ChampionHold(HoldKind::MergeRisk)
        );
    }

    #[test]
    fn verdict_contradictions_are_repaired_even_on_a_hold() {
        for other in CONTRADICTING_VERDICTS {
            assert_eq!(classify(&["loom:pr", other], &NONE), PrClass::Contradiction(other));
            assert_eq!(
                classify(&["loom:pr", other, "loom:blocked"], &[hold()]),
                PrClass::Contradiction(other),
                "a Judge race on a held PR"
            );
        }
        // Both: the fixed report order, regardless of label order.
        let both = ["loom:review-requested", "loom:pr", "loom:changes-requested"];
        assert_eq!(classify(&both, &NONE), PrClass::Contradiction("loom:changes-requested"));
    }

    #[test]
    fn anything_else_is_clear() {
        assert_eq!(classify(&["loom:pr"], &NONE), PrClass::Clear);
        assert_eq!(classify(&["loom:review-requested"], &NONE), PrClass::Clear);
        assert_eq!(classify::<&str, &str>(&[], &[]), PrClass::Clear);
        // Out of scope here: merge-pr.sh and the criteria own these parks.
        assert_eq!(classify(&["loom:pr", "loom:operator"], &NONE), PrClass::Clear);
    }

    #[test]
    fn comments_are_needed_only_for_a_blocked_pr() {
        assert!(needs_comments(&["loom:pr", "loom:blocked"]));
        assert!(!needs_comments(&["loom:pr", "loom:changes-requested"]));
    }

    #[test]
    fn the_line_names_state_pr_and_detail() {
        assert_eq!(PrClass::Clear.line(7), "LOOM-CHAMPION-HOLD state=clear pr=7 detail=-");
        assert_eq!(
            PrClass::ForeignPark.line(7),
            "LOOM-CHAMPION-HOLD state=foreign-park pr=7 detail=-"
        );
        assert_eq!(
            PrClass::Contradiction("loom:review-requested").line(7),
            "LOOM-CHAMPION-HOLD state=contradiction pr=7 detail=loom:review-requested"
        );
        assert_eq!(
            PrClass::ChampionHold(HoldKind::CriticalFile).line(7),
            "LOOM-CHAMPION-HOLD state=champion-hold pr=7 detail=critical-file"
        );
    }

    #[test]
    fn the_merge_failed_notice_posts_once_per_head() {
        let first = merge_failed_notice_due(&NONE, "abc123").expect("first failure posts");
        assert_eq!(first, "<!-- champion:merge-failed head=abc123 -->");
        let posted = format!("{first}\n**Champion: Merge Failed**");
        assert_eq!(merge_failed_notice_due(&[posted.as_str()], "abc123"), None, "same head");
        assert!(merge_failed_notice_due(&[posted.as_str()], "def456").is_some(), "new head");
        // A quote of the marker is not the notice.
        let quoted = format!("Earlier: {first}");
        assert!(merge_failed_notice_due(&[quoted.as_str()], "abc123").is_some());
        // No head, or a head that is not a SHA: never post.
        assert_eq!(merge_failed_notice_due(&NONE, ""), None);
        assert_eq!(merge_failed_notice_due(&NONE, "abc -->"), None);
    }

    #[test]
    fn a_marker_for_a_longer_head_is_not_this_heads_notice() {
        let other = format!("{}\nx", merge_failed_marker("abc1234"));
        assert!(merge_failed_notice_due(&[other.as_str()], "abc123").is_some());
    }
}
