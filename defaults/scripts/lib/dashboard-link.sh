#!/usr/bin/env bash
# dashboard-link.sh — the bash twin of the daemon's dashboard footer (#9774).
#
# THE FOOTER
#
# Every comment Loom posts, and every issue/PR body Loom creates, ends with a
# small visible link to that object's own dashboard page:
#
#   {body}\n\n[loom dashboard]({base}/github.com/{nwo}/(issues|pull)/{N})\n<!-- loom:dashboard-link -->\n
#
# so any Loom-authored text is one click from its fleet view. `{base}` is
# `$LOOM_DASHBOARD_URL` when set and non-blank (trailing slashes trimmed), else
# `https://dashboard.2amlogic.com`.
#
# WHY THIS FILE EXISTS AT ALL (ADR-0018)
#
# The behaviour OWNS in Rust: `loom_daemon::forge_comment` (#9772) builds this
# footer and is the single POST chokepoint, and `post-comment.sh` is a thin stub
# over `loom-daemon forge comment`. This file is the BINARY-ABSENT twin — the
# bytes a script must still produce on a host where the daemon is unbuilt or
# being replaced, which is exactly when a comment most needs to reach the forge.
# It is not a second implementation of the policy: it is the same format, pinned
# byte-for-byte to the Rust one by
# `defaults/scripts/tests/test-dashboard-link.sh`, which diffs this output
# against `loom-daemon forge dashboard-link` whenever a binary is available.
# Change the format here and that test fails; change it there and it fails too.
# Change both or neither.
#
# WHERE IT IS USED
#
#   * `forge_gh_comment_rl_safe` (lib/forge-helpers.sh) — the shell comment
#     transport. Appending here, rather than at each of its callers, is what
#     makes "a script comment with no dashboard link" structurally impossible.
#   * `create-issue.sh` / `create-pr.sh` — the created BODY, patched after the
#     create (the number is only knowable then).
#
# Sourcing contract: this file only defines functions and two readonly-by-
# convention name constants. It deliberately does NOT `set -e` — it is sourced
# into scripts with their own flag choices (post-verdict.sh runs `set -uo
# pipefail` on purpose), and a library that silently changes its caller's error
# handling is a bug.

# The production dashboard origin, used when $LOOM_DASHBOARD_URL is unset/blank.
LOOM_DASHBOARD_DEFAULT_BASE_URL="https://dashboard.2amlogic.com"
# The hidden marker the footer is idempotent on — identical to the Rust
# `FOOTER_MARKER`, and what the acceptance greps match.
LOOM_DASHBOARD_FOOTER_MARKER="<!-- loom:dashboard-link -->"

# loom_dashboard_base_url -> echoes the configured dashboard origin.
# $LOOM_DASHBOARD_URL when set and not whitespace-only, with trailing slashes
# trimmed so a configured "https://d.example.com/" still builds
# ".../github.com/o/r/..." and never a doubled slash.
loom_dashboard_base_url() {
  local raw="${LOOM_DASHBOARD_URL:-}"
  # Trim surrounding whitespace the same way the Rust `.trim()` does.
  raw="${raw#"${raw%%[![:space:]]*}"}"
  raw="${raw%"${raw##*[![:space:]]}"}"
  if [[ -z "$raw" ]]; then
    printf '%s\n' "$LOOM_DASHBOARD_DEFAULT_BASE_URL"
    return 0
  fi
  while [[ "$raw" == */ ]]; do raw="${raw%/}"; done
  printf '%s\n' "$raw"
}

# _loom_dashboard_kind <kind> -> "pull" for a pull request, else "issues".
# Accepts the spellings the shell call sites actually have on hand: a literal
# `pull`/`pr`, or a boolean-ish `1`/`true` from a flag variable. Anything else
# (including the empty default) is an issue — a PR mislabelled as an issue still
# resolves to the right page in loom-ui, a crash does not.
_loom_dashboard_kind() {
  case "${1:-}" in
    pull | pr | true | 1) printf 'pull' ;;
    *) printf 'issues' ;;
  esac
}

# loom_dashboard_url <nwo> <number> [kind] -> echoes the dashboard page URL.
loom_dashboard_url() {
  printf '%s/github.com/%s/%s/%s\n' \
    "$(loom_dashboard_base_url)" "$1" "$(_loom_dashboard_kind "${3:-}")" "$2"
}

