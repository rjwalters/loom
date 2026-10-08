#!/usr/bin/env bash
# Tests scripts/ci-image-inputs.sh and its ci.yml wiring (#10825), against a
# fake `gh` — no forge, no network.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1
S=scripts/ci-image-inputs.sh
fail=0
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

check() { # name want-docker actual-stdout [stderr-pattern stderr]
  if [[ "$3" != "docker=$2" ]] || { [[ -n "${4:-}" ]] && ! grep -q -- "$4" <<<"${5:-}"; }; then
    echo "FAIL: $1 (got '$3', want 'docker=$2', pattern '${4:-}')"; echo "${5:-}"; fail=1
  else echo "ok: $1"; fi
}
decide() { # name want [status] <<< paths
  local out err
  out="$("$S" decide "${3:-ok}" 2>"$TMP/err")"; err="$(cat "$TMP/err")"
  check "$1" "$2" "$out" "" "$err"
}

# --- the pure decision, against the real .dockerignore ----------------------
decide "daemon source only -> false" false <<<"loom-daemon/src/x.rs"
decide "session entrypoint -> true" true <<<"docker/session/entrypoint.sh"
decide "private-control guard -> true" true <<<"defaults/hooks/guard-codex-bridge.sh"
decide "private-control lib -> true" true <<<"defaults/scripts/lib/worktree-root.sh"
decide ".dockerignore -> true" true <<<".dockerignore"
decide "ci.yml -> true" true <<<".github/workflows/ci.yml"
decide "run-job seam -> true" true <<<"defaults/scripts/run-job.sh"
decide "this script -> true" true <<<"scripts/ci-image-inputs.sh"
decide "bare !defaults/ is not a prefix -> false" false <<<"defaults/roles/builder.md"
decide "unrelated defaults script -> false" false <<<"defaults/scripts/lib/other.sh"
decide "mixed list, one input -> true" true <<<$'README.md\nloom-daemon/src/a.rs\ndocker/native/Dockerfile'
decide "empty list -> false" false <<<""
decide "errored base -> true" true "no green ci.yml push run on main to diff against" <<<"loom-daemon/src/x.rs"
decide "truncated list -> true" true "truncated" <<<"loom-daemon/src/x.rs"
mkdir -p "$TMP/noroot"
out="$(CI_IMAGE_INPUTS_ROOT="$TMP/noroot" "$S" decide 2>"$TMP/err" <<<"README.md")"
check "unreadable .dockerignore -> true" true "$out" "cannot read" "$(cat "$TMP/err")"

pats="$("$S" patterns)"
while IFS= read -r f; do
  grep -qxF "$f" <<<"$pats" || { echo "FAIL: patterns misses .dockerignore entry $f"; fail=1; }
done < <(sed -n 's/^!\(defaults\/.*[^/]\)$/\1/p' .dockerignore)
if grep -qx 'defaults' <<<"$pats" || grep -qx 'defaults/' <<<"$pats"; then
  echo "FAIL: patterns turned the bare !defaults/ line into an input"; fail=1
else echo "ok: patterns derive the defaults/ inputs from .dockerignore"; fi

# --- push / pr modes, against a fake gh -------------------------------------
mkdir -p "$TMP/bin"
cat >"$TMP/bin/gh" <<'GH'
#!/usr/bin/env bash
case "$1 $2" in
  "run list") [[ "${FAKE_RUNS_RC:-0}" == 0 ]] || exit 1; printf '%s\n' "${FAKE_RUNS:-[]}" ;;
  "api repos/"*) [[ "${FAKE_API_RC:-0}" == 0 ]] || exit 1; printf '%s\n' "$FAKE_API" ;;
  "api --paginate") [[ "${FAKE_API_RC:-0}" == 0 ]] || exit 1; printf '%s\n' "$FAKE_API" ;;
  *) echo "fake gh: unexpected $*" >&2; exit 9 ;;
esac
GH
chmod +x "$TMP/bin/gh"
B=0123456789abcdef0123456789abcdef01234567
GREEN="[{\"headSha\":\"$B\"}]"
files() { # status path... -> compare JSON
  local st="$1"; shift
  jq -cn --arg st "$st" '{status:$st, files:[$ARGS.positional[] | {filename:.}]}' --args "$@"
}
push() { # name want pattern [env...]
  local name="$1" want="$2" pat="$3" out; shift 3
  out="$(env PATH="$TMP/bin:$PATH" "$@" "$S" push --repo o/r --sha deadbeef --branch main 2>"$TMP/err")"
  check "$name" "$want" "$out" "$pat" "$(cat "$TMP/err")"
}
push "push: daemon-only diff since last green -> false" false "base = $B" \
  FAKE_RUNS="$GREEN" FAKE_API="$(files ahead loom-daemon/src/x.rs Cargo.lock)"
push "push: docker change since last green -> true" true "docker/worker/Dockerfile" \
  FAKE_RUNS="$GREEN" FAKE_API="$(files ahead README.md docker/worker/Dockerfile)"
push "push: rename out of docker/ -> true" true "docker/worker/old.sh" FAKE_RUNS="$GREEN" \
  FAKE_API='{"status":"ahead","files":[{"filename":"scripts/new.sh","previous_filename":"docker/worker/old.sh"}]}'
