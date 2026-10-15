#!/usr/bin/env bash
# Test suite for scripts/install/forge-detect.sh
#
# Usage: ./tests/install/test-forge-detect.sh
#
# Tests forge detection logic and URL parsing. Mocks network calls
# so no real API access is needed.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

PASS=0
FAIL=0
TOTAL=0

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

assert_eq() {
  local desc="$1"
  local expected="$2"
  local actual="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$expected" == "$actual" ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"
    echo "  expected: '$expected'"
    echo "  actual:   '$actual'"
    FAIL=$((FAIL + 1))
  fi
}

# Source forge-detect (we'll test its helper functions directly)
source "$REPO_ROOT/scripts/install/forge-detect.sh"

# ============================================================================
# Test _extract_host
# ============================================================================
echo ""
echo "=== Testing _extract_host ==="

assert_eq "HTTPS github.com" \
  "github.com" \
  "$(_extract_host "https://github.com/owner/repo.git")"

assert_eq "SSH github.com" \
  "github.com" \
  "$(_extract_host "git@github.com:owner/repo.git")"

assert_eq "HTTPS gitea self-hosted" \
  "gitea.example.com" \
  "$(_extract_host "https://gitea.example.com/owner/repo.git")"

assert_eq "SSH gitea self-hosted" \
  "gitea.example.com" \
  "$(_extract_host "git@gitea.example.com:owner/repo.git")"

assert_eq "HTTPS with port" \
  "gitea.example.com:3000" \
  "$(_extract_host "https://gitea.example.com:3000/owner/repo.git")"

assert_eq "SSH protocol URL" \
  "gitea.example.com" \
  "$(_extract_host "ssh://git@gitea.example.com/owner/repo.git")"

assert_eq "HTTPS no .git suffix" \
  "github.com" \
  "$(_extract_host "https://github.com/owner/repo")"

# ============================================================================
# Test detect_forge_and_repo - GitHub URLs
# ============================================================================
echo ""
echo "=== Testing detect_forge_and_repo (GitHub) ==="

detect_forge_and_repo "https://github.com/rjwalters/loom.git" 2>/dev/null
assert_eq "GitHub HTTPS - forge type" "github" "$FORGE_TYPE"
assert_eq "GitHub HTTPS - owner" "rjwalters" "$FORGE_OWNER"
assert_eq "GitHub HTTPS - repo" "loom" "$FORGE_REPO"

detect_forge_and_repo "git@github.com:rjwalters/loom.git" 2>/dev/null
assert_eq "GitHub SSH - forge type" "github" "$FORGE_TYPE"
assert_eq "GitHub SSH - owner" "rjwalters" "$FORGE_OWNER"
assert_eq "GitHub SSH - repo" "loom" "$FORGE_REPO"

detect_forge_and_repo "https://github.com/owner/repo" 2>/dev/null
assert_eq "GitHub HTTPS no .git - forge type" "github" "$FORGE_TYPE"
assert_eq "GitHub HTTPS no .git - owner" "owner" "$FORGE_OWNER"
assert_eq "GitHub HTTPS no .git - repo" "repo" "$FORGE_REPO"

# ============================================================================
# Test detect_forge_and_repo - error cases
# ============================================================================
echo ""
echo "=== Testing detect_forge_and_repo (error cases) ==="

TOTAL=$((TOTAL + 1))
if detect_forge_and_repo "" 2>/dev/null; then
  echo -e "${RED}FAIL${NC}: Empty URL should fail"
  FAIL=$((FAIL + 1))
else
  echo -e "${GREEN}PASS${NC}: Empty URL returns error"
  PASS=$((PASS + 1))
fi

# Non-GitHub URL with unreachable host should fail
TOTAL=$((TOTAL + 1))
if detect_forge_and_repo "https://unreachable.invalid/owner/repo.git" 2>/dev/null; then
  echo -e "${RED}FAIL${NC}: Unreachable host should fail"
  FAIL=$((FAIL + 1))
else
  echo -e "${GREEN}PASS${NC}: Unreachable host returns error"
  PASS=$((PASS + 1))
