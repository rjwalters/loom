#!/usr/bin/env bash
# Test suite for scripts/install/setup-branch-protection.sh's
# required_status_checks rule (#8103).
#
# Why this exists: until #8103 the installer's ruleset payload carried only
# `deletion` / `non_fast_forward` / `required_linear_history` / `pull_request`.
# Every repo it configured therefore had purely ADVISORY CI — a red check could
# not block a merge, and nothing could force a stale branch to re-run against
# the current base before landing (rjwalters/loom #8095 is the incident: a PR's
# only CI run measured a tree main had since moved off, and the squash-merge
# tree was never measured at all).
#
# The rule is configuration-driven on purpose: this script installs into ANY
# repository, and a required check that never reports blocks every merge in
# that repo forever. So the suite asserts both directions — the rule appears
# when the target repo names contexts, and stays absent when it does not.
#
# Like its sibling test-setup-repository-settings-merge-method.sh, this drives
# the installer script as a subprocess with a `gh` stub on PATH, so no network
# access or forge credentials are required.
#
# Usage: ./tests/install/test-setup-branch-protection-required-checks.sh

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
cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
# Only the admin-permission probe is reachable in LOOM_DRY_RUN mode; anything
# else would mean the script tried to mutate the repository during a preview.
if [[ "$*" == *".permissions.admin"* ]]; then
  echo "true"
  exit 0
fi
echo "stub gh: unexpected API call in dry-run: $*" >&2
exit 1
STUB
chmod +x "$STUB_DIR/gh"

REPO_DIR="$(make_fake_github_repo)"
trap 'rm -rf "$STUB_DIR" "$REPO_DIR"' EXIT

# The script prints progress lines before the payload; the payload is the JSON
# object it ends with, so take everything from the first column-0 `{`.
run_dry() {
  PATH="$STUB_DIR:$PATH" bash "$SETUP_SCRIPT" "$REPO_DIR" main 2>/dev/null | sed -n '/^{/,$p'
}

# ============================================================================
# No configuration -> no required_status_checks rule (unchanged behavior for
# every repo that has not opted in).
# ============================================================================
echo ""
echo "=== No configured contexts: rule is absent ==="

payload="$(LOOM_DRY_RUN=true run_dry)"
assert_eq "payload is valid JSON" "0" "$(printf '%s' "$payload" | jq -e . >/dev/null 2>&1; echo $?)"
assert_eq "no required_status_checks rule without configuration" \
  "0" "$(printf '%s' "$payload" | jq '[.rules[] | select(.type == "required_status_checks")] | length')"
assert_eq "the pre-existing rules are untouched" \
  "deletion non_fast_forward required_linear_history pull_request" \
  "$(printf '%s' "$payload" | jq -r '[.rules[].type] | join(" ")')"

# ============================================================================
# Configured via .loom/config.json -> the rule is emitted with those contexts.
# ============================================================================
echo ""
echo "=== .loom/config.json contexts become the required checks ==="

mkdir -p "$REPO_DIR/.loom"
cat > "$REPO_DIR/.loom/config.json" <<'EOF'
{
  "branchProtection": {
    "requiredStatusChecks": [
      "CLAUDE.md Line Budget",
      "Shell Syntax (ubuntu-latest)"
    ]
  }
}
EOF

payload="$(LOOM_DRY_RUN=true run_dry)"
assert_eq "required_status_checks rule is emitted" \
  "1" "$(printf '%s' "$payload" | jq '[.rules[] | select(.type == "required_status_checks")] | length')"
assert_eq "contexts are carried through verbatim, including spaces and parens" \
  "CLAUDE.md Line Budget|Shell Syntax (ubuntu-latest)" \
  "$(printf '%s' "$payload" | jq -r '.rules[] | select(.type == "required_status_checks") | [.parameters.required_status_checks[].context] | join("|")')"
assert_eq "up-to-date-branch (strict) policy defaults to false" \
  "false" \
  "$(printf '%s' "$payload" | jq -r '.rules[] | select(.type == "required_status_checks") | .parameters.strict_required_status_checks_policy')"

# ============================================================================
# The strict ("require branches to be up to date") toggle is opt-in.
# ============================================================================
echo ""
echo "=== strictRequiredStatusChecks opts in to the up-to-date requirement ==="

cat > "$REPO_DIR/.loom/config.json" <<'EOF'
{
  "branchProtection": {
    "requiredStatusChecks": ["CLAUDE.md Line Budget"],
    "strictRequiredStatusChecks": true
  }
}
EOF

