#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's `_post_premature_close_comment` (#4569) as it
# stood immediately before #8191's slice moved its body to
# `loom-daemon/src/merge_pr/partial_comment.rs`.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# WHY THIS FILE EXISTS AND merge-pr-partial-reset-retired.sh DOES NOT COVER IT
#
# That fixture froze `_reset_one_partial_issue` whole, including the
# `## Partial Increment Merged` body it builds inline — so the OTHER comment
# this slice ports already has a frozen oracle, and
# `merge_pr_partial_comment_differential.rs` drives that fixture (with a
# RECORDING comment stub, where the partial-reset differential uses a silent
# one) rather than keeping a second copy of the same bytes here. Two frozen
# copies of one text can drift apart; one cannot.
#
# `_post_premature_close_comment` was never frozen anywhere: the partial-reset
# fixture calls it, and that differential's harness defines it as `{ :; }`.
# This file is its first and only frozen copy.
#
# WHAT IS VERBATIM, AND WHAT IS NOT
#
# Per defaults/docs/verification-recipes.md §6 ("say explicitly WHICH
# implementation it models"):
#
#   * VERBATIM, character for character: the whole function body — the `local`
#     line, the `date -u +%Y-%m-%dT%H:%M:%SZ` read, the `comment="…"` heredoc-
#     shaped assignment with every em dash, backtick, ellipsis and curly quote
#     in it, and the closing `forge_gh_comment_rl_safe … || warning …` post.
#     The assignment ends at its closing quote, so the body has NO trailing
#     newline; that is a property the differential asserts rather than assumes.
#
#   * OUT OF SCOPE: nothing. The function is copied whole, and the harness —
#     not an edit here — decides what is observed, by defining
#     `forge_gh_comment_rl_safe` as a recording stub, `warning` as a no-op, and
#     `date` as a fixed clock.
#
# DO NOT "fix" anything here. If the Rust should diverge from this, that is a
# deliberate behaviour change that belongs in its own issue, and this file
# should be left alone while the test's expectation is updated with a comment
# saying why.

# Audit trail for a reverted premature auto-close (#4569). Posted right after
# the reopen so the record survives even when the label swap below is skipped
# (e.g. the issue no longer carries loom:building). Best-effort.
_post_premature_close_comment() {
  local issue_num="$1" ts comment
  ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  comment="## Premature Auto-Close Reverted

PR #$PR_NUMBER referenced this issue with a **non-closing** \`Part of\` / \`Contributes to\` keyword — a declared partial increment, so this issue was meant to stay **open** after the merge. GitHub closed it anyway, because a **closing keyword** (\`close\`/\`fix\`/\`resolve\` and their tense variants) immediately followed by \`#$issue_num\` appeared elsewhere in the PR — in the body, or in one of the PR's commit messages (this merge squashes without overriding the commit message, so GitHub composes the squash message from those commits).

GitHub honors a closing keyword **anywhere** in a PR body or squash commit message — not only in a line-leading trailer — so prose like \"…then close #$issue_num\" in a follow-up checklist, or a stray \`close #$issue_num\` in a commit message, creates a real closing link that overrides the intended \`Contributes to #$issue_num\`.

**Action taken**: reopened this issue.

**To avoid this**: never put a closing keyword immediately before \`#$issue_num\` anywhere in a partial-increment PR's body **or commit messages**. Write \`close the issue\` or \`close issue #$issue_num\` instead of \`close #$issue_num\`.

---
*Reopened by merge-pr.sh (#4569) at $ts*"
  # forge_gh_comment_rl_safe (#4856): REST fallback on GraphQL rate limit.
  forge_gh_comment_rl_safe "$REPO_NWO" "$issue_num" "$comment" 2>/dev/null || \
    warning "Could not post premature-close comment on issue #$issue_num (reopen still applied)"
}
