#!/usr/bin/env bash
# FROZEN COPY of merge-pr.sh's #5031 dirty-worktree data-loss guard — its
# marker filter and its #5658 real-work-vs-artifact-churn classification — as
# both stood immediately before #8191 ported them to Rust.
#
# This is a TEST FIXTURE, not a live script. Nothing sources it in production.
#
# It exists so `tests/merge_pr_dirty_guard_differential.rs` can keep comparing
# the port against the exact implementation it replaced, forever, rather than
# only at the moment of the port. Reading the pipeline out of the live
# merge-pr.sh stopped being possible the instant that file started delegating;
# reading it from git history would pin the test to a moving ref.
#
# Only the two PREDICATES are frozen here, not the whole function. The
# `git status --porcelain` read stayed in the shell (see the port's module docs
# for why), and the `warning`/`echo` renders are display. What is interesting —
# and what was defective — is which lines count as user work and which of them
# look like genuine in-flight work, so those are what this file preserves.
#
# DO NOT "fix" anything here. Its value is being a faithful record of the
# retired behaviour, including the two defects the port deliberately corrects
# (a rename INTO a marker name filtered away as bookkeeping, and a marker under
# a git-quoted path counted as user work). If the Rust should diverge from this
# further, that is a deliberate behaviour change belonging in its own issue, and
# this file should be left alone while the test's expectation is updated with a
# comment saying why.

# _retired_dirty_lines — reads `git status --porcelain` on stdin, prints the
# lines the retired guard treated as USER WORK.
#
# Verbatim from `_remove_loom_worktree`, which ran this as
# `git -C "$worktree_path" status --porcelain 2>/dev/null | <these two greps>`
# inside a command substitution with `|| true`. `cat`'s place in the pipeline is
# git's; everything downstream is unchanged, `|| true` included — under
# `set -o pipefail` a `grep` that matches nothing exits 1 and would otherwise
# take the whole merge down.
_retired_dirty_lines() {
  grep -vE '[ /]\.loom-managed$|[ /]\.loom-in-use$|[ /]\.loom-checkpoint$|[ /]\.no-changes-needed$|[ /]\.snapshots/' \
    | grep -vE '^[[:space:]]*$' || true
}

# _retired_has_real_work — reads the ALREADY-FILTERED dirty lines on stdin
# (what `$dirty` held) and prints `true` / `false`: whether the retired guard
# offered the cross-host-duplicate-dispatch hypothesis (#5658).
_retired_has_real_work() {
  local dirty dirty_has_real_work=false dirty_line dirty_path
  dirty="$(cat)"
  [[ -n "$dirty" ]] || { printf 'false\n'; return 0; }
  while IFS= read -r dirty_line; do
    [[ -z "$dirty_line" ]] && continue
    dirty_path="${dirty_line:3}"
    # Rename entries look like "old -> new" — classify by the new path.
    [[ "$dirty_path" == *" -> "* ]] && dirty_path="${dirty_path##* -> }"
    case "$dirty_path" in
      *.lock | *-lock.json) ;; # trivial/generated-artifact pattern
      *) dirty_has_real_work=true ;;
    esac
  done <<<"$dirty"
  printf '%s\n' "$dirty_has_real_work"
}
