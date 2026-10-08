#!/usr/bin/env bash
# ci-image-inputs.sh — does this change touch anything the image smokes prove?
# (#10825). The answer gates ci.yml's `worker-base-image` and the three
# `*-image-smoke` jobs on BOTH `pull_request` (the `changes` job) and `push` to
# main (the `changes-push` job). It is the ONE definition of "image input":
# there is no second copy in a paths-filter group (ci-principles rule 9).
#
# An image input is:
#   - every path `.dockerignore` re-includes with a `!` line (the build
#     context: `docker/**`, `dist/**`, and the file-by-file private-control
#     bundle under `defaults/`). Read from the checked-out `.dockerignore`, so
#     a new allowlist entry is an input the moment it lands. A bare directory
#     line (`!defaults/`) matches no changed FILE, so it does not turn every
#     `defaults/**` edit into a smoke run;
#   - `.dockerignore` itself, `ci.yml` (the jobs' own definition), this script,
#     and the run-job seam `docker/worker/test-run-job.sh` drives inside a real
#     container (`defaults/scripts/run-job.sh`, `lib/run-job-exec.sh`, #7853).
# The loom-daemon SOURCE is deliberately not an input (operator-approved,
# 2026-10-07): it would defeat the skip. `ci-daily.yml`'s `docker-smokes` job
# builds the daemon from the commit and runs every smoke with no filter, daily
# (ci-principles rules 3 and 10).
#
# Push base (the subtle part). A push run does NOT diff `github.event.before`:
# main's concurrency (one running + newest pending, ci-principles rule 2)
# cancels pending runs in a burst, so `before` can be a commit nobody verified,
# and after a red smoke run the next clean diff would go green over the
# failure (rule 6). The base is the head of the most recent `push` run of this
# workflow on this branch that concluded `success`: by induction its image
# inputs were smoked green, so only what changed since then needs a smoke.
#
# Fails CLOSED to `docker=true`, with the reason on stderr, when: there is no
# green run to diff against, the base is not an ancestor of the commit, the
# forge call fails or returns unparseable data, the file list may be truncated
# (compare caps at 300 files, the PR files API at 3000), or `.dockerignore` is
# unreadable. Only a usage error exits non-zero.
#
# Usage:
#   ci-image-inputs.sh push --repo OWNER/NAME --sha SHA --branch BRANCH [--workflow FILE]
#   ci-image-inputs.sh pr --repo OWNER/NAME --pr NUMBER
#   ci-image-inputs.sh decide [STATUS] < changed-paths   (STATUS other than `ok` fails closed)
#   ci-image-inputs.sh patterns
# Prints exactly one `docker=true|false` line on stdout (append it to
# $GITHUB_OUTPUT); everything else goes to stderr.
# Env: CI_IMAGE_INPUTS_ROOT overrides the repo root that holds `.dockerignore`.
set -euo pipefail

ROOT="${CI_IMAGE_INPUTS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
DOCKERIGNORE="$ROOT/.dockerignore"
COMPARE_FILE_CAP=300
PR_FILE_CAP=3000
STATIC_INPUTS=(
  '.dockerignore'
  '.github/workflows/ci.yml'
  'scripts/ci-image-inputs.sh'
  'defaults/scripts/run-job.sh'
  'defaults/scripts/lib/run-job-exec.sh'
)

usage() { sed -n '/^# Usage:/,/^# Env:/p' "$0" | sed 's/^# \{0,1\}//' >&2; exit 2; }

emit() { # docker reason
  echo "image inputs: docker=$1 ($2)" >&2
  echo "docker=$1"
}

patterns() {
  printf '%s\n' "${STATIC_INPUTS[@]}"
  # `!path` lines, minus a leading `/`; a line ending in `/` names only a
  # directory, which is never itself a changed file.
  sed -n -e 's/[[:space:]]*$//' -e 's#^!/\{0,1\}\(.*[^/]\)$#\1#p' "$DOCKERIGNORE"
}

