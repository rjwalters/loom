#!/usr/bin/env bash
# scripts/install/local-mode.sh — Loom "local" (gitignore/untracked) install mode
# helper (issue #3836).
#
# `install-loom.sh --local` (alias `--gitignore`) installs Loom into a consumer
# repo WITHOUT publishing/duplicating the Loom implementation in that repo's git
# history. This helper provides the two self-contained primitives that mode needs:
#
#   1. loom_local_apply_gitignore <target>
#        Idempotently write a Loom-managed block of the installed implementation
#        paths into <target>/.gitignore (marker-delimited, refreshed in place).
#
#   2. loom_local_tracked_paths / loom_local_untrack_commands / loom_local_run_untrack
#        Detect which of those paths git already tracks (gitignore alone does not
#        stop tracking already-committed files) and either print or run the
#        `git rm -r --cached` commands to untrack them.
#
# The helper is deliberately isolated in its own file (matching manifest.sh,
# stash-scope.sh, dogfood-commands.sh) so the test suite can exercise it without
# sourcing the full installer, which has argv-parsing side effects.
#
# Source this file with:
#     source "$LOOM_ROOT/scripts/install/local-mode.sh"
#
# Bash 3.2 compatible (stock macOS) — no associative arrays, no mapfile.

# Marker sentinels for the Loom-managed local-mode block. Distinct from the
# daemon's runtime-state block ("# >>> loom-managed (do not edit) >>>", written
# by loom-daemon's update_gitignore) so the two blocks never collide: the daemon
# refreshes its block in place on every init and would otherwise strip these
# implementation paths back out.
LOOM_LOCAL_GITIGNORE_BEGIN="# >>> loom-local install (do not edit) >>>"
LOOM_LOCAL_GITIGNORE_END="# <<< loom-local install <<<"
LOOM_LOCAL_GITIGNORE_HEADER="# Loom implementation files — installed via 'install-loom.sh --local', not committed to this repo (#3836)"

# The installed Loom implementation paths kept out of version control in local
# mode. Anchored with a leading '/' so they only match at the repo root.
#
# Deliberately excludes genuinely project-specific config that SHOULD stay
# tracked and shared across a team — notably `.github/labels.yml` (not under any
# ignored path here). `.loom/config.json` lives under the ignored `/.loom/`
# tree; moving project-specific config under a future `.loom-project/` is left to
# a follow-up increment (see the issue's scope notes).
LOOM_LOCAL_IGNORE_PATTERNS=(
  "/.loom/"
  "/.claude/commands/loom/"
  "/.claude/agents/loom-*.md"
  "/.loom-local/"
)

# Concrete pathspecs used to detect + untrack already-committed implementation
# files. Directories are removed recursively; the agents entry is a git
# pathspec glob (git expands it, not the shell).
LOOM_LOCAL_UNTRACK_PATHS=(
  ".loom"
  ".claude/commands/loom"
  ".claude/agents/loom-*.md"
  ".loom-local"
)

# Emit the full Loom-managed local-mode block (begin marker, header, patterns,
# end marker), one line each, with a trailing newline.
_loom_local_block() {
  printf '%s\n' "$LOOM_LOCAL_GITIGNORE_BEGIN"
  printf '%s\n' "$LOOM_LOCAL_GITIGNORE_HEADER"
  local _p
  for _p in "${LOOM_LOCAL_IGNORE_PATTERNS[@]}"; do
    printf '%s\n' "$_p"
  done
  printf '%s\n' "$LOOM_LOCAL_GITIGNORE_END"
}

# Idempotently write the managed block into <target>/.gitignore.
#   - No .gitignore            → create one containing only the block.
#   - Block already present    → replace it in place (refresh).
#   - Block absent, file exists → append, separated by one blank line.
# Returns non-zero on any filesystem failure.
loom_local_apply_gitignore() {
  local target="$1"
  local gitignore="$target/.gitignore"

  if [[ ! -f "$gitignore" ]]; then
    _loom_local_block > "$gitignore"
    return $?
  fi

  # Strip any existing managed block so the refresh is idempotent.
  local tmp
  tmp="$(mktemp)" || return 1
  awk -v begin="$LOOM_LOCAL_GITIGNORE_BEGIN" -v end="$LOOM_LOCAL_GITIGNORE_END" '
    $0 == begin { inblock=1; next }
    inblock && $0 == end { inblock=0; next }
    inblock { next }
    { print }
  ' "$gitignore" > "$tmp" || { rm -f "$tmp"; return 1; }

  # Drop trailing blank lines so we fully control the spacing before the block.
  local tmp2
  tmp2="$(mktemp)" || { rm -f "$tmp"; return 1; }
  awk '
    { lines[NR] = $0 }
    END {
      last = NR
      while (last > 0 && lines[last] ~ /^[[:space:]]*$/) last--
      for (i = 1; i <= last; i++) print lines[i]
    }
  ' "$tmp" > "$tmp2" || { rm -f "$tmp" "$tmp2"; return 1; }
  rm -f "$tmp"

  # Separate the block from any preceding content with a single blank line.
  if [[ -s "$tmp2" ]]; then
    printf '\n' >> "$tmp2"
  fi
  _loom_local_block >> "$tmp2" || { rm -f "$tmp2"; return 1; }
  mv "$tmp2" "$gitignore"
}

# Print (one per line) the installed Loom implementation pathspecs that git
# currently tracks in <target>. Nothing is printed for paths with no tracked
# files, so an empty result means there is nothing to untrack.
loom_local_tracked_paths() {
  local target="$1"
  local p tracked
  for p in "${LOOM_LOCAL_UNTRACK_PATHS[@]}"; do
    # `git ls-files -- <pathspec>` honors glob pathspecs (e.g. loom-*.md).
    tracked="$(git -C "$target" ls-files -- "$p" 2>/dev/null)"
    if [[ -n "$tracked" ]]; then
      printf '%s\n' "$p"
    fi
  done
}

# Print the exact `git rm -r --cached` commands needed to untrack the currently
# tracked implementation paths. Safe to copy/paste; each pathspec is quoted so
# the shell does not expand the agents glob before git sees it.
loom_local_untrack_commands() {
  local target="$1"
  local p
  while IFS= read -r p; do
    [[ -n "$p" ]] || continue
    printf "git rm -r --cached -- '%s'\n" "$p"
  done < <(loom_local_tracked_paths "$target")
}

# Actually untrack the currently tracked implementation paths in <target>. Files
# stay on disk; only their index entries are removed (staged as deletions). The
# caller is responsible for committing.
loom_local_run_untrack() {
  local target="$1"
  local p
  while IFS= read -r p; do
    [[ -n "$p" ]] || continue
    git -C "$target" rm -r --cached --quiet -- "$p"
  done < <(loom_local_tracked_paths "$target")
}
