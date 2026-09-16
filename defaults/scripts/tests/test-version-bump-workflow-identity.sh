#!/usr/bin/env bash
# test-version-bump-workflow-identity.sh - Regression guard for the pushing
# identity of .github/workflows/version-bump-on-merge.yml (issues #7743, #7829).
#
# Background: that workflow is the single post-merge owner of VERSION and the
# other version-bearing files (#7743). Its final step pushes straight to `main`,
# and this repo's `main` ruleset (8809610) requires a pull request -- so the
# push only lands when the pushing identity is a bypass actor. The default
# `github-actions[bot]` GITHUB_TOKEN can NEVER be one here: adding the GitHub
# Actions integration (app id 15368) to `bypass_actors` is rejected outright on
# a user-owned repo with
#
#   422 Validation Failed: Actor GitHub Actions integration must be part of
#   the ruleset source or owner organization
#
# The only viable identity is the repo's own `loom-fleet-dispatch` App (id
# 4486636), which IS listed in the ruleset's `bypass_actors` (#7829). The
# workflow therefore mints a short-lived, repository-scoped App installation
# token and uses that -- and only that -- for checkout, push, and commit
# identity.
#
# Why a test and not just review: every property below is invisible at a
# glance and silently reverts the workflow to a permanently-403ing state if
# dropped (a plausible "simplification" is to delete the token step and let
# checkout use its default token -- which reads fine and fails only at merge
# time, in a workflow nobody watches). The four ad-hoc shell controls run
# while implementing #7871 were never committed, so nothing pinned these
# invariants until now.
#
# What this asserts:
#   1. Token minting: the App-token action is pinned by 40-hex commit SHA (not
#      a mutable tag), carries a human-readable version comment, and reads the
#      App id / PEM from the two named repository secrets -- never a literal.
#   2. Least privilege: the workflow-level default token is read-only, the
#      installation token is scoped to this owner + this repository with only
#      `contents: write`, and no `permissions:` block grants `contents: write`.
#   3. No GITHUB_TOKEN push path remains (#7829 acceptance criterion 2): no
#      executable line references `secrets.GITHUB_TOKEN` / `github.token`, and
#      the `github-actions[bot]` identity appears nowhere -- there is exactly
#      one pushing identity and no fallback to a second one.
#   4. Wiring + ordering: the token is minted BEFORE checkout, checkout
#      consumes it, the push step gets it via `GH_TOKEN`, and the commit
#      identity is derived from the App slug the mint step returned.
#   5. Failure behavior: the bot-identity lookup runs under `set -euo pipefail`
#      and BEFORE the first git mutation, so absent/failed credentials abort
#      the job rather than committing with a wrong identity.
#   6. The no-self-trigger invariant (#7743): the trigger is exactly
#      `push` -> `main` -> `defaults/**`, and the bump commit stages only
#      version-bearing files -- never anything under `defaults/` -- so the
#      workflow's own push can never re-trigger it. An App installation token
#      DOES trigger workflows (unlike GITHUB_TOKEN), which makes this the load-
#      bearing loop guard rather than a belt-and-braces one.
#   7. Serialized-not-cancelled concurrency and the bounded retry survive.
#
# Any of these that changes deliberately should be updated here in the same
# PR -- a failure means "confirm this was intended", not "revert blindly".
#
# Source-tree-only by design (#6194): .github/workflows/ lives at the repo
# root, not under defaults/, so it is never shipped into an installed consumer
# repo. This suite SKIPs (exit 0) rather than errors when run outside Loom's
# own checkout.
#
# Usage:
#   bash .loom/scripts/tests/test-version-bump-workflow-identity.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
WORKFLOW="$REPO_ROOT/.github/workflows/version-bump-on-merge.yml"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

pass() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: $1"
}

fail() {
    TESTS_RUN=$((TESTS_RUN + 1))
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: $1"
    shift
    local detail
    for detail in "$@"; do
        echo "    $detail"
    done
}

# Literal substring must be present somewhere in the workflow.
assert_present() {
    local needle="$1" msg="$2"
    if grep -qF -- "$needle" "$WORKFLOW"; then
        pass "$msg"
    else
        fail "$msg" "Expected to find literally: $needle"
    fi
}

# Literal substring must be absent from every non-comment line. Full-line `#`
# comments are stripped first so the workflow's own prose (which necessarily
# NAMES GITHUB_TOKEN to explain why it is unusable here) does not trip a check
# about what the workflow actually DOES.
assert_absent_from_code() {
    local needle="$1" msg="$2" hits
    hits="$(grep -vE '^[[:space:]]*#' "$WORKFLOW" | grep -nF -- "$needle" || true)"
    if [[ -z "$hits" ]]; then
        pass "$msg"
    else
        fail "$msg" "Unexpected executable reference to: $needle" "$hits"
    fi
}

