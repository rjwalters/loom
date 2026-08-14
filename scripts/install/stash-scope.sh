#!/usr/bin/env bash
# scripts/install/stash-scope.sh — scope the reinstall stash/reconcile guard
# to Loom-owned paths (issue #3597; issue #5289 added root CLAUDE.md; issue
# #6196 added root AGENTS.md).
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
# (`_emit_loom_ownership_set` from manifest.sh, plus `.gitignore`, root
# `CLAUDE.md`, and root `AGENTS.md` — all three rewritten by `loom-daemon init`
# but not enumerated by the defaults/ walk).
#
# Source with:
#     source "$LOOM_ROOT/scripts/install/stash-scope.sh"
#
# Public functions:
#   _emit_loom_ownership_paths <loom_root> <target>
#       One target-relative path per line: Loom's manifest ownership set plus
#       `.gitignore`, root `CLAUDE.md`, and root `AGENTS.md`. Missing
#       manifest.sh → just those three (loud caller fallback expected).
#
#   _emit_loom_owned_dirty_paths <loom_root> <target>
#       One target-relative path per line: the dirty set (unstaged ∪ staged
#       changes) intersected with the ownership set. Empty output means no
#       Loom-owned path is dirty → callers skip the stash entirely.

# Emit the Loom ownership set (manifest paths + .gitignore + root CLAUDE.md +
# root AGENTS.md), one per line.
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
  #
  # Issue #5289: root `CLAUDE.md` has the identical gap and a worse failure
  # mode. `_emit_installed_files_manifest` walks `defaults/` and translates
  # each file 1:1 (e.g. `defaults/.loom/CLAUDE.md` -> target `.loom/CLAUDE.md`,
  # the full-guide copy) -- but the root `CLAUDE.md`'s Loom section is
  # synthesized at install time from `LOOM_ROOT_POINTER`
  # (loom-daemon/src/init/scaffolding.rs), not copied from a literal
  # `defaults/CLAUDE.md` file, so the defaults/ walk never enumerates it and
  # it was silently absent from the ownership set entirely (unlike
  # `.gitignore`, which at least got this explicit carve-out). Without it,
  # `_emit_loom_owned_dirty_paths` never includes a dirty root `CLAUDE.md` in
  # the reinstall's pre-uninstall stash, so an uncommitted edit -- including
  # one made *inside* the `<!-- BEGIN/END LOOM ORCHESTRATION -->` marker
  # block -- sits unprotected in the working tree while
  # `scripts/uninstall-loom.sh`'s marker-based `sed` unconditionally deletes
  # the block (STEP 6 "Smart Remove CLAUDE.md"), destroying the edit before
  # `loom-daemon init --force` ever runs. No stash means no 3-way conflict to
  # surface later, so the loss is silent -- see the reproduction in #5289.
  #
  # Issue #6196: root `AGENTS.md` has the exact same gap. Its Loom section is
  # likewise synthesized at install time (from `AGENTS_ROOT_POINTER`, the same
  # `loom-daemon/src/init/scaffolding.rs` code path, with its own
  # `AGENTS_SECTION_START`/`AGENTS_SECTION_END` marker pair) rather than copied
  # from a literal `defaults/AGENTS.md` file, so it was likewise silently
  # absent from the ownership set -- meaning a repo-authored edit placed above
  # or below AGENTS.md's marker block (the very thing #6196 exists to make
  # possible: guidance an AGENTS.md-aware runtime can see) had no stash
  # protection across a `--quick` reinstall.
  printf '%s\n.gitignore\nCLAUDE.md\nAGENTS.md\n' "$ownership_set" | awk 'NF'
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
