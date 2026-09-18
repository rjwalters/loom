#!/usr/bin/env bash
# Test suite for scripts/install/setup-branch-protection.sh's handling of
# bypass_actors when it UPDATES an existing ruleset (#8239).
#
# Why this exists: a GitHub ruleset PUT REPLACES `bypass_actors` wholesale. The
# installer builds one hardcoded list containing only the admin RepositoryRole,
# so before #8239 both update paths — the cross-name overlap "update" branch and
# the same-name `PUT .../rulesets/{id}` branch — silently revoked every other
# actor's bypass on re-run. On rjwalters/loom that would have dropped the
# `loom-fleet-dispatch` App (Integration:4486636), whose post-merge
# `git push origin HEAD:main` in .github/workflows/version-bump-on-merge.yml
# depends on bypassing the pull_request rule — i.e. running the installer as
# #8103 instructed would have broken every version bump.
#
# Like its sibling test-setup-branch-protection-required-checks.sh, this drives
# the installer script as a subprocess with a `gh` stub on PATH, so no network
# access or forge credentials are required. The stub serves ruleset fixtures
# from a directory and captures the body of any mutating call instead of
# performing it, which is what lets the update paths be asserted end to end.
#
# Usage: ./tests/install/test-setup-branch-protection-bypass-actors.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SETUP_SCRIPT="$REPO_ROOT/scripts/install/setup-branch-protection.sh"

PASS=0
FAIL=0
TOTAL=0

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

assert_eq() {
  local desc="$1" expected="$2" actual="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$expected" == "$actual" ]]; then
    echo -e "  ${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "  ${RED}FAIL${NC}: $desc"
    echo "    expected: '$expected'"
    echo "    actual:   '$actual'"
    FAIL=$((FAIL + 1))
  fi
}

# A throwaway git repo with a GitHub-style origin, so detect_forge_and_repo
# resolves FORGE_TYPE=github with no network call.
make_fake_github_repo() {
  local dir
  dir="$(mktemp -d)"
  git -C "$dir" init -q
  git -C "$dir" remote add origin "https://github.com/owner/repo.git"
  printf '%s\n' "$dir"
}

STUB_DIR="$(mktemp -d)"
FIX_DIR="$(mktemp -d)"
CAPTURE_DIR="$(mktemp -d)"
REPO_DIR="$(make_fake_github_repo)"
trap 'rm -rf "$STUB_DIR" "$FIX_DIR" "$CAPTURE_DIR" "$REPO_DIR"' EXIT

# `gh` stub: serves GETs from $LOOM_TEST_FIXTURES, records mutating calls under
# $LOOM_TEST_CAPTURE (body -> body.json, method+endpoint -> meta.txt) without
# performing them.
cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
set -uo pipefail

method="GET"
endpoint=""
jq_expr=""
input=""

args=("$@")
i=0
while [[ $i -lt ${#args[@]} ]]; do
  a="${args[$i]}"
  case "$a" in
    api) ;;
    --method) i=$((i + 1)); method="${args[$i]}" ;;
    --jq|-q) i=$((i + 1)); jq_expr="${args[$i]}" ;;
    --input) i=$((i + 1)); input="${args[$i]}" ;;
    -*) ;;
    *) [[ -z "$endpoint" ]] && endpoint="$a" ;;
  esac
  i=$((i + 1))
done

if [[ "$method" != "GET" ]]; then
  body=""
  [[ "$input" == "-" ]] && body="$(cat)"
  printf '%s %s\n' "$method" "$endpoint" >> "$LOOM_TEST_CAPTURE/meta.txt"
  printf '%s' "$body" > "$LOOM_TEST_CAPTURE/body.json"
  exit 0
fi

out=""
case "$endpoint" in
  repos/owner/repo)
    out='{"permissions":{"admin":true}}'
    ;;
  repos/owner/repo/rulesets)
    out="$(cat "$LOOM_TEST_FIXTURES/rulesets.json")"
    ;;
  repos/owner/repo/rulesets/*)
    rs_id="${endpoint##*/}"
    if [[ -f "$LOOM_TEST_FIXTURES/ruleset-${rs_id}.json" ]]; then
      out="$(cat "$LOOM_TEST_FIXTURES/ruleset-${rs_id}.json")"
    else
      echo "stub gh: no fixture for ruleset ${rs_id}" >&2
      exit 1
    fi
    ;;
  *)
    echo "stub gh: unexpected endpoint: $endpoint" >&2
    exit 1
    ;;
esac