assert_matches() {
    local pattern="$1" msg="$2"
    if grep -qE -- "$pattern" "$WORKFLOW"; then
        pass "$msg"
    else
        fail "$msg" "No line matched: $pattern"
    fi
}

assert_no_match() {
    local pattern="$1" msg="$2" hits
    hits="$(grep -nE -- "$pattern" "$WORKFLOW" || true)"
    if [[ -z "$hits" ]]; then
        pass "$msg"
    else
        fail "$msg" "Unexpected match for: $pattern" "$hits"
    fi
}

# First line number containing a literal substring ("" when absent).
line_of() {
    grep -nF -- "$1" "$WORKFLOW" | head -1 | cut -d: -f1
}

# Assert one literal appears strictly before another in file order.
assert_order() {
    local first="$1" second="$2" msg="$3"
    local a b
    a="$(line_of "$first")"
    b="$(line_of "$second")"
    if [[ -z "$a" || -z "$b" ]]; then
        fail "$msg" "Could not locate both anchors (first='$a' second='$b')" \
            "  first : $first" "  second: $second"
    elif [[ "$a" -lt "$b" ]]; then
        pass "$msg (line $a before line $b)"
    else
        fail "$msg" "Expected line $a ('$first') to come before line $b ('$second')"
    fi
}

if [[ ! -f "$WORKFLOW" ]]; then
    echo "SKIP: source-tree-only test, $WORKFLOW not found (not shipped into an installed repo)" >&2
    exit 0
fi

echo "Testing version-bump-on-merge.yml pushing identity ($WORKFLOW)"
echo ""

# ---------------------------------------------------------------------------
# Case 1: the App installation token is minted from SHA-pinned action + secrets
# ---------------------------------------------------------------------------
echo "Case 1: App-token minting step"

assert_matches \
    '^ +uses: actions/create-github-app-token@[0-9a-f]{40}([[:space:]]|$)' \
    "Case 1: create-github-app-token is pinned by 40-hex commit SHA (not a mutable tag)"

assert_matches \
    '^ +uses: actions/create-github-app-token@[0-9a-f]{40} +# +v[0-9]' \
    "Case 1: the SHA pin carries a '# vX.Y.Z' comment so the version is readable"

assert_present 'id: app-token' \
    "Case 1: the mint step is addressable as steps.app-token"

assert_present 'app-id: ${{ secrets.LOOM_FLEET_DISPATCH_APP_ID }}' \
    "Case 1: app id comes from the LOOM_FLEET_DISPATCH_APP_ID secret"

assert_present 'private-key: ${{ secrets.LOOM_FLEET_DISPATCH_APP_PRIVATE_KEY }}' \
    "Case 1: PEM comes from the LOOM_FLEET_DISPATCH_APP_PRIVATE_KEY secret"

# The App id is public, but hardcoding it (or, far worse, any PEM material)
# defeats the secret indirection the operator provisions.
assert_no_match '^[^#]*(4486636|BEGIN [A-Z ]*PRIVATE KEY)' \
    "Case 1: no hardcoded App id or private-key material in the workflow"

echo ""

# ---------------------------------------------------------------------------
# Case 2: least privilege
# ---------------------------------------------------------------------------
echo "Case 2: least privilege"

DEFAULT_PERMS="$(awk '/^permissions:/{found=1; next} found && NF {print; exit}' "$WORKFLOW")"
if [[ "$DEFAULT_PERMS" == *"contents: read"* ]]; then
    pass "Case 2: workflow-level default token is read-only (contents: read)"
else
    fail "Case 2: workflow-level default token is read-only (contents: read)" \
        "First entry under 'permissions:' was: '${DEFAULT_PERMS:-<none>}'"
fi

# `permission-contents: write` (an input to the mint step) is a different key
# and must not be confused with a `permissions:` grant -- hence the anchor.
assert_no_match '^ +contents: write[[:space:]]*$' \
    "Case 2: no permissions: block grants contents: write to the default token"

assert_present 'owner: ${{ github.repository_owner }}' \
    "Case 2: installation token is scoped to this repository owner"

assert_present 'repositories: ${{ github.event.repository.name }}' \
    "Case 2: installation token is scoped to this repository only"

assert_present 'permission-contents: write' \
    "Case 2: installation token requests exactly contents: write"

echo ""

# ---------------------------------------------------------------------------
# Case 3: no GITHUB_TOKEN-based push path remains (#7829 acceptance #2)
# ---------------------------------------------------------------------------
echo "Case 3: no GITHUB_TOKEN push path"

assert_absent_from_code 'secrets.GITHUB_TOKEN' \
    "Case 3: no executable reference to secrets.GITHUB_TOKEN"

