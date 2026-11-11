#!/usr/bin/env bash
# Evidence pins for defaults/docs/guard-confinement-evidence.md (#13777).
#
# Pins, against the CURRENT guards, the case families imported from the
# gf180-opamp guard-confinement fixture (29 synthetic cases, families A-F) that
# no other suite already covers:
#   - E1-E9 negative controls (must stay denied),
#   - C2/C4 mktemp shapes (recorded: denied as unresolved-var),
#   - F1-F4 other-agent-worktree writes: the guards ALLOW these today. That is
#     a known, documented gap (guard-worktree-paths.sh cannot tell which
#     session owns a worktree, #3719) -- these are characterization pins, not
#     an endorsement. If a guard change flips one, this test fails and forces
#     a decision; changing guard behaviour is a separate issue.
#
# Usage: ./tests/hooks/test-guard-confinement-evidence.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

EDIT_GUARD="$REPO_ROOT/defaults/hooks/guard-worktree-paths.sh"

echo -e "${YELLOW}--- Guard-confinement evidence pins (#13777) ---${NC}"

REPO=$(make_wt_repo_two_linked)
WT="$REPO/.loom/worktrees/issue-1"
WT2="$REPO/.loom/worktrees/issue-2"
SCRATCH=$(cd "$(mktemp -d)" && pwd -P)
LINK="$SCRATCH/link-to-agent2"
ln -s "$WT2" "$LINK"

# Isolation must be in play and no ambient override may mask it.
unset LOOM_GUARD_WORKTREE_ISOLATION LOOM_WORKTREE_PATH LOOM_FORCE_SCOPE

# --- E1-E9 negative controls (opamp fixture family E) ----------------------
assert_deny_reason_matches "E1 unresolved destination denies as unresolved-var" \
    'echo inert > $V/fresh.json' 'unexpanded|unresolved' "$WT"
assert_deny "E2 variable rebound to the main checkout denies" \
    "S=$SCRATCH; S=$REPO; echo inert > \$S/verdict.md" "$WT"
# E3 is a RECORDED case (no presumed contract): today it fails closed.
assert_deny "E3 (recorded) variable rebound main-then-scratch fails closed" \
    "S=$REPO; S=$SCRATCH; echo inert > \$S/verdict.md" "$WT"
assert_deny "E4 literal redirect into the main checkout denies" \
    "echo inert > $REPO/verdict.md" "$WT"
assert_deny "E5 mkdir into the main checkout denies" \
    "mkdir -p $REPO/.loom/sweep-checkpoint-inert" "$WT"
assert_deny "E6 relative redirect with cwd at the main checkout denies" \
    "echo inert > verdict.md" "$REPO"
assert_deny_reason_matches "E7 scratch write then unresolved write denies the whole command" \
    "S=$SCRATCH; echo a > \$S/a.md; echo b > \$U/b.md" 'unexpanded|unresolved' "$WT"
assert_deny "E8 scratch write bundled with a main-checkout write denies" \
    "echo a > $SCRATCH/a.md && echo b > $REPO/b.md" "$WT"
assert_deny "E9 mktemp handling bundled with a main-checkout write denies" \
    "D=\$(mktemp -d); echo a > \$D/a.md; echo b > $REPO/b.md" "$WT"

# --- C2/C4 (recorded, no presumed contract): denied today ------------------
assert_deny "C2 (recorded) mktemp -d with template, cp into it: denied today" \
    "D=\$(mktemp -d $SCRATCH/c2-XXXXXX); cp $WT/README.md \$D/verdict.md" "$WT"
assert_deny "C4 (recorded) inline \$(mktemp -d)/ redirect destination: denied today" \
    'echo inert-verdict > $(mktemp -d)/verdict.md' "$WT"

# --- F1/F2 (Bash) other agent's worktree: allowed today (known gap) --------
assert_allow "F1 (known gap, characterization) Bash redirect into another agent's worktree allows" \
    "echo inert > $WT2/verdict.md" "$WT"
assert_allow "F2 (known gap, characterization) Bash redirect via symlink to another agent's worktree allows" \
    "echo inert > $LINK/verdict.md" "$WT"

# --- F3/F4/F5 (Edit/Write tool) --------------------------------------------
write_decision() { # <file_path>
    local out
    out=$(jq -n --arg p "$1" --arg c "$WT" \
        '{tool_name:"Write",tool_input:{file_path:$p,content:"inert"},cwd:$c}' |
        (cd "$WT" && "$EDIT_GUARD" 2>&1)) || true
    [[ -z "$out" ]] && { echo allow; return; }
    jq -r '.hookSpecificOutput.permissionDecision // "allow"' <<<"$out" 2>/dev/null || echo allow
}
check_write() { # <desc> <path> <want>
    local got; got=$(write_decision "$2"); TOTAL=$((TOTAL + 1))
    if [[ "$got" == "$3" ]]; then PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: $1"
    else FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: $1 (want=$3 got=$got)"; fi
}
check_write "F3 (known gap, characterization) Write into another agent's worktree allows" "$WT2/verdict.md" allow
check_write "F4 (known gap, characterization) Write via symlink to another agent's worktree allows" "$LINK/verdict.md" allow
check_write "F5 control: Write inside the acting agent's own worktree allows" "$WT/verdict.md" allow
check_write "control: Write into the main checkout denies" "$REPO/verdict.md" deny

rm -rf "$REPO" "$SCRATCH"
print_summary