if [[ -n "$jq_expr" ]]; then
  printf '%s' "$out" | jq -r "$jq_expr"
else
  printf '%s' "$out"
fi
STUB
chmod +x "$STUB_DIR/gh"

# Install a ruleset list fixture; each element also becomes the detail response
# for its own id (the stub serves both from one object).
set_rulesets() {
  local json="$1" id
  printf '%s' "$json" > "$FIX_DIR/rulesets.json"
  rm -f "$FIX_DIR"/ruleset-*.json
  while IFS= read -r id; do
    [[ -z "$id" ]] && continue
    printf '%s' "$json" | jq --argjson i "$id" '.[] | select(.id == $i)' \
      > "$FIX_DIR/ruleset-${id}.json"
  done < <(printf '%s' "$json" | jq -r '.[].id')
}

# Run the installer for real (no dry run) and print the body it would have sent.
# $1 (optional) is fed to stdin, for the interactive overlap prompt.
run_apply() {
  local reply="${1:-}"
  rm -f "$CAPTURE_DIR/meta.txt" "$CAPTURE_DIR/body.json"
  if [[ -n "$reply" ]]; then
    printf '%s' "$reply" | PATH="$STUB_DIR:$PATH" \
      LOOM_TEST_FIXTURES="$FIX_DIR" LOOM_TEST_CAPTURE="$CAPTURE_DIR" \
      bash "$SETUP_SCRIPT" "$REPO_DIR" main >/dev/null 2>&1
  else
    PATH="$STUB_DIR:$PATH" LOOM_NON_INTERACTIVE=true \
      LOOM_TEST_FIXTURES="$FIX_DIR" LOOM_TEST_CAPTURE="$CAPTURE_DIR" \
      bash "$SETUP_SCRIPT" "$REPO_DIR" main </dev/null >/dev/null 2>&1
  fi
  cat "$CAPTURE_DIR/body.json" 2>/dev/null || true
}

# The script prints progress lines before the payload; the payload is the JSON
# object it ends with, so take everything from the first column-0 `{`.
run_dry() {
  rm -f "$CAPTURE_DIR/meta.txt" "$CAPTURE_DIR/body.json"
  PATH="$STUB_DIR:$PATH" LOOM_DRY_RUN=true LOOM_NON_INTERACTIVE=true \
    LOOM_TEST_FIXTURES="$FIX_DIR" LOOM_TEST_CAPTURE="$CAPTURE_DIR" \
    bash "$SETUP_SCRIPT" "$REPO_DIR" main </dev/null 2>/dev/null | sed -n '/^{/,$p'
}

actors_of() {
  jq -r '[.bypass_actors[] | "\(.actor_type):\(.actor_id):\(.bypass_mode)"] | sort | join(" ")'
}

ADMIN_ACTOR='{"actor_id":5,"actor_type":"RepositoryRole","bypass_mode":"always"}'
APP_ACTOR='{"actor_id":4486636,"actor_type":"Integration","bypass_mode":"always"}'

# ============================================================================
# Same-name in-place update: the live App bypass survives.
# This is the exact rjwalters/loom shape from #8239 — a ruleset named "main"
# whose bypass list carries both the admin role and the loom-fleet-dispatch App.
# ============================================================================
echo ""
echo "=== same-name update preserves a live Integration bypass actor ==="

set_rulesets '[{
  "id": 8809610,
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": ['"$ADMIN_ACTOR"','"$APP_ACTOR"'],
  "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}}
}]'

body="$(run_apply)"
assert_eq "the update is a PUT at the existing ruleset" \
  "PUT repos/owner/repo/rulesets/8809610" \
  "$(tr -d '\n' < "$CAPTURE_DIR/meta.txt")"
assert_eq "both live bypass actors are sent back" \
  "Integration:4486636:always RepositoryRole:5:always" \
  "$(printf '%s' "$body" | actors_of)"

# ============================================================================
# The dry-run preview must show what would actually be SENT (#8239 AC2) — the
# preview is the documented way to review a ruleset change before applying it,
# so a preview that omits a live actor is exactly how the bug went unnoticed.
# ============================================================================
echo ""
echo "=== LOOM_DRY_RUN preview shows the live extra bypass actor ==="

payload="$(run_dry)"
assert_eq "preview payload is valid JSON" \
  "0" "$(printf '%s' "$payload" | jq -e . >/dev/null 2>&1; echo $?)"
assert_eq "preview lists both actors, matching the PUT body" \
  "Integration:4486636:always RepositoryRole:5:always" \
  "$(printf '%s' "$payload" | actors_of)"