assert_absent_from_code 'github.token' \
    "Case 3: no executable reference to github.token"

assert_absent_from_code 'github-actions[bot]' \
    "Case 3: the github-actions[bot] identity is gone entirely"

echo ""

# ---------------------------------------------------------------------------
# Case 4: token wiring and step ordering
# ---------------------------------------------------------------------------
echo "Case 4: token wiring and ordering"

assert_present 'token: ${{ steps.app-token.outputs.token }}' \
    "Case 4: checkout persists the App token in the remote URL"

assert_present 'GH_TOKEN: ${{ steps.app-token.outputs.token }}' \
    "Case 4: the bump/push step gets the App token via GH_TOKEN"

assert_present 'APP_SLUG: ${{ steps.app-token.outputs.app-slug }}' \
    "Case 4: the commit identity is derived from the minted App's slug"

assert_present 'git config user.name "$bot_login"' \
    "Case 4: git identity is the App bot login, not a hardcoded name"

assert_order 'uses: actions/create-github-app-token@' 'uses: actions/checkout@' \
    "Case 4: the token is minted before checkout"

assert_order 'token: ${{ steps.app-token.outputs.token }}' 'git push origin HEAD:main' \
    "Case 4: checkout consumes the token before the push runs"

echo ""

# ---------------------------------------------------------------------------
# Case 5: failure behavior -- abort before mutating anything
# ---------------------------------------------------------------------------
echo "Case 5: failure behavior"

assert_present 'set -euo pipefail' \
    "Case 5: the bump script runs under set -euo pipefail (a failed lookup aborts)"

assert_order 'bot_id="$(gh api' 'for attempt in' \
    "Case 5: bot identity is resolved before the first git mutation"

assert_order 'bot_id="$(gh api' './scripts/version.sh bump' \
    "Case 5: a failed identity lookup cannot reach the version bump"

echo ""

# ---------------------------------------------------------------------------
# Case 6: the no-self-trigger invariant (#7743)
# ---------------------------------------------------------------------------
echo "Case 6: no-self-trigger invariant"

assert_present 'branches: [main]' \
    "Case 6: trigger is restricted to pushes on main"

TRIGGER_PATHS="$(awk '/^ +paths:/{found=1; next} found {if ($1 == "-") print $2; else exit}' "$WORKFLOW")"
if [[ "$TRIGGER_PATHS" == '"defaults/**"' ]]; then
    pass "Case 6: path filter is exactly defaults/** (one entry)"
else
    fail "Case 6: path filter is exactly defaults/** (one entry)" \
        "Parsed path list was: '${TRIGGER_PATHS:-<none>}'" \
        "Widening this filter lets the bump commit re-trigger the workflow." \
        "App-token pushes DO trigger workflows, so this is the real loop guard."
fi

# Everything the bump commit stages, from the first `git add` through the
# `git commit` that seals it. None of it may live under defaults/.
STAGED_BLOCK="$(awk '/git add /{found=1} found {print} /git commit -m/{exit}' "$WORKFLOW")"
if [[ -z "$STAGED_BLOCK" ]]; then
    fail "Case 6: bump commit stages no defaults/ path" \
        "Could not locate the git add ... git commit block"
elif grep -q 'defaults/' <<<"$STAGED_BLOCK"; then
    fail "Case 6: bump commit stages no defaults/ path" \
        "A staged path under defaults/ would make the workflow re-trigger itself:" \
        "$STAGED_BLOCK"
else
    pass "Case 6: bump commit stages no defaults/ path (cannot re-trigger itself)"
fi

for managed in VERSION package.json mcp-loom/package.json Cargo.toml CLAUDE.md \
    .loom/install-metadata.json; do
    if grep -qF -- "$managed" <<<"$STAGED_BLOCK"; then
        pass "Case 6: version-bearing file still staged: $managed"
    else
        fail "Case 6: version-bearing file still staged: $managed" \
            "scripts/version.sh rewrites it, so an unstaged one desyncs main"
    fi
done

echo ""

# ---------------------------------------------------------------------------
# Case 7: concurrency and retry bound survive
# ---------------------------------------------------------------------------
echo "Case 7: concurrency and retry bound"

assert_present 'group: version-bump-on-merge' \
    "Case 7: runs are serialized in one concurrency group"

assert_present 'cancel-in-progress: false' \
    "Case 7: queued runs are never cancelled (each merge needs its own bump)"

assert_present 'attempts=5' \
    "Case 7: the bounded push retry is still 5 attempts"

assert_present 'git checkout -B main origin/main' \
    "Case 7: each retry re-syncs to the current tip before re-bumping"

echo ""

# --- Summary ---
echo "Tests run: $TESTS_RUN, Passed: $TESTS_PASSED, Failed: $TESTS_FAILED"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