fi

# ============================================================================
# Test gitea_api function structure (just verify it's callable)
# ============================================================================
echo ""
echo "=== Testing gitea_api function ==="

TOTAL=$((TOTAL + 1))
if type gitea_api &>/dev/null; then
  echo -e "${GREEN}PASS${NC}: gitea_api function is defined"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: gitea_api function is not defined"
  FAIL=$((FAIL + 1))
fi

# ============================================================================
# The Gitea token never reaches curl's argv
#
# argv is readable by every process of the user (ps, /proc/*/cmdline), so
# _probe_gitea_api and gitea_api must hand the token to curl through a config
# read from a pipe. A stub `curl` first on PATH records its argv and the
# contents of any --config file. Stub values only.
# ============================================================================
echo ""
echo "=== Testing the token stays out of curl argv ==="

ARGV_WORK="$(mktemp -d)"
mkdir -p "$ARGV_WORK/bin"
cat >"$ARGV_WORK/bin/curl" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$CURL_ARGV_LOG"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config|-K) [[ -r "$2" ]] && cat "$2" >>"$CURL_CFG_LOG"; shift 2 ;;
    *) shift ;;
  esac
done
printf '{"version":"1.21.0"}\n200'
STUB
chmod +x "$ARGV_WORK/bin/curl"
STUB_FORGE_TOKEN='stub-gitea-token-0123456789-not-real'
export CURL_ARGV_LOG="$ARGV_WORK/argv.log" CURL_CFG_LOG="$ARGV_WORK/cfg.log"

# with_stub_curl CMD... -> run CMD with the recording curl first on PATH.
# bash scopes a VAR=value prefix on a function call to that call.
with_stub_curl() {
  PATH="$ARGV_WORK/bin:$PATH" "$@"
}

# assert_token_off_argv DESC EXPECTED_CFG_LINE
assert_token_off_argv() {
  local desc="$1" want="$2"
  TOTAL=$((TOTAL + 1))
  if [[ ! -s "$CURL_ARGV_LOG" ]]; then
    echo -e "${RED}FAIL${NC}: $desc: stub curl never invoked"
    FAIL=$((FAIL + 1))
  elif grep -qF "$STUB_FORGE_TOKEN" "$CURL_ARGV_LOG" || grep -q 'Authorization' "$CURL_ARGV_LOG"; then
    echo -e "${RED}FAIL${NC}: $desc: credential in curl argv: $(cat "$CURL_ARGV_LOG")"
    FAIL=$((FAIL + 1))
  elif ! grep -qF "$want" "$CURL_CFG_LOG"; then
    echo -e "${RED}FAIL${NC}: $desc: token not delivered via config: $(cat "$CURL_CFG_LOG")"
    FAIL=$((FAIL + 1))
  else
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  fi
}

: >"$CURL_ARGV_LOG"; : >"$CURL_CFG_LOG"
probe_rc=0
with_stub_curl _probe_gitea_api "https://gitea.example.com/api/v1" "$STUB_FORGE_TOKEN" || probe_rc=$?
assert_token_off_argv "_probe_gitea_api: token via config, not argv" \
  "header = \"Authorization: token $STUB_FORGE_TOKEN\""
assert_eq "_probe_gitea_api still detects Gitea with a token" "0" "$probe_rc"

: >"$CURL_ARGV_LOG"; : >"$CURL_CFG_LOG"
with_stub_curl _probe_gitea_api "https://gitea.example.com/api/v1" "" >/dev/null || true
TOTAL=$((TOTAL + 1))
if grep -q -- '--config\|Authorization' "$CURL_ARGV_LOG" "$CURL_CFG_LOG"; then
  echo -e "${RED}FAIL${NC}: _probe_gitea_api without a token sent auth"
  FAIL=$((FAIL + 1))
else
  echo -e "${GREEN}PASS${NC}: _probe_gitea_api without a token is anonymous"
  PASS=$((PASS + 1))
fi

