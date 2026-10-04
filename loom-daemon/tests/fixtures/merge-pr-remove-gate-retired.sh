# merge-pr-remove-gate-retired.sh — the frozen, byte-for-byte copy of the
# #3710 primary-worktree hard guard and the `.loom-managed` sentinel guard
# (with its --worktree-path bypass) that opened merge-pr.sh's
# `_remove_loom_worktree`, as they stood immediately before the #8191 slice
# that ported them to `loom-daemon merge-pr remove-gate`
# (loom-daemon/src/merge_pr/remove_gate.rs).
#
# Not frozen here: the "lookup could not run" refusal. That became the shell
# wrapper's fail-closed branch (a missing/older daemon), which a differential
# against the Rust verb cannot reach by construction.
#
# The harness defines `info` / `warning` (printing `INFO<TAB>…` /
# `WARNING<TAB>…`) and `_primary_worktree_path` (the retired porcelain parse:
# the FIRST `worktree ` record, `substr($0, 10)`) BEFORE sourcing this file.
# Sourced, never executed.
#
# The block is wrapped in a function only so it can be called. The two edits to
# the frozen text are the function header and the trailing `echo`, which
# publishes "the block fell through" — a refusal `return 0`s before it.

_retired_remove_gate() {
  local worktree_path="$1" worktree_real="$2" allow_unmanaged="$3" primary_real
  # ---- frozen block (merge-pr.sh, pre-port) ----
  primary_real="$(_primary_worktree_path)"
  if [[ -n "$primary_real" ]] && [[ "$worktree_real" == "$primary_real" ]]; then
    warning "Refusing to remove the primary/main worktree at $worktree_real (never removable regardless of .loom-managed sentinel, branch, or worktree.root)"
    return 0
  fi
  if [[ "$allow_unmanaged" != "true" ]] && [[ ! -f "$worktree_path/.loom-managed" ]]; then
    warning "Worktree at $worktree_path lacks .loom-managed sentinel — refusing to remove (user-owned)"
    return 0
  fi
  if [[ "$allow_unmanaged" == "true" ]] && [[ ! -f "$worktree_path/.loom-managed" ]]; then
    info "Bypassing sentinel guard (--worktree-path explicit opt-in for $worktree_path)"
  fi
  # ---- end frozen block ----
  echo "LOOM-FELL-THROUGH"
}
