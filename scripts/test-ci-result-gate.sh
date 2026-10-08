#!/usr/bin/env bash
# Tests scripts/ci-result-gate.sh and the ci.yml wiring of `CI Result` (#10444).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
G=scripts/ci-result-gate.sh
fail=0
run() { # name expected_rc event needs_json [grep-pattern]
  local out rc
  out="$(EVENT_NAME="$3" NEEDS_JSON="$4" "$G" 2>&1)"; rc=$?
  if [[ $rc -ne $2 ]] || { [[ -n "${5:-}" ]] && ! grep -q -- "$5" <<<"$out"; }; then
    echo "FAIL: $1 (rc=$rc, want $2, pattern '${5:-}')"; echo "$out"; fail=1
  else echo "ok: $1"; fi
}
S='{"result":"success"}'; K='{"result":"skipped"}'; C='{"result":"cancelled"}'; F='{"result":"failure"}'
run "all green PR" 0 pull_request "{\"changes\":$S,\"a\":$S,\"b\":$K}"
run "path-filter skip passes" 0 pull_request "{\"changes\":$S,\"rust\":$K,\"s\":$S}"
run "push: changes skipped passes" 0 push "{\"changes\":$K,\"a\":$S}"
run "Detect Changes cancelled fails (#10403)" 1 pull_request "{\"changes\":$C,\"rust\":$K,\"s\":$S}" "changes (Detect Changes): cancelled"
run "skipped-because-upstream-cancelled fails" 1 pull_request "{\"changes\":$C,\"rust\":$K}" "rust: skipped because Detect Changes was cancelled"
run "Detect Changes skipped on PR fails" 1 pull_request "{\"changes\":$K,\"a\":$S}"
run "needed job cancelled fails" 1 pull_request "{\"changes\":$S,\"a\":$C}" "a: cancelled"
run "needed job failed fails" 1 push "{\"changes\":$K,\"a\":$F}" "a: failure"
# #10825: the push-only image filter (`changes-push`).
run "push: image jobs skipped by changes-push passes" 0 push "{\"changes\":$K,\"changes-push\":$S,\"worker-base-image\":$K,\"worker-image-smoke\":$K,\"a\":$S}"
run "push: changes-push failed fails" 1 push "{\"changes\":$K,\"changes-push\":$F,\"worker-base-image\":$S,\"worker-image-smoke\":$S}" "changes-push: failure"
run "push: changes-push cancelled fails" 1 push "{\"changes\":$K,\"changes-push\":$C,\"worker-image-smoke\":$S}" "changes-push: cancelled"
run "PR: image jobs skipped with Detect Changes failed fails" 1 pull_request "{\"changes\":$F,\"changes-push\":$K,\"worker-image-smoke\":$K}" "worker-image-smoke: skipped because Detect Changes was failure"
run "empty needs fails closed" 2 pull_request "{}"
run "missing needs fails closed" 2 pull_request ""

# Wiring: the job exists, is always-run, and needs every other job.
wf=.github/workflows/ci.yml
if command -v jq >/dev/null && command -v python3 >/dev/null && python3 -c 'import yaml' 2>/dev/null; then
  python3 - "$wf" <<'PY' || fail=1
import sys, yaml
d = yaml.safe_load(open(sys.argv[1]))
jobs = d["jobs"]
r = [k for k, v in jobs.items() if v.get("name") == "CI Result"]
assert len(r) == 1, "exactly one job named 'CI Result'"
j = jobs[r[0]]
assert str(j.get("if", "")).strip().startswith("always()"), "CI Result must be if: always()"
missing = set(jobs) - {r[0]} - set(j["needs"])
assert not missing, f"CI Result must need every job; missing {sorted(missing)}"
print("ok: CI Result wiring")
PY
else
  echo "skip: PyYAML unavailable, wiring check not run"
fi
exit $fail
