#!/usr/bin/env bash
# Regression test: check-guard-destructive-drift.sh must never put the
# GH_TOKEN / GITHUB_TOKEN credential in curl's argv. argv is readable by every
# process of the user (ps, /proc/*/cmdline), so the token has to reach curl
# through a config read from a pipe instead.
#
# A stub `curl` first on PATH records its argv, plus the contents of any
# `--config` / `-K` file and anything it reads from stdin, then serves canned
# responses so the script runs end to end with no network. Stub values only;
# no real credential is ever read.
#
# Usage: ./scripts/test-check-guard-destructive-drift-argv.sh
# Exit 0 = all pass, 1 = a failure.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DRIFT="$REPO_ROOT/scripts/check-guard-destructive-drift.sh"

PASS=0
FAIL=0
pass() { echo "PASS: $1"; PASS=$((PASS + 1)); }
fail() { echo "FAIL: $1"; FAIL=$((FAIL + 1)); }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/bin"

cat >"$WORK/bin/curl" <<'STUB'
#!/usr/bin/env bash
# Recording curl stub. One argv line per call to $CURL_ARGV_LOG; config and
# stdin contents to $CURL_CFG_LOG.
printf '%s\n' "$*" >>"$CURL_ARGV_LOG"
out="" url="" stdin_hdr=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --config|-K) [[ -r "$2" ]] && cat "$2" >>"$CURL_CFG_LOG"; shift 2 ;;
    -H) [[ "$2" == "@-" ]] && stdin_hdr=true; shift 2 ;;
    -o) out="$2"; shift 2 ;;
    --max-time|-m|-X|-w|-d) shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
$stdin_hdr && cat >>"$CURL_CFG_LOG"
case "$url" in
  */tags) printf '[{"name": "v0.0.1-stub"}]\n' ;;
  *raw.githubusercontent.com*)
    [[ -n "$out" ]] && printf 'stub_fn() {\n  :\n}\n' >"$out" ;;
esac
exit 0
STUB
chmod +x "$WORK/bin/curl"

STUB_TOKEN="stub-gh-token-0123456789-not-a-real-credential"

run_drift() {
  : >"$WORK/argv.log"
  : >"$WORK/cfg.log"
  PATH="$WORK/bin:$PATH" CURL_ARGV_LOG="$WORK/argv.log" CURL_CFG_LOG="$WORK/cfg.log" \
    GITHUB_STEP_SUMMARY="" "$@" bash "$DRIFT" >"$WORK/out.log" 2>&1
}

# --- 1. GH_TOKEN set: token reaches curl via config, never argv ---
run_drift env GH_TOKEN="$STUB_TOKEN" GITHUB_TOKEN=
if [[ ! -s "$WORK/argv.log" ]]; then
  fail "stub curl was never invoked (test would be a false pass)"
elif grep -qF "$STUB_TOKEN" "$WORK/argv.log"; then
  fail "GH_TOKEN leaked into curl argv"
else
  pass "GH_TOKEN absent from every curl argv"
fi
if grep -q 'Authorization' "$WORK/argv.log"; then
  fail "an Authorization header was passed in curl argv"
else
  pass "no Authorization header in curl argv"
fi
if grep -qF "header = \"Authorization: Bearer $STUB_TOKEN\"" "$WORK/cfg.log"; then
  pass "token still sent, as a Bearer header in the curl config"
else
  fail "token not delivered through the curl config (behaviour changed)"
fi
if grep -q 'v0.0.1-stub' "$WORK/out.log"; then
  pass "resolved ref from the authenticated tags call"
else
  fail "tags response was not used: $(cat "$WORK/out.log")"
fi
if [[ "$(grep -c 'raw.githubusercontent.com' "$WORK/argv.log")" -ge 1 ]] \
    && [[ "$(grep -c 'Authorization' "$WORK/cfg.log")" -eq 1 ]]; then
  pass "token sent on the tags call only, not to raw.githubusercontent.com"
else
  fail "token sent on an unexpected number of calls"
fi

# --- 2. GITHUB_TOKEN fallback behaves the same ---
run_drift env GH_TOKEN= GITHUB_TOKEN="$STUB_TOKEN"
if grep -qF "$STUB_TOKEN" "$WORK/argv.log"; then
  fail "GITHUB_TOKEN leaked into curl argv"
elif grep -qF "Bearer $STUB_TOKEN" "$WORK/cfg.log"; then
  pass "GITHUB_TOKEN fallback sent via config, absent from argv"
else
  fail "GITHUB_TOKEN fallback not delivered"
fi

# --- 3. No token: anonymous request, no Authorization anywhere ---
run_drift env GH_TOKEN= GITHUB_TOKEN=
if grep -q 'Authorization' "$WORK/argv.log" "$WORK/cfg.log"; then
  fail "Authorization sent with no token configured"
else
  pass "no token -> anonymous request"
fi

# --- 4. Characters outside the token alphabet cannot reach the config ---
run_drift env GH_TOKEN=$'stub"tok\\en\nheader = "X: y"' GITHUB_TOKEN=
if [[ "$(cat "$WORK/cfg.log")" == 'header = "Authorization: Bearer stubtokenheaderXy"' ]]; then
  pass "quote/backslash/newline stripped; no config line can be injected"
else
  fail "config escaping wrong: $(cat "$WORK/cfg.log")"
fi

echo ""
echo "Results: $PASS passed, $FAIL failed"
[[ $FAIL -eq 0 ]]
