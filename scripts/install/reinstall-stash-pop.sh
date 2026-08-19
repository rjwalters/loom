#!/usr/bin/env bash
# scripts/install/reinstall-stash-pop.sh — install.sh's reinstall-time
# `git stash pop --index` against the consumer's PRIMARY checkout
# ($TARGET_PATH), routed through defaults/scripts/safe-stash-pop.sh (#6501) so
# a conflicting pop can never leave conflict markers / unmerged index entries
# behind (#6509, follow-up to the #6499/#6502 incident: commit 7d169a06
# shipped a `.loom/config.json` full of conflict markers because the pop's
# conflict branch reported well but never cleaned up after itself).
#
# Source with:
#     source "$LOOM_ROOT/scripts/install/reinstall-stash-pop.sh"
#
# Public function:
#   _reinstall_safe_stash_pop <loom_root> <target> [<stash_ref>]
#
#     Pops <stash_ref> (default stash@{0}) with --index against <target>
#     (preserving the #3611 staged/unstaged-split guarantee on the clean
#     path). Wrapper resolution order:
#
#       1. "<target>/.loom/scripts/safe-stash-pop.sh" — by the time this runs,
#          the reinstall has already copied defaults/scripts/ into
#          <target>/.loom/scripts/, so this is normally present.
#       2. "<loom_root>/defaults/scripts/safe-stash-pop.sh" — the source
#          tree's copy, for a --local/dogfood/dev-install path where step 1
#          has not (yet) run.
#       3. Neither found: fall back to a raw `git stash pop --index` with a
#          warning rather than failing the install outright (an older
#          install, a partial tree, or a curl-piped standalone install that
#          predates #6501). This is the one path that can still leave
#          conflict markers behind — it reproduces today's pre-#6509
#          behavior exactly, on purpose, since there is nothing here to
#          route through.
#
#     Sets on return (globals — never `local`, read by the caller):
#
#       REINSTALL_POP_STATUS   0 on a fully successful restore, non-zero
#                               otherwise. Mirrors a plain `git stash pop`'s
#                               exit convention so existing call sites that
#                               branch on `[[ $REINSTALL_POP_STATUS -eq 0 ]]`
#                               need no other change.
#       REINSTALL_POP_OUTPUT    Human-readable report. In wrapper mode this is
#                               safe-stash-pop.sh's own combined stdout+stderr
#                               (git's raw pop output, the conflicting-file
#                               names, and — on the restored/unsafe paths — its
#                               own recovery recipe). In raw mode this is
#                               `git stash pop --index`'s own output, exactly
#                               as before #6509.
#       REINSTALL_POP_MODE      "wrapper" or "raw" — which path ran.
#       REINSTALL_POP_RESULT    One of:
#         "clean"        — popped cleanly (wrapper-verified or raw exit 0).
#         "restored"     — wrapper only: conflicted, but the pre-pop tree was
#                           restored and VERIFIED marker-free; the stash entry
#                           is preserved. The safe, expected conflict outcome.
#         "unsafe"        — wrapper only: conflicted and the wrapper could not
#                           safely verify/complete an automatic rollback (a
#                           pre-existing abandoned-conflict state, a mid-merge
#                           repo, or the rare rollback-itself-failed case). The
#                           wrapper's own contract guarantees nothing was
#                           silently discarded, but the tree may still carry
#                           the conflict as git left it — callers MUST NOT
#                           assume it is marker-free and must not overwrite
#                           anything on top of it blind.
#         "no_stash"      — wrapper only: no stash entry was found at
#                           <stash_ref>. No pop ran, so nothing could have
#                           been left conflicted; unexpected in the reinstall
#                           flow (a stash was just pushed) but handled, not
#                           fatal.
#         "raw_conflict"  — raw mode only: the fallback raw pop conflicted.
#                           Reproduces the pre-#6509 conflict branch exactly.
#       REINSTALL_POP_WRAPPER   Absolute path to the wrapper actually used, or
#                               "" in raw mode.
_reinstall_safe_stash_pop() {
  local loom_root="$1" target="$2" stash_ref="${3:-stash@{0}}"

  REINSTALL_POP_STATUS=1
  REINSTALL_POP_OUTPUT=""
  REINSTALL_POP_MODE="raw"
  REINSTALL_POP_RESULT="raw_conflict"
  REINSTALL_POP_WRAPPER=""

  local wrapper=""
  if [[ -x "$target/.loom/scripts/safe-stash-pop.sh" ]]; then
    wrapper="$target/.loom/scripts/safe-stash-pop.sh"
  elif [[ -x "$loom_root/defaults/scripts/safe-stash-pop.sh" ]]; then
    wrapper="$loom_root/defaults/scripts/safe-stash-pop.sh"
  fi

  if [[ -n "$wrapper" ]]; then
    REINSTALL_POP_MODE="wrapper"
    REINSTALL_POP_WRAPPER="$wrapper"
    local wrapper_output wrapper_status
    wrapper_output="$("$wrapper" --repo "$target" --index "$stash_ref" 2>&1)"
    wrapper_status=$?
    REINSTALL_POP_OUTPUT="$wrapper_output"
    case "$wrapper_status" in
      0)
        REINSTALL_POP_STATUS=0
        REINSTALL_POP_RESULT="clean"
        ;;
      3)
        # Conflict; the wrapper restored + verified the pre-pop tree (no
        # markers, no unmerged entries) and kept the stash entry. The user's
        # change is still unreconciled -- same as today's conflict branch --
        # but the checkout itself is provably clean.
        REINSTALL_POP_STATUS=3
        REINSTALL_POP_RESULT="restored"
        ;;
      2)
        # No stash entry at $stash_ref. Unexpected here (the caller only
        # invokes this right after pushing one) but not itself unsafe: no pop
        # ran, so nothing could have been left in a conflicted state.
        REINSTALL_POP_STATUS=2
        REINSTALL_POP_RESULT="no_stash"
        ;;
      *)
        # 1 (precondition refused -- e.g. an already-unmerged index or a
        # mid-merge state), 4 (conflict + rollback could not be verified), or
        # 5 (--no-restore -- this caller never passes it, but treat
        # defensively the same way if it is ever seen): the wrapper's own
        # contract guarantees nothing was silently discarded, but the tree is
        # left exactly as git left it, which may still carry the conflict.
        # Callers must not overwrite anything on top of an unverified state.
        REINSTALL_POP_STATUS="$wrapper_status"
        REINSTALL_POP_RESULT="unsafe"
        ;;
    esac
    return 0
  fi

  # Fallback: neither copy of safe-stash-pop.sh is available (older install,
  # partial tree, or a curl-piped standalone install predating #6501). Fall
  # back to today's raw pop with a warning rather than failing the install.
  REINSTALL_POP_MODE="raw"
  local raw_output raw_status
  if raw_output="$(git -C "$target" stash pop --index "$stash_ref" 2>&1)"; then
    raw_status=0
  else
    raw_status=$?
  fi
  REINSTALL_POP_OUTPUT="$raw_output"
  REINSTALL_POP_STATUS="$raw_status"
  if [[ "$raw_status" -eq 0 ]]; then
    REINSTALL_POP_RESULT="clean"
  else
    REINSTALL_POP_RESULT="raw_conflict"
  fi
  return 0
}
