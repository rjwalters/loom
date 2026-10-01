#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's `_check_loom_pr_label`'s `--allow-unapproved`
# override audit-comment body (#7419) as it stood immediately before #8191's
# slice moved it to `loom-daemon/src/merge_pr/loom_pr_guard.rs`
# (`override_comment`).
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# WHAT IS VERBATIM, AND WHAT IS NOT
#
# Per defaults/docs/verification-recipes.md §6 ("say explicitly WHICH
# implementation it models"):
#
#   * VERBATIM, character for character: the `comment="…"` heredoc-shaped
#     assignment — the `## Merge Proceeded Without \`loom:pr\` (Override)`
#     heading, every em dash and backtick, the `${PR_LABELS:-<none>}`
#     substitution (UN-trimmed — only the empty string substitutes), and the
#     `date -u +%Y-%m-%dT%H:%M:%SZ` sign-off read — plus the closing
#     `forge_gh_comment_rl_safe` post. The assignment ends at its closing
#     quote, so the body has NO trailing newline.
#
#   * RECONSTRUCTED, not verbatim: this is extracted as its OWN function
#     (`_frozen_loom_pr_override_comment`), not the whole
#     `_check_loom_pr_label` — the retired code built the comment inline
#     inside that function's `--allow-unapproved` branch, guarded by
#     `$DRY_RUN != true` and a once-per-run dedup flag neither of which is
#     part of the comment BODY itself (both stay shell-side after the port,
#     unchanged). The extraction changes no byte of the body; only where the
#     three statements (`local override_comment=…`, the post, nothing else)
#     live. The differential drives it with the same PR_NUMBER / PR_HEAD_SHA /
#     PR_LABELS globals the inline version read.
#
# DO NOT "fix" anything here. If the Rust should diverge from this, that is a
# deliberate behaviour change that belongs in its own issue, and this file
# should be left alone while the test's expectation is updated with a comment
# saying why.

_frozen_loom_pr_override_comment() {
  local override_comment="## Merge Proceeded Without \`loom:pr\` (Override)

PR #$PR_NUMBER was merged via \`merge-pr.sh --allow-unapproved\` while the \`loom:pr\` label was absent — no forge-visible Judge review signal existed for the head being merged.

- **Head SHA**: \`$PR_HEAD_SHA\`
- **Labels at merge time**: ${PR_LABELS:-<none>}

The operator running this merge explicitly asserted responsibility for this override (#7419).

---
*Recorded by merge-pr.sh at $(date -u +%Y-%m-%dT%H:%M:%SZ)*"
  forge_gh_comment_rl_safe "$REPO_NWO" "$PR_NUMBER" "$override_comment"
}