# decide [STATUS] < paths: the pure decision, unit-tested without a forge.
decide() {
  local status="${1:-ok}" f p
  local -a pats=()
  if [[ "$status" != "ok" ]]; then
    emit true "fail closed: $status"
    return 0
  fi
  if [[ ! -r "$DOCKERIGNORE" ]]; then
    emit true "fail closed: cannot read $DOCKERIGNORE"
    return 0
  fi
  while IFS= read -r p; do pats+=("$p"); done < <(patterns)
  while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    for p in "${pats[@]}"; do
      # shellcheck disable=SC2053  # $p is a glob on purpose; `*` spans `/` here
      if [[ "$f" == $p ]]; then
        emit true "$f is an image input (matches $p)"
        return 0
      fi
    done
  done
  emit false "no changed path is an image input"
}

# paths_from TSV CAP: TSV is `filename<TAB>previous_filename` rows. A rename
# OUT of an input path counts, hence both columns.
paths_from() {
  local rows n
  rows="$(grep -c . <<<"$1" || true)"
  n="${rows:-0}"
  if (( n >= $2 )); then
    echo "truncated"
    return 0
  fi
  echo "ok"
  cut -f1,2 <<<"$1" | tr '\t' '\n'
}

cmd_push() {
  local repo="" sha="" branch="" workflow="ci.yml" runs base cmp status tsv out verdict
  while (($#)); do
    case "$1" in
      --repo) repo="${2:-}"; shift 2 ;;
      --sha) sha="${2:-}"; shift 2 ;;
      --branch) branch="${2:-}"; shift 2 ;;
      --workflow) workflow="${2:-}"; shift 2 ;;
      *) usage ;;
    esac
  done
  [[ -n "$repo" && -n "$sha" && -n "$branch" ]] || usage
  if ! runs="$(gh run list --repo "$repo" --workflow "$workflow" --branch "$branch" \
      --event push --status success --limit 1 --json headSha)"; then
    decide "could not list green $workflow push runs on $branch" </dev/null
    return 0
  fi
  base="$(jq -r '.[0].headSha // empty' <<<"$runs" 2>/dev/null || true)"
  if [[ ! "$base" =~ ^[0-9a-f]{40}$ ]]; then
    decide "no green $workflow push run on $branch to diff against" </dev/null
    return 0
  fi
  echo "image inputs: base = $base (head of the last green $workflow push run on $branch), head = $sha" >&2
  if ! cmp="$(gh api "repos/$repo/compare/$base...$sha")"; then
    decide "compare $base...$sha failed" </dev/null
    return 0
  fi
  status="$(jq -r '.status // empty' <<<"$cmp" 2>/dev/null || true)"
  case "$status" in
    ahead | identical) ;;
    *)
      decide "base $base is not an ancestor of $sha (compare status '${status:-unreadable}')" </dev/null
      return 0
      ;;
  esac
  if ! tsv="$(jq -r '.files // [] | .[] | [.filename, (.previous_filename // "")] | @tsv' <<<"$cmp")"; then
    decide "compare $base...$sha returned no readable file list" </dev/null
    return 0
  fi
  out="$(paths_from "$tsv" "$COMPARE_FILE_CAP")"
  verdict="${out%%$'\n'*}"
  [[ "$verdict" == "ok" ]] || verdict="compare $base...$sha lists $COMPARE_FILE_CAP+ files and may be truncated"
  tail -n +2 <<<"$out" | decide "$verdict"
}

cmd_pr() {
  local repo="" pr="" pages tsv out verdict
  while (($#)); do
    case "$1" in
      --repo) repo="${2:-}"; shift 2 ;;
      --pr) pr="${2:-}"; shift 2 ;;
      *) usage ;;
    esac
  done
  [[ -n "$repo" && "$pr" =~ ^[0-9]+$ ]] || usage
  if ! pages="$(gh api --paginate "repos/$repo/pulls/$pr/files?per_page=100")" \
    || ! tsv="$(jq -r '.[] | [.filename, (.previous_filename // "")] | @tsv' <<<"$pages")"; then
    decide "could not list the files of PR #$pr" </dev/null
    return 0
  fi
  out="$(paths_from "$tsv" "$PR_FILE_CAP")"
  verdict="${out%%$'\n'*}"
  [[ "$verdict" == "ok" ]] || verdict="PR #$pr lists $PR_FILE_CAP+ files and may be truncated"
  tail -n +2 <<<"$out" | decide "$verdict"
}

case "${1:-}" in
  push) shift; cmd_push "$@" ;;
  pr) shift; cmd_pr "$@" ;;
  decide) shift; decide "${1:-ok}" ;;
  patterns) patterns ;;
  *) usage ;;
esac
