#!/usr/bin/env bash
# Tests scripts/ci-daily-classify.sh and its wiring into ci-daily.yml's
# `report` job (#11964): a job cut off by its own `timeout-minutes` must open
# or update its `[ci-daily]` issue; a hand cancel must leave it alone.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
C=scripts/ci-daily-classify.sh
fail=0
run() { # name want conclusion [annotations_json]
  local out rc
  out="$(CONCLUSION="$3" ANNOTATIONS_JSON="${4-}" "$C" 2>&1)"; rc=$?
  if [[ $rc -ne 0 || "$out" != "$2" ]]; then
    echo "FAIL: $1 (rc=$rc, got '$out', want '$2')"; fail=1
  else echo "ok: $1"; fi
}
# Annotation shapes as returned by GET repos/{repo}/check-runs/{job_id}/annotations
# for run 38569355382's macOS job (timed out) and for a hand cancel.
TIMEOUT='{"path":".github","annotation_level":"failure","message":"The job has exceeded the maximum execution time of 3h0m0s"}'
CANCEL='{"path":".github","annotation_level":"failure","message":"The operation was canceled."}'
NOTICE='{"path":".github","annotation_level":"notice","message":"Due to capacity constraints, jobs targeting macOS arm64 runners may experience longer queue times."}'

run "timed out: cancelled + timeout annotation is timed_out" timed_out cancelled "[$TIMEOUT,$CANCEL,$NOTICE]"
run "timeout annotation alone is enough" timed_out cancelled "[$TIMEOUT]"
run "hand cancel stays cancelled" cancelled cancelled "[$CANCEL]"
run "hand cancel with an unrelated notice stays cancelled" cancelled cancelled "[$CANCEL,$NOTICE]"
run "timeout text at notice level is not a timeout" cancelled cancelled \
  '[{"annotation_level":"notice","message":"The job has exceeded the maximum execution time of 3h0m0s"}]'
run "no annotations stays cancelled" cancelled cancelled '[]'
run "empty annotations input stays cancelled" cancelled cancelled ''
run "unparseable annotations stay cancelled" cancelled cancelled 'not json'
run "an object, not an array, stays cancelled" cancelled cancelled "$TIMEOUT"
run "failure passes through" failure failure "[$TIMEOUT]"
run "success passes through, annotations ignored" success success "[$TIMEOUT]"
run "skipped passes through" skipped skipped ''
run "timed_out passes through" timed_out timed_out ''
run "null (still running) passes through" null null ''
run "missing conclusion reads as null" null '' ''

# Wiring: the report job checks the classifier out, may read annotations, and
# still opens/updates on timed_out.
wf=.github/workflows/ci-daily.yml
if command -v python3 >/dev/null && python3 -c 'import yaml' 2>/dev/null; then
  python3 - "$wf" <<'PY' || fail=1
import sys, yaml
jobs = yaml.safe_load(open(sys.argv[1]))["jobs"]
r = [v for v in jobs.values() if v.get("name") == "Daily Failure Report"]
assert len(r) == 1, "exactly one 'Daily Failure Report' job"
j = r[0]
assert j["permissions"].get("checks") == "read", "report needs checks: read for annotations"
steps = j["steps"]
idx = [i for i, s in enumerate(steps) if s.get("name") == "Open, update and close tracking issues"]
assert len(idx) == 1, "the tracking step exists"
co = [i for i, s in enumerate(steps) if str(s.get("uses", "")).startswith("actions/checkout@")]
assert co and co[0] < idx[0], "the classifier is checked out before the tracking step"
assert "scripts/ci-daily-classify.sh" in str(steps[co[0]].get("with", {}).get("sparse-checkout", "")), \
    "the checkout includes the classifier"
body = steps[idx[0]]["run"]
assert "scripts/ci-daily-classify.sh" in body, "the tracking step calls the classifier"
assert "check-runs/$job_id/annotations" in body, "annotations are fetched for the job"
assert 'if [ "$conclusion" = "cancelled" ]' in body, "annotations are fetched only for cancelled jobs"
assert body.index("ci-daily-classify.sh") < body.index('case "$conclusion" in'), \
    "the classifier runs before the conclusion switch"
assert "failure|timed_out)" in body, "the failure|timed_out branch still opens/updates"
print("ok: ci-daily.yml report wiring")
PY
else
  echo "skip: PyYAML unavailable, wiring check not run"
fi
exit $fail
