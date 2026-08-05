#!/usr/bin/env bash
# Test suite for install.sh's pnpm runnability probe (issue #5394).
#
# Usage: ./tests/install/test-pnpm-runnable-check.sh
#
# install.sh runs top-level installer logic when sourced, so we extract just
# the two functions under test -- pnpm_probe_version() and
# pnpm_failure_hint() -- plus the LOOM_PNPM_PIN_FOR_LEGACY_NODE assignment,
# and eval them in isolation. Same extraction pattern as
# test-daemon-build-shortcut.sh / test-hooks-preserve.sh.
#
# The probe is exercised against FAKE `pnpm` binaries placed on PATH, including
# one that reproduces the exact Node-20 + pnpm-11 failure from #5394 (corepack
# floats to a pnpm that hard-requires Node >= 22.13, so `pnpm --version` dies
# with ERR_UNKNOWN_BUILTIN_MODULE: node:sqlite). No real pnpm, corepack, or
# network access is needed.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

PASS=0
FAIL=0
TOTAL=0

assert_eq() {
  local desc="$1" expected="$2" actual="$3"
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

assert_contains() {
  local desc="$1" needle="$2" haystack="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$haystack" == *"$needle"* ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"
    echo "  expected to contain: '$needle'"
    echo "  actual:              '$haystack'"
    FAIL=$((FAIL + 1))
  fi
}

assert_not_contains() {
  local desc="$1" needle="$2" haystack="$3"
  TOTAL=$((TOTAL + 1))
  if [[ "$haystack" != *"$needle"* ]]; then
    echo -e "${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "${RED}FAIL${NC}: $desc"
    echo "  expected NOT to contain: '$needle'"
    echo "  actual:                  '$haystack'"
    FAIL=$((FAIL + 1))
  fi
}

# --- Extract the units under test -------------------------------------------

# Grab a top-level function definition (header at column 0 through the first
# closing brace at column 0) by exact string match on the header line.
extract_fn() {
  local name="$1" src
  src="$(awk -v start="${name}() {" '$0 == start {f=1} f {print} f && $0 == "}" {exit}' "$INSTALL_SH")"
  if [[ -z "$src" ]]; then
    echo -e "${RED}FATAL${NC}: could not extract ${name}() from $INSTALL_SH" >&2
    exit 1
  fi
  printf '%s\n' "$src"
}

_PIN_SRC="$(grep -E '^LOOM_PNPM_PIN_FOR_LEGACY_NODE=' "$INSTALL_SH")"
if [[ -z "$_PIN_SRC" ]]; then
  echo -e "${RED}FATAL${NC}: could not extract LOOM_PNPM_PIN_FOR_LEGACY_NODE from $INSTALL_SH" >&2
  exit 1
fi
eval "$_PIN_SRC"
eval "$(extract_fn pnpm_probe_version)"
eval "$(extract_fn pnpm_failure_hint)"

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
FAKE_BIN="$WORKDIR/bin"
mkdir -p "$FAKE_BIN"
ORIGINAL_PATH="$PATH"
export PATH="$FAKE_BIN:$ORIGINAL_PATH"

# Write a fake `pnpm` that replays fixed stdout/stderr and exits with a fixed
# status. The payloads live in files so multi-line stack traces survive intact.
make_fake_pnpm() {
  local stdout_text="$1" stderr_text="$2" exit_code="$3"
  printf '%s' "$stdout_text" > "$WORKDIR/pnpm.out"
  printf '%s' "$stderr_text" > "$WORKDIR/pnpm.err"
  # stderr first, then stdout: real pnpm emits its warnings before the version
  # it was asked for, and the probe merges the two streams with 2>&1.
  cat > "$FAKE_BIN/pnpm" <<EOF
#!/usr/bin/env bash
cat "$WORKDIR/pnpm.err" >&2
cat "$WORKDIR/pnpm.out"
exit $exit_code
EOF
  chmod +x "$FAKE_BIN/pnpm"
}

# The failure output from issue #5394 (Node 20 host, corepack-floated pnpm
# 11.20.0), used as both a probe fixture and a hint-classification input.
BROKEN_PNPM_STDERR=' WARN  This version of pnpm requires at least Node.js v22.13
The current version of Node.js is v20.20.2
Error [ERR_UNKNOWN_BUILTIN_MODULE]: No such built-in module: node:sqlite
    at /home/u/.cache/node/corepack/v1/pnpm/11.20.0/dist/pnpm.mjs:1:1
Node.js v20.20.2'

echo "=== pnpm_probe_version(): a working pnpm ==="

make_fake_pnpm '10.15.1' '' 0
if pnpm_probe_version; then probe_status=0; else probe_status=1; fi
assert_eq "working pnpm: probe succeeds" "0" "$probe_status"
assert_eq "working pnpm: version captured" "10.15.1" "$LOOM_PNPM_VERSION"
assert_eq "working pnpm: no diagnostic recorded" "" "$LOOM_PNPM_PROBE_ERROR"

echo ""
echo "=== pnpm_probe_version(): present-but-broken pnpm (#5394) ==="

make_fake_pnpm '' "$BROKEN_PNPM_STDERR" 1
# The precondition that made this bug invisible: the shim IS on PATH, so the
# old `command -v pnpm` check passed while every invocation died.
TOTAL=$((TOTAL + 1))
if command -v pnpm > /dev/null 2>&1; then
  echo -e "${GREEN}PASS${NC}: broken pnpm is still found by 'command -v' (the old check's blind spot)"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: fixture setup -- fake pnpm not on PATH"
  FAIL=$((FAIL + 1))
fi

if pnpm_probe_version; then probe_status=0; else probe_status=1; fi
assert_eq "broken pnpm: probe fails" "1" "$probe_status"
assert_eq "broken pnpm: no version reported" "" "$LOOM_PNPM_VERSION"
assert_contains "broken pnpm: stderr captured for the operator" \
  "ERR_UNKNOWN_BUILTIN_MODULE" "$LOOM_PNPM_PROBE_ERROR"

echo ""
echo "=== pnpm_probe_version(): pnpm exits 0 but prints no version ==="

make_fake_pnpm '' '' 0
if pnpm_probe_version; then probe_status=0; else probe_status=1; fi
assert_eq "silent pnpm: probe fails rather than reporting an empty version" "1" "$probe_status"
assert_eq "silent pnpm: no version reported" "" "$LOOM_PNPM_VERSION"

echo ""
echo "=== pnpm_probe_version(): warnings before the version still parse ==="

make_fake_pnpm '10.15.1' ' WARN  corepack is deprecated
' 0
if pnpm_probe_version; then probe_status=0; else probe_status=1; fi
assert_eq "noisy-but-working pnpm: probe succeeds" "0" "$probe_status"
assert_eq "noisy-but-working pnpm: version parsed from mixed output" "10.15.1" "$LOOM_PNPM_VERSION"

echo ""
echo "=== pnpm_failure_hint(): Node/pnpm mismatch is diagnosed by name ==="

HINT="$(pnpm_failure_hint "$BROKEN_PNPM_STDERR" "v20.20.2")"
assert_contains "mismatch hint: names the cause (pnpm too new for this Node)" \
  "too new for this Node.js" "$HINT"
assert_contains "mismatch hint: echoes the running Node version" "v20.20.2" "$HINT"
assert_contains "mismatch hint: states pnpm 11's Node floor" "22.13" "$HINT"
assert_contains "mismatch hint: gives the pinning command" \
  "corepack prepare pnpm@${LOOM_PNPM_PIN_FOR_LEGACY_NODE} --activate" "$HINT"
assert_contains "mismatch hint: offers the Node-upgrade alternative" \
  "upgrade to Node >= 22.13" "$HINT"
assert_contains "mismatch hint: warns about the sudo/user corepack cache split" \
  "do NOT run" "$HINT"
assert_contains "mismatch hint: names the per-user corepack cache" \
  ".cache/node/corepack/" "$HINT"
assert_not_contains "mismatch hint: escape sequences are expanded, not printed literally" \
  '\n' "$HINT"
HINT_LINES="$(printf '%s\n' "$HINT" | wc -l | tr -d ' ')"
TOTAL=$((TOTAL + 1))
if [[ "$HINT_LINES" -ge 5 ]]; then
  echo -e "${GREEN}PASS${NC}: mismatch hint renders as multiple real lines ($HINT_LINES)"
  PASS=$((PASS + 1))
else
  echo -e "${RED}FAIL${NC}: mismatch hint should span several lines, got $HINT_LINES"
  FAIL=$((FAIL + 1))
fi

echo ""
echo "=== pnpm_failure_hint(): unrelated failures stay honest ==="

GENERIC_HINT="$(pnpm_failure_hint "bash: line 1: /usr/bin/pnpm: Permission denied" "v22.14.0")"
assert_contains "generic hint: reports the failure without diagnosing a version mismatch" \
  "pnpm --version" "$GENERIC_HINT"
assert_not_contains "generic hint: does not falsely claim pnpm is too new" \
  "too new for this Node.js" "$GENERIC_HINT"
assert_contains "generic hint: still offers a concrete remediation command" \
  "corepack prepare pnpm@${LOOM_PNPM_PIN_FOR_LEGACY_NODE} --activate" "$GENERIC_HINT"

echo ""
echo "=== install.sh wiring ==="

export PATH="$ORIGINAL_PATH"
INSTALL_SRC="$(cat "$INSTALL_SH")"

assert_contains "dependency check calls the probe instead of bare 'pnpm --version'" \
  "if pnpm_probe_version; then" "$INSTALL_SRC"
assert_contains "a failed probe marks pnpm as a missing dependency" \
  'MISSING_DEPS+=("pnpm (present but not runnable)")' "$INSTALL_SRC"
assert_contains "a failed probe appends the actionable hint to the install instructions" \
  'pnpm_failure_hint "$LOOM_PNPM_PROBE_ERROR"' "$INSTALL_SRC"
assert_contains "install.sh declares a minimum Node major" \
  "LOOM_MIN_NODE_MAJOR=" "$INSTALL_SRC"

echo ""
echo "======================================"
echo "Total:  $TOTAL"
echo -e "${GREEN}Passed: $PASS${NC}"
if [[ $FAIL -gt 0 ]]; then
  echo -e "${RED}Failed: $FAIL${NC}"
  exit 1
fi
echo "All tests passed."
exit 0
