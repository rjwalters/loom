//! The pure half of the no-op hold (Issue #10156): what "nothing changed"
//! means, and which park the issue gets.
//!
//! Everything here is a pure function over already-fetched forge facts so each
//! rule is unit-testable without a `gh`. The forge reads that produce an
//! [`IssueSnapshot`] live in [`super::hold`].

/// Labels the daemon itself flips around every dispatch. They are churn the
/// hold must not mistake for a change in what the sweep depends on: a claim
/// (`loom:building`), its release (`loom:issue`), and the Curator / Doctor
/// working markers.
///
/// `loom:curated` is churn too (#12024): Champion's promotion and a Builder
/// step's release move an issue between `loom:curated` and `loom:issue`, and
/// that cycle is exactly the loop the hold must count through, not a change
/// that restarts it.
const DAEMON_CHURN_LABELS: &[&str] = &[
    "loom:building",
    "loom:issue",
    "loom:curated",
    "loom:curating",
    "loom:treating",
];

/// Most dependency issues one fingerprint will read. Every named dependency is
/// folded in (a truncated prefix would miss a later dependency changing state),
/// so a body naming more than this makes the snapshot inconclusive instead of
/// silently partial: never counted, never a release.
pub(super) const MAX_DEPENDENCY_READS: usize = 50;

/// What the no-op loop looks like, which decides the notice's wording and the
/// telemetry class. Since #12024 both kinds park on `loom:blocked` alone: the
/// daemon re-evaluates its own holds, and `loom:operator` is only ever
/// removed by a human (#11861), so it is never this hold's label.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkKind {
    /// Remaining work is blocked on other issues.
    Blocked,
    /// Remaining work looks like a human decision (or we cannot tell).
    /// Before #12024 this kind wrote `loom:operator` (#11795).
    HumanGate,
}

/// The one label every no-op park adds and its release removes (#12024).
const NOOP_PARK_LABELS: &[&str] = &[crate::work_finder::BLOCKED_HOLD_LABEL];

impl ParkKind {
    /// Labels the park adds: `loom:blocked`, for either kind (#12024).
    pub(super) fn labels(self) -> &'static [&'static str] {
        NOOP_PARK_LABELS
    }

    /// Labels a release of this park removes: exactly [`Self::labels`].
    /// A hold an older daemon applied as `loom:operator` is never lifted by
    /// the daemon (only a human removes that label, #11861); it is moved to
    /// `loom:blocked` by `loom-daemon labels migrate --holds` (#12024).
    pub(super) fn lift_labels(self) -> &'static [&'static str] {
        self.labels()
    }

    /// Stable tag for the persisted-holds file.
    pub(super) fn tag(self) -> &'static str {
        match self {
            Self::Blocked => "blocked",
            Self::HumanGate => "human-gate",
        }
    }

    /// Inverse of [`Self::tag`].
    pub(super) fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "blocked" => Some(Self::Blocked),
            "human-gate" => Some(Self::HumanGate),
            _ => None,
        }
    }

    /// Closed telemetry vocabulary for the loop's failure class.
    pub(crate) fn failure_class(self) -> &'static str {
        match self {
            Self::Blocked => "noop-loop:blocked",
            Self::HumanGate => "noop-loop:human-gate",
        }
    }
}

/// The short, stable digest of a fingerprint (#12024): the first 16 hex
/// characters of its SHA-256. It is what the streak, the persisted holds, the
/// hold notice and the fleet-wide `NoopStreak` ad compare, so every host
/// (and every daemon version from #12024 on) derives the same value.
#[must_use]
pub(crate) fn digest(fingerprint: &str) -> String {
    use sha2::{Digest, Sha256};
    let full = hex::encode(Sha256::digest(fingerprint.as_bytes()));
    full[..16].to_string()
}

/// The digest of a full fingerprint written by an older daemon (a
/// pre-#12024 hold notice or holds file), after dropping the labels this
/// version counts as churn from its `labels=[…]` segment. Without this a
/// legacy hold would never match a fresh read once `loom:curated` became
/// churn, and would be released on its first reconciliation.
#[must_use]
pub(crate) fn legacy_digest(full: &str) -> String {
    let normalized = full
        .strip_prefix("labels=[")
        .and_then(|rest| rest.split_once("];"))
        .map_or_else(
            || full.to_string(),
            |(labels, tail)| {
                let kept: Vec<&str> = labels
                    .split(',')
                    .filter(|l| !l.is_empty() && !DAEMON_CHURN_LABELS.contains(l))
                    .collect();
                format!("labels=[{}];{tail}", kept.join(","))
            },
        );
    digest(&normalized)
}

