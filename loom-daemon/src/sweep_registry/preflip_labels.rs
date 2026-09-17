//! Pre-flip label classification for the cross-host dispatch-collision guard
//! (Issue #4085, enforced since #5789; corrected by Issue #7873).
//!
//! Split out of [`crate::sweep_registry::guards`] (whose child module this is)
//! so the *decision* — "does this observed label set evidence a peer host's
//! claim?" — is a pure function over a `Vec<String>`, testable without a `gh`
//! fixture, and so the refusal text
//! ([`CollisionSource`](crate::sweep_registry::CollisionSource)) can name the
//! exact claim label(s) that were observed rather than asserting a peer flip
//! that may never have happened.
//!
//! ## Why the original predicate was wrong (#7873)
//!
//! The first implementation collided on `!has_issue || has_building`. The
//! `!has_issue` half was meant to catch "a peer already *removed* `loom:issue`
//! as part of its own claim flip" — but the forge label read is a *snapshot*,
//! not a diff, so that predicate is equally true for an issue that never had
//! `loom:issue` in the first place: an uncurated issue, a `loom:triage` one, or
//! a `loom:curated`-but-not-yet-promoted one. The daemon's work finder only ever
//! offers `loom:issue` candidates so it never tripped this, but every explicit
//! `dispatch_sweep` / `loom-daemon dispatch <N>` of an unpromoted issue was
//! refused as a "cross-host collision" with no peer anywhere in the fleet
//! (observed on #7743, #7812, #7849).
//!
//! ## The corrected rule
//!
//! Absence of `loom:issue` is not evidence of anything — only the *presence* of
//! a [claim label](CLAIM_LABELS) is. A claim label is applied by the claimant
//! itself, so observing one pre-flip means some agent already took this issue:
//! that is the collision, whether or not `loom:issue` is still there. An issue
//! with neither `loom:issue` nor any claim label is simply **not yet promoted**
//! ([`CollisionClass::NotYetApproved`]) — the child sweep's own pre-flight
//! curates and promotes it, exactly as an operator `/loom:sweep N` does.

// `super` is the parent `guards` module, which owns `CollisionClass`.
use super::CollisionClass;

/// Labels whose *presence* evidences that some agent has already claimed this
/// issue — the only positive signal the pre-flip snapshot carries (#7873).
///
/// Each of these is applied by its claimant at the moment it takes ownership of
/// a *dispatched* unit of work (`.github/labels.yml`: "Applied by: Builder only
/// (claim label)" / Judge / Doctor), so seeing one before this host's own flip
/// means this host is not the first claimant:
///
/// - `loom:building` — a Builder (this sweep's own phase) already claimed it.
/// - `loom:reviewing` — a Judge is mid-review, i.e. a sweep got that far.
/// - `loom:treating` — a Doctor is mid-fix, likewise.
///
/// The last two are normally PR-scoped, so an *issue* rarely carries them; they
/// are included because the failure they guard against is asymmetric — a missed
/// claim label silently duplicates live work, while a spurious one only defers
/// a dispatch that the next tick retries. This is the exact set #7873's proposal
/// names.
///
/// Deliberately **not** included:
///
/// - `loom:curating` / `loom:evaluating` — Curator and Champion claims. Both
///   are *pre-dispatch* lifecycle states, not evidence of a competing sweep;
///   refusing on them would re-introduce the #7873 false positive for the
///   ordinary "issue is still being enriched/evaluated" case.
/// - `loom:issue`'s *absence* — not a label, and not evidence: see this
///   module's header.
pub(crate) const CLAIM_LABELS: [&str; 3] = ["loom:building", "loom:reviewing", "loom:treating"];

/// The subset of `labels` that are [claim labels](CLAIM_LABELS), in the order
/// the forge reported them. Empty ⇒ the snapshot evidences no claim at all.
///
/// Used both to classify ([`classify_observed_labels`]) and to render the
/// refusal text, so the message names exactly the labels the decision was made
/// on (#7873's last acceptance bullet).
pub(crate) fn claim_labels_in(labels: &[String]) -> Vec<String> {
    labels
        .iter()
        .filter(|l| CLAIM_LABELS.contains(&l.as_str()))
        .cloned()
        .collect()
}

