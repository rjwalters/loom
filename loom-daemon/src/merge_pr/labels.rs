//! The PR verdict-label mutual-exclusion guard (#8112, slice 2 of #8191).
//!
//! # The incident
//!
//! Two Judge passes reviewed the same head of PR #8076 about 45 seconds apart
//! and disagreed. The first approved and applied `loom:pr`; the second found a
//! real blocking defect and applied `loom:changes-requested`. Nothing removed
//! the first label, so the PR carried both at once.
//!
//! `loom:pr` is the merge signal — `merge-pr.sh` gates on it and Champion
//! auto-merges on it — and neither path looked for a label saying the
//! opposite. The window is therefore: **a PR with an open, unaddressed
//! blocking finding gets auto-merged**, because one of two concurrent
//! reviewers approved it. On #8076 that was one manual intervention away from
//! happening.
//!
//! # Order-independence is the property, not an implementation detail
//!
//! The guard walks a FIXED list of blocking labels and asks whether each is
//! present. It never asks "which label came first", because the forge's
//! `labels` array order is not a contract and a guard that depended on it
//! could be silently defeated by relabelling. [`contradiction`] returns the
//! first blocker in the FIXED order regardless of input order, and a test
//! drives the same set through every permutation to pin that.
//!
//! # Why `loom:operator` is in the list
//!
//! `.github/labels.yml` defines it as "engine stops work on this item; human
//! is the only transition out". A human is exactly what an unattended
//! Champion auto-merge is not, so a held PR must not merge itself. The human's
//! transition out is to clear the label and then merge — which is a recorded
//! decision, rather than a flag buried in a command line.
//!
//! That transition has to actually be available, and for one hold kind it was
//! not: Champion's critical-file hold (criterion #3) re-applied `loom:operator`
//! on its next tick, so clearing the label bought the operator ~90 seconds
//! before this guard refused again (#9016, merge train #8996). The fix is on
//! Champion's side, not this guard's — `champion-critical-file-hold.md` now
//! reads a hand-removed `loom:operator`, at the head the hold was written
//! against, as a durable release and stands down on the label for that head.
//! Nothing here changed, and nothing here should: the contradiction is
//! resolved by the label set moving, which is what this guard reads.
//!
//! # Why `loom:sequenced` is in the list (#9378)
//!
//! `.github/labels.yml` defines it as "approved, but sequenced behind another
//! PR — mechanically cleared when the recorded predecessor lands". It is the
//! missing *durable ordering hold*: not an escalation (`loom:operator` means a
//! human must rule; a landing order does not), and not `loom:blocked` (an
//! issue-status label the merge script has never read). A PR approved by
//! Judge AND sequenced behind an unlanded predecessor is exactly the state
//! that merged #112 before #111 in the incident that filed #9378 — correct by
//! every per-PR check, wrong for the pair. The condition it waits on lives in
//! a trusted `<!-- loom:sequence … -->` marker on the PR
//! ([`super::sequence`]); the LABEL is the gate so every merge path refuses
//! with no live forge reads, and the separate `merge-pr sequence-eval`
//! evaluation is what moves the label when the condition is met. Human path
//! out: the predecessor lands and the pass (or an agent running the verb)
//! clears the label — a recorded condition, not a flag.
//!
//! # There is deliberately no override flag
//!
//! `--allow-unapproved` bypasses the *missing*-`loom:pr` guard. This one is
//! not bypassable by it or by anything else: overriding "nobody reviewed it"
//! and overriding "a reviewer said no" are different acts, and only the first
//! is a matter of the operator accepting risk on their own behalf.

/// The only stdout a caller may treat as "reviewed and clean".
///
/// A sentinel rather than silence. Passing this guard requires a POSITIVE
/// signal, so that a missing, old, or substituted binary cannot produce a pass
/// by falling over quietly — see `cli::merge_pr_labels` for the contract and
/// the fail-open contract it replaced.
pub const CLEAN: &str = "LOOM-VERDICT-CLEAN";

/// Labels that contradict `loom:pr`, in the order they are reported.
///
/// Fixed and ordered so the verdict is a function of the label SET alone.
/// Derived (#10013): the registry's `contradicts_approval` labels, in their
/// registry-declared report order.
pub static BLOCKING: crate::label_registry::LabelSet = crate::label_registry::LabelSet::new(|| {
    crate::label_registry::embedded_set("contradicts_approval")
});

/// The blocking label contradicting `loom:pr`, if the set is contradictory.
///
/// `labels` is newline-separated, as both forges' `labels[].name` renders.
/// Returns `None` when `loom:pr` is absent — this guard is only ever about a
/// PR claiming approval.
#[must_use]
pub fn contradiction(labels: &str) -> Option<&'static str> {
    let present = |want: &str| labels.lines().any(|l| l.trim() == want);
    if !present("loom:pr") {
        return None;
    }
    BLOCKING.iter().copied().find(|b| present(b))
}

/// The refusal text, naming both labels and the head it was computed against.
#[must_use]
pub fn message(pr: &str, blocker: &str, labels: &str, head_sha: &str) -> String {
    let shown = if labels.trim().is_empty() {
        "<none>".to_string()
    } else {
        labels.trim().to_string()
    };
    format!(
        "Merge blocked: PR #{pr} carries both `loom:pr` and `{blocker}` simultaneously — a \
contradictory verdict state (see the mutual-exclusion invariant documented in \
.github/labels.yml). `loom:pr` means a reviewer approved this head; `{blocker}` means a \
reviewer (possibly a different, concurrent one) found a blocking problem, is re-requesting \
review, or the item is otherwise on hold. Merging now could ship an unreviewed or explicitly \
rejected change on the strength of a racing approval.\n\nCurrent labels: {shown}\nCurrent head \
SHA: {head_sha}\n\nGet a fresh Judge verdict on the current head, then re-run this merge once \
the contradiction is gone. There is no override flag for this guard: overriding an explicit \
rejection/hold is a different act from overriding a merely-missing review (which \
--allow-unapproved already covers), so this check is not bypassable by any flag (#8112)."
    )
}

#[cfg(test)]
mod tests;
