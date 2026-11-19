#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — primary-checkout
# mutation guard (#13589): git commit / switch / checkout <branch> in a
# workspace's PRIMARY checkout during an agent step is denied.
#
# Usage: ./tests/hooks/test-guard-destructive-primary-checkout.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

# Hermetic: a dispatched sweep exports these; the "operator session" cases need them unset.
unset LOOM_SWEEP_ID LOOM_WORKTREE_PATH
AGENT="LOOM_SWEEP_ID=step-builder-1-1"

echo -e "${YELLOW}--- Primary-checkout mutation guard (#13589) ---${NC}"

PC_REPO=$(make_wt_repo_linked)
PC_WT="$PC_REPO/.loom/worktrees/issue-1"
git -C "$PC_REPO" branch other >/dev/null 2>&1
DEFAULT_BR=$(git -C "$PC_REPO" symbolic-ref --short HEAD)
# The guard exempts only main/master as "return to default branch".
if [[ "$DEFAULT_BR" != "main" && "$DEFAULT_BR" != "master" ]]; then
    git -C "$PC_REPO" branch -m main >/dev/null 2>&1
    DEFAULT_BR=main
fi

# --- DENY in the primary checkout during an agent step ---
assert_deny_env "git commit in primary is denied" "$AGENT" "git commit -m x" "$PC_REPO"
assert_deny_env "git checkout -b in primary is denied" "$AGENT" "git checkout -b x" "$PC_REPO"
assert_deny_env "git checkout -B (non-default) in primary is denied" "$AGENT" "git checkout -B x origin/main" "$PC_REPO"
assert_deny_env "git switch <branch> in primary is denied" "$AGENT" "git switch other" "$PC_REPO"
assert_deny_env "git switch -c in primary is denied" "$AGENT" "git switch -c x" "$PC_REPO"
assert_deny_env "git checkout <branch> in primary is denied" "$AGENT" "git checkout other" "$PC_REPO"
assert_deny_env "git checkout --detach in primary is denied" "$AGENT" "git checkout --detach" "$PC_REPO"
assert_deny_env "git checkout <sha> (detached) in primary is denied" "$AGENT" \
    "git checkout $(git -C "$PC_REPO" rev-parse HEAD)" "$PC_REPO"
assert_deny_env "git -C <primary> commit from a worktree cwd is denied" "$AGENT" \
    "git -C $PC_REPO commit -m x" "$PC_WT"
assert_deny_env "cd <primary> && git commit from a worktree cwd is denied" "$AGENT" \
    "cd $PC_REPO && git commit -m x" "$PC_WT"
assert_deny_env "LOOM_WORKTREE_PATH also marks an agent step" "LOOM_WORKTREE_PATH=$PC_WT" \
    "git commit -m x" "$PC_REPO"
PC_OUT=$(run_guard_env "$AGENT" "git commit -m x" "$PC_REPO") || true
TOTAL=$((TOTAL + 1))
if echo "$PC_OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason' | grep -q 'worktree.sh <issue-number>' \
   && echo "$PC_OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason' | grep -q 'guards.primaryCheckoutMutation'; then
    PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: deny names worktree.sh and the toggle"
else
    FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: deny names worktree.sh and the toggle: $PC_OUT"
fi

# --- ALLOW: worktrees unaffected ---
assert_allow_env "git commit inside a worktree is allowed" "$AGENT" "git commit -m x" "$PC_WT"
assert_allow_env "git checkout -b inside a worktree is allowed" "$AGENT" "git checkout -b y" "$PC_WT"
assert_allow_env "git switch inside a worktree is allowed" "$AGENT" "git switch other" "$PC_WT"
assert_allow_env "cd <worktree> && git commit from primary cwd is allowed" "$AGENT" \
    "cd $PC_WT && git commit -m x" "$PC_REPO"

# --- ALLOW: legitimate primary-clone shapes ---
assert_allow_env "git checkout main (Auditor) in primary is allowed" "$AGENT" "git checkout main" "$PC_REPO"
assert_allow_env "git checkout -B main origin/main (Auditor) is allowed" "$AGENT" "git checkout -B main origin/main" "$PC_REPO"
assert_allow_env "git switch main in primary is allowed" "$AGENT" "git switch main" "$PC_REPO"
assert_allow_env "git checkout -- <file> restore is allowed" "$AGENT" "git checkout -- somefile" "$PC_REPO"
assert_allow_env "git checkout <path> (not a ref) is allowed" "$AGENT" "git checkout somefile" "$PC_REPO"
assert_allow_env "git switch --help is allowed" "$AGENT" "git switch --help" "$PC_REPO"
assert_allow_env "read-only git status is allowed" "$AGENT" "git status" "$PC_REPO"
assert_allow_env "read-only git log is allowed" "$AGENT" "git log --oneline" "$PC_REPO"
assert_allow_env "git commit-tree is not git commit" "$AGENT" "git commit-tree HEAD^{tree} -m x" "$PC_REPO"
assert_allow_env "quoted text mentioning git commit is inert" "$AGENT" "echo 'git commit -m x'" "$PC_REPO"
assert_allow_env "worktree.sh invocation is allowed" "$AGENT" "./.loom/scripts/worktree.sh 5" "$PC_REPO"
assert_allow_env "merge-pr.sh invocation is allowed" "$AGENT" "./.loom/scripts/merge-pr.sh 5" "$PC_REPO"
assert_allow_env "resync-installed.sh invocation is allowed" "$AGENT" "./.loom/scripts/resync-installed.sh" "$PC_REPO"

# --- not an agent step / toggle off ---
assert_allow_env "operator session (no agent env) is untouched" "" "git commit -m x" "$PC_REPO"
TOTAL=$((TOTAL + 1))
PC_OUT=$(make_input "git commit -m x" "$PC_REPO" | env LOOM_SWEEP_ID=s LOOM_GUARD_PRIMARY_CHECKOUT_MUTATION=0 "$GUARD" 2>&1) || true
if ! echo "$PC_OUT" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
    PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: toggle off via env allows"
else
    FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: toggle off via env allows: $PC_OUT"
fi

PC_CFG=$(make_wt_repo_linked)
mkdir -p "$PC_CFG/.loom"
printf '%s' '{"guards":{"primaryCheckoutMutation":false}}' > "$PC_CFG/.loom/config.json"
assert_allow_env "toggle off via config allows" "$AGENT" "git commit -m x" "$PC_CFG"

rm -rf "$PC_REPO" "$PC_CFG"
echo ""
print_summary
