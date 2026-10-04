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
//! stdout/exit code. The shell side still owns one thing this module
//! deliberately does NOT: the champion:hold-state staleness warning (a
//! separate, independent check that only fires when `loom:pr` IS present —
//! see `_check_champion_hold_state_staleness` in `merge-pr.sh`) — that is
//! forge I/O, not a decision. [`override_comment`] below is the audit-comment
//! BODY posted on a real (non-dry-run) override (a later #8191 slice than the
//! rest of this module); the shell still owns the POST itself.

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

/// The `--allow-unapproved` audit-comment BODY (#7419), posted on the PR
/// after a REAL (non-dry-run) override — byte-frozen from the retired shell's
/// `_check_loom_pr_label`, a later #8191 slice than [`assess`]/[`Verdict`]
/// above. See `cli::merge_pr_loom_pr_override_comment` for the stdout
/// sentinel protocol this is rendered behind, matching
/// `super::partial_comment`'s `LOOM-MERGE-PR-COMMENT` convention.
///
/// `labels` renders EXACTLY as the retired shell's `${PR_LABELS:-<none>}`
/// did: only an EMPTY string substitutes the placeholder. Deliberately NOT
/// [`shown`] above, which `.trim()`s for the inline WARNING text — the
/// retired shell used two different expansions for its two different
/// strings (a bare `${VAR:-default}` here, versus nothing shown trims for in
/// `override_message`), and the port keeps that distinction rather than
/// unifying them under one newer helper that would change this body's bytes
/// for a whitespace-only label line.
#[must_use]
pub fn override_comment(pr_number: &str, head_sha: &str, labels: &str, timestamp: &str) -> String {
    let labels_shown = if labels.is_empty() { "<none>" } else { labels };
    format!(
        "## Merge Proceeded Without `loom:pr` (Override)\n\n\
PR #{pr_number} was merged via `merge-pr.sh --allow-unapproved` while the `loom:pr` label was \
absent — no forge-visible Judge review signal existed for the head being merged.\n\n\
- **Head SHA**: `{head_sha}`\n\
- **Labels at merge time**: {labels_shown}\n\n\
The operator running this merge explicitly asserted responsibility for this override (#7419).\n\n\
---\n\
*Recorded by merge-pr.sh at {timestamp}*"
    )
}

#[cfg(test)]
mod tests;
