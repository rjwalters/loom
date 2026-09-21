#!/usr/bin/env bash
# guard-destructive-harness.sh — shared fixtures and assertions for the
# tests/hooks/test-guard-destructive-*.sh suites.
#
# Sourced, never executed. Split out of the single 8.9k-line
# test-guard-destructive.sh (#7741): every suite needs the same hermetic env
# baseline, the same GUARD path, the same pass/fail counters and the same
# assert_* vocabulary, so those live here exactly once.
#
# check-ci-suite-manifest.sh ignores lib/ by design — only `test-*.sh` files are
# suites, so this file needs no ci-wired.txt entry.
#
# Portability: bash 3.2 clean (macOS stock). No associative arrays, no ${x,,}.

# Test suite for defaults/hooks/guard-destructive-generic.sh
#
# Usage: ./tests/hooks/test-guard-destructive.sh
#
# Tests the generic destructive-command pattern guard against various command
# patterns. Exit code 0 = all tests pass, 1 = failures detected.
#
# Since #4041 the generic pattern list lives in guard-destructive-generic.sh —
# the vendored copy of Repo Skills' canonical guard that Loom ships for
# standalone repos (no Repo Skills installed). guard-destructive.sh itself is now
# a thin dispatcher that defers to the canonical guard when present, else to this
# vendored generic; so the pattern-matching behavior Loom is responsible for
# shipping is validated here against the generic file directly.
#
# The guard under test is the canonical source at defaults/hooks/ (the
# version-controlled source of truth), NOT the gitignored .loom/hooks/ install
# artifact — so the suite validates exactly what ships.

set -euo pipefail

# Hermetic baseline: ambient guard-behavior overrides must not leak into tests
# (#4325). Tests that exercise env-driven behavior deliberately inject their
# vars explicitly per invocation (run_guard_env / assert_ask_env / assert_allow_env),
# or via run_guard_in_worktree, so they are unaffected by this unset.
unset LOOM_FORCE_SCOPE LOOM_DEFAULT_BRANCH LOOM_GUARD_SQL LOOM_GUARD_CLOUD \
      LOOM_GUARD_REVERSIBLE_GH LOOM_RM_SCOPE LOOM_GUARD_READONLY_FASTPATH \
      LOOM_GUARD_WORKTREE_ISOLATION LOOM_WORKTREE_PATH LOOM_WORKTREE_ROOT \
      LOOM_GUARD_DECISION_LOG LOOM_GUARD_DECISION_LOG_FILE LOOM_GUARD_STASH_SCOPE \
      LOOM_ROLE LOOM_GUARD_CARGO_CLEAN CARGO_TARGET_DIR LOOM_GUARD_SCRATCH_ROOT

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
# $LOOM_GUARD_OVERRIDE (#8217) points the whole corpus at a DIFFERENT guard
# binary for one run. Its only purpose is the differential sweep every
# guard-parsing fix in this family owes (#8003 → #8035 → #8217): run the corpus
# once against a pre-fix copy of the hook, once against the fixed one, and diff
# the per-assertion verdicts so a verdict flip cannot hide behind a green suite.
# Deliberately NOT in the hermetic unset list above — it has to survive to do
# its job — so it is the one var that can change what this harness tests; leave
# it unset for every ordinary run (CI never sets it). The pre-fix copy belongs
# under defaults/hooks/, NOT /tmp: the guard sources
# $SCRIPT_DIR/../scripts/lib/config-resolver.sh, so a /tmp copy silently loses
# guards.* config resolution and manufactures phantom verdict flips (#8218's
# methodology note).
GUARD="${LOOM_GUARD_OVERRIDE:-$REPO_ROOT/defaults/hooks/guard-destructive-generic.sh}"

# Hermetic default cwd (#7808). The guard resolves REPO_ROOT from the hook
# input's cwd (git rev-parse --show-toplevel) and reads THAT repo's
# .loom/config.json for every guards.* toggle. With the checkout as the
# default cwd, every assertion silently tested this repo's committed dogfood
# config instead of the guard's defaults — #7799 flipped guards.sqlDdl and
# four suites went red on main with the PR's run skipped. So the default cwd
# is a throwaway repo with NO .loom/config.json: every toggle at its shipped
# default, whatever this repo's own config says. Suites that need the real
# checkout (rm-scope's path-confinement cases, decision-log's file paths)
# pass "$REPO_ROOT" explicitly; suites that need a specific config build a
# temp repo via make_sql_repo. Canonicalised with pwd -P because the guard
# compares git's resolved toplevel (/private/var/... on macOS) against cwd.
TEST_REPO=$(mktemp -d 2>/dev/null || mktemp -d -t loom-guard-test)
TEST_REPO=$(cd "$TEST_REPO" && pwd -P)
git -C "$TEST_REPO" init -q >/dev/null 2>&1
git -C "$TEST_REPO" checkout -q -b main >/dev/null 2>&1 || true
git -C "$TEST_REPO" -c user.email=guard-test@loom -c user.name=guard-test \
    commit -q --allow-empty -m "hermetic guard test repo" >/dev/null 2>&1

