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

# Read the Loom-internal skip list at `<defaults>/.loom-internal.list` and
# emit one defaults-relative path per line on stdout (comments, blank
# lines, and surrounding whitespace stripped). Missing file → no output.
#
# This mirrors `loom_daemon::init::scaffolding::load_internal_skip_list` so
# both surfaces consult the same declarative source. See issue #3464.
_read_loom_internal_skip_list() {
  local defaults_dir="$1"
  local list_file="$defaults_dir/.loom-internal.list"
  [[ -r "$list_file" ]] || return 0
  # Strip comments and blanks; trim surrounding whitespace from each entry.
  awk '
    {
      sub(/^[[:space:]]+/, "")
      sub(/[[:space:]]+$/, "")
      if ($0 == "" || $0 ~ /^#/) next
      print
    }
  ' "$list_file"
}

# Print the installed-files manifest as a JSON array.
#
# Skip rules (mirror install-side behavior):
#   - defaults/README.md            → source docs, not installed
#   - defaults/optional/**          → opt-in extras, not auto-installed
#   - defaults/hooks/example-context/** → example methodology content
#   - defaults/hooks/*.template     → templates, not installed verbatim
#   - defaults/package.json         → only installed if target lacks one
#   - any entry in defaults/.loom-internal.list (issue #3464) — Loom-
#     internal files (e.g. .claude/commands/loom/release.md) that the
#     Rust installer also skips at copy time
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

  # Issue #3464: load the declarative Loom-internal skip list once and stash
  # it in a newline-delimited string. We use grep -Fx for the per-file check
  # below so additions to the list don't require any code change here.
  local internal_skip_paths
  internal_skip_paths="$(_read_loom_internal_skip_list "$defaults_dir")"

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

    # The skip list itself is metadata about Loom's install boundary; it
    # is not a shipped file.
    if [[ "$rel_path" == ".loom-internal.list" ]]; then
      continue
    fi

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

    # Issue #3464: drop Loom-internal files declared in
    # defaults/.loom-internal.list. Exact-match against the
    # defaults-relative path; behavior mirrors the Rust installer's
    # `load_internal_skip_list` + `_filtered` copy variants.
    if [[ -n "$internal_skip_paths" ]] \
        && printf '%s\n' "$internal_skip_paths" | grep -Fxq -- "$rel_path"; then
      continue
    fi

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
  done < <(find -L "$defaults_dir" -type f \
    -not -name '.DS_Store' \
    -not -name '*.log' \
    -not -name '*.sock' \
    -print0 2>/dev/null | sort -z)

  json="${json}]"
  printf '%s' "$json"
}

# Emit the current Loom ownership boundary as a newline-delimited list of
# target-relative paths on stdout. This is the SAME set of paths that
# `_emit_installed_files_manifest` produces (same `defaults/` walk, same
# skip rules, same `.loom-internal.list` consultation, same path
# translations), but rendered as plain lines for grep/fgrep consumption
# in the install-/uninstall-side deletion gates.
#
# Issue #3492: pre-#3450 installs persisted an over-broad on-disk
# manifest under `.loom/install-metadata.json` that captured consumer-
# authored files. Both deletion call sites (the uninstall hard-delete
# loop in scripts/uninstall-loom.sh and the upgrade stale-file sweep in
# scripts/install-loom.sh) trusted that manifest as authoritative and
# wiped consumer content under `.claude/skills/`, `.claude/commands/<non-
# loom>/`, etc. The existing `.github/*` allowlist and the
# CLAUDE.md/.gitignore/.claude/settings.json carve-outs only covered a
# narrow slice of the problem.
#
# This helper generalizes that carve-out: callers intersect each
# deletion candidate against the current ownership set
# (`deletion_set = stale_manifest ∩ loom_owns_now`). Paths the previous
# manifest claimed Loom owned but that the current `defaults/` no longer
# ships are preserved and warned, never deleted.
#
# Output: one target-relative path per line, sorted deterministically by
# the underlying find. Missing `defaults/` → empty output.
_emit_loom_ownership_set() {
  local json
  json="$(_emit_installed_files_manifest)"

  # Strip JSON array delimiters and split the comma-delimited string
  # into one entry per line, stripping surrounding quotes/whitespace.
  # Awk avoids a perl/python dependency.
  printf '%s' "$json" | awk '
    {
      # Strip leading "[" and trailing "]"
      sub(/^\[/, "")
      sub(/\]$/, "")
      # Split on the "," that separates JSON string entries. Loom-
      # shipped paths never contain commas, so a naive split is safe.
      n = split($0, items, ",")
      for (i = 1; i <= n; i++) {
        entry = items[i]
        # Strip leading/trailing whitespace and surrounding quotes.
        sub(/^[[:space:]]+/, "", entry)
        sub(/[[:space:]]+$/, "", entry)
        sub(/^"/, "", entry)
        sub(/"$/, "", entry)
        if (entry != "") {
          print entry
        }
      }
    }
  '
}
