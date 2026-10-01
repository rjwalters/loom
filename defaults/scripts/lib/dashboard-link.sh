#!/usr/bin/env bash
# dashboard-link.sh — the bash twin of loom-daemon's comment footer (#9772).
#
# Every comment/body Loom posts ends with a small visible link to its own
# dashboard page, so any comment is one click from its fleet view. The URL
# scheme is the one loom-ui's resolver serves (2AMLogic/loom-ui#632):
#
#   https://dashboard.2amlogic.com/github.com/<owner>/<repo>/(issues|pull)/<N>
#
# This file is the *format twin* of `loom-daemon/src/forge_comment.rs`'s
# `build_dashboard_footer` — the Rust module doc carries the canonical bytes.
# It exists only for the binary-absent path (scripts that must still post
# when no `loom-daemon` is installed); `defaults/scripts/tests/test-dashboard-link.sh`
# pins the two implementations byte-for-byte against `forge dashboard-link`,
# so a format change here without the Rust side (or vice versa) fails CI.
# Change both or neither.
#
# Contract (identical to the Rust core):
#   body + "\n\n[loom dashboard](<url>)\n<!-- loom:dashboard-link -->\n"
#   - base URL from $LOOM_DASHBOARD_URL (default https://dashboard.2amlogic.com,
#     surrounding whitespace stripped, trailing slashes trimmed; blank = default)
#   - idempotent: a body already carrying the marker is returned unchanged
#
# Usage:
#   forge_dashboard_base_url                       -> the configured origin
#   forge_dashboard_url NWO NUMBER IS_PR(0|1)      -> the dashboard page URL
#   forge_append_dashboard_footer NWO NUMBER IS_PR BODY -> body with footer

# Guard against double-source.
: "${FORGE_DASHBOARD_LINK_SOURCED:=}"
if [[ -n "$FORGE_DASHBOARD_LINK_SOURCED" ]]; then
  return 0
fi
FORGE_DASHBOARD_LINK_SOURCED=1

DASHBOARD_LINK_MARKER='<!-- loom:dashboard-link -->'
DASHBOARD_LINK_DEFAULT_BASE="https://dashboard.2amlogic.com"

# The configured dashboard origin: $LOOM_DASHBOARD_URL when set and non-blank
# (whitespace stripped, trailing slashes trimmed), else the default.
forge_dashboard_base_url() {
  local base="${LOOM_DASHBOARD_URL:-}"
  base="$(printf '%s' "$base" | tr -d '[:space:]' | sed 's#/*$##')"
  if [[ -z "$base" ]]; then
    printf '%s' "$DASHBOARD_LINK_DEFAULT_BASE"
    return 0
  fi
  printf '%s' "$base"
}

# The dashboard page for NUMBER in NWO; IS_PR=1 says pull/N over issues/N.
# loom-ui lands both on the same issue page; the link itself stays truthful.
forge_dashboard_url() {
  local nwo="$1" number="$2" is_pr="$3" kind="issues"
  [[ "$is_pr" == "1" ]] && kind="pull"
  printf '%s/github.com/%s/%s/%s' "$(forge_dashboard_base_url)" "$nwo" "$kind" "$number"
}

# BODY with the dashboard footer appended — the pinned byte format. Idempotent
# on the hidden marker, so a body that already carries a footer is untouched.
forge_append_dashboard_footer() {
  local nwo="$1" number="$2" is_pr="$3" body="$4" url
  case "$body" in
    *"$DASHBOARD_LINK_MARKER"*)
      printf '%s' "$body"
      return 0
      ;;
  esac
  url="$(forge_dashboard_url "$nwo" "$number" "$is_pr")"
  printf '%s\n\n[loom dashboard](%s)\n%s\n' "$body" "$url" "$DASHBOARD_LINK_MARKER"
}