PASS=0
FAIL=0
TOTAL=0

# Colors (if terminal supports them)
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

# Build a JSON input blob for the guard script

# Run the guard and capture output + exit code

# Run the guard with LOOM_WORKTREE_PATH set (simulates worktree context)

# --- SQL opt-out helpers (guards.sqlDdl / LOOM_GUARD_SQL) ---

# Create a throwaway git repo whose .loom/config.json holds the given JSON.
# Echoes the repo path (which becomes the guard's cwd / resolved REPO_ROOT).
# NB: callers invoke this via command substitution (a subshell), so this must
# not try to record state in the parent — cleanup is done by path at the end.

# Run the guard with an optional env assignment (e.g. "LOOM_GUARD_SQL=0").

# Assert deny with an env assignment + cwd (repo root).

# Assert allow (exit 0, no decision) with an env assignment + cwd.

# Assert ask with an env assignment + cwd (repo root).

# Assert the guard denies a command when inside a worktree

# Assert the guard allows a command when inside a worktree

# Assert the guard denies a command

# Assert the guard asks for confirmation

# Assert the guard asks AND the ask reason matches an extended regex.

# Assert the guard denies AND the deny reason matches an extended regex.
# Mirrors assert_ask_reason_matches above; added for #5754, where the
# create-side stash redirect's value is entirely in what its message SAYS
# (the literal per-issue replacement command), not just that it denies.

# Assert the guard allows a command (no output, exit 0)

# =========================================================================
echo ""
echo -e "${YELLOW}=== Testing guard-destructive.sh ===${NC}"
echo ""

# =========================================================================

# Per-invocation `session_id` on the hook's stdin (#8460). Claude Code always
# supplies it; the corpus historically did not, so it stays OPTIONAL — empty
# (the default) omits the key entirely, exactly as before, and every existing
# assertion is unaffected. A suite that exercises session identity sets this
# global around the block that needs it and clears it afterwards, which keeps
# all six assert_* wrappers unchanged.
GUARD_TEST_SESSION_ID="${GUARD_TEST_SESSION_ID:-}"

make_input() {
    local cmd="$1"
    local cwd="${2:-$TEST_REPO}"
    jq -n --arg cmd "$cmd" --arg cwd "$cwd" --arg sid "${GUARD_TEST_SESSION_ID:-}" '{
        tool_name: "Bash",
        tool_input: { command: $cmd },
        cwd: $cwd
    } + (if $sid == "" then {} else { session_id: $sid } end)'
}

run_guard() {
    local cmd="$1"
    local cwd="${2:-$TEST_REPO}"
    local output
    local exit_code
    output=$(make_input "$cmd" "$cwd" | "$GUARD" 2>&1) || exit_code=$?
    exit_code=${exit_code:-0}
    echo "$output"
    return $exit_code
}

run_guard_in_worktree() {
    local cmd="$1"
    local cwd="${2:-$TEST_REPO}"
    local output
    local exit_code
    output=$(LOOM_WORKTREE_PATH="$cwd" make_input "$cmd" "$cwd" | LOOM_WORKTREE_PATH="$cwd" "$GUARD" 2>&1) || exit_code=$?
    exit_code=${exit_code:-0}
    echo "$output"
    return $exit_code
}

make_sql_repo() {
    local config_json="$1"
    local dir
    dir=$(mktemp -d 2>/dev/null)
    git -C "$dir" init -q >/dev/null 2>&1
    mkdir -p "$dir/.loom"
    printf '%s' "$config_json" > "$dir/.loom/config.json"
    echo "$dir"
}