/// Render the claim evidence behind a refused dispatch, for
/// [`CollisionSource::ForgeLabel`](crate::sweep_registry::CollisionSource)'s
/// `Display` (#7873's last acceptance bullet). Names the observed claim
/// label(s) first — the actual grounds — with the full snapshot after, instead
/// of asserting a peer flip the snapshot may not evidence at all.
pub(crate) fn describe_claim_evidence(labels: &[String]) -> String {
    format!(
        "a peer host's claim label(s) [{}] already on the forge (full pre-flip labels=[{}])",
        claim_labels_in(labels).join(", "),
        labels.join(", ")
    )
}

/// Classify a pre-flip label snapshot (#4085's verdict set, widened and
/// corrected by #7873). Pure: the `gh` round trip and its fail-closed
/// [`CollisionClass::Unknown`] handling stay in
/// [`SweepRegistry::classify_preflip_labels`](crate::sweep_registry::SweepRegistry::classify_preflip_labels).
///
/// - Any [claim label](CLAIM_LABELS) present ⇒ [`CollisionClass::Collision`] —
///   a peer claimed it first, regardless of whether `loom:issue` survives
///   alongside (a peer's flip is two API calls; the remove half may not have
///   landed yet).
/// - Otherwise `loom:issue` present ⇒ [`CollisionClass::Clean`] — this host is
///   the first claimant, the pre-#7873 meaning of `Clean` exactly.
/// - Otherwise ⇒ [`CollisionClass::NotYetApproved`] — unpromoted, not a
///   collision; the caller proceeds and the child sweep promotes it itself.
pub(crate) fn classify_observed_labels(labels: Vec<String>) -> CollisionClass {
    if !claim_labels_in(&labels).is_empty() {
        CollisionClass::Collision { labels }
    } else if labels.iter().any(|l| l == "loom:issue") {
        CollisionClass::Clean
    } else {
        CollisionClass::NotYetApproved { labels }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn classify(labels: &[&str]) -> CollisionClass {
        classify_observed_labels(labels.iter().map(|s| (*s).to_string()).collect())
    }

    /// #7873's headline case: an issue nobody has ever labeled is NOT a
    /// collision. This is the exact snapshot (`labels=[]`) that refused
    /// dispatch of #7812 with a peer-host claim that did not exist.
    #[test]
    fn empty_label_set_is_not_a_collision() {
        assert_eq!(classify(&[]), CollisionClass::NotYetApproved { labels: vec![] });
    }

    /// Curated-but-not-yet-promoted: still no claim anywhere, still not a
    /// collision. Same for the `loom:triage` snapshot seen on #7743/#7849.
    #[test]
    fn unpromoted_lifecycle_labels_are_not_a_collision() {
        for labels in [
            vec!["loom:curated"],
            vec!["loom:triage"],
            vec!["loom:curating"],
            vec!["loom:curated", "tier:goal-supporting"],
        ] {
            assert!(
                matches!(classify(&labels), CollisionClass::NotYetApproved { .. }),
                "{labels:?} evidences no claim and must not be refused as a collision (#7873)"
            );
        }
    }

    /// The safety property #7873 must NOT weaken: a claim label present
    /// pre-flip is a real collision (#5789 enforcement), whether or not
    /// `loom:issue` is still alongside it.
    #[test]
    fn claim_labels_are_still_collisions() {
        for labels in [
            vec!["loom:building"],
            vec!["loom:curated", "loom:building"],
            vec!["loom:issue", "loom:building"],
            vec!["loom:reviewing"],
            vec!["loom:treating"],
        ] {
            let expected: Vec<String> = labels.iter().map(|s| (*s).to_string()).collect();
            assert_eq!(
                classify(&labels),
                CollisionClass::Collision { labels: expected },
                "{labels:?} carries a claim label and must still be refused (#5789)"
            );
        }
    }

    /// `loom:issue` present, no claim label ⇒ the pre-#7873 `Clean` verdict,
    /// unchanged: this host is the first claimant.
    #[test]
    fn issue_label_without_claim_is_clean() {
        assert_eq!(classify(&["loom:issue"]), CollisionClass::Clean);
        assert_eq!(classify(&["loom:issue", "loom:curated"]), CollisionClass::Clean);
    }

    /// The refusal text is built from this: only genuine claim labels are
    /// named, never the incidental ones that shared the snapshot.
    #[test]
    fn claim_labels_in_extracts_only_claim_labels() {
        let labels: Vec<String> = ["loom:curated", "loom:building", "tier:goal-supporting"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert_eq!(claim_labels_in(&labels), vec!["loom:building".to_string()]);
        assert!(claim_labels_in(&[]).is_empty());
        assert!(claim_labels_in(&["loom:issue".to_string()]).is_empty());
    }
}