payload="$(LOOM_DRY_RUN=true run_dry)"
assert_eq "strict policy is honored when configured" \
  "true" \
  "$(printf '%s' "$payload" | jq -r '.rules[] | select(.type == "required_status_checks") | .parameters.strict_required_status_checks_policy')"

# ============================================================================
# The env override wins over config, and splits on commas AND newlines with
# surrounding whitespace trimmed (a hand-typed list must not produce contexts
# that can never match a check-run name).
# ============================================================================
echo ""
echo "=== LOOM_REQUIRED_STATUS_CHECKS overrides config ==="

payload="$(LOOM_DRY_RUN=true LOOM_REQUIRED_STATUS_CHECKS='Alpha Check, Beta Check' run_dry)"
assert_eq "env contexts replace the configured ones" \
  "Alpha Check|Beta Check" \
  "$(printf '%s' "$payload" | jq -r '.rules[] | select(.type == "required_status_checks") | [.parameters.required_status_checks[].context] | join("|")')"

payload="$(LOOM_DRY_RUN=true LOOM_REQUIRED_STATUS_CHECKS=$'One\nTwo\n' run_dry)"
assert_eq "newline-separated env contexts split without empty entries" \
  "One|Two" \
  "$(printf '%s' "$payload" | jq -r '.rules[] | select(.type == "required_status_checks") | [.parameters.required_status_checks[].context] | join("|")')"

payload="$(LOOM_DRY_RUN=true LOOM_REQUIRED_STATUS_CHECKS_STRICT=true run_dry)"
assert_eq "strict env override applies to the configured contexts" \
  "true" \
  "$(printf '%s' "$payload" | jq -r '.rules[] | select(.type == "required_status_checks") | .parameters.strict_required_status_checks_policy')"

# ============================================================================
# This repository's OWN configuration must never require a path-filtered job:
# a `needs: changes` job that is skipped reports nothing on a run where its
# paths did not change, and a required check with no report blocks the merge
# indefinitely. This is the guardrail from #8103's acceptance criteria, checked
# against ci.yml itself so it survives a later edit to either file.
# ============================================================================
echo ""
echo '=== loom own required set contains no path-filtered job ==='

CI_YML="$REPO_ROOT/.github/workflows/ci.yml"
OWN_CONFIG="$REPO_ROOT/.loom/config.json"

if [[ -f "$CI_YML" && -f "$OWN_CONFIG" ]]; then
  # Names of the jobs that sit behind `needs: changes`, and the full set of job
  # names, both read from ci.yml itself so this stays true across later edits to
  # either file.
  gated_names="$(awk '
    function flush() { if (name != "" && gated) print name; name=""; gated=0 }
    /^  [a-zA-Z0-9_-]+:[ \t]*$/ { flush(); next }
    /^    name:[ \t]/ { if (name == "") { line=$0; sub(/^    name:[ \t]*/, "", line); name=line } }
    /^    needs:[ \t]*changes[ \t]*$/ { gated=1 }
    END { flush() }
  ' "$CI_YML")"

  all_names=""
  while IFS= read -r n; do
    [[ -z "$n" ]] && continue
    # Matrix legs report one check-run per value, with the expression replaced.
    all_names+="$n"$'\n'
    all_names+="${n/'${{ matrix.os }}'/ubuntu-latest}"$'\n'
    all_names+="${n/'${{ matrix.os }}'/macos-latest}"$'\n'
  done < <(awk '/^    name:[ \t]/ { line=$0; sub(/^    name:[ \t]*/, "", line); print line }' "$CI_YML")

  violations=""
  missing=""
  while IFS= read -r ctx; do
    [[ -z "$ctx" ]] && continue
    grep -Fxq "$ctx" <<<"$gated_names" && violations+="$ctx; "
    grep -Fxq "$ctx" <<<"$all_names" || missing+="$ctx; "
  done < <(jq -r '.branchProtection.requiredStatusChecks // [] | .[]' "$OWN_CONFIG")

  assert_eq "no path-filtered (needs: changes) job is in the required set" "" "${violations%; }"
  # A renamed job leaves a required check that can never report — the same
  # permanent block as a path-filtered one, reached by a different route.
  assert_eq "every required context names a job that exists in ci.yml" "" "${missing%; }"
  assert_eq "the required set is non-empty (an emptied list protects nothing)" \
    "yes" \
    "$([[ "$(jq -r '.branchProtection.requiredStatusChecks // [] | length' "$OWN_CONFIG")" -gt 0 ]] && echo yes || echo no)"
else
  echo "  (skipped: ci.yml or .loom/config.json not found at repo root)"
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