: >"$CURL_ARGV_LOG"; : >"$CURL_CFG_LOG"
api_out="$(FORGE_API_BASE="https://gitea.example.com/api/v1" FORGE_TOKEN="$STUB_FORGE_TOKEN" \
  with_stub_curl gitea_api POST /repos/o/r/labels '{"name":"x"}')" || true
assert_token_off_argv "gitea_api: token via config, not argv" \
  "header = \"Authorization: token $STUB_FORGE_TOKEN\""
assert_eq "gitea_api still passes method and body" "1" \
  "$(grep -cF -- '-X POST -H Content-Type: application/json -w' "$CURL_ARGV_LOG" | tr -d ' ')"
assert_eq "gitea_api still passes the JSON body" "1" \
  "$(grep -cF -- '-d {"name":"x"}' "$CURL_ARGV_LOG" | tr -d ' ')"
assert_eq "gitea_api output unchanged" $'{"version":"1.21.0"}\n200' "$api_out"

: >"$CURL_ARGV_LOG"; : >"$CURL_CFG_LOG"
FORGE_API_BASE="https://gitea.example.com/api/v1" FORGE_TOKEN='stub"tok\en' \
  with_stub_curl gitea_api GET /version >/dev/null || true
assert_eq "gitea_api escapes quote/backslash in the config line" \
  'header = "Authorization: token stub\"tok\\en"' "$(cat "$CURL_CFG_LOG")"

rm -rf "$ARGV_WORK"
unset CURL_ARGV_LOG CURL_CFG_LOG
unset -f with_stub_curl

# ============================================================================
# Test detect_merge_method (issue #7844)
#
# The installer's FORCE_AUTO_MERGE path must merge with a strategy the target
# repo actually allows rather than a hardcoded "squash". Both `gh` and
# `gitea_api` are shadowed by shell functions here, so no network is touched.
# ============================================================================
echo ""
echo "=== Testing detect_merge_method (#7844) ==="

# Stubbed `gh`: answers the merge-method probe from $STUB_GH_JSON. Setting
# $STUB_GH_FAIL makes it exit non-zero (network/auth failure).
gh() {
  if [[ "${STUB_GH_FAIL:-}" == "1" ]]; then
    return 1
  fi
  if [[ "$1" == "api" ]]; then
    # Emit the tab-separated triple detect_merge_method's --jq expression
    # produces, sourced from the JSON the test declared.
    printf '%s' "${STUB_GH_JSON:-}" | python3 -c '
import json, sys
data = json.load(sys.stdin)
print("\t".join(
    str(data.get(k, False)).lower()
    for k in ("allow_squash_merge", "allow_merge_commit", "allow_rebase_merge")
))
'
    return 0
  fi
  return 0
}

# Stubbed `gitea_api`: emits $STUB_GITEA_BODY followed by $STUB_GITEA_CODE,
# matching the real helper's `body\nhttp_code` output shape.
gitea_api() {
  printf '%s\n%s\n' "${STUB_GITEA_BODY:-}" "${STUB_GITEA_CODE:-200}"
}

FORGE_OWNER="test-owner"
FORGE_REPO="test-repo"

# --- GitHub ---
FORGE_TYPE="github"
STUB_GH_FAIL=""

STUB_GH_JSON='{"allow_squash_merge":true,"allow_merge_commit":true,"allow_rebase_merge":true}'
assert_eq "GitHub: all allowed prefers merge (#9105)" "merge" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":false,"allow_merge_commit":true,"allow_rebase_merge":true}'
assert_eq "GitHub: merge + rebase allowed prefers merge over rebase" "merge" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":false,"allow_merge_commit":false,"allow_rebase_merge":true}'
assert_eq "GitHub: rebase-only repo uses rebase" "rebase" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":true,"allow_merge_commit":false,"allow_rebase_merge":false}'
assert_eq "GitHub: squash-only repo still honored (no forced migration)" "squash" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":false,"allow_merge_commit":false,"allow_rebase_merge":false}'
assert_eq "GitHub: degenerate all-false fails open to merge (#9105)" "merge" "$(detect_merge_method)"

