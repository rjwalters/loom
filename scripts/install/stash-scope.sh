#!/usr/bin/env bash
# scripts/install/stash-scope.sh — scope the reinstall stash/reconcile guard
# to Loom-owned paths (issue #3597).
#
# Both install.sh (`--quick` reinstall) and scripts/install-loom.sh (`--clean`)
# guard uncommitted user changes across the uninstall→reinstall cycle by
# stashing them first. The original guards ran an unscoped `git stash push`
# (no pathspec), which swept EVERY uncommitted tracked change in the tree —
# including sibling installers' work (`.anvil/install-metadata.json`,
# `.claude/skills/repo/install-metadata.json`, renamed-away `anvil:*` files,
# non-Loom CLAUDE.md sections) — into the stash. Untracked files were not
# stashed, leaving the reporter's half-old/half-new hybrid tree.
#
# This helper narrows the guard to paths Loom actually owns: the intersection
# of the dirty set (unstaged ∪ staged changes) with Loom's ownership set
# (`_emit_loom_ownership_set` from manifest.sh, plus `.gitignore`, which
# `loom-daemon init` rewrites but which is not part of the defaults/ walk).
#
# Source with:
#     source "$LOOM_ROOT/scripts/install/stash-scope.sh"
#
# Public functions:
#   _emit_loom_ownership_paths <loom_root> <target>
#       One target-relative path per line: Loom's manifest ownership set plus
#       `.gitignore`. Missing manifest.sh → just `.gitignore` (loud caller
#       fallback expected).
#
#   _emit_loom_owned_dirty_paths <loom_root> <target>
#       One target-relative path per line: the dirty set (unstaged ∪ staged
#       changes) intersected with the ownership set. Empty output means no
#       Loom-owned path is dirty → callers skip the stash entirely.

# Emit the Loom ownership set (manifest paths + .gitignore), one per line.
_emit_loom_ownership_paths() {
  local loom_root="$1"
  local target="$2"
  local ownership_set=""

  if [[ -f "$loom_root/scripts/install/manifest.sh" ]]; then
    # shellcheck source=/dev/null
    source "$loom_root/scripts/install/manifest.sh"
    ownership_set="$(LOOM_ROOT="$loom_root" TARGET_PATH="$target" \
      _emit_loom_ownership_set 2>/dev/null)"
  fi

  # `.gitignore` is rewritten by `loom-daemon init` (update_gitignore in
  # loom-daemon/src/init/post_init.rs) but is not enumerated by the defaults/
  # walk, so add it explicitly.
  printf '%s\n.gitignore\n' "$ownership_set" | awk 'NF'
}

# Emit dirty ∩ ownership-set, one target-relative path per line.
_emit_loom_owned_dirty_paths() {
  local loom_root="$1"
  local target="$2"

  # Dirty set: union of unstaged (working tree vs index) and staged
  # (index vs HEAD) changes. Staged deletions appear here too, which is
  # exactly what the reconcile step needs to unstage.
  local dirty_set
  dirty_set="$( { git -C "$target" diff --name-only 2>/dev/null; \
                  git -C "$target" diff --staged --name-only 2>/dev/null; } \
                | sort -u )"

  [[ -z "$dirty_set" ]] && return 0

  local ownership_set
  ownership_set="$(_emit_loom_ownership_paths "$loom_root" "$target")"
  [[ -z "$ownership_set" ]] && return 0

  # Intersect: first pass loads the ownership set into a map, second pass
  # prints dirty paths present in the map. Awk avoids a perl/python dep.
  awk 'NR==FNR { if ($0 != "") owned[$0]=1; next } { if ($0 != "" && ($0 in owned)) print }' \
    <(printf '%s\n' "$ownership_set") \
    <(printf '%s\n' "$dirty_set") \
    | sort -u
}
