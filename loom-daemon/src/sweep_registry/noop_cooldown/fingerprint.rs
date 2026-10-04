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
const DAEMON_CHURN_LABELS: &[&str] = &[
    "loom:building",
    "loom:issue",
    "loom:curating",
    "loom:treating",
];

/// Most dependency issues whose state is folded into one fingerprint. Bounds
/// the forge reads a single no-op can cost.
pub(super) const MAX_DEPENDENCY_READS: usize = 6;

/// Which park a held issue gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParkKind {
    /// Remaining work is blocked on other issues: `loom:blocked`.
    Blocked,
    /// Remaining work is a human gate (or we cannot tell):
    /// `loom:operator-only` + `loom:operator-decision`.
    HumanGate,
}

impl ParkKind {
    /// Labels the park adds.
    pub(super) fn labels(self) -> &'static [&'static str] {
        match self {
            Self::Blocked => &["loom:blocked"],
            Self::HumanGate => &["loom:operator-only", "loom:operator-decision"],
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
    /// The linked-PR segment: `none`, or `open:#N`.
    pub(super) linked_pr: String,
    /// `(reference, state)` per dependency the body names.
    pub(super) dependencies: Vec<(String, String)>,
}

impl IssueSnapshot {
    /// Canonical fingerprint. Equal strings mean "the sweep's inputs are
    /// unchanged". `loom:operator-priority` is deliberately part of the label
    /// set (an operator starring or unstarring is a human signal) but its mere
    /// presence never exempts the issue from the hold.
    pub(super) fn fingerprint(&self) -> String {
        let mut labels: Vec<&str> = self
            .labels
            .iter()
            .map(String::as_str)
            .filter(|l| !DAEMON_CHURN_LABELS.contains(l))
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
            "labels=[{}];open={};comments={}:{};pr={};deps=[{}]",
            labels.join(","),
            self.open,
            self.comments.len(),
            self.comments.last().map_or("", String::as_str),
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

/// Choose the park. A reason naming a human gate wins; a reason naming
/// dependencies (or an open dependency in the snapshot, when the reason says
/// nothing either way) selects `loom:blocked`; everything else — including an
/// absent reason — is the safer, more visible operator pair.
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
            labels: vec!["loom:issue".into(), "loom:operator-priority".into()],
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
        b.labels = vec!["loom:building".into(), "loom:operator-priority".into()];
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
    fn park_labels_never_touch_the_claim_or_the_star() {
        for kind in [ParkKind::Blocked, ParkKind::HumanGate] {
            for l in kind.labels() {
                assert_ne!(*l, "loom:building");
                assert_ne!(*l, "loom:operator-priority");
            }
        }
    }
}
