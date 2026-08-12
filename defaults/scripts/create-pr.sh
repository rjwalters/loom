#!/usr/bin/env bash
# create-pr.sh - Open a PR, surviving a transient GitHub App permission window
# and never leaving a pushed-but-PR-less branch behind (#6074).
#
# A GitHub App installation token carries the permissions it was minted with
# and is then cached for ~1h. In the window after a permission grant
# propagates but before that cache turns over, `git push` can succeed
# (Contents:write present) while the very next `gh pr create` fails with
#
#     HTTP 403: Resource not accessible by integration
#
# Before this script, that killed the sweep with no PR: the issue stayed
# ready, the daemon re-dispatched it, and the next Builder REBUILT the
# identical work -- one duplicate build per pass, each leaving another
# orphaned `feature/issue-N` branch (observed on 2AMLogic/klayout-tools#851,
# rebuilt 3+ times, and gf180-gate-driver#6; post-mortem 2AMLogic/2am#252).
#
# This is the single-sourced replacement for a bare `gh pr create` in a role
# prompt. It does two things a bare call cannot:
#
#   1. ADOPT-FIRST. If an open PR already exists for the head branch, its URL
#      is printed and the script exits 0 without creating anything. So a
#      re-dispatched Builder that finds its predecessor's branch already
#      pushed converges on the existing PR instead of failing or duplicating,
#      and a partially-completed earlier attempt is never re-done.
#   2. CREDENTIAL ESCALATION on -- and only on -- the App permission-scope
#      403: force a fresh installation-token mint (bypassing the ~1h cache),
#      then a personal token. See `forge_gh_perm_safe` in lib/forge-helpers.sh
#      for the full ladder and why this is NOT the rate-limit fallback.
#
# Usage:
#   create-pr.sh --title TITLE (--body BODY | --body-file PATH) \
#                [--label LABEL]... [--base BRANCH] [--head BRANCH] \
#                [--draft] [--repo OWNER/REPO]
#   create-pr.sh --help
#
# Flags are a subset of `gh pr create`'s, chosen so a role prompt's existing
# invocation can be switched over by changing the command name:
#   --title, -t TITLE     PR title (required).
#   --body, -b BODY       PR body as literal text.
#   --body-file, -F PATH  Read the body from PATH ("-" = stdin). Mutually
#                         exclusive with --body.
#   --label, -l LABEL     Label to apply at creation. Repeatable. A single
#                         comma-separated value is also accepted, matching
#                         `gh pr create --label "a,b"`.
#   --base, -B BRANCH     Base branch (omit for the repo default).
#   --head, -H BRANCH     Head branch (omit for the current branch).
#   --draft, -d           Create as a draft.
#   --repo, -R OWNER/REPO Target repository. Omit for the current repo.
#
# Output: the PR's URL on stdout -- newly created OR adopted (identical to
# `gh pr create`, so a caller parsing the URL needs no change).
#
# Exit codes:
#   0 - A PR exists for this branch (created by this call, or adopted).
#   1 - Creation failed (message on stderr).
#   2 - Invalid arguments.
#
# NOTE: GitHub-specific, like create-issue.sh. On a Gitea forge it exits 2 --
# Gitea has no GitHub App installation tokens, so it has no equivalent
# failure mode.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
# shellcheck source=./lib/forge-helpers.sh
source "$SCRIPT_DIR/lib/forge-helpers.sh"

usage() {
  sed -n '2,59p' "${BASH_SOURCE[0]:-$0}" | sed 's/^# \{0,1\}//'
}

TITLE=""
BODY=""
BODY_FILE=""
BASE_BRANCH=""
HEAD_BRANCH=""
REPO_NWO=""
DRAFT=false
LABELS=()
HAVE_BODY=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --help | -h)
      usage
      exit 0
      ;;
    --title | -t)
      TITLE="${2:-}"
      shift 2
      ;;
    --body | -b)
      BODY="${2:-}"
      HAVE_BODY=true
      shift 2
      ;;
    --body-file | -F)
      BODY_FILE="${2:-}"
      shift 2
      ;;
    --label | -l)
      # `gh pr create --label "a,b"` splits on commas; match that so a
      # prompt's existing invocation transfers unchanged.
      IFS=',' read -r -a _split <<< "${2:-}"
      for _l in "${_split[@]}"; do
        _l="${_l#"${_l%%[![:space:]]*}"}"
        _l="${_l%"${_l##*[![:space:]]}"}"
        [[ -n "$_l" ]] && LABELS+=("$_l")
      done
      shift 2
      ;;
    --base | -B)
      BASE_BRANCH="${2:-}"
      shift 2
      ;;
    --head | -H)
      HEAD_BRANCH="${2:-}"
      shift 2
      ;;
    --draft | -d)
      DRAFT=true
      shift
      ;;
    --repo | -R)
      REPO_NWO="${2:-}"
      shift 2
      ;;
    *)
      echo "create-pr.sh: unknown argument: $1" >&2
      echo "Run 'create-pr.sh --help' for usage." >&2
      exit 2
      ;;
  esac
