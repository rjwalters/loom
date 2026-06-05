#!/usr/bin/env bash
# scripts/install/manifest.sh — Loom installation manifest helper
#
# Provides _emit_installed_files_manifest, which prints a JSON array of
# target-relative paths corresponding to the files Loom ACTUALLY installs.
# The manifest is consumed by scripts/uninstall-loom.sh to decide what to
# remove on uninstall/reinstall.
#
# Source this file with:
#     source "$LOOM_ROOT/scripts/install/manifest.sh"
#
# Inputs (env):
#   LOOM_ROOT     — Loom source checkout root (defaults/ lives here)
#   TARGET_PATH   — target repo root (consulted for the package.json carve-out)
#   DOGFOOD_MODE  — "true" to exclude .claude/agents/* (issue #3311)
#
# Output: a single JSON array literal on stdout, e.g.
#   [".loom/config.json",".loom/roles/builder.json",".claude/settings.json"]
#
# Issue #3450 background — the previous implementation walked the *target*
# repo with `find .loom .claude .codex .github .githooks CLAUDE.md .gitignore`
# and captured every file under those roots, including consumer-authored
# files (e.g. .github/workflows/ci.yml, the consumer's full .gitignore, any
# pre-existing CLAUDE.md). The uninstaller trusted that manifest as
# authoritative for deletion and silently destroyed consumer files. The fix
# below enumerates defaults/ instead — files Loom never shipped are not in
# the manifest, so the uninstaller can't accidentally delete them.

# Print the installed-files manifest as a JSON array.
#
# Skip rules (mirror install-side behavior):
#   - defaults/README.md            → source docs, not installed
#   - defaults/optional/**          → opt-in extras, not auto-installed
#   - defaults/hooks/example-context/** → example methodology content
#   - defaults/hooks/*.template     → templates, not installed verbatim
#   - defaults/package.json         → only installed if target lacks one
#
# Path translations (defaults-relative → target-relative):
#   .loom-README.md       → .loom/README.md
#   config.json           → .loom/config.json
#   config/X              → .loom/config/X
#   roles/X               → .loom/roles/X
#   scripts/X             → .loom/scripts/X
#   hooks/X               → .loom/hooks/X
#   docs/X                → .loom/docs/X
#   .loom/X               → .loom/X         (literal)
#   .claude/X             → .claude/X       (literal)
#   .codex/X              → .codex/X        (literal)
#   .github/X             → .github/X       (literal)
#   CLAUDE.md             → CLAUDE.md
#   loom.sh               → loom.sh
#   package.json          → package.json    (only if target lacks one)
#
# Files that DO NOT appear in defaults/ are NOT in the manifest, even if
# they exist in the target repo's .loom/, .github/, etc. directories. This
# is the entire point of issue #3450 — the manifest defines Loom's ownership
# boundary, not the target's directory layout.
_emit_installed_files_manifest() {
  local defaults_dir="${LOOM_ROOT:-}/defaults"
  if [[ ! -d "$defaults_dir" ]]; then
    echo "[]"
    return 0
  fi

  local json="["
  local first=true
  local rel_path target_path

  _append() {
    local p="$1"
    if [[ "$first" == "true" ]]; then
      first=false
    else
      json="${json},"
    fi
    json="${json}\"${p}\""
  }

  # Walk defaults/ and translate each defaults-relative path to its target.
  # `-print0 | sort -z` gives a deterministic ordering and handles paths
  # with spaces safely.
  while IFS= read -r -d '' file; do
    rel_path="${file#"${defaults_dir}"/}"

    # Skip rules — files Loom does NOT install verbatim into the target.
    case "$rel_path" in
      README.md)
        continue
        ;;
      optional/*)
        continue
        ;;
      hooks/example-context/*)
        continue
        ;;
      hooks/*.template)
        continue
        ;;
    esac

    # Translate defaults-relative → target-relative.
    case "$rel_path" in
      .loom-README.md)
        target_path=".loom/README.md"
        ;;
      config.json)
        target_path=".loom/config.json"
        ;;
      config/*)
        target_path=".loom/${rel_path}"
        ;;
      roles/*)
        target_path=".loom/${rel_path}"
        ;;
      scripts/*)
        target_path=".loom/${rel_path}"
        ;;
      hooks/*)
        target_path=".loom/${rel_path}"
        ;;
      docs/*)
        target_path=".loom/${rel_path}"
        ;;
      .loom/*)
        target_path="${rel_path}"
        ;;
      .claude/*|.codex/*|.github/*)
        target_path="${rel_path}"
        ;;
      CLAUDE.md|loom.sh)
        target_path="${rel_path}"
        ;;
      package.json)
        # Only installed if the target lacks one. Don't register it as
        # Loom-owned when the consumer already has their own package.json.
        if [[ -n "${TARGET_PATH:-}" ]] && [[ -f "${TARGET_PATH}/package.json" ]]; then
          continue
        fi
        target_path="${rel_path}"
        ;;
      *)
        # Unknown defaults entry — skip rather than guess at a target path.
        continue
        ;;
    esac

    # Dogfood mode (issue #3311): .claude/agents/* are gitignored locally
    # so the manifest must not list them — verify-install.sh would flag drift.
    if [[ "${DOGFOOD_MODE:-}" == "true" ]] && [[ "$target_path" == .claude/agents/* ]]; then
      continue
    fi

    _append "$target_path"
  done < <(find "$defaults_dir" -type f \
    -not -name '.DS_Store' \
    -not -name '*.log' \
    -not -name '*.sock' \
    -print0 2>/dev/null | sort -z)

  json="${json}]"
  printf '%s' "$json"
}