assert_eq "preview made no mutating call" \
  "no" \
  "$([[ -f "$CAPTURE_DIR/meta.txt" ]] && echo yes || echo no)"

# ============================================================================
# Cross-name overlap "update" branch (issue #3216's path) — same guarantee.
# The admin role is unioned IN here, because the live ruleset lacks it.
# ============================================================================
echo ""
echo "=== overlap 'update' branch preserves live actors and adds admin ==="

set_rulesets '[{
  "id": 4242,
  "name": "legacy-protection",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": ['"$APP_ACTOR"'],
  "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}}
}]'

body="$(run_apply u)"
assert_eq "the overlapping ruleset is updated in place" \
  "PUT repos/owner/repo/rulesets/4242" \
  "$(tr -d '\n' < "$CAPTURE_DIR/meta.txt")"
assert_eq "live actor kept, admin role added" \
  "Integration:4486636:always RepositoryRole:5:always" \
  "$(printf '%s' "$body" | actors_of)"
assert_eq "the existing ruleset's name is preserved" \
  "legacy-protection" \
  "$(printf '%s' "$body" | jq -r '.name')"

echo ""
echo "=== dry run previews the overlap candidate when no same-name ruleset exists ==="

payload="$(run_dry)"
assert_eq "preview merges the overlapping ruleset's actors" \
  "Integration:4486636:always RepositoryRole:5:always" \
  "$(printf '%s' "$payload" | actors_of)"

# ============================================================================
# A fresh ruleset keeps today's admin-only default — the fix widens what an
# UPDATE preserves, it must not change what a CREATE grants.
# ============================================================================
echo ""
echo "=== fresh POST keeps the admin-only default ==="

set_rulesets '[]'

body="$(run_apply)"
assert_eq "no existing ruleset means a POST" \
  "POST repos/owner/repo/rulesets" \
  "$(tr -d '\n' < "$CAPTURE_DIR/meta.txt")"
assert_eq "only the admin role is granted a bypass" \
  "RepositoryRole:5:always" \
  "$(printf '%s' "$body" | actors_of)"

payload="$(run_dry)"
assert_eq "dry run with no live ruleset is unchanged from before the fix" \
  "RepositoryRole:5:always" \
  "$(printf '%s' "$payload" | actors_of)"

# ============================================================================
# Edge cases on the merge itself.
# ============================================================================
echo ""
echo "=== existing ruleset with no bypass actors ==="

set_rulesets '[{
  "id": 77,
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": [],
  "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}}
}]'

body="$(run_apply)"
assert_eq "an empty live list still gets the admin role" \
  "RepositoryRole:5:always" \
  "$(printf '%s' "$body" | actors_of)"

echo ""
echo "=== existing ruleset whose bypass_actors field is absent ==="

set_rulesets '[{
  "id": 78,
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}}
}]'

body="$(run_apply)"
assert_eq "a missing live list is treated as empty, not as an error" \
  "RepositoryRole:5:always" \
  "$(printf '%s' "$body" | actors_of)"

echo ""
echo "=== the admin role is never duplicated, and its live mode is respected ==="

set_rulesets '[{
  "id": 79,
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": [
    {"actor_id": 5, "actor_type": "RepositoryRole", "bypass_mode": "pull_request"},
    '"$APP_ACTOR"'
  ],
  "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}}
}]'

body="$(run_apply)"
assert_eq "exactly two actors are sent (no admin duplicate)" \
  "2" "$(printf '%s' "$body" | jq '.bypass_actors | length')"
assert_eq "a deliberately narrowed admin bypass_mode is not widened back" \
  "Integration:4486636:always RepositoryRole:5:pull_request" \
  "$(printf '%s' "$body" | actors_of)"

echo ""
echo "=== an actor with the same id but a different type is a distinct actor ==="

set_rulesets '[{
  "id": 80,
  "name": "main",
  "target": "branch",
  "enforcement": "active",
  "bypass_actors": [{"actor_id": 5, "actor_type": "Team", "bypass_mode": "always"}],
  "conditions": {"ref_name": {"include": ["~DEFAULT_BRANCH"], "exclude": []}}
}]'

body="$(run_apply)"
assert_eq "Team:5 is kept and RepositoryRole:5 is still added" \
  "RepositoryRole:5:always Team:5:always" \
  "$(printf '%s' "$body" | actors_of)"

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "=========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed, ${TOTAL} total"
echo "=========================================="

if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
exit 0
