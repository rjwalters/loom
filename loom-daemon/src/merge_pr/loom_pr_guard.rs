//! The pre-merge `loom:pr` review-signal guard (#7419, slice N of #8191).
//!
//! # What this guards
//!
//! `loom:pr` is the only forge-visible statement that the CURRENT head passed
//! Judge review. A real incident (#7419) showed the gap: a Doctor rebase
//! cleared `loom:pr` via the staleness guard, and a human running
//! `merge-pr.sh` directly moments later squash-merged a head no Judge had
//! reviewed, with zero friction at the one point it was cheap. This guard
//! closes that gap — by default it hard-blocks a merge whose current head
//! does not carry `loom:pr`, naming the label set and head SHA so the
//! operator can see exactly what they are about to merge.
//!
//! `--allow-unapproved` overrides the block (the operator asserts
//! responsibility for merging an unreviewed head), mirroring
//! `--allow-stacked-children` and `--worktree-path`'s own override
//! precedent — always recorded as a loud warning.
//!
//! # Two different acts, two different overrides
//!
//! This guard only ever fires on `loom:pr`'s ABSENCE — "nobody reviewed
//! this head". [`super::labels`] fires on a PRESENT `loom:pr` standing next
//! to a contradicting label — "a reviewer explicitly said no". Those are
//! different acts: only the first has a documented override
//! (`--allow-unapproved`); the second has none, deliberately.
//!
//! # Contract
//!
//! `assess` takes the label set and `allow_unapproved`, and returns a
//! [`Verdict`] with no forge I/O of its own — the caller (`cli::
//! merge_pr_loom_pr_guard`) supplies the label set (already fetched into the
//! shell's `$PR_LABELS`, piped over stdin — a label is forge-controlled text
//! and belongs on a stream, not in argv) and renders the verdict to
//! stdout/exit code. The shell side still owns two things this module
//! deliberately does NOT: the champion:hold-state staleness warning (a
//! separate, independent check that only fires when `loom:pr` IS present —
//! see `_check_champion_hold_state_staleness` in `merge-pr.sh`) and the
//! audit-comment body posted on a real (non-dry-run) override — both are
//! forge I/O / display plumbing, not the decision itself.

/// The only stdout a caller may treat as "loom:pr is present, proceed".
///
/// A sentinel rather than silence, for the same reason
/// [`super::labels::CLEAN`] is: passing requires a POSITIVE signal, so a
/// missing, old, or substituted binary cannot produce a pass by falling over
/// quietly.
pub const CLEAN: &str = "LOOM-PR-GUARD-CLEAN";

/// This guard's decision over one PR's label set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// `loom:pr` is present — nothing to do here (the hold-state staleness
    /// check, if any, is the shell caller's job).
    Approved,
    /// `loom:pr` is absent, but the operator explicitly asserted
    /// responsibility via `--allow-unapproved`. Carries the warning text to
    /// display (never a hard block).
    Overridden(String),
    /// `loom:pr` is absent and there is no override: the merge must be
    /// refused. Carries the refusal text.
    Blocked(String),
}

/// True when `labels` (newline-separated, as both forges' `labels[].name`
/// renders) carries `loom:pr` as a whole line — `grep -qx`, not a substring
/// match, matching the shell this ports from (`loom:private` must not count).
fn has_loom_pr(labels: &str) -> bool {
    labels.lines().any(|l| l.trim() == "loom:pr")
}

/// The label set rendered for a message: `<none>` rather than a blank line
/// when it is empty, matching `${PR_LABELS:-<none>}` in the shell this ports
/// from (which triggers on unset OR empty).
fn shown(labels: &str) -> String {
    let trimmed = labels.trim();
    if trimmed.is_empty() {
        "<none>".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Decide this guard's verdict. No forge I/O — a pure function of the inputs
/// the caller already has.
#[must_use]
pub fn assess(pr: &str, labels: &str, head_sha: &str, allow_unapproved: bool) -> Verdict {
    if has_loom_pr(labels) {
        return Verdict::Approved;
    }
    if allow_unapproved {
        return Verdict::Overridden(override_message(labels, head_sha));
    }
    Verdict::Blocked(block_message(pr, labels, head_sha))
}

/// The override warning: printed on every `--allow-unapproved` run
/// (dry-run or real), naming the label set and head SHA.
fn override_message(labels: &str, head_sha: &str) -> String {
    format!(
        "loom:pr guard: --allow-unapproved set; proceeding without loom:pr (labels: {}; head: \
{head_sha}) — operator asserts responsibility for merging an unreviewed head",
        shown(labels)
    )
}

/// The hard-block refusal: names the label set and head SHA, and the
/// `--allow-unapproved` escape hatch.
fn block_message(pr: &str, labels: &str, head_sha: &str) -> String {
    format!(
        "Merge blocked: PR #{pr} does not carry the `loom:pr` label — no forge-visible signal \
exists that Judge reviewed the CURRENT head.\n\nCurrent labels: {}\nCurrent head SHA: \
{head_sha}\n\nloom:pr may have been cleared by a staleness guard (e.g. after a Doctor rebase \
moved the head) or never applied. Get the PR (re-)reviewed by Judge and re-labeled loom:pr, \
then re-run this merge.\n\nIf you are deliberately merging without that review signal and take \
responsibility for it, re-run with --allow-unapproved to bypass this guard.",
        shown(labels)
    )
}

#[cfg(test)]
mod tests;
