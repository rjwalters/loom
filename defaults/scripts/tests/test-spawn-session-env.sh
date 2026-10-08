#!/usr/bin/env bash
# test-spawn-session-env.sh — the dispatch session identity does not leak into
# the agent's own environment (#10830).
#
# The daemon hands a dispatch its session identity in the environment:
# LOOM_CLAUDE_SESSION_ID and LOOM_AGENT_SCOPE_UNIT for a fresh launch,
# LOOM_RESUME_SESSION_ID + LOOM_RESUME_PROMPT for a resume. spawn-claude.sh,
# claude-wrapper.sh and spawn-codex.sh consume them. If they stayed exported,
# a nested spawn-claude.sh run from one of the agent's own Bash tool calls
# would reuse the parent's session id (Claude refuses it, or the wrapper
# resumes the parent's live conversation) and, on Linux, the name of the
# parent's still-running systemd scope.
#
# Each stub runtime below starts a CHILD process, standing in for a Bash tool
# call, and reports what that child inherited. LOOM_DAEMON_ITEM_ID stays
# exported on purpose: a nested agent shares the item's pause state.
#
# Sibling of test-spawn-claude.sh, which is frozen by the file-size ratchet.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_ROOT="$(cd "$SCRIPTS_DIR/../.." && pwd)"

DAEMON_BIN=""
for _candidate in \
    "${CARGO_TARGET_DIR:+$CARGO_TARGET_DIR/release/loom-daemon}" \
    "${CARGO_TARGET_DIR:+$CARGO_TARGET_DIR/debug/loom-daemon}" \
    "$REPO_ROOT/target/release/loom-daemon" \
    "$REPO_ROOT/target/debug/loom-daemon"; do
    if [[ -n "$_candidate" && -x "$_candidate" ]]; then
        DAEMON_BIN="$_candidate"
        break
    fi
done
if [[ -z "$DAEMON_BIN" ]]; then
    echo "FATAL: no loom-daemon binary found under \$CARGO_TARGET_DIR or $REPO_ROOT/target" >&2
    echo "  Build one first: cargo build -p loom-daemon" >&2
    exit 1
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_contains() {
    local needle="$1" haystack="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$haystack" == *"$needle"* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected substring: '$needle'"
        echo "    In: '$haystack'"
    fi
}

TMPROOT="$(mktemp -d)"
# Keeps session-exec locks out of the real ~/.loom and installs the EXIT trap
# that removes TMPROOT.
source "$SCRIPT_DIR/lib/session-lock-sandbox.sh" "$TMPROOT"

SESSION="11111111-2222-4333-8444-555555555555"
SCOPE="loom-agent-sweep-test.scope"

# A workspace with one token, and the wrapper where spawn-claude.sh looks for it.
WS="$TMPROOT/ws"
mkdir -p "$WS/.loom/tokens"
chmod 700 "$WS/.loom/tokens"
printf '%s' "fake-token" > "$WS/.loom/tokens/solo.token"
chmod 600 "$WS/.loom/tokens/solo.token"
ln -s "$SCRIPTS_DIR" "$WS/.loom/scripts"

# What a child of the runtime inherits. `-` (not `:-`) so an exported-but-empty
# variable would still show up as leaked.
# shellcheck disable=SC2016  # expanded by the child, on purpose
CHILD_REPORT='echo "child sid=${LOOM_CLAUDE_SESSION_ID-unset} scope=${LOOM_AGENT_SCOPE_UNIT-unset} resume=${LOOM_RESUME_SESSION_ID-unset} prompt=${LOOM_RESUME_PROMPT-unset} item=${LOOM_DAEMON_ITEM_ID-unset}"'

STUB_DIR="$TMPROOT/stub"
mkdir -p "$STUB_DIR"
cat > "$STUB_DIR/claude" <<STUB
#!/usr/bin/env bash
# Answer only a real launch; the wrapper's pre-flight probes get a silent 0.
case " \$* " in
  *" -p "*|*" --resume "*) ;;
  *) exit 0 ;;
esac
echo "stub-claude args=\$*"
bash -c '$CHILD_REPORT'
STUB
cat > "$STUB_DIR/codex" <<STUB
#!/usr/bin/env bash
echo "stub-codex args=\$*"
bash -c '$CHILD_REPORT'
STUB
chmod +x "$STUB_DIR/claude" "$STUB_DIR/codex"

CLAUDE_CONFIG="$TMPROOT/claude-config"
mkdir -p "$CLAUDE_CONFIG/projects/proj"

