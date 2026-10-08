#!/usr/bin/env bash
# Tests scripts/release-decision.sh and the release.yml wiring that feeds it (#10826).
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
D=scripts/release-decision.sh
fail=0
SHA=0123456789abcdef0123456789abcdef01234567
TAGS=$'v0.19.870\nv0.19.871\nv0.19.872\nv0.9.1\nsome-other-tag'
# run name expected_rc expected_decision pattern [VAR=value ...]  (defaults: an eligible green run)
run() {
  local name="$1" want_rc="$2" want="$3" pat="$4" out rc
  shift 4
  out="$(env -i PATH="$PATH" WR_CONCLUSION=success WR_EVENT=push WR_HEAD_BRANCH=main \
    WR_HEAD_REPO=rjwalters/loom REPO=rjwalters/loom HEAD_SHA="$SHA" VERSION_AT_HEAD=0.19.873 \
    EXISTING_TAGS="$TAGS" RELEASE_EXISTS=false ANCESTRY=ahead "$@" bash "$D" 2>&1)"; rc=$?
  if [[ $rc -ne $want_rc ]] || ! grep -qx "decision=$want" <<<"$out" || ! grep -q -- "$pat" <<<"$out"; then
    echo "FAIL: $name (rc=$rc want $want_rc, decision want $want, pattern '$pat')"; echo "$out"; fail=1
  else echo "ok: $name"; fi
}

run "green main push, unreleased VERSION publishes at head_sha" 0 publish "^sha=$SHA$"
run "publish names v<VERSION at head_sha>" 0 publish "^tag=v0.19.873$"
run "main tip equal to head_sha publishes" 0 publish "" ANCESTRY=identical
run "first green run, no tags at all" 0 publish "" EXISTING_TAGS=
for c in failure cancelled skipped timed_out action_required neutral ""; do
  run "conclusion '${c}' refuses" 1 refuse "not success" WR_CONCLUSION="$c"
done
for e in pull_request merge_group workflow_dispatch schedule; do
  run "event $e refuses" 1 refuse "not push" WR_EVENT="$e"
done
run "branch other than main refuses" 1 refuse "not main" WR_HEAD_BRANCH=feature/x
run "fork head repository refuses" 1 refuse "is not 'rjwalters/loom'" WR_HEAD_REPO=evil/loom
run "missing REPO refuses" 1 refuse "is not" REPO=
run "tag already exists skips" 0 skip "already released" VERSION_AT_HEAD=0.19.872
run "Release exists without a listed tag skips" 0 skip "already released" RELEASE_EXISTS=true
run "a higher release exists skips (late re-run of old green CI)" 0 skip "v0.19.872 is already released" VERSION_AT_HEAD=0.19.869
run "higher compared numerically, not lexically" 0 publish "" VERSION_AT_HEAD=0.19.873 EXISTING_TAGS=$'v0.9.999\nv0.19.80'
run "higher minor wins" 0 skip "v0.20.0" VERSION_AT_HEAD=0.19.900 EXISTING_TAGS=$'v0.20.0\nv0.19.1'
run "non-semver tags ignored" 0 publish "" EXISTING_TAGS=$'v9.9.9-rc1\nnightly'
run "head_sha behind main's history (rewritten) skips" 0 skip "not reachable from main" ANCESTRY=diverged
run "head_sha not on main (behind) skips" 0 skip "not reachable from main" ANCESTRY=behind
run "unknown ancestry fails closed" 2 refuse "could not determine" ANCESTRY=
run "garbage ancestry fails closed" 2 refuse "could not determine" ANCESTRY=error
run "short sha fails closed" 2 refuse "40-hex" HEAD_SHA=0123456
run "bad VERSION fails closed" 2 refuse "not X.Y.Z" VERSION_AT_HEAD=0.19
run "empty VERSION fails closed" 2 refuse "not X.Y.Z" VERSION_AT_HEAD=

# Wiring: release.yml triggers on green CI and binds every decision to head_sha.
if command -v python3 >/dev/null && python3 -c 'import yaml' 2>/dev/null; then
  python3 - .github/workflows/release.yml .github/workflows/ci.yml <<'PY' || fail=1
import re, sys, yaml

wf = yaml.safe_load(open(sys.argv[1]))
ci = yaml.safe_load(open(sys.argv[2]))
on = wf.get("on", wf.get(True))
ok = lambda m: print("ok: " + m)
assert ci["name"] == "CI", "ci.yml must stay named exactly CI (the workflow_run trigger matches it)"
assert "push" not in on, "release.yml must not trigger on push (a VERSION bump push would bypass the CI gate)"
wr = on["workflow_run"]
assert wr["workflows"] == ["CI"] and wr["types"] == ["completed"] and wr["branches"] == ["main"], wr
assert "release" in on and "workflow_dispatch" in on, "hand-cut release and dry-run paths must remain"
ok("triggers: workflow_run on CI completed for main; no push trigger")

conc = wf["concurrency"]
assert conc["cancel-in-progress"] is False, "release runs must never cancel in progress"
jobs = wf["jobs"]
resolve = jobs["resolve"]

# Evaluate GitHub expressions with Python and/or (same short-circuit, value-
# returning semantics; && binds tighter than ||, as in Python).
def evaluate(expr, ctx):
    e = expr.strip()
    if e.startswith("${{"):
        e = e[3:-2]
    e = e.replace("&&", " and ").replace("||", " or ").replace("!=", " != ")
    e = re.sub(r"\bformat\(", "fmt(", e)
    e = re.sub(r"(?<!['\w.])((?:github|inputs)(?:\.[A-Za-z_][\w-]*)+)", r'ctx("\1")', e)
    def look(path):
        cur = ctx
        for part in path.split("."):
            cur = cur.get(part) if isinstance(cur, dict) else None
        return cur
    return eval(e, {"ctx": look, "fmt": lambda f, *a: f.format(*a)})

