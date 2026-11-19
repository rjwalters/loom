#!/usr/bin/env bash
# Parity test (#13569): guard-worktree-paths.sh (Edit/Write) and
# guard-destructive-generic.sh (Bash write confinement) must give the SAME
# verdict for the same main-checkout target, and both deny messages must name
# the allowed alternative (issue worktree or /tmp).
#
# A consumer reported Bash writes to a gitignored notes dir denied while the
# Edit tool wrote the same file. This pins "one policy across tools": gitignore
# status never exempts a path, and no managed worktree means both allow.
#
# Usage: ./tests/hooks/test-guard-worktree-edit-bash-parity.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

EDIT_GUARD="$REPO_ROOT/defaults/hooks/guard-worktree-paths.sh"
BASH_GUARD="$REPO_ROOT/defaults/hooks/guard-destructive-generic.sh"

echo -e "${YELLOW}--- Edit/Write vs Bash worktree-confinement parity (#13569) ---${NC}"

# Echo "deny" or "allow" plus the reason for a hook run. Scrubs ambient overrides.
edit_verdict() { # <repo> <path> [worktree-path]
    local repo="$1" path="$2" wt="${3:-}" out
    out=$(jq -n --arg p "$path" --arg c "$repo" \
        '{tool_name:"Write",tool_input:{file_path:$p,content:"x"},cwd:$c}' |
        (cd "$repo" && env -u LOOM_FORCE_SCOPE -u LOOM_WORKTREE_PATH -u LOOM_GUARD_WORKTREE_ISOLATION \
            ${wt:+LOOM_WORKTREE_PATH="$wt"} "$EDIT_GUARD" 2>&1)) || true
    printf '%s' "$out"
}
bash_verdict() { # <repo> <path> [worktree-path]
    local repo="$1" path="$2" wt="${3:-}" out
    out=$(jq -n --arg cmd "echo x >> $path" --arg c "$repo" \
        '{tool_name:"Bash",tool_input:{command:$cmd},cwd:$c}' |
        (cd "$repo" && env -u LOOM_FORCE_SCOPE -u LOOM_WORKTREE_PATH -u LOOM_GUARD_WORKTREE_ISOLATION \
            ${wt:+LOOM_WORKTREE_PATH="$wt"} "$BASH_GUARD" 2>&1)) || true
    printf '%s' "$out"
}
decision() { [[ -z "$1" ]] && { echo allow; return; }; jq -r '.hookSpecificOutput.permissionDecision // "allow"' <<<"$1" 2>/dev/null || echo allow; }

check_parity() { # <desc> <repo> <path> <expected> [worktree-path]
    local desc="$1" repo="$2" path="$3" want="$4" wt="${5:-}"
    local e b ed bd
    e=$(edit_verdict "$repo" "$path" "$wt"); b=$(bash_verdict "$repo" "$path" "$wt")
    ed=$(decision "$e"); bd=$(decision "$b")
    TOTAL=$((TOTAL + 1))
    if [[ "$ed" == "$want" && "$bd" == "$want" ]]; then
        PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: $desc ($want/$want)"
    else
        FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: $desc"
        echo "       want=$want edit=$ed bash=$bd path=$path"
        echo "       edit: $e"; echo "       bash: $b"
    fi
}

REPO=$(make_wt_repo)
printf '.notes/\n' > "$REPO/.gitignore"
mkdir -p "$REPO/.notes" "$REPO/.loom/hooks"
WT="$REPO/.loom/worktrees/issue-1"

for wtp in "" "$WT"; do
    tag="LOOM_WORKTREE_PATH=${wtp:+set}${wtp:-unset}"
    check_parity "gitignored notes dir ($tag)" "$REPO" "$REPO/.notes/j.md" deny "$wtp"
    check_parity "tracked-style file ($tag)" "$REPO" "$REPO/defaults/hooks/f.sh" deny "$wtp"
    check_parity "installed .loom/hooks file ($tag)" "$REPO" "$REPO/.loom/hooks/x.sh" deny "$wtp"
done
check_parity "inside the issue worktree allows" "$REPO" "$WT/src/f.txt" allow "$WT"

# No managed worktree anywhere: both guards allow (isolation not in play).
BARE=$(mktemp -d); BARE=$(cd "$BARE" && pwd -P)
git -C "$BARE" init -q >/dev/null 2>&1
printf '.notes/\n' > "$BARE/.gitignore"; mkdir -p "$BARE/.notes"
check_parity "no managed worktree: ignored notes dir" "$BARE" "$BARE/.notes/j.md" allow

# Deny messages name the allowed alternative and say the tools are confined alike.
for who in edit bash; do
    out=$("${who}_verdict" "$REPO" "$REPO/.notes/j.md")
    reason=$(jq -r '.hookSpecificOutput.permissionDecisionReason // ""' <<<"$out")
    TOTAL=$((TOTAL + 1))
    if grep -q '/tmp' <<<"$reason" && grep -q 'issue worktree' <<<"$reason" \
        && grep -q 'same rule' <<<"$reason"; then
        PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: $who deny message names the alternative"
    else
        FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: $who deny message lacks the alternative"
        echo "       $reason"
    fi
done

rm -rf "$REPO" "$BARE"
print_summary
