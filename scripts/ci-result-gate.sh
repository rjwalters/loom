#!/usr/bin/env bash
# ci-result-gate.sh — the pass/fail rule behind ci.yml's `CI Result` job (#10444).
#
# Why it exists: the `main` ruleset requires only always-run jobs, so a
# cancelled/unacquired `Detect Changes` (hosted runner capacity) SKIPPED every
# path-filtered job behind it and the PR still merged green with no Rust lint
# or tests run (#10403). `CI Result` aggregates every job so that "tested
# nothing" cannot read as "passed".
#
# Inputs (environment):
#   EVENT_NAME   github.event_name
#   NEEDS_JSON   toJSON(needs) — {"<job>": {"result": "...", ...}, ...}
#
# Rule:
#   - pull_request: `changes` (Detect Changes) must be `success`. On push and
#     merge_group it is legitimately `skipped` (it is PR-only). Push has one
#     filter of its own, `changes-push` (#10825, the image jobs only): those
#     jobs skip only when it succeeded with docker=false, and a failed or
#     cancelled `changes-push` fails this gate by the next rule, so no extra
#     case is needed here.
#   - any job `failure` / `cancelled` (or any result other than success/skipped)
#     fails the gate.
#   - a `skipped` job passes only when the path filter really ran (`changes`
#     succeeded) or the event has no path filter; otherwise the skip came from a
#     cancelled/failed upstream and fails.
# Remedy printed on failure: `gh run rerun --failed <run>` (re-runs in place;
# never cancel a distinct commit's run — ci-principles.md).
#
# Exit: 0 pass, 1 gate failed, 2 bad input. Runs before any loom-daemon binary
# exists (the daemon build is itself one of the jobs gated), hence shell.
set -euo pipefail

event="${EVENT_NAME:-}"
needs="${NEEDS_JSON:-}"
if [[ -z "$needs" ]] || ! jq -e 'type == "object" and length > 0' <<<"$needs" >/dev/null 2>&1; then
  echo "CI Result: NEEDS_JSON is missing or not a non-empty object; refusing (fails closed)." >&2
  exit 2
fi

problems="$(jq -r --arg event "$event" '
  (.changes.result // "missing") as $ch
  | ($event == "pull_request") as $pr
  | (if $pr and $ch != "success"
       then ["changes (Detect Changes): " + $ch + " — the path filter did not run, so every filtered job was skipped or untrusted"]
       else [] end)
    + [ to_entries[] | select(.key != "changes")
        | (.value.result // "missing") as $r
        | if ($r == "success") then empty
          elif ($r == "skipped") then
            (if $pr and $ch != "success"
               then (.key + ": skipped because Detect Changes was " + $ch)
               else empty end)
          else (.key + ": " + $r) end ]
  | .[]' <<<"$needs")"

if [[ -n "$problems" ]]; then
  echo "CI Result: FAILED — this run did not verify the tree:" >&2
  while IFS= read -r line; do echo "  - $line" >&2; done <<<"$problems"
  echo "Re-run in place with: gh run rerun --failed <run-id> (do not cancel the run)." >&2
  exit 1
fi
echo "CI Result: ok (every job succeeded or was legitimately path-filtered)."