/// Whether `s` is already a [`digest`] (a pre-#12024 persisted hold stored
/// the full fingerprint).
#[must_use]
pub(crate) fn is_digest(s: &str) -> bool {
    s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The forge facts a no-op's fingerprint is built from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct IssueSnapshot {
    /// Every label on the issue, unfiltered.
    pub(super) labels: Vec<String>,
    /// `open` for an open issue.
    pub(super) open: bool,
    /// One `id:updated_at` entry per **non-bot, non-Loom-marker** comment, in
    /// forge order.
    pub(super) comments: Vec<String>,
    /// The linked-PR segment: `none`, or each linking PR as `#N:open|closed|merged`.
    pub(super) linked_pr: String,
    /// `(reference, state)` per dependency the body names.
    pub(super) dependencies: Vec<(String, String)>,
}

impl IssueSnapshot {
    /// Canonical fingerprint. Equal strings mean "the sweep's inputs are
    /// unchanged". A priority label (`loom:important` /
    /// `loom:very-important`) is deliberately part of the label set (raising
    /// or lowering priority is a human signal) but its mere presence never
    /// exempts the issue from the hold.
    pub(super) fn fingerprint(&self) -> String {
        self.fingerprint_ignoring(&[])
    }

    /// [`Self::fingerprint`] with `ignored` labels (the hold's own park
    /// labels) left out, so a parked issue can be compared against the
    /// fingerprint taken before the park landed.
    pub(super) fn fingerprint_ignoring(&self, ignored: &[&str]) -> String {
        let mut labels: Vec<&str> = self
            .labels
            .iter()
            .map(String::as_str)
            .filter(|l| !DAEMON_CHURN_LABELS.contains(l) && !ignored.contains(l))
            .collect();
        labels.sort_unstable();
        labels.dedup();
        let mut deps: Vec<String> = self
            .dependencies
            .iter()
            .map(|(r, s)| format!("{r}={s}"))
            .collect();
        deps.sort_unstable();
        format!(
            "labels=[{}];open={};comments=[{}];pr={};deps=[{}]",
            labels.join(","),
            self.open,
            self.comments.join(","),
            self.linked_pr,
            deps.join(","),
        )
    }

    /// Whether any dependency the body names is still open.
    fn has_open_dependency(&self) -> bool {
        self.dependencies.iter().any(|(_, s)| s != "closed")
    }
}

/// Words that say the remaining work is a human decision.
const HUMAN_WORDS: &[&str] = &[
    "human",
    "operator",
    "approv",
    "reject",
    "decision",
    "gate",
    "sign-off",
    "signoff",
    "maintainer",
];

/// Words that say the remaining work waits on other issues.
const DEPENDENCY_WORDS: &[&str] = &[
    "depend",
    "blocked on",
    "blocked by",
    "waiting on",
    "upstream",
];