STUB_GH_JSON='not json at all'
assert_eq "GitHub: unparseable response fails open to merge (#9105)" "merge" "$(detect_merge_method)"

STUB_GH_JSON='{"allow_squash_merge":false,"allow_merge_commit":true,"allow_rebase_merge":false}'
STUB_GH_FAIL=1
assert_eq "GitHub: probe error fails open to merge (#9105)" "merge" "$(detect_merge_method)"
STUB_GH_FAIL=""

# --- Gitea (note: the merge-commit flag is `allow_merge_commits`, plural) ---
FORGE_TYPE="gitea"
STUB_GITEA_CODE=200

STUB_GITEA_BODY='{"allow_squash_merge":true,"allow_merge_commits":true,"allow_rebase_merge":true}'
assert_eq "Gitea: all allowed prefers merge (#9105)" "merge" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":false,"allow_merge_commits":true,"allow_rebase_merge":true}'
assert_eq "Gitea: merge + rebase allowed prefers merge over rebase" "merge" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":false,"allow_merge_commits":false,"allow_rebase_merge":true}'
assert_eq "Gitea: rebase-only repo uses rebase" "rebase" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":true,"allow_merge_commits":false,"allow_rebase_merge":false}'
assert_eq "Gitea: squash-only repo still honored (no forced migration)" "squash" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":false,"allow_merge_commits":false,"allow_rebase_merge":false}'
assert_eq "Gitea: degenerate all-false fails open to merge (#9105)" "merge" "$(detect_merge_method)"

STUB_GITEA_BODY='<html>not json</html>'
assert_eq "Gitea: unparseable response fails open to merge (#9105)" "merge" "$(detect_merge_method)"

STUB_GITEA_BODY='{"allow_squash_merge":false,"allow_merge_commits":true,"allow_rebase_merge":false}'
STUB_GITEA_CODE=401
assert_eq "Gitea: non-200 response fails open to merge (#9105)" "merge" "$(detect_merge_method)"
STUB_GITEA_CODE=200

# Never emit anything but the three known strategies.
STUB_GITEA_BODY='{"allow_squash_merge":"yes","allow_merge_commits":"yes"}'
MERGE_METHOD_OUT="$(detect_merge_method)"
TOTAL=$((TOTAL + 1))
if [[ "$MERGE_METHOD_OUT" == "squash" || "$MERGE_METHOD_OUT" == "merge" || "$MERGE_METHOD_OUT" == "rebase" ]]; then
  echo -e "${GREEN}PASS${NC}: output is always one of squash|merge|rebase"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: unexpected merge method '$MERGE_METHOD_OUT'"
  FAIL=$((FAIL + 1))
fi

unset -f gh gitea_api

# The stubs above emulate what `gh api --jq` returns; this check exercises the
# real jq expression from forge-detect.sh (extracted, so it cannot drift) to
# catch a typo in the field names or the @tsv shape. Skipped when jq is absent
# (the installer treats jq as optional — gh ships its own engine).
TOTAL=$((TOTAL + 1))
if command -v jq >/dev/null 2>&1; then
  JQ_EXPR=""
  IFS= read -r JQ_EXPR < <(grep -o "\[(\.allow_squash_merge.*@tsv" "$REPO_ROOT/scripts/install/forge-detect.sh") || true
  JQ_OUT=$(printf '%s' \
    '{"allow_squash_merge":false,"allow_merge_commit":true,"allow_rebase_merge":false}' \
    | jq -r "$JQ_EXPR" 2>/dev/null || echo "")
  if [[ "$JQ_OUT" == $'false\ttrue\tfalse' ]]; then
    echo -e "${GREEN}PASS${NC}: forge-detect.sh jq expression yields the expected TSV triple"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: forge-detect.sh jq expression produced '$JQ_OUT'"
    FAIL=$((FAIL + 1))
  fi
else
  echo -e "${YELLOW}SKIP${NC}: jq not available — cannot verify the --jq expression"
  PASS=$((PASS + 1))
fi

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