push "push: identical to last green -> false" false "" FAKE_RUNS="$GREEN" FAKE_API='{"status":"identical","files":[]}'
push "push: compare with no files key -> true" true "no readable file list" FAKE_RUNS="$GREEN" FAKE_API='{"status":"ahead"}'
push "push: compare with null files -> true" true "no readable file list" FAKE_RUNS="$GREEN" FAKE_API='{"status":"ahead","files":null}'
push "push: no green run -> true" true "no green" FAKE_RUNS='[]' FAKE_API='{}'
push "push: run list fails -> true" true "could not list" FAKE_RUNS_RC=1 FAKE_API='{}'
push "push: compare fails -> true" true "failed" FAKE_RUNS="$GREEN" FAKE_API_RC=1 FAKE_API='{}'
push "push: base not an ancestor -> true" true "not an ancestor" FAKE_RUNS="$GREEN" FAKE_API="$(files diverged README.md)"
push "push: unreadable compare -> true" true "not an ancestor" FAKE_RUNS="$GREEN" FAKE_API='not json'
# shellcheck disable=SC2046  # one argument per generated path, on purpose
push "push: 300 files (truncated) -> true" true "truncated" FAKE_RUNS="$GREEN" \
  FAKE_API="$(files ahead $(seq -f 'loom-daemon/src/f%g.rs' 1 300))"

pr() { # name want pattern [env...]
  local name="$1" want="$2" pat="$3" out; shift 3
  out="$(env PATH="$TMP/bin:$PATH" "$@" "$S" pr --repo o/r --pr 7 2>"$TMP/err")"
  check "$name" "$want" "$out" "$pat" "$(cat "$TMP/err")"
}
pr "pr: no image input -> false" false "" FAKE_API='[{"filename":"loom-daemon/src/x.rs"}]'
pr "pr: paginated, input on page 2 -> true" true "guard-loom-workflow" \
  FAKE_API=$'[{"filename":"a.md"}]\n[{"filename":"defaults/hooks/guard-loom-workflow.sh"}]'
pr "pr: files API fails -> true" true "could not list" FAKE_API_RC=1 FAKE_API='[]'

# --- usage errors are the only non-zero exit ---------------------------------
"$S" push --repo o/r >/dev/null 2>&1; rc=$?
if [[ $rc -eq 2 ]]; then echo "ok: missing --sha is a usage error"; else echo "FAIL: usage rc=$rc"; fail=1; fi

# --- ci.yml wiring ------------------------------------------------------------
wf=.github/workflows/ci.yml
if command -v python3 >/dev/null && python3 -c 'import yaml' 2>/dev/null; then
  python3 - "$wf" <<'PY' || fail=1
import sys, yaml
jobs = yaml.safe_load(open(sys.argv[1]))["jobs"]
cp = jobs["changes-push"]
assert cp["if"].strip() == "github.event_name == 'push'", "changes-push must be push-only"
assert any("ci-image-inputs.sh push" in s.get("run", "") for s in cp["steps"]), "changes-push must run the script"
ci = jobs["changes-images"]
assert ci["if"].strip() == "github.event_name == 'pull_request'", "changes-images must be PR-only"
assert any("ci-image-inputs.sh pr" in s.get("run", "") for s in ci["steps"]), "changes-images must run the script"
# The script is unmerged PR code on a pull_request checkout: it must never run in
# a job holding a write permission (the old `changes` job has actions: write).
assert not any("ci-image-inputs.sh" in s.get("run", "") or "ci-image-inputs.sh" in str(s.get("with", "")) for s in jobs["changes"]["steps"]), "changes (actions: write) must not run or check out PR-head code"
perms = ci.get("permissions", {})
assert perms and all(v == "read" for v in perms.values()), f"changes-images must be read-only: {perms}"
gate = ("always() && (github.event_name == 'merge_group' || (github.event_name == 'push' && "
        "(needs.changes-push.result != 'success' || needs.changes-push.outputs.docker != 'false')) || "
        "(github.event_name == 'pull_request' && needs.changes-images.outputs.docker == 'true'))")
for j in ("worker-base-image", "worker-image-smoke", "session-image-smoke", "native-image-smoke"):
    assert " ".join(jobs[j]["if"].split()) == gate, f"{j}: image gate drifted: {jobs[j]['if']}"
    assert "changes-push" in jobs[j]["needs"], f"{j} must need changes-push"
    assert "changes-images" in jobs[j]["needs"], f"{j} must need changes-images"
# release-build produces the binary worker-base-image downloads: its gate must
# stay a superset (unconditional on push and merge_group, backend||docker on PRs).
rb = " ".join(jobs["release-build"]["if"].split())
for frag in ("github.event_name == 'push' ||", "github.event_name == 'merge_group' ||", "needs.changes-images.outputs.docker == 'true'"):
    assert frag in rb, f"release-build gate must keep `{frag}` (superset of the image gate)"
print("ok: ci.yml image-gate wiring")
PY
else
  echo "skip: PyYAML unavailable, wiring check not run"
fi
exit $fail
