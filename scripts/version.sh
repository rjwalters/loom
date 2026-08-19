#!/usr/bin/env bash
# version.sh - Manage version across all Loom packages
#
# Usage:
#   ./scripts/version.sh                  # Show current version
#   ./scripts/version.sh list             # List version-bearing files (one per line)
#   ./scripts/version.sh check            # Verify all files are in sync
#   ./scripts/version.sh bump patch       # 0.4.1 → 0.4.2
#   ./scripts/version.sh bump minor       # 0.4.1 → 0.5.0
#   ./scripts/version.sh bump major       # 0.4.1 → 1.0.0
#   ./scripts/version.sh set 1.2.3        # Set explicit version
#   ./scripts/version.sh set 1.2.3 --tag  # Set version, commit, and tag
#
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"

# All files that contain the version string
#
# "VERSION" (issue #5517) is the plain-text root file required by the
# tool-package installer contract (rjwalters/repo#156, C8 — "Honest source
# version"): a populated VERSION file at the source root is the single source
# of truth other tools/consumers can read without Loom-specific knowledge of
# where the version lives. package.json remains the file this script derives
# `get_version()` from; VERSION is kept in sync alongside it, same as every
# other entry here.
VERSION_FILES=(
  "package.json"
  "mcp-loom/package.json"
  "loom-daemon/Cargo.toml"
  "loom-api/Cargo.toml"
  "CLAUDE.md"
  "VERSION"
)

# .loom/install-metadata.json only exists on a dogfooded install (loom
# installed on its own repo, e.g. this repo) — absent in a normal consumer
# checkout. It is JSON but deliberately NOT added to VERSION_FILES above:
# every VERSION_FILES entry is assumed to always exist, whereas this file
# must be existence-checked everywhere it's touched (#4842). Only the
# `loom_version` field is managed here; `loom_commit`/`last_resync` remain
# resync-installed.sh's restamp_metadata() responsibility (defaults/scripts/
# resync-installed.sh) so the two paths don't fight over the same fields —
# they compose in either order because each only ever writes the fields it
# owns.
INSTALL_METADATA_FILE="$REPO_ROOT/.loom/install-metadata.json"

get_version() {
  jq -r '.version' "$REPO_ROOT/package.json"
}

get_version_from_file() {
  local file="$1"
  case "$file" in
    *.json)
      jq -r '.version' "$REPO_ROOT/$file"
      ;;
    *.toml)
      grep -m1 '^version' "$REPO_ROOT/$file" | sed 's/version = "\(.*\)"/\1/'
      ;;
    CLAUDE.md)
      grep -o 'Loom Version\*\*: [0-9]*\.[0-9]*\.[0-9]*' "$REPO_ROOT/$file" | grep -o '[0-9]*\.[0-9]*\.[0-9]*'
      ;;
    VERSION)
      # Plain-text file: the version string, trimmed of surrounding whitespace
      # (a trailing newline in particular).
      tr -d '[:space:]' < "$REPO_ROOT/$file"
      ;;
  esac
}

check_versions() {
  local expected
  expected=$(get_version)
  local all_match=true

  for file in "${VERSION_FILES[@]}"; do
    local actual
    actual=$(get_version_from_file "$file")
    if [ "$actual" != "$expected" ]; then
      echo "MISMATCH  $file: $actual (expected $expected)"
      all_match=false
    else
      echo "OK        $file: $actual"
    fi
  done

  if [ -f "$INSTALL_METADATA_FILE" ]; then
    local meta_actual
    meta_actual=$(jq -r '.loom_version' "$INSTALL_METADATA_FILE")
    if [ "$meta_actual" != "$expected" ]; then
      echo "MISMATCH  .loom/install-metadata.json: $meta_actual (expected $expected)"
      all_match=false
    else
      echo "OK        .loom/install-metadata.json: $meta_actual"
    fi
  fi

  # Check Cargo.lock
  local lock_versions
  lock_versions=$(grep -A1 'name = "loom-daemon"\|name = "loom-api"' "$REPO_ROOT/Cargo.lock" | grep '^version' | sed 's/version = "\(.*\)"/\1/' | sort -u)
  local lock_count
  lock_count=$(echo "$lock_versions" | wc -l | tr -d ' ')
  if [ "$lock_count" -eq 1 ] && [ "$(echo "$lock_versions" | tr -d '[:space:]')" = "$expected" ]; then
    echo "OK        Cargo.lock: all workspace crates at $expected"
  else
    echo "MISMATCH  Cargo.lock: workspace crates not all at $expected"
    all_match=false
  fi

  # Check mcp-loom/package-lock.json — npm lockfiles carry the version in
  # (at least) two places: the top-level `version` field and the matching
  # `packages[""].version` entry, so a single `jq -r '.version'` read (what
  # get_version_from_file() does for every VERSION_FILES entry) would
  # silently ignore the second occurrence. Grep both instead.
  local mcp_lock_versions
  mcp_lock_versions=$(grep -m2 '"version"' "$REPO_ROOT/mcp-loom/package-lock.json" | sed 's/.*"version": "\(.*\)".*/\1/' | sort -u)
  local mcp_lock_count
  mcp_lock_count=$(echo "$mcp_lock_versions" | wc -l | tr -d ' ')
  if [ "$mcp_lock_count" -eq 1 ] && [ "$(echo "$mcp_lock_versions" | tr -d '[:space:]')" = "$expected" ]; then
    echo "OK        mcp-loom/package-lock.json: both version fields at $expected"
  else
    echo "MISMATCH  mcp-loom/package-lock.json: version fields not all at $expected"
    all_match=false
  fi

  if $all_match; then
    echo ""
    echo "All versions in sync: $expected"
    return 0
  else
    echo ""
    echo "Version mismatch detected. Run: ./scripts/version.sh set $expected"
    return 1
  fi
}

