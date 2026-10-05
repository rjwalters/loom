#!/usr/bin/env bash
# Tests for the gh-body heredoc mask in guard-loom-workflow.sh (issue #10335):
# the BODY of a quoted heredoc fed to `gh issue|pr create|comment|edit` is inert
# data, so a merge command quoted inside it must not trip the merge-pr.sh deny,
# while every executable shape still denies.
#
# The matcher is `loom-daemon guard-hook mask-gh-body-heredocs` (unit-tested in
# loom-daemon/src/guard_hook/tests.rs); this suite drives the REAL hook file so
# it also covers the call-site. It NEEDS A BUILT loom-daemon (pinned via
# lib/require-daemon-bin.sh, which FAILS rather than SKIPs), so it is listed in
# ci-excluded.txt and wired into the "Native Port Suites" CI job.
set -uo pipefail
unset LOOM_GUARDS_ENABLED LOOM_WORKTREE_PATH LOOM_GUARD_WORKTREE_ISOLATION

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
GUARD="$ROOT/defaults/hooks/guard-loom-workflow.sh"
PASS=0; FAIL=0
# shellcheck source=../../defaults/scripts/tests/lib/require-daemon-bin.sh
source "$ROOT/defaults/scripts/tests/lib/require-daemon-bin.sh"
loom_test_require_daemon_bin "$ROOT/defaults/scripts" "guard-hook"

run_guard() { # cmd -> hook stdout
    jq -n --arg c "$1" --arg d "$ROOT" '{tool_name:"Bash",tool_input:{command:$c},cwd:$d}' \
        | "$GUARD" 2>/dev/null
}
check() { # desc expect(deny|allow) cmd
    local got=allow out
    out=$(run_guard "$3")
    [[ "$(jq -r '.hookSpecificOutput.permissionDecision // empty' <<<"$out" 2>/dev/null)" == deny ]] && got=deny
    if [[ "$got" == "$2" ]]; then PASS=$((PASS+1)); echo "  PASS: $1"; else FAIL=$((FAIL+1)); echo "  FAIL: $1 (expected $2, got $got)"; fi
}

P="gh pr"; P="$P merge"
echo "gh issue/pr body heredocs (#10335)"
check "allow quoted heredoc fed to gh issue create --body-file -" allow \
    "gh issue create --title 'x' --body-file - <<'EOF'
Never run $P directly.
EOF"
check "allow cat quoted-heredoc piped into gh issue create" allow \
    "cat <<'EOF' | gh issue create --title x --body-file -
Never run $P directly.
EOF"
check "allow quoted heredoc fed to gh pr comment" allow \
    "gh pr comment 12 --body-file - <<'EOF'
see $P rule
EOF"
check "still deny a real merge invocation" deny "$P 123"
check "still deny real merge after the gh issue heredoc" deny \
    "gh issue create --title x --body-file - <<'EOF'
body
EOF
$P 123"
check "still deny unquoted-delimiter heredoc to gh issue create" deny \
    "gh issue create --title x --body-file - <<EOF
\$($P 5)
EOF"
check "still deny interpreter heredoc after a gh issue create" deny \
    "gh issue create --title x; bash <<'EOF'
$P 123
EOF"
check "still deny cat heredoc piped to gh then bash" deny \
    "cat <<'EOF' | gh issue create --title x --body-file - | bash
$P 123
EOF"
check "still deny when an earlier quoted line could hide the opener" deny \
    "echo \"
gh issue create --body-file - <<'EOF'
\"; $P 123
EOF"
# Fail closed: with no daemon to ask, nothing is masked and the quoted body
# denies exactly as it did before #10335.
LOOM_DAEMON_SELF_BIN=/nonexistent/loom-daemon check "no daemon: the quoted body still denies" deny \
    "gh issue create --title 'x' --body-file - <<'EOF'
Never run $P directly.
EOF"

echo "Passed: $PASS  Failed: $FAIL"
[[ $FAIL -eq 0 ]]