# loom_dashboard_footer <nwo> <number> <kind> <body> -> the body WITH the footer
# on stdout, byte-for-byte as `loom-daemon forge comment` would append it
# (trailing newline included — capture it with a pipe, not `$(...)`, when the
# exact bytes matter).
#
# Returns the body UNCHANGED, and still exit 0, when:
#   * it already carries the marker (idempotence — a re-posted verdict must not
#     accumulate links), or
#   * the slug or number is missing. This mirrors the Rust `footer_or_body`
#     decision: a link to nowhere is worse than no link, so an unresolvable
#     target posts the caller's text untouched rather than a wrong URL.
loom_dashboard_footer() {
  local nwo="$1" number="$2" kind="$3" body="$4"
  if [[ "$body" == *"$LOOM_DASHBOARD_FOOTER_MARKER"* || -z "$nwo" || -z "$number" ]]; then
    printf '%s' "$body"
    return 0
  fi
  printf '%s\n\n[loom dashboard](%s)\n%s\n' \
    "$body" "$(loom_dashboard_url "$nwo" "$number" "$kind")" \
    "$LOOM_DASHBOARD_FOOTER_MARKER"
}

# loom_dashboard_number_from_url <url> -> the trailing issue/PR number, or
# nothing at all when the last path segment is not a bare number. Empty is the
# right answer for an unrecognised shape: loom_dashboard_patch_body treats it as
# "no target" and skips, which is exactly the no-wrong-link rule above.
loom_dashboard_number_from_url() {
  local tail_segment="${1##*/}"
  case "$tail_segment" in '' | *[!0-9]*) return 0 ;; esac
  printf '%s' "$tail_segment"
}

# loom_dashboard_patch_body <nwo> <kind> <number> <body> — append the footer to
# an ALREADY-CREATED issue/PR body, best effort.
#
# One extra REST call, deliberately after the create: the number the link needs
# does not exist until the object does. `repos/{nwo}/issues/{N}` serves both (a
# PR IS an issue for body edits), so `kind` only selects `/pull/N` vs
# `/issues/N` inside the link text.
#
# NEVER fails the caller: a footer that did not land is a cosmetic loss, while a
# `create-issue.sh` that exits non-zero after the issue exists makes the caller
# believe nothing was filed and file again. A failure is reported on stderr and
# the function still returns 0.
loom_dashboard_patch_body() {
  local nwo="$1" kind="$2" number="$3" body="$4"
  [[ -n "$nwo" && -n "$number" ]] || return 0
  case "$body" in *"$LOOM_DASHBOARD_FOOTER_MARKER"*) return 0 ;; esac
  # Piped, not `$(...)`: command substitution would strip the format's trailing
  # newline. `jq -Rs` slurps stdin raw, so no shell quoting touches the body.
  if ! loom_dashboard_footer "$nwo" "$number" "$kind" "$body" \
    | jq -Rs '{body: .}' \
    | gh api --method PATCH "repos/$nwo/issues/$number" --input - >/dev/null 2>&1; then
    echo "dashboard-link: could not append the dashboard footer to $nwo#$number (the $kind itself was created successfully)" >&2
  fi
  return 0
}

# loom_dashboard_patch_created <created-url> <kind> <body> — the form the create
# scripts call: footer the object a `gh issue create` / `gh pr create` just
# returned the URL of.
#
# The URL is the authority for BOTH the slug and the number. A `--repo`-less
# create (the overwhelmingly common case) never had the slug in a variable — it
# let `gh` resolve the ambient repo — so reading it back out of the URL names
# where the object ACTUALLY landed rather than where this shell guessed it would.
# An unparseable URL yields an empty target and skips, per the no-wrong-link
# rule; like loom_dashboard_patch_body it always returns 0.
loom_dashboard_patch_created() {
  local url="$1" kind="$2" body="$3" rest owner name
  rest="${url#*://}" # strip any scheme: host/owner/name/<issues|pull>/N
  rest="${rest#*/}"  # strip the host: owner/name/<issues|pull>/N
  owner="${rest%%/*}"
  rest="${rest#*/}"
  name="${rest%%/*}"
  # An empty half, or whitespace in either, is not a slug. The degenerate
  # too-few-segments cases are caught downstream instead: they leave a
  # non-numeric trailing segment, which loom_dashboard_number_from_url rejects.
  case "$owner/$name" in '/'* | *'/' | *[[:space:]]*) return 0 ;; esac
  loom_dashboard_patch_body "$owner/$name" "$kind" \
    "$(loom_dashboard_number_from_url "$url")" "$body"
}
