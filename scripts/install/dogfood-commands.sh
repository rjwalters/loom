#!/usr/bin/env bash
# scripts/install/dogfood-commands.sh — Loom dogfood command-dir linker
#
# Provides `link_dogfood_commands <target_path>`, which wires loom's own live
# `.claude/commands/loom/` to the shipped source-of-truth at
# `defaults/.claude/commands/loom/` when loom is installed *on* its own source
# repo (dogfood mode). Source this file with:
#
#     source "$LOOM_ROOT/scripts/install/dogfood-commands.sh"
#     link_dogfood_commands "$TARGET_PATH"
#
# ---------------------------------------------------------------------------
# Why a SCOPED SYMLINK of the `loom/` subdir (issue #3682), not a full copy:
#
# The previous mechanism (issue #3565) materialized `.claude/commands/loom/`
# as a real COPY of `defaults/.claude/commands/loom/`. That copy drifts: any
# commit that touches `defaults/.claude/commands/loom/*.md` leaves loom's live
# dogfood copies stale until the next install re-run. The staleness is
# invisible and has produced a false bug report (#3665) when agents loaded old
# guidance. A symlink cannot drift — it always reflects the committed source.
#
# #3565 rejected symlinking `.claude/commands` because the WHOLE directory was
# a symlink into `defaults/`, so a co-installed tool that wrote a *sibling*
# namespace (`.claude/commands/repo/foo.md`, from the `repo` tool) wrote THROUGH
# that symlink into loom's shipped `defaults/` distribution artifact. This
# helper avoids that failure mode by keeping `.claude/commands/` itself a REAL
# directory and symlinking ONLY the `loom/` subdir:
#
#     .claude/commands/            (real dir — sibling namespaces land here)
#     .claude/commands/loom  ->  ../../defaults/.claude/commands/loom  (symlink)
#     .claude/commands/repo/       (real dir, written by the repo tool — safe)
#
# The only writer into the `loom/` namespace is loom's own installer/daemon;
# no co-installed tool writes `.claude/commands/loom/*`, so nothing pollutes
# `defaults/` through this scoped symlink. This mirrors the `.claude/agents`
# dogfood symlink (#3311), scoped one level deeper.
#
# `.claude/commands` stays gitignored in loom's OWN committed .gitignore, so
# the symlink is never staged by `git add -A`. Consumer repos are untouched:
# they keep `.claude/commands/loom/` as TRACKED real files (the daemon's
# consumer `update_gitignore` list is intentionally NOT changed).
# ---------------------------------------------------------------------------

# Fallback logging helpers — only defined when not already provided by the
# caller (install-loom.sh defines its own). This lets the function run
# standalone under the test harness.
if ! command -v info >/dev/null 2>&1; then
  info() { echo -e "\033[0;34mℹ $*\033[0m"; }
fi
if ! command -v success >/dev/null 2>&1; then
  success() { echo -e "\033[0;32m✓ $*\033[0m"; }
fi
if ! command -v warning >/dev/null 2>&1; then
  warning() { echo -e "\033[1;33m⚠ $*\033[0m" >&2; }
fi

# link_dogfood_commands <target_path>
#
# Establishes `.claude/commands/loom` as a relative symlink into
# `defaults/.claude/commands/loom`. Idempotent. Preserves sibling namespaces
# under `.claude/commands/`. Returns non-zero only on unexpected filesystem
# failure; a missing defaults source is a soft warning (returns 0).
link_dogfood_commands() {
  local target_path="$1"
  if [[ -z "$target_path" ]]; then
    warning "link_dogfood_commands: target path required"
    return 2
  fi

  local cmd_src="$target_path/defaults/.claude/commands/loom"
  local cmd_live_dir="$target_path/.claude/commands"
  local cmd_live_loom="$cmd_live_dir/loom"
  # Relative to the directory that CONTAINS the link (`.claude/commands/`), the
  # source of truth is `../../defaults/.claude/commands/loom`. A relative link
  # keeps the repo relocatable.
  local cmd_link_target="../../defaults/.claude/commands/loom"

  if [[ ! -d "$cmd_src" ]]; then
    warning "Dogfood commands source does not exist: $cmd_src"
    warning "Skipping .claude/commands/loom symlink; commands may be missing or stale"
    return 0
  fi

  mkdir -p "$target_path/.claude"

  # If `.claude/commands` is still the legacy WHOLE-dir symlink into defaults/
  # (pre-#3565), remove it so we can build a real destination directory in its
  # place. Sibling namespaces from co-installed tools already live in a real
  # dir and are preserved.
  if [[ -L "$cmd_live_dir" ]]; then
    info "Removing legacy .claude/commands symlink -> $(readlink "$cmd_live_dir")"
    rm -f "$cmd_live_dir"
  fi
  mkdir -p "$cmd_live_dir"

  if [[ -L "$cmd_live_loom" ]]; then
    # Already a symlink — fix the target if it drifted, otherwise no-op.
    local existing_target
    existing_target="$(readlink "$cmd_live_loom")"
    if [[ "$existing_target" == "$cmd_link_target" ]]; then
      success ".claude/commands/loom symlink already correct (-> $cmd_link_target)"
    else
      info "Updating .claude/commands/loom symlink: $existing_target -> $cmd_link_target"
      rm -f "$cmd_live_loom"
      ln -s "$cmd_link_target" "$cmd_live_loom"
      success "Updated .claude/commands/loom symlink -> $cmd_link_target"
    fi
  elif [[ -e "$cmd_live_loom" ]]; then
    # A real directory occupies the path (the old #3565 materialized copy, or a
    # stale dogfood copy). Replace it with the symlink. Safe because (a)
    # defaults/ holds the canonical content and (b) `.claude/commands` is
    # gitignored so local edits were never committed. Guard against silently
    # discarding any local-only files not present in defaults/ (mirrors the
    # .claude/agents handling in install-loom.sh).
    local local_only_files
    local_only_files=$(comm -23 \
      <(cd "$cmd_live_loom" 2>/dev/null && find . -type f | sort) \
      <(cd "$cmd_src" 2>/dev/null && find . -type f | sort) \
      2>/dev/null || true)
    if [[ -n "$local_only_files" ]]; then
      warning ".claude/commands/loom contains local-only files not present in defaults:"
      echo "$local_only_files" | sed 's/^/    /'
      warning "Refusing to replace with symlink. Move or commit these files, then re-run."
      return 0
    fi
    info "Replacing copied .claude/commands/loom/ directory with symlink to defaults/..."
    rm -rf "$cmd_live_loom"
    ln -s "$cmd_link_target" "$cmd_live_loom"
    success "Replaced .claude/commands/loom/ with symlink -> $cmd_link_target"
  else
    ln -s "$cmd_link_target" "$cmd_live_loom"
    success "Created .claude/commands/loom symlink -> $cmd_link_target"
  fi
}
