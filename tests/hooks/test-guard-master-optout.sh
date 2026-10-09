#!/usr/bin/env bash
# Tests for the guards.enabled master opt-out (issue #10335): with
# guards.enabled:false in .loom/config.json (or LOOM_GUARDS_ENABLED=0) the three
# Loom PreToolUse guard hooks allow everything; absent/true keeps them on.
#
# The hooks ask `loom-daemon guard-hook opted-out`, so this suite NEEDS A BUILT
# loom-daemon (pinned via lib/require-daemon-bin.sh, which FAILS rather than
# SKIPs). It is listed in ci-excluded.txt and wired into the "Native Port
# Suites" CI job, which downloads the shared debug build.
set -uo pipefail
unset LOOM_GUARDS_ENABLED LOOM_GUARD_SQL LOOM_GUARD_WORKTREE_ISOLATION

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
H="$ROOT/defaults/hooks"
PASS=0; FAIL=0
# shellcheck source=../../defaults/scripts/tests/lib/require-daemon-bin.sh
source "$ROOT/defaults/scripts/tests/lib/require-daemon-bin.sh"
loom_test_require_daemon_bin "$ROOT/defaults/scripts" "guard-hook"

mk_repo() { # $1 = guards JSON fragment ('' for none)
    local d; d=$(mktemp -d)
    git -C "$d" init -q
    mkdir -p "$d/.loom"
    if [[ -n "$1" ]]; then echo "{\"guards\": $1}" > "$d/.loom/config.json"; else echo '{}' > "$d/.loom/config.json"; fi
    echo "$d"
}
denied() { grep -q '"permissionDecision": *"deny"'; }
bash_in() { # hook repo cmd [env...]
    local hook="$1" repo="$2" cmd="$3"
    jq -n --arg c "$cmd" --arg d "$repo" '{tool_name:"Bash",tool_input:{command:$c},cwd:$d}' \
        | (cd "$repo" && env LOOM_PROJECT_ROOT="$repo" bash "$H/$hook" 2>/dev/null)
}
check() { # desc expect(deny|allow) output
    local got=allow; printf '%s' "$3" | denied && got=deny
    if [[ "$got" == "$2" ]]; then PASS=$((PASS+1)); echo "  PASS: $1"; else FAIL=$((FAIL+1)); echo "  FAIL: $1 (expected $2, got $got)"; fi
}

MERGE_CMD="gh pr"; MERGE_CMD="$MERGE_CMD merge 12"
SQL_CMD="psql -c 'DELETE FROM users;'"

ON=$(mk_repo '');  OFF=$(mk_repo '{"enabled": false}'); EXPLICIT_ON=$(mk_repo '{"enabled": true}')
trap 'rm -rf "$ON" "$OFF" "$EXPLICIT_ON"' EXIT

echo "guards.enabled master opt-out (#10335)"
check "workflow: absent key keeps guard on"      deny  "$(bash_in guard-loom-workflow.sh "$ON" "$MERGE_CMD")"
check "workflow: enabled:true keeps guard on"    deny  "$(bash_in guard-loom-workflow.sh "$EXPLICIT_ON" "$MERGE_CMD")"
check "workflow: enabled:false allows"           allow "$(bash_in guard-loom-workflow.sh "$OFF" "$MERGE_CMD")"
check "workflow: LOOM_GUARDS_ENABLED=0 allows"   allow "$(LOOM_GUARDS_ENABLED=0 bash_in guard-loom-workflow.sh "$ON" "$MERGE_CMD")"
check "destructive: absent key keeps guard on"   deny  "$(bash_in guard-destructive.sh "$ON" "$SQL_CMD")"
check "destructive: enabled:false allows"        allow "$(bash_in guard-destructive.sh "$OFF" "$SQL_CMD")"
check "destructive: LOOM_GUARDS_ENABLED=0 allows" allow "$(LOOM_GUARDS_ENABLED=0 bash_in guard-destructive.sh "$ON" "$SQL_CMD")"
check "destructive: catastrophic floor also off" allow "$(bash_in guard-destructive.sh "$OFF" 'rm -rf /')"
check "destructive: floor on when absent"        deny  "$(bash_in guard-destructive.sh "$ON" 'rm -rf /')"
# Fail closed: with no daemon to ask, the opt-out cannot be confirmed, so the
# guards stay ON even under an explicit opt-out.
NO_DAEMON=/nonexistent/loom-daemon
check "no daemon: enabled:false keeps workflow on" deny \
    "$(LOOM_DAEMON_SELF_BIN=$NO_DAEMON bash_in guard-loom-workflow.sh "$OFF" "$MERGE_CMD")"
check "no daemon: LOOM_GUARDS_ENABLED=0 keeps destructive on" deny \
    "$(LOOM_DAEMON_SELF_BIN=$NO_DAEMON LOOM_GUARDS_ENABLED=0 bash_in guard-destructive.sh "$ON" "$SQL_CMD")"

# guard-worktree-paths.sh: a Write into the main checkout while a managed
# worktree exists is denied; with the opt-out it is allowed.
wt_case() { # repo -> output
    local repo="$1"
    mkdir -p "$repo/.loom/worktrees/issue-1"; touch "$repo/.loom/worktrees/issue-1/.loom-managed"
    git -C "$repo" -c user.email=t@t -c user.name=t commit -q --allow-empty -m init
    jq -n --arg p "$repo/file.txt" --arg d "$repo" '{tool_name:"Write",tool_input:{file_path:$p,content:"x"},cwd:$d}' \
        | (cd "$repo" && bash "$H/guard-worktree-paths.sh" 2>/dev/null)
}
check "worktree-paths: absent key keeps guard on" deny  "$(wt_case "$ON")"
check "worktree-paths: enabled:false allows"      allow "$(wt_case "$OFF")"

echo "Passed: $PASS  Failed: $FAIL"
[[ $FAIL -eq 0 ]]