def group(ctx):
    g = conc["group"]
    pre, expr = g.split("${{", 1)
    return pre + str(evaluate(expr[:-2], ctx))

def wr_ctx(**kw):
    run = {"conclusion": "success", "event": "push", "head_branch": "main",
           "head_sha": "a" * 40, "head_repository": {"full_name": "rjwalters/loom"}}
    run.update(kw)
    return {"github": {"event_name": "workflow_run", "repository": "rjwalters/loom", "sha": "f" * 40,
                       "ref": "refs/heads/main", "run_id": 42, "event": {"workflow_run": run}},
            "inputs": {}}

def gh_ctx(event_name, event, inputs=None):
    return {"github": {"event_name": event_name, "repository": "rjwalters/loom", "sha": "f" * 40,
                       "ref": "refs/heads/main", "run_id": 42, "event": event},
            "inputs": inputs or {}}

green = wr_ctx()
assert group(green) == "release-main", group(green)
assert group(wr_ctx(head_sha="b" * 40)) == "release-main", "the automated group must be constant"
bad = [wr_ctx(conclusion=c) for c in ("failure", "cancelled", "skipped")] + [
    wr_ctx(event="pull_request"), wr_ctx(event="merge_group"), wr_ctx(head_branch="dev"),
    wr_ctx(head_repository={"full_name": "evil/loom"})]
for c in bad:
    assert group(c) == "release-ineligible-42", ("an ineligible completion must not enter release-main", group(c))
assert group(gh_ctx("release", {"release": {"tag_name": "v1.2.3"}})) == "release-v1.2.3"
assert group(gh_ctx("workflow_dispatch", {"inputs": {"tag": "v1.2.3"}}, {"tag": "v1.2.3"})) == "release-v1.2.3"
assert group(gh_ctx("workflow_dispatch", {"inputs": {"gate_sha": "c" * 40}}, {"gate_sha": "c" * 40})) == "release-" + "c" * 40
assert group(gh_ctx("workflow_dispatch", {"inputs": {}})) == "release-" + "f" * 40
ok("concurrency: constant release-main for green main runs, per-run for ineligible, per-tag for release/dispatch")

rif = resolve["if"]
assert evaluate(rif, green) is True, "resolve must run for a green main push"
for c in bad:
    assert not evaluate(rif, c), "resolve must not run for an ineligible CI completion"
assert evaluate(rif, gh_ctx("release", {"release": {"tag_name": "v1"}}))
assert evaluate(rif, gh_ctx("workflow_dispatch", {"inputs": {}}, {"gate_sha": ""}))
assert not evaluate(rif, gh_ctx("workflow_dispatch", {"inputs": {"gate_sha": "c" * 40}}, {"gate_sha": "c" * 40}))
ok("resolve if: runs only for green push-to-main CI (and the unchanged release/dispatch paths)")

steps = resolve["steps"]
checkout = steps[0]
assert checkout["uses"].startswith("actions/checkout@")
assert evaluate(checkout["with"]["ref"], green) == "a" * 40, "resolve must check out workflow_run.head_sha"
assert evaluate(checkout["with"]["ref"], gh_ctx("workflow_dispatch", {"inputs": {"tag": "v1"}}, {"tag": "v1"})) == "v1"
tooling = [s for s in steps if s.get("with", {}).get("path") == ".release-tooling"]
assert len(tooling) == 1 and tooling[0]["with"]["sparse-checkout"] == "scripts/release-decision.sh"
step = next(s for s in steps if s.get("id") == "resolve")
assert step["env"]["SHA"] == "${{ github.event.workflow_run.head_sha }}", "SHA must be the tested commit"
body = step["run"]
assert "workflow_run)" in body and "\n            push)" not in body
assert "bash .release-tooling/scripts/release-decision.sh" in body
assert '--target "$SHA"' in body and "--latest=false" in body
assert 'tag_commit" != "$SHA"' in body, "resolve must verify the tag names the tested commit"
assert 'git rev-parse HEAD)" != "$SHA"' in body, "resolve must assert the checkout is the tested commit (git rev-parse HEAD == $SHA)"
ok("resolve: checkout ref and SHA are workflow_run.head_sha; decision script wired; tag verified")

# Nothing that decides what is built or tagged may read github.sha/github.ref,
# except the decision-script checkout (which must match the running workflow).
for name, job in jobs.items():
    for s in job.get("steps", []):
        if s is not tooling[0]:
            for k, v in (s.get("env") or {}).items():
                assert "github.sha" not in str(v) and "github.ref" not in str(v), (name, k, v)
        if str(s.get("uses", "")).startswith("actions/checkout@") and name != "resolve":
            ref = s.get("with", {}).get("ref", "")
            assert ref.startswith("${{ needs.resolve.outputs.tag"), (name, ref)
    if "resolve" in str(job.get("needs", "")):
        assert "needs.resolve.outputs" in str(job.get("if", "")), (name, "must be gated on resolve's verdict")
ok("downstream jobs: check out the resolved tag and are gated on resolve's verdict")
PY
else
  if [ "${CI:-}" = "true" ]; then
    echo "FAIL: PyYAML unavailable in CI; the wiring check must not be skipped" >&2
    fail=1
  else
    echo "skip: PyYAML unavailable, wiring check not run"
  fi
fi
exit $fail