bump_version() {
  local current="$1"
  local part="$2"

  IFS='.' read -r major minor patch <<< "$current"

  case "$part" in
    major) echo "$((major + 1)).0.0" ;;
    minor) echo "$major.$((minor + 1)).0" ;;
    patch) echo "$major.$minor.$((patch + 1))" ;;
    *) echo "Unknown bump type: $part (use major, minor, or patch)" >&2; exit 1 ;;
  esac
}

set_version() {
  local new_version="$1"

  if ! [[ "$new_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "Invalid version format: $new_version (expected X.Y.Z)" >&2
    exit 1
  fi

  local old_version
  old_version=$(get_version)

  echo "Updating version: $old_version → $new_version"
  echo ""

  # JSON files - use jq for clean updates
  for file in package.json mcp-loom/package.json; do
    local tmp
    tmp=$(mktemp)
    jq --arg v "$new_version" '.version = $v' "$REPO_ROOT/$file" > "$tmp"
    mv "$tmp" "$REPO_ROOT/$file"
    echo "  Updated $file"
  done

  # Cargo.toml files - sed the version line in [package] section
  # Uses awk instead of sed to reliably replace only the first 'version =' line
  # (BSD sed on macOS doesn't support GNU sed's 0,/pattern/ address)
  for file in loom-daemon/Cargo.toml loom-api/Cargo.toml; do
    awk -v ver="$new_version" '!done && /^version = "/ { print "version = \"" ver "\""; done=1; next } 1' \
      "$REPO_ROOT/$file" > "$REPO_ROOT/$file.tmp" && mv "$REPO_ROOT/$file.tmp" "$REPO_ROOT/$file"
    echo "  Updated $file"
  done

  # CLAUDE.md — portable in-place edit via temp file + mv (matches the
  # Cargo.toml idiom above; avoids BSD vs GNU `sed -i` divergence).
  sed "s/\*\*Loom Version\*\*: .*/\*\*Loom Version\*\*: $new_version/" "$REPO_ROOT/CLAUDE.md" > "$REPO_ROOT/CLAUDE.md.tmp" && mv "$REPO_ROOT/CLAUDE.md.tmp" "$REPO_ROOT/CLAUDE.md"
  echo "  Updated CLAUDE.md"

  # VERSION (#5517) — plain text, single line.
  printf '%s\n' "$new_version" > "$REPO_ROOT/VERSION"
  echo "  Updated VERSION"

  # .loom/install-metadata.json (#4842) — dogfooded-install-only, no-op if
  # absent. Only loom_version is touched; see the field-ownership note above
  # INSTALL_METADATA_FILE's declaration.
  if [ -f "$INSTALL_METADATA_FILE" ]; then
    local meta_tmp
    meta_tmp=$(mktemp)
    jq --arg v "$new_version" '.loom_version = $v' "$INSTALL_METADATA_FILE" > "$meta_tmp"
    mv "$meta_tmp" "$INSTALL_METADATA_FILE"
    echo "  Updated .loom/install-metadata.json"
  fi

  # Cargo.lock — stderr is deliberately NOT swallowed (a prior `2>/dev/null`
  # here made a lock-contention failure invisible in scrollback, #6536) and
  # the exit status is checked explicitly rather than relying on `set -e`
  # inside the subshell, so a failure here is reported immediately with a
  # clear message naming the step, instead of silently leaving Cargo.lock
  # stale while every other version-bearing file has already moved on.
  if ! (cd "$REPO_ROOT" && cargo update loom-daemon loom-api); then
    echo "ERROR: 'cargo update loom-daemon loom-api' failed — Cargo.lock was NOT updated to $new_version." >&2
    exit 1
  fi
  echo "  Updated Cargo.lock"

  # mcp-loom/package-lock.json — regenerate the npm-native way rather than
  # hand-editing the JSON, so nested packages[""] entries stay consistent.
  # Exit status checked explicitly for the same reason as the cargo step above.
  if ! (cd "$REPO_ROOT/mcp-loom" && npm install --package-lock-only); then
    echo "ERROR: 'npm install --package-lock-only' failed — mcp-loom/package-lock.json was NOT updated to $new_version." >&2
    exit 1
  fi
  echo "  Updated mcp-loom/package-lock.json"

  echo ""
  echo "Version set to $new_version"
}

do_tag() {
  local version="$1"

  echo ""
  echo "Committing and tagging..."
  (
    cd "$REPO_ROOT"
    git add package.json mcp-loom/package.json mcp-loom/package-lock.json \
           loom-daemon/Cargo.toml loom-api/Cargo.toml \
           CLAUDE.md VERSION Cargo.lock
    [ -f "CHANGELOG.md" ] && git add CHANGELOG.md
    [ -f ".loom/install-metadata.json" ] && git add .loom/install-metadata.json
    git commit -m "chore: bump version to $version"
    git tag -a "v$version" -m "v$version"
  )
  echo ""
  echo "Created commit and tag v$version"
  echo "Push with: git push origin main --tags"
}

# --- Main ---

case "${1:-}" in
  ""|show)
    echo "$(get_version)"
    ;;
  list)
    # Emit the VERSION_FILES array, one entry per line.
    # Consumed by /repo:release (rjwalters/repo) to discover version-bearing
    # files without hardcoding the count or names in prose. Cargo.lock is
    # intentionally excluded — it's a derived artifact updated by
    # `cargo update` as a side effect of the bump, not a directly-edited
    # version source.
    printf '%s\n' "${VERSION_FILES[@]}"
    # .loom/install-metadata.json is dogfooded-install-only (#4842); list it
    # only when present rather than as a fixed VERSION_FILES entry. Uses
    # if/then (not `[ -f ... ] && echo ...`) so a false condition here —
    # the common case in a non-dogfooded checkout — doesn't leak as this
    # `list` case arm's (and thus the whole script's) exit status, since
    # this is the last command in the arm.
    if [ -f "$INSTALL_METADATA_FILE" ]; then
      echo ".loom/install-metadata.json"
    fi
    ;;
  check)
    check_versions
    ;;
  bump)
    part="${2:-patch}"
    current=$(get_version)
    new_version=$(bump_version "$current" "$part")
    set_version "$new_version"
    # Self-verify (#6536): reuse the existing checker rather than duplicating
    # its logic. This is the loud-failure backstop for BOTH suspected causes
    # — a Builder hand-editing files instead of running this script, and this
    # script itself silently under-delivering (e.g. a `cargo update`/`npm
    # install` step that no-ops under lock contention) — so a partial version
    # bump can never complete without a clear, non-zero-exit signal naming
    # the still-mismatched file(s).
    echo ""
    echo "Self-check: verifying all version-bearing files landed at $new_version..."
    if ! check_versions; then
      echo "" >&2
      echo "ERROR: 'version.sh bump $part' produced an inconsistent version state — see MISMATCH line(s) above." >&2
      exit 1
    fi
    if [ "${3:-}" = "--tag" ]; then
      do_tag "$new_version"
    fi
    ;;
  set)
    if [ -z "${2:-}" ]; then
      echo "Usage: $0 set <version> [--tag]" >&2
      exit 1
    fi
    set_version "$2"
    # Self-verify (#6536) — see the matching comment in the 'bump' arm above.
    echo ""
    echo "Self-check: verifying all version-bearing files landed at $2..."
    if ! check_versions; then
      echo "" >&2
      echo "ERROR: 'version.sh set $2' produced an inconsistent version state — see MISMATCH line(s) above." >&2
      exit 1
    fi
    if [ "${3:-}" = "--tag" ]; then
      do_tag "$2"
    fi
    ;;
  *)
    echo "Usage: $0 [show|list|check|bump <major|minor|patch> [--tag]|set <version> [--tag]]"
    exit 1
    ;;
esac
