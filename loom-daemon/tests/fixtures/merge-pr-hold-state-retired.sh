#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's champion:hold-state marker extraction as it
# stood immediately before #8191 ported it to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_hold_state_differential.rs` can keep comparing
# the Rust against the exact implementation it replaced, forever, rather than
# only at the moment of the port. Reading the pipeline out of the live
# merge-pr.sh stopped being possible the instant that file started delegating;
# reading it from git history would pin the test to a moving ref.
#
# Only the EXTRACTION is frozen here, not the whole function: the surrounding
# `forge_get_pr_comments` call and the `warning` render are forge I/O and
# display, and stayed in the shell. The `hold_head != $PR_HEAD_SHA` comparison
# is a plain string inequality reproduced on the Rust side; the interesting —
# and defective — part is which SHA this pipeline picks.
#
# DO NOT "fix" anything here. Its value is being a faithful record of the
# retired behaviour, including the two defects the port deliberately corrects
# (an empty `[0-9a-f]*` capture from a documentation line winning `tail -1`,
# and a bare substring anywhere counting as recorded state). If the Rust
# should diverge from this, that is a deliberate behaviour change that belongs
# in its own issue, and this file should be left alone while the test's
# expectation is updated with a comment saying why.
#
# Reads the concatenated comment bodies on stdin (the live function had them
# in "$comments"); prints the recorded head, or nothing.
_retired_hold_head() {
  local comments hold_head
  comments="$(cat)"
  [[ -n "$comments" ]] || return 0

  hold_head="$(printf '%s\n' "$comments" \
    | grep -o 'champion:hold-state head=[0-9a-f]*' \
    | tail -1 \
    | sed -n 's/.*head=\([0-9a-f]*\)/\1/p')" || true
  [[ -n "$hold_head" ]] || return 0

  printf '%s\n' "$hold_head"
}