/// Classify the loop. A reason naming a human gate wins; a reason naming
/// dependencies (or an open dependency in the snapshot, when the reason says
/// nothing either way) is [`ParkKind::Blocked`]; everything else, including an
/// absent reason, is [`ParkKind::HumanGate`]. Both park on `loom:blocked`
/// (#12024); the kind only words the notice and names the telemetry class.
pub(crate) fn choose_park(reason: Option<&str>, snapshot: Option<&IssueSnapshot>) -> ParkKind {
    let lower = reason.unwrap_or_default().to_ascii_lowercase();
    if HUMAN_WORDS.iter().any(|w| lower.contains(w)) {
        return ParkKind::HumanGate;
    }
    if DEPENDENCY_WORDS.iter().any(|w| lower.contains(w)) {
        return ParkKind::Blocked;
    }
    if snapshot.is_some_and(IssueSnapshot::has_open_dependency) {
        return ParkKind::Blocked;
    }
    ParkKind::HumanGate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> IssueSnapshot {
        IssueSnapshot {
            labels: vec!["loom:issue".into(), "loom:important".into()],
            open: true,
            comments: vec!["1:2026-10-01T00:00:00Z".into()],
            linked_pr: "none".into(),
            dependencies: vec![("o/r#5".into(), "open".into())],
        }
    }

    #[test]
    fn daemon_label_churn_does_not_change_the_fingerprint() {
        let a = base();
        let mut b = base();
        b.labels = vec!["loom:building".into(), "loom:important".into()];
        assert_eq!(a.fingerprint(), b.fingerprint());
        b.labels.push("loom:curating".into());
        assert_eq!(a.fingerprint(), b.fingerprint());
    }

    #[test]
    fn each_input_changes_the_fingerprint() {
        let a = base().fingerprint();
        let mut s = base();
        s.labels.push("loom:operator".into());
        assert_ne!(a, s.fingerprint(), "a label change");
        let mut s = base();
        s.comments.push("2:2026-10-02T00:00:00Z".into());
        assert_ne!(a, s.fingerprint(), "a new comment");
        let mut s = base();
        s.comments[0] = "1:2026-10-03T00:00:00Z".into();
        assert_ne!(a, s.fingerprint(), "an edited comment");
        // Editing an *earlier* comment must register even when the last one
        // is untouched.
        let mut two = base();
        two.comments.push("2:2026-10-02T00:00:00Z".into());
        let before = two.fingerprint();
        two.comments[0] = "1:2026-10-05T00:00:00Z".into();
        assert_ne!(before, two.fingerprint(), "an edited earlier comment");
        let mut s = base();
        s.linked_pr = "open:#9".into();
        assert_ne!(a, s.fingerprint(), "a linked PR");
        let mut s = base();
        s.dependencies[0].1 = "closed".into();
        assert_ne!(a, s.fingerprint(), "a dependency changing state");
        let mut s = base();
        s.open = false;
        assert_ne!(a, s.fingerprint(), "the issue closing");
    }

    #[test]
    fn ignoring_the_park_labels_matches_the_pre_park_fingerprint() {
        let before = base().fingerprint();
        let mut parked = base();
        parked.labels = vec!["loom:important".into(), "loom:blocked".into()];
        assert_ne!(before, parked.fingerprint());
        assert_eq!(before, parked.fingerprint_ignoring(&["loom:blocked"]));
    }

    #[test]
    fn park_choice_prefers_the_operator_pair_when_ambiguous() {
        assert_eq!(choose_park(None, None), ParkKind::HumanGate);
        assert_eq!(choose_park(Some("nothing to do"), None), ParkKind::HumanGate);
        assert_eq!(choose_park(Some("remaining work is a human gate"), None), ParkKind::HumanGate);
        assert_eq!(choose_park(Some("blocked on #12 and #13"), None), ParkKind::Blocked);
        // A human word beats a dependency word.
        assert_eq!(choose_park(Some("blocked on operator approval"), None), ParkKind::HumanGate);
        // Silent reason + an open dependency in the snapshot => blocked.
        assert_eq!(choose_park(None, Some(&base())), ParkKind::Blocked);
        let mut closed = base();
        closed.dependencies[0].1 = "closed".into();
        assert_eq!(choose_park(None, Some(&closed)), ParkKind::HumanGate);
    }

    #[test]
    fn park_labels_never_touch_the_claim_the_priority_or_the_operator_hold() {
        for kind in [ParkKind::Blocked, ParkKind::HumanGate] {
            assert_eq!(kind.labels(), &["loom:blocked"], "#12024: one park label");
            for l in kind.labels().iter().chain(kind.lift_labels()) {
                assert_ne!(*l, "loom:building");
                assert_ne!(*l, "loom:important");
                assert!(
                    !crate::work_finder::OPERATOR_HOLD_NAMES.contains(l),
                    "the no-op hold never writes or removes an operator hold: {l}"
                );
            }
        }
    }

    #[test]
    fn the_digest_is_short_stable_and_recognisable() {
        let d = digest(&base().fingerprint());
        assert!(is_digest(&d), "{d}");
        assert_eq!(d, digest(&base().fingerprint()));
        assert_ne!(d, digest("something else"));
        assert!(!is_digest(&base().fingerprint()));
    }

    #[test]
    fn a_legacy_fingerprint_digests_like_a_fresh_read_of_the_same_state() {
        let mut s = base();
        s.labels = vec!["loom:curated".into(), "loom:important".into()];
        let full = s.fingerprint();
        assert!(full.starts_with("labels=[loom:important];"), "curated is churn: {full}");
        // What an older daemon recorded for the same state.
        let recorded = full.replace("labels=[", "labels=[loom:curated,");
        assert_ne!(digest(&recorded), digest(&full));
        assert_eq!(legacy_digest(&recorded), digest(&full));
    }
}
