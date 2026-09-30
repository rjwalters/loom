//! The two post-merge partial-increment AUDIT COMMENTS (#3667 / #4569), a
//! slice of the merge-pr port #8191.
//!
//! # What this owns
//!
//! [`super::partial_reset`] already owns the *decision* the post-merge
//! partial-increment pass makes — reopen, swap, or leave alone. What stayed in
//! `merge-pr.sh` after that slice was the pass's two operator-facing comment
//! bodies, ~40 lines of heredoc-shaped text inside the mutation arms:
//!
//! * [`partial_merged_comment`] — "## Partial Increment Merged", posted after
//!   the `loom:building` → `loom:issue` swap succeeds, with a conditional
//!   first bullet when the same pass had to reopen the issue first.
//! * [`premature_close_comment`] — "## Premature Auto-Close Reverted", posted
//!   right after a #4569 reopen, and deliberately *before* the swap, so the
//!   record survives even when the swap is skipped.
//!
//! This module is [`super::reconcile::defer_comment`]'s sibling: same family,
//! same reason. That module already established that a byte-frozen
//! operator-visible comment belongs beside the decision that emits it rather
//! than inside a shell file nothing can hold a fixture against.
//!
//! # Why the text is worth porting, not just the logic around it
//!
//! These two bodies are the entire operator-facing explanation of Loom's most
//! confusing merge outcome — an issue that closed itself and then reopened —
//! and [`premature_close_comment`] is also *instructional*: it is where a
//! Builder is told the rule (never put a closing keyword immediately before
//! `#N` in a partial-increment PR) that stops the bug recurring. Until this
//! slice not one assertion anywhere held either body: the retained suite
//! stubs the comment post, and
//! `tests/merge_pr_partial_reset_differential.rs`'s harness stubs
//! `forge_gh_comment_rl_safe` to `:` and `_post_premature_close_comment` to
//! `:`, precisely so the decision under test is not drowned in prose. So the
//! bodies could have been silently mangled — a lost `$reopen_note`, an
//! interpolation that stopped interpolating — by any edit to the file, and
//! nothing would have failed.
//!
//! They are also the file's largest remaining block of pure data, in a file
//! the size ratchet freezes (`scripts/file-size-baseline.txt`) and which
//! therefore cannot afford to spend code lines on text.
//!
//! # Fail direction
//!
//! Fail OPEN, and the wrapper's refusal must be a *warning*, never a block.
//! Both comments are audit trails posted **after** the mutation they describe
//! — the reopen and the label swap have already happened, and the merge itself
//! happened long before that. A daemon that cannot render the body leaves the
//! issue in exactly the state the mutations put it in; only the note is
//! missing, and the warning names it.
//!
//! The one thing the wrapper must not do is post *silence*. An old binary
//! exits non-zero and prints nothing, and a caller that read that as "the
//! body" would post an empty comment over the real explanation. That is why
//! the CLI leads its output with a sentinel line (see
//! `cli::merge_pr_partial_comment`) and the shell only posts what follows a
//! sentinel it recognises.
//!
//! # Byte fidelity
//!
//! Both bodies end **without** a trailing newline: the retired shell built
//! them as `comment="…"` and the string ended at the closing quote. Both are
//! held byte-for-byte against the retired shell by
//! `loom-daemon/tests/merge_pr_partial_comment_differential.rs` — the
//! partial-merged body against the already-frozen
//! `tests/fixtures/merge-pr-partial-reset-retired.sh` (driven with a
//! *recording* comment stub instead of that differential's silent one), and
//! the premature-close body against
//! `tests/fixtures/merge-pr-partial-comment-retired.sh`, a frozen copy of the
//! one function no fixture carried yet.

/// The UTC instant both comments sign off with, in the retired shell's own
/// `date -u +%Y-%m-%dT%H:%M:%SZ` format.
///
/// Second resolution, `Z` suffix, no fractional part — the format is part of
/// the byte-frozen text, so it is spelled here once rather than at the two
/// call sites.
#[must_use]
pub fn now_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The `## Partial Increment Merged` body (#3667), byte-frozen from the
/// retired shell.
///
/// `reopened` is the retired `$reopened` local: true only when THIS pass had
/// to revert a #4569 premature auto-close before the swap. It selects the
/// conditional `$reopen_note` bullet, which is a leading-newline fragment
/// interpolated directly after `**Action taken**:` — so the false case leaves
/// that heading and the first real bullet on adjacent lines, exactly as the
/// shell's empty `reopen_note=""` did.
///
/// Written as one wide literal with real newlines rather than backslash
/// continuations, for the reason [`super::reconcile::defer_comment`] gives:
/// this is Markdown, a hard wrap would be a rendering change, and a
/// continuation's joining space is invisible in review.
///
/// There is deliberately **no** issue-number parameter: the retired body names
/// only the PR. It is posted *on* the issue, so the issue identifies itself,
/// and inventing a reference here would be a text change smuggled in under a
/// port.
#[must_use]
pub fn partial_merged_comment(pr_number: &str, reopened: bool, timestamp: &str) -> String {
    let reopen_note = if reopened {
        format!(
            "\n- **Reopened** this issue (GitHub had auto-closed it from a stray closing keyword in PR #{pr_number}'s body or one of its commit messages — see #4569)"
        )
    } else {
        String::new()
    };
    format!(
        "## Partial Increment Merged

PR #{pr_number} merged with a non-closing `Part of` / `Contributes to` reference, so this issue remains **open** for further work.

**Action taken**:{reopen_note}
- Removed `loom:building` label
- Added `loom:issue` label to return to the ready queue

This issue is now available for the next increment (a subsequent `/loom:sweep` will treat it as ready rather than in-flight).

---
*Reset by merge-pr.sh (#3667) at {timestamp}*"
    )
}

/// The `## Premature Auto-Close Reverted` body (#4569), byte-frozen from the
/// retired shell.
///
/// Posted immediately after the reopen, before the label swap is even
/// attempted — the retired call site is inside the `REOPEN` arm, not the
/// `SWAP` one, so a run whose swap fails still leaves this record behind.
///
/// `issue_number` appears five times, and every occurrence is inside prose
/// that is explaining the `close #N` hazard to the Builder who caused it, so
/// the interpolation is load-bearing rather than decorative.
#[must_use]
pub fn premature_close_comment(issue_number: &str, pr_number: &str, timestamp: &str) -> String {
    format!(
        "## Premature Auto-Close Reverted

PR #{pr_number} referenced this issue with a **non-closing** `Part of` / `Contributes to` keyword — a declared partial increment, so this issue was meant to stay **open** after the merge. GitHub closed it anyway, because a **closing keyword** (`close`/`fix`/`resolve` and their tense variants) immediately followed by `#{issue_number}` appeared elsewhere in the PR — in the body, or in one of the PR's commit messages (this merge squashes without overriding the commit message, so GitHub composes the squash message from those commits).

GitHub honors a closing keyword **anywhere** in a PR body or squash commit message — not only in a line-leading trailer — so prose like \"…then close #{issue_number}\" in a follow-up checklist, or a stray `close #{issue_number}` in a commit message, creates a real closing link that overrides the intended `Contributes to #{issue_number}`.

**Action taken**: reopened this issue.

**To avoid this**: never put a closing keyword immediately before `#{issue_number}` anywhere in a partial-increment PR's body **or commit messages**. Write `close the issue` or `close issue #{issue_number}` instead of `close #{issue_number}`.

---
*Reopened by merge-pr.sh (#4569) at {timestamp}*"
    )
}

#[cfg(test)]
mod tests;