run_guard_env() {
    local env_kv="$1"
    local cmd="$2"
    local cwd="${3:-$TEST_REPO}"
    local output
    local exit_code=0
    if [[ -n "$env_kv" ]]; then
        output=$(make_input "$cmd" "$cwd" | env "$env_kv" "$GUARD" 2>&1) || exit_code=$?
    else
        output=$(make_input "$cmd" "$cwd" | "$GUARD" 2>&1) || exit_code=$?
    fi
    echo "$output"
    return $exit_code
}

assert_deny_env() {
    local description="$1"; local env_kv="$2"; local cmd="$3"; local cwd="${4:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))
    local output
    output=$(run_guard_env "$env_kv" "$cmd" "$cwd") || true
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd (env: ${env_kv:-none}, cwd: $cwd)"
        echo -e "       Expected: deny"
        echo -e "       Got: $output"
    fi
}

assert_allow_env() {
    local description="$1"; local env_kv="$2"; local cmd="$3"; local cwd="${4:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))
    local output
    local exit_code=0
    output=$(run_guard_env "$env_kv" "$cmd" "$cwd") || exit_code=$?
    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd (env: ${env_kv:-none}, cwd: $cwd)"
        echo -e "       Expected: allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

assert_ask_env() {
    local description="$1"; local env_kv="$2"; local cmd="$3"; local cwd="${4:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))
    local output
    output=$(run_guard_env "$env_kv" "$cmd" "$cwd") || true
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "ask"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd (env: ${env_kv:-none}, cwd: $cwd)"
        echo -e "       Expected: ask"
        echo -e "       Got: $output"
    fi
}

assert_deny_in_worktree() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))

    local output
    output=$(run_guard_in_worktree "$cmd" "$cwd") || true

    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny"
        echo -e "       Got: $output"
    fi
}

assert_allow_in_worktree() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))

    local output
    local exit_code=0
    output=$(run_guard_in_worktree "$cmd" "$cwd") || exit_code=$?

    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

assert_deny() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))

    local output
    output=$(run_guard "$cmd" "$cwd") || true

    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny"
        echo -e "       Got: $output"
    fi
}

assert_ask() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))

    local output
    output=$(run_guard "$cmd" "$cwd") || true

    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "ask"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: ask"
        echo -e "       Got: $output"
    fi
}

assert_ask_reason_matches() {
    local description="$1"
    local cmd="$2"
    local pattern="$3"
    local cwd="${4:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))

    local output reason
    output=$(run_guard "$cmd" "$cwd") || true
    reason=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null)

    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "ask"' >/dev/null 2>&1 && \
       echo "$reason" | grep -qE "$pattern"; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: ask with reason matching /$pattern/"
        echo -e "       Got: $output"
    fi
}

assert_deny_reason_matches() {
    local description="$1"
    local cmd="$2"
    local pattern="$3"
    local cwd="${4:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))

    local output reason
    output=$(run_guard "$cmd" "$cwd") || true
    reason=$(echo "$output" | jq -r '.hookSpecificOutput.permissionDecisionReason // empty' 2>/dev/null)

    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1 && \
       echo "$reason" | grep -qE "$pattern"; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: deny with reason matching /$pattern/"
        echo -e "       Got: $output"
    fi
}