done

if [[ -z "$TITLE" ]]; then
  echo "create-pr.sh: --title is required" >&2
  exit 2
fi

if [[ -n "$BODY_FILE" ]] && [[ "$HAVE_BODY" == "true" ]]; then
  echo "create-pr.sh: --body and --body-file are mutually exclusive" >&2
  exit 2
fi

if [[ -n "$BODY_FILE" ]]; then
  if [[ "$BODY_FILE" == "-" ]]; then
    BODY="$(cat)"
  elif [[ -r "$BODY_FILE" ]]; then
    BODY="$(cat "$BODY_FILE")"
  else
    echo "create-pr.sh: cannot read --body-file: $BODY_FILE" >&2
    exit 2
  fi
fi

forge_detect
if [[ "$FORGE_TYPE" != "github" ]]; then
  echo "create-pr.sh: this GitHub App permission-window fallback is \
GitHub-specific; on $FORGE_TYPE open the PR with your forge's own CLI." >&2
  exit 2
fi

# `gh pr create` infers the head branch from the checkout, but the adopt check
# below needs it explicitly -- and passing it explicitly also stops a failed
# origin auto-detect from orphaning the remote branch (#3244).
if [[ -z "$HEAD_BRANCH" ]]; then
  HEAD_BRANCH="$(git rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")"
fi
if [[ -z "$HEAD_BRANCH" || "$HEAD_BRANCH" == "HEAD" ]]; then
  echo "create-pr.sh: could not determine the head branch (pass --head)" >&2
  exit 2
fi

# --- Adopt-first ------------------------------------------------------------
#
# An existing open PR for this head branch means the work is already in
# review: print its URL and stop. This is what turns a re-dispatch into a
# no-op instead of a duplicate build, and it makes the whole script idempotent
# (safe to re-run after any partial failure). A failure of the LOOKUP itself
# (rate limit, network) is never fatal -- fall through and let the create call
# be the authority; GitHub rejects a genuine duplicate on its own.
_adopt_args=(pr list --head "$HEAD_BRANCH" --state open --json url --jq '.[0].url')
if [[ -n "$REPO_NWO" ]]; then
  _adopt_args+=(--repo "$REPO_NWO")
fi
EXISTING_URL="$(gh "${_adopt_args[@]}" 2>/dev/null || true)"
if [[ -n "$EXISTING_URL" && "$EXISTING_URL" != "null" ]]; then
  echo "create-pr.sh: an open PR already exists for $HEAD_BRANCH — adopting it" >&2
  printf '%s\n' "$EXISTING_URL"
  exit 0
fi

# --- Create -----------------------------------------------------------------

CREATE_ARGS=(pr create --head "$HEAD_BRANCH" --title "$TITLE" --body "$BODY")
if [[ -n "$BASE_BRANCH" ]]; then
  CREATE_ARGS+=(--base "$BASE_BRANCH")
fi
if [[ -n "$REPO_NWO" ]]; then
  CREATE_ARGS+=(--repo "$REPO_NWO")
fi
if [[ "$DRAFT" == "true" ]]; then
  CREATE_ARGS+=(--draft)
fi
for _label in "${LABELS[@]+"${LABELS[@]}"}"; do
  CREATE_ARGS+=(--label "$_label")
done

if ! forge_gh_perm_safe "${CREATE_ARGS[@]}"; then
  echo "create-pr.sh: could not open a PR for $HEAD_BRANCH. If the commits are \
pushed, do NOT rebuild — re-run this script (it adopts an existing PR) or open \
the PR by hand from that branch." >&2
  exit 1
fi
