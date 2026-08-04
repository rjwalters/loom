#!/usr/bin/env bash
# changelog.sh - Generate and verify Keep-a-Changelog entries from a git log
# range (#5196).
#
# Every release used to hand-reconstruct ~150 commits into CHANGELOG.md
# sections because the commit stream is already ~99% conventional-commit
# formatted (feat/fix/docs/...) with a trailing "(#NNN)" ref. This script
# turns that stream directly into a Keep-a-Changelog skeleton so a release
# only has to review/curate the result (and write the human "### Summary"
# prose) instead of grouping commits by hand.
#
# Usage:
#   ./scripts/changelog.sh draft <from>..<to>
#       Print a Keep-a-Changelog body (### Added / ### Changed / ### Removed /
#       ### Fixed / ### Other sections, omitting empty ones) for every commit
#       in <from>..<to>, to stdout. Deterministic: re-running against the same
#       (unchanged) range produces byte-identical output.
#
#   ./scripts/changelog.sh verify <from>..<to> [<file>]
#       Check that every "shipping" commit in <from>..<to> which cites a
#       "(#NNN)" ref has that ref present somewhere in <file> (default:
#       CHANGELOG.md at the repo root). Prints one MISSING line per absent
#       ref and exits 1 if any are missing, else prints a summary and exits 0.
#       Intended for /repo:release's Phase 1.5 completeness gate and a final
#       pre-tag/pre-release safety check (Phase 5/6) against the *actual*
#       tagged range, closing the mid-release race where a commit lands
#       between drafting the entry and cutting the tag.
#
# Bucketing (Keep a Changelog section <- Conventional Commit type, matched
# case-insensitively against the "type(scope)!: subject" prefix):
#   Added    <- feat
#   Changed  <- docs, refactor, perf, style
#   Removed  <- revert
#   Fixed    <- fix
#   (dropped, non-shipping) <- test, chore, ci, build
#   Other    <- anything else: an unrecognized prefix (e.g. this repo's rare
#              "config(...):" commits) or no conventional prefix at all. Kept
#              rather than silently dropped -- surfaced separately so the
#              human curating the release decides where (if anywhere) it
#              belongs, rather than the generator guessing.
#
# This feeds /repo:release's existing CHANGELOG phases; it does not replace
# them, and it does not generate the "### Summary" narrative (that stays
# human-written by design -- see issue #5196's non-goals).
set -euo pipefail

# CHANGELOG_REPO_ROOT overrides the repo the git-log range and default
# CHANGELOG.md are resolved against -- a testability hook (scripts/
# test-changelog.sh points it at a disposable scratch repo) that has no
# effect on normal invocations, which always resolve against this script's
# own checkout.
REPO_ROOT="${CHANGELOG_REPO_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"

usage() {
  cat <<'EOF'
Usage:
  changelog.sh draft <from>..<to>
  changelog.sh verify <from>..<to> [<file>]
EOF
}

# Non-shipping conventional-commit types: excluded from both draft output and
# verify's ref-coverage check (a release entry is not expected to mention
# them, so their absence is not a "dropped commit").
is_non_shipping_type() {
  case "$1" in
    test | chore | ci | build) return 0 ;;
    *) return 1 ;;
  esac
}

# Extract the conventional-commit type from a subject line, lowercased, or
# empty if the subject has no "type:" / "type(scope):" / "type!:" prefix.
commit_type() {
  local subject="$1"
  if [[ "$subject" =~ ^([A-Za-z]+)(\([^\)]*\))?\!?:[[:space:]] ]]; then
    printf '%s' "${BASH_REMATCH[1]}" | tr '[:upper:]' '[:lower:]'
  fi
}

# Strip a "type(scope)!: " prefix, returning the remainder verbatim --
# including any trailing "(#NNN)" ref, which is therefore preserved as-is
# rather than re-derived. Subjects with no such prefix pass through unchanged
# (they land in the "Other" bucket, not silently dropped).
commit_description() {
  local subject="$1"
  if [[ "$subject" =~ ^[A-Za-z]+(\([^\)]*\))?\!?:[[:space:]](.*)$ ]]; then
    printf '%s' "${BASH_REMATCH[2]}"
  else
    printf '%s' "$subject"
  fi
}