assert_allow() {
    local description="$1"
    local cmd="$2"
    local cwd="${3:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))

    local output
    local exit_code=0
    output=$(run_guard "$cmd" "$cwd") || exit_code=$?

    # Allow = exit 0 with no deny/ask decision
    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

assert_deny_permissive() {
    local description="$1"; local cmd="$2"; local cwd="$3"
    TOTAL=$((TOTAL + 1))
    local output exit_code=0
    output=$(make_input "$cmd" "$cwd" | env "${PERMISSIVE_GUARD_ENV[@]}" "$GUARD" 2>&1) || exit_code=$?
    if echo "$output" | jq -e '.hookSpecificOutput.permissionDecision == "deny"' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd (cwd: $cwd, all guards.* + LOOM_* set permissive)"
        echo -e "       Expected: deny"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

assert_allow_permissive() {
    local description="$1"; local cmd="$2"; local cwd="$3"
    TOTAL=$((TOTAL + 1))
    local output exit_code=0
    output=$(make_input "$cmd" "$cwd" | env "${PERMISSIVE_GUARD_ENV[@]}" "$GUARD" 2>&1) || exit_code=$?
    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd (cwd: $cwd)"
        echo -e "       Expected: allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

assert_allow_silent() {
    local description="$1"; local cmd="$2"; local cwd="${3:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))
    local output; local exit_code=0
    output=$(run_guard "$cmd" "$cwd") || exit_code=$?
    if [[ $exit_code -eq 0 && -z "$output" ]]; then
        PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: $cmd"
        echo -e "       Expected: allow with EMPTY output (exit 0, 0 bytes)"
        echo -e "       Exit code: $exit_code  Output bytes: ${#output}"
        echo -e "       Got: $output"
    fi
}

make_project_tier_repo() {
    local project_json="$1" legacy_json="${2:-}"
    local dir
    dir=$(mktemp -d 2>/dev/null)
    git -C "$dir" init -q >/dev/null 2>&1
    mkdir -p "$dir/.loom-project"
    printf '%s' "$project_json" > "$dir/.loom-project/project.json"
    if [[ -n "$legacy_json" ]]; then
        mkdir -p "$dir/.loom"
        printf '%s' "$legacy_json" > "$dir/.loom/config.json"
    fi
    echo "$dir"
}

dl_assert() {
    TOTAL=$((TOTAL + 1))
    if [[ "$2" -eq 0 ]]; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $1"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $1"
        [[ -n "${3:-}" ]] && echo -e "       ${3}"
    fi
}

make_wt_repo() {
    local config_json="${1:-}"
    local dir
    dir=$(mktemp -d 2>/dev/null)
    # Canonicalize: on macOS, mktemp -d returns a path under the /var/folders
    # symlink whose real target is /private/var/folders. The guard resolves
    # REPO_ROOT via `git rev-parse --show-toplevel`, which returns the
    # SYMLINK-RESOLVED form — so comparing an unresolved dir against it would
    # spuriously mismatch (the guard would see the write as "outside the main
    # checkout" and allow it). `cd ... && pwd -P` resolves symlinks the same
    # way the guard's own git-based resolution does.
    dir=$(cd "$dir" && pwd -P)
    git -C "$dir" init -q >/dev/null 2>&1
    mkdir -p "$dir/.loom/worktrees/issue-1/src" "$dir/defaults/hooks"
    : > "$dir/.loom/worktrees/issue-1/.loom-managed"
    if [[ -n "$config_json" ]]; then
        mkdir -p "$dir/.loom"
        printf '%s' "$config_json" > "$dir/.loom/config.json"
    fi
    echo "$dir"
}

make_wt_repo_linked() {
    local dir
    dir=$(mktemp -d 2>/dev/null)
    dir=$(cd "$dir" && pwd -P)
    git -C "$dir" init -q >/dev/null 2>&1
    git -C "$dir" -c user.email=loom@test -c user.name=loom \
        commit -q --allow-empty -m init >/dev/null 2>&1
    mkdir -p "$dir/defaults/hooks" "$dir/.loom/worktrees"
    git -C "$dir" worktree add -q "$dir/.loom/worktrees/issue-1" \
        -b feature/issue-1 >/dev/null 2>&1
    mkdir -p "$dir/.loom/worktrees/issue-1/src"
    : > "$dir/.loom/worktrees/issue-1/.loom-managed"
    echo "$dir"
}

make_wt_repo_nested_unmanaged() {
    local dir
    dir=$(mktemp -d 2>/dev/null)
    dir=$(cd "$dir" && pwd -P)
    git -C "$dir" init -q >/dev/null 2>&1
    git -C "$dir" -c user.email=loom@test -c user.name=loom \
        commit -q --allow-empty -m init >/dev/null 2>&1
    mkdir -p "$dir/defaults/hooks" "$dir/.loom/worktrees" "$dir/.claude/worktrees"
    # A MANAGED worktree elsewhere, so worktree isolation is genuinely in play
    # (_wt_isolation_in_play) and the deny below is a real one, not a fail-open.
    git -C "$dir" worktree add -q "$dir/.loom/worktrees/issue-1" \
        -b feature/issue-1 >/dev/null 2>&1
    : > "$dir/.loom/worktrees/issue-1/.loom-managed"
    # ...and the unmanaged, nested worktree this case is about (no sentinel).
    git -C "$dir" worktree add -q "$dir/.claude/worktrees/x" \
        -b nested/x >/dev/null 2>&1
    mkdir -p "$dir/.claude/worktrees/x/src"
    # A plain directory alongside it that merely LOOKS like a worktree.
    mkdir -p "$dir/.claude/worktrees/not-a-worktree"
    echo "$dir"
}

make_wt_repo_two_linked() {
    local dir
    dir=$(make_wt_repo_two_linked_no_helper)
    mkdir -p "$dir/.loom/scripts"
    printf '#!/usr/bin/env bash\n' > "$dir/.loom/scripts/worktree.sh"
    chmod +x "$dir/.loom/scripts/worktree.sh"
    echo "$dir"
}

make_wt_repo_two_linked_no_helper() {
    local dir
    dir=$(make_wt_repo_linked)
    git -C "$dir" worktree add -q "$dir/.loom/worktrees/issue-2" \
        -b feature/issue-2 >/dev/null 2>&1
    mkdir -p "$dir/.loom/worktrees/issue-2/src"
    : > "$dir/.loom/worktrees/issue-2/.loom-managed"
    echo "$dir"
}

make_wt_repo_linked_spacepath() {
    local base dir
    base=$(mktemp -d 2>/dev/null)
    base=$(cd "$base" && pwd -P)
    dir="$base/Real Estate CRM"
    mkdir -p "$dir"
    git -C "$dir" init -q >/dev/null 2>&1
    git -C "$dir" -c user.email=loom@test -c user.name=loom \
        commit -q --allow-empty -m init >/dev/null 2>&1
    mkdir -p "$dir/defaults/hooks" "$dir/.loom/worktrees"
    git -C "$dir" worktree add -q "$dir/.loom/worktrees/issue-1" \
        -b feature/issue-1 >/dev/null 2>&1
    mkdir -p "$dir/.loom/worktrees/issue-1/src"
    : > "$dir/.loom/worktrees/issue-1/.loom-managed"
    echo "$dir"
}

make_cargo_repo() {
    local cargo_toml="$1"        # empty -> no .cargo/config.toml at all
    local loom_config_json="$2"  # empty -> no .loom/config.json at all
    local dir
    dir=$(mktemp -d 2>/dev/null)
    git -C "$dir" init -q >/dev/null 2>&1
    if [[ -n "$cargo_toml" ]]; then
        mkdir -p "$dir/.cargo"
        printf '%s' "$cargo_toml" > "$dir/.cargo/config.toml"
    fi
    if [[ -n "$loom_config_json" ]]; then
        mkdir -p "$dir/.loom"
        printf '%s' "$loom_config_json" > "$dir/.loom/config.json"
    fi
    echo "$dir"
}

make_cargo_symlinked_repo() {
    local cargo_toml="$1"
    local base
    base=$(mktemp -d 2>/dev/null)
    mkdir -p "$base/real/repo"
    git -C "$base/real/repo" init -q >/dev/null 2>&1
    if [[ -n "$cargo_toml" ]]; then
        mkdir -p "$base/real/repo/.cargo"
        printf '%s' "$cargo_toml" > "$base/real/repo/.cargo/config.toml"
    fi
    ln -s "$base/real" "$base/link"
    echo "$base/link/repo"
}

# ---------------------------------------------------------------------------
# Shared catastrophic-phrase payloads.
#
# Assembled from concatenated fragments on purpose: this file's own source must
# never carry a raw force-push-to-protected or DROP TABLE literal, or the live
# guard hook would trip on the test suite itself.
#
# They live here rather than in one suite because several suites reference them.
# In the monolith they sat mid-file, which is invisible until a split moves a
# consumer away from its producer — `set -u` caught exactly that for _FP_MAIN
# and _HD_DDL while #7741 was being built.
# ---------------------------------------------------------------------------
_PB=main
_MB=master
_FP_MAIN="git push --force origin $_PB"       # direct force-push to protected main
_FP_MASTER="git push --force origin $_MB"     # …to protected master
_FP_MAIN_F="git push -f origin $_PB"           # short -f form
_HD_DDL="DR""OP TA""BLE"
_S3RB="aws s3 r""b"
_S3RB_CAT="catastrophic:${_S3RB}"
_DPRUNE="docker system pr""une"
_S3SYNC="aws s3 syn""c"

# Printed by every suite at exit; the partition's totals must sum to the
# monolith's count.
print_summary() {
    rm -rf "$TEST_REPO"
    echo ""
    echo "========================================="
    echo -e "  Total:  $TOTAL"
    echo -e "  ${GREEN}Passed${NC}: $PASS"
    echo -e "  ${RED}Failed${NC}: $FAIL"
    echo "========================================="
    if [[ $FAIL -gt 0 ]]; then
        echo -e "\n${RED}TESTS FAILED${NC}"
        exit 1
    else
        echo -e "\n${GREEN}ALL TESTS PASSED${NC}"
        exit 0
    fi
}