# usage: run_claude [VAR=val ...] -- <spawn-claude.sh args...>
# LOOM_SWEEP_NICE=0 / LOOM_SWEEP_CPU_QUOTA=0 keep this to one pass with no
# systemd-run; the scope name must be dropped whether or not a scope is made.
run_claude() {
    local -a envs=()
    while [[ $# -gt 0 && "$1" != "--" ]]; do envs+=("$1"); shift; done
    shift || true
    env -u LOOM_CLAUDE_SESSION_ID -u LOOM_AGENT_SCOPE_UNIT \
        -u LOOM_RESUME_SESSION_ID -u LOOM_RESUME_PROMPT \
        LOOM_WORKSPACE="$WS" LOOM_DAEMON_BIN="$DAEMON_BIN" LOOM_DAEMON_SELF_BIN="$DAEMON_BIN" \
        LOOM_SWEEP_NICE=0 LOOM_SWEEP_CPU_QUOTA=0 LOOM_DAEMON_ITEM_ID=4242 \
        LOOM_MAX_RETRIES=1 LOOM_INITIAL_WAIT=0 LOOM_STARTUP_MONITOR_WINDOW=1 \
        LOOM_SHEPHERD_TASK_ID="test-session-env" \
        CLAUDE_CONFIG_DIR="$CLAUDE_CONFIG" PATH="$STUB_DIR:$PATH" \
        ${envs[@]+"${envs[@]}"} \
        "$SCRIPTS_DIR/spawn-claude.sh" "$@" 2>&1 || true
}

CLEAN="child sid=unset scope=unset resume=unset prompt=unset item=4242"

echo "Testing spawn-claude.sh: a pinned launch (direct exec)..."
out="$(run_claude LOOM_CLAUDE_SESSION_ID="$SESSION" LOOM_AGENT_SCOPE_UNIT="$SCOPE" -- -p ping)"
assert_contains "stub-claude args=-p ping --session-id $SESSION" "$out" \
    "the session id is pinned on the command line"
assert_contains "$CLEAN" "$out" \
    "a child of the agent inherits neither the session id nor the scope unit, and keeps the item id"

echo "Testing spawn-claude.sh: a resume launch (direct exec)..."
out="$(run_claude LOOM_RESUME_SESSION_ID="$SESSION" LOOM_RESUME_PROMPT="carry on" LOOM_AGENT_SCOPE_UNIT="$SCOPE" --)"
assert_contains "stub-claude args=--resume $SESSION carry on" "$out" \
    "the resume id and prompt are on the command line"
assert_contains "$CLEAN" "$out" \
    "a child of the resumed agent inherits neither the resume id nor the resume prompt"

echo "Testing spawn-claude.sh --use-wrapper: a pinned launch..."
out="$(run_claude LOOM_CLAUDE_SESSION_ID="$SESSION" LOOM_AGENT_SCOPE_UNIT="$SCOPE" -- --use-wrapper -p ping)"
assert_contains "--session-id $SESSION" "$out" \
    "the wrapper launches with the pinned session id"
assert_contains "$CLEAN" "$out" \
    "a child of the wrapped agent inherits neither the session id nor the scope unit"

# The wrapper still knows the id after dropping it from the environment: once
# the transcript exists, a launch must resume the session instead.
echo "Testing spawn-claude.sh --use-wrapper: the wrapper keeps the id for its retry..."
: > "$CLAUDE_CONFIG/projects/proj/$SESSION.jsonl"
out="$(run_claude LOOM_CLAUDE_SESSION_ID="$SESSION" -- --use-wrapper -p ping)"
assert_contains "--resume $SESSION" "$out" \
    "with a transcript on disk the wrapper turns --session-id into --resume"
assert_contains "$CLEAN" "$out" \
    "a child of the resumed wrapped agent does not inherit the session id"

echo "Testing spawn-codex.sh: a resume launch..."
out="$(env -u CODEX_HOME -u LOOM_CODEX_PROFILE -u LOOM_CLAUDE_SESSION_ID -u LOOM_AGENT_SCOPE_UNIT \
    LOOM_SWEEP_NICE=0 LOOM_SPAWN_NO_EXPORT=1 LOOM_DAEMON_SELF_BIN="$DAEMON_BIN" \
    LOOM_DAEMON_ITEM_ID=4242 LOOM_CODEX_HOME="$TMPROOT/codex-home" \
    LOOM_RESUME_SESSION_ID="$SESSION" LOOM_RESUME_PROMPT="carry on" \
    PATH="$STUB_DIR:$PATH" \
    bash "$SCRIPTS_DIR/spawn-codex.sh" 2>&1 || true)"
assert_contains "exec resume" "$out" "codex is launched in resume mode"
assert_contains "$SESSION carry on" "$out" "the resume id and prompt are on the command line"
assert_contains "$CLEAN" "$out" \
    "a child of the resumed codex agent inherits neither the resume id nor the resume prompt"

echo ""
echo "==================================="
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if ((TESTS_FAILED > 0)); then
    echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"
    exit 1
fi
echo "All tests passed."
