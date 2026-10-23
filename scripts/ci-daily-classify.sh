#!/usr/bin/env bash
# ci-daily-classify.sh — the effective conclusion ci-daily.yml's `report` job
# acts on for one job of the run (#11964).
#
# Why it exists: GitHub records a job killed by its own `timeout-minutes` as
# `cancelled`, not `timed_out`, and the reporter deliberately leaves
# `cancelled` alone (a cancel is a non-result, ci-principles.md rule 6). So the
# macOS shell leg hit its 180-min cap night after night and no `[ci-daily]`
# issue ever opened. The jobs API carries neither a `timed_out` flag nor the
# cap; the job's check-run annotations do: a timeout leaves "The job has
# exceeded the maximum execution time of 3h0m0s", a hand cancel leaves only
# "The operation was canceled."
#
# Inputs (environment):
#   CONCLUSION        the job's `conclusion` from the jobs API ("null" while
#                     running)
#   ANNOTATIONS_JSON  the job's check-run annotations, a JSON array (only read
#                     when CONCLUSION is `cancelled`; the caller fetches them
#                     only then, so the REST cost is one call per cancelled job)
#
# Output (stdout): the effective conclusion — `timed_out` for a `cancelled` job
# carrying the timeout annotation, otherwise CONCLUSION unchanged. Missing,
# empty or unparseable annotations leave `cancelled` as `cancelled`: a
# non-result is never upgraded to a result on a guess.
#
# Exit 0 always. Runs in the hosted `report` job, which builds no loom-daemon,
# hence shell (scripts/shell-allowlist.txt: bootstrap).
set -uo pipefail

conclusion="${CONCLUSION:-null}"
[[ -n "$conclusion" ]] || conclusion="null"

if [[ "$conclusion" != "cancelled" ]]; then
  printf '%s\n' "$conclusion"
  exit 0
fi

if jq -e '
    type == "array"
    and any(.[];
      type == "object"
      and ((.annotation_level // "failure") == "failure")
      and ((.message // "") | test("has exceeded the maximum execution time")))
  ' <<<"${ANNOTATIONS_JSON:-}" >/dev/null 2>&1; then
  printf 'timed_out\n'
else
  printf 'cancelled\n'
fi
