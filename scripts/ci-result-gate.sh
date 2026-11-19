#!/usr/bin/env bash
# ci-result-gate.sh — the pass/fail rule behind ci.yml's `CI Result` job, the
# one required check (#10444, #12106). It aggregates every job so that "tested
# nothing" (a cancelled `Detect Changes` skipping every filtered job, #10403)
# cannot read as "passed".
#
# Inputs (environment):
#   EVENT_NAME   github.event_name
#   NEEDS_JSON   toJSON(needs) — {"<job>": {"result": "...", ...}, ...}
#
# Rule:
#   - pull_request and push: `changes` (Detect Changes) must be `success`.
#     merge_group skips it and has no filter.
#   - any job result other than success/skipped fails the gate.
#   - a `skipped` job passes only when `changes` succeeded or the event has no
#     filter (merge_group); otherwise the skip came from a failed upstream.
#
# Exit: 0 pass, 1 gate failed, 2 bad input. Shell, because it runs before any
# loom-daemon binary exists (the daemon build is itself a gated job).
set -euo pipefail

event="${EVENT_NAME:-}"
needs="${NEEDS_JSON:-}"
if [[ -z "$needs" ]] || ! jq -e 'type == "object" and length > 0' <<<"$needs" >/dev/null 2>&1; then
  echo "CI Result: NEEDS_JSON is missing or not a non-empty object; refusing (fails closed)." >&2
  exit 2
fi

problems="$(jq -r --arg event "$event" '
  (.changes.result // "missing") as $ch
  | ($event != "merge_group" and $ch != "success") as $unfiltered
  | (if $unfiltered
       then ["changes (Detect Changes): " + $ch + " — the change detection did not run, so every filtered job was skipped or untrusted"]
       else [] end)
    + [ to_entries[] | select(.key != "changes")
        | (.value.result // "missing") as $r
        | if ($r == "success") then empty
          elif ($r == "skipped") then
            (if $unfiltered then (.key + ": skipped because Detect Changes was " + $ch) else empty end)
          else (.key + ": " + $r) end ]
  | .[]' <<<"$needs")"

if [[ -n "$problems" ]]; then
  echo "CI Result: FAILED — this run did not verify the tree:" >&2
  while IFS= read -r line; do echo "  - $line" >&2; done <<<"$problems"
  echo "Re-run in place with: gh run rerun --failed <run-id> (do not cancel the run)." >&2
  exit 1
fi
echo "CI Result: ok (every job succeeded or was legitimately path-filtered)."
