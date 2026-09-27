#!/usr/bin/env bash
# FROZEN COPY — do not "fix", do not sync with merge-pr.sh.
#
# `defaults/scripts/merge-pr.sh`'s merge-retry classification ladder exactly as
# it stood immediately before #8191 slice ported it to
# `loom-daemon/src/merge_pr/response.rs`. `merge_pr_response_differential.rs`
# feeds this and the Rust the same corpus and requires identical answers.
#
# WHY A COPY AND NOT THE LIVE SCRIPT
#
# The live ladder now delegates to `loom-daemon merge-pr classify-response`, so
# reading it from `merge-pr.sh` would compare the port against itself and pass
# unconditionally — the "measured nothing" failure
# `defaults/docs/verification-recipes.md` §6 names. Reading it out of git
# history instead would pin this test to a moving ref. A frozen copy is the only
# form that keeps saying something true once the shell is gone.
#
# WHAT THIS MODELS, AND WHAT IT DOES NOT — READ THIS BEFORE TRUSTING IT
#
# Per the same recipe ("If the test needs its own copy of the pattern, the
# parser, or the ordering rule, say explicitly WHICH implementation it models"):
# this models the **retired** implementation, and it is a *reconstruction* of an
# inline ladder rather than a verbatim copy of a function body, because the
# retired ladder was not a function. The distinction matters, so it is spelled
# out precisely:
#
#   * VERBATIM: all three matcher invocations. `_is_head_mismatch_response`'s
#     body is copied character for character (including `-Eiq`, the alternation
#     order and the escaped `\.`), and the two siblings are the exact
#     `echo "$MERGE_RESPONSE" | grep -q "…"` lines from the retry loop — note
#     the bare `grep -q`, i.e. case-SENSITIVE, which the `-Ei` matcher is not.
#
#   * RECONSTRUCTED: the `if`/`elif` ORDER. In `merge-pr.sh` these were three
#     separate `if` blocks inside the `for MERGE_ATTEMPT` loop, at (pre-port)
#     lines 2288, 2313 and 2318 — 405 first, then head-mismatch, then
#     base-modified. That order was the safety property and the only thing
#     asserting it was an `awk` scan over the script's own source text. It is
#     reproduced here as an explicit ladder so a differential can measure it.
#
#   * OUT OF SCOPE: what each route then DOES. The 405 route slept, re-read
#     `.merged` and continued; the base-modified route called
#     forge_update_branch and retried; the head-mismatch route went through
#     _head_moved_or_resync (#8164). None of that moved in the port — it is all
#     still in `merge-pr.sh` — so none of it is modelled here. This fixture
#     answers only "which route", which is exactly what the port replaced.
#
#   * ALSO OUT OF SCOPE: the `.merged` recheck that ran BEFORE all three (a
#     forge round-trip, not a string test), and the `MERGE_ATTEMPT -lt
#     MAX_MERGE_RETRIES` bound inside the base-modified arm. Both are control
#     flow around the classification, not part of it.
#
# `echo "$1"` is kept rather than modernised to `printf '%s'`. bash's `echo`
# swallows an argument that is exactly `-n` / `-e` / `-E`, and that quirk was
# part of the retired behaviour; the corpus includes those three inputs so the
# port is checked against it rather than assumed equivalent.

# retired_classify_merge_response <response-text>
# Prints one of: merge-in-progress | head-mismatch | base-modified | other
retired_classify_merge_response() {
  # VERBATIM from merge-pr.sh's `_is_head_mismatch_response`.
  _is_head_mismatch_response() {
    echo "$1" | grep -Eiq 'Head branch was modified\.|head out of date|expectedHeadOid'
  }

  local MERGE_RESPONSE="$1"

  # VERBATIM matcher, RECONSTRUCTED position: first of the three.
  if echo "$MERGE_RESPONSE" | grep -q "Merge already in progress"; then
    echo "merge-in-progress"
    return 0
  fi

  # VERBATIM matcher, RECONSTRUCTED position: before "Base branch was
  # modified". Reordering these two is the divergence this whole file exists to
  # be able to detect.
  if _is_head_mismatch_response "$MERGE_RESPONSE"; then
    echo "head-mismatch"
    return 0
  fi

  # VERBATIM matcher, RECONSTRUCTED position: third.
  if echo "$MERGE_RESPONSE" | grep -q "Base branch was modified"; then
    echo "base-modified"
    return 0
  fi

  # The loop's terminal `error "Failed to merge PR #…: $MERGE_RESPONSE"`.
  echo "other"
}