# Map a lowercased conventional type to its Keep-a-Changelog section name.
# "Other" = unrecognized (caller must check is_non_shipping_type first to
# distinguish "excluded/non-shipping" from "unrecognized -> Other").
bucket_for_type() {
  case "$1" in
    feat) echo "Added" ;;
    fix) echo "Fixed" ;;
    docs | refactor | perf | style) echo "Changed" ;;
    revert) echo "Removed" ;;
    *) echo "Other" ;;
  esac
}

cmd_draft() {
  local range="${1:?usage: changelog.sh draft <from>..<to>}"

  # Keep-a-Changelog canonical section order (Added, Changed, Removed, Fixed);
  # "Other" is appended last as the deliberate escape hatch described above.
  local sections=(Added Changed Removed Fixed Other)
  local added="" changed="" removed="" fixed="" other=""

  # `|| [ -n "$subject" ]` matters: git log's last line (the range's oldest
  # commit) has no trailing newline, so a plain `while read` would see `read`
  # return non-zero on that final line and drop it silently -- exactly the
  # "commit silently dropped" failure mode this tool exists to prevent.
  while IFS= read -r subject || [ -n "$subject" ]; do
    [ -n "$subject" ] || continue
    local type section desc
    type="$(commit_type "$subject")"
    if [ -n "$type" ] && is_non_shipping_type "$type"; then
      continue
    fi
    section="$(bucket_for_type "$type")"
    desc="$(commit_description "$subject")"
    case "$section" in
      Added) added+="- ${desc}"$'\n' ;;
      Changed) changed+="- ${desc}"$'\n' ;;
      Removed) removed+="- ${desc}"$'\n' ;;
      Fixed) fixed+="- ${desc}"$'\n' ;;
      Other) other+="- ${desc}"$'\n' ;;
    esac
  done < <(git -C "$REPO_ROOT" log --no-merges --pretty=format:'%s' "$range")

  local printed_any=""
  for s in "${sections[@]}"; do
    local body=""
    case "$s" in
      Added) body="$added" ;;
      Changed) body="$changed" ;;
      Removed) body="$removed" ;;
      Fixed) body="$fixed" ;;
      Other) body="$other" ;;
    esac
    if [ -n "$body" ]; then
      printed_any=1
      printf '### %s\n\n%s\n' "$s" "$body"
    fi
  done

  if [ -z "$printed_any" ]; then
    # Empty-but-valid skeleton: a range with zero shipping commits (all
    # excluded types, or genuinely empty) must not error.
    printf '(no shipping changes in this range)\n'
  fi
}

cmd_verify() {
  local range="${1:?usage: changelog.sh verify <from>..<to> [<file>]}"
  local file="${2:-$REPO_ROOT/CHANGELOG.md}"

  if [ ! -f "$file" ]; then
    echo "ERROR: $file not found" >&2
    return 1
  fi

  local missing=0 checked=0
  # See the matching comment in cmd_draft: git log's last line has no
  # trailing newline, so the `|| [ -n "$subject" ]` guard is required to
  # avoid silently skipping the range's oldest commit.
  while IFS= read -r subject || [ -n "$subject" ]; do
    [ -n "$subject" ] || continue
    local type ref
    type="$(commit_type "$subject")"
    if [ -n "$type" ] && is_non_shipping_type "$type"; then
      continue
    fi
    # Grab every "#NNN" reference in the subject (usually one trailing ref,
    # occasionally more, e.g. two refs cited on a follow-up commit).
    while IFS= read -r ref; do
      [ -n "$ref" ] || continue
      checked=$((checked + 1))
      if ! grep -qF "$ref" "$file"; then
        missing=$((missing + 1))
        echo "MISSING: $ref  <-  $subject"
      fi
    done < <(printf '%s\n' "$subject" | grep -oE '#[0-9]+' || true)
  done < <(git -C "$REPO_ROOT" log --no-merges --pretty=format:'%s' "$range")

  if [ "$missing" -gt 0 ]; then
    echo "FAIL: $missing/$checked referenced commit(s) in $range not found in $file" >&2
    return 1
  fi

  echo "OK: all $checked referenced shipping commit(s) in $range found in $file"
}

# --- Main ---

case "${1:-}" in
  draft)
    shift
    cmd_draft "$@"
    ;;
  verify)
    shift
    cmd_verify "$@"
    ;;
  *)
    usage >&2
    exit 1
    ;;
esac
