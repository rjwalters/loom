#!/usr/bin/env bash
# test-spawn-model-class-mark.sh — a per-model exhaustion mark survives the
# whole spawn chain (#13872).
#
# The 2026-11-13 fleet incident: five roles pinned to `fable` hit that model's
# per-model ceiling, and every `.bad_tokens` mark on every worker came out
# CLASS-LESS (`[model-class:…]` absent), so the opus children were parked too —
# `tokens select` saw 0/21 spawnable accounts. Two independent causes, both
# pinned here END TO END through the real chain (spawn-claude.sh →
# claude-wrapper.sh → this checkout's loom-daemon `retry-classify` /
# `tokens mark-bad` / `tokens select`):
#
#   1. `retry-classify model-class` accepted only "reached your <X> limit";
#      the CLI writes "hit your <X> limit" (the verb EXHAUSTION_FALLBACK has
#      always matched). Pinned at the unit level in retry_classify; here it is
#      exercised through the real binary and the real wrapper splice.
#   2. The daemon dispatches a role child with an explicit `--model <m>` argv
#      (sweep_registry::dispatch) and NO LOOM_MODEL in the environment, while
#      claude-wrapper.sh's exhaustion mark reads only LOOM_MODEL.
#      spawn-claude.sh now exports the resolved model.
#
# The role-runner environment is reproduced exactly: no hand-set LOOM_MODEL
# anywhere, the model arriving as an argv flag the way the daemon appends it,
# and the failure text is the incident's own line.
#
# Sibling of test-spawn-session-env.sh. Needs a built loom-daemon.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck source=lib/require-daemon-bin.sh
source "$SCRIPT_DIR/lib/require-daemon-bin.sh"
loom_test_require_daemon_bin "$SCRIPTS_DIR" "retry-classify model-class" "tokens select" "tokens mark-bad"
DAEMON_BIN="$LOOM_DAEMON_SELF_BIN"

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

assert_not_contains() {
    local needle="$1" haystack="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$haystack" != *"$needle"* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected NOT to contain: '$needle'"
        echo "    In: '$haystack'"
    fi
}

TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

# A fresh one-token workspace with the wrapper where spawn-claude.sh looks for
# it (the layout every installed repo has: .loom/scripts → defaults/scripts).
make_ws() {
    local ws="$1"
    mkdir -p "$ws/.loom/tokens"
    chmod 700 "$ws/.loom/tokens"
    printf '%s' "tok-$RANDOM" > "$ws/.loom/tokens/alpha.token"
    chmod 600 "$ws/.loom/tokens/alpha.token"
    ln -s "$SCRIPTS_DIR" "$ws/.loom/scripts"
}

# The stub runtime: die with $FAILURE_TEXT on a real launch; the wrapper's
# pre-flight probes get a silent 0 (same shape as test-spawn-session-env.sh).
STUB_DIR="$TMPROOT/stub"
mkdir -p "$STUB_DIR"
cat > "$STUB_DIR/claude" <<'STUB'
#!/usr/bin/env bash
case " $* " in
  *" -p "*|*" --resume "*) ;;
  *) exit 0 ;;
esac
echo "${FAILURE_TEXT:?}"
exit 1
STUB
chmod +x "$STUB_DIR/claude"

CLAUDE_CONFIG="$TMPROOT/claude-config"
mkdir -p "$CLAUDE_CONFIG/projects/proj"

# run_spawn <workspace> <failure-text> [spawn-claude.sh args...]
# Daemon-style dispatch: no LOOM_MODEL in the environment (it arrives as an
# argv flag, exactly how sweep_registry::dispatch appends it).
run_spawn() {
    local ws="$1" failure="$2"
    shift 2
    env -u LOOM_MODEL -u LOOM_RUNTIME \
        LOOM_WORKSPACE="$ws" \
        LOOM_DAEMON_BIN="$DAEMON_BIN" LOOM_DAEMON_SELF_BIN="$DAEMON_BIN" \
        FAILURE_TEXT="$failure" \
        LOOM_SWEEP_NICE=0 LOOM_SWEEP_CPU_QUOTA=0 \
        LOOM_MAX_RETRIES=1 LOOM_INITIAL_WAIT=0 LOOM_STARTUP_MONITOR_WINDOW=1 \
        LOOM_SHEPHERD_TASK_ID="test-model-class" \
        CLAUDE_CONFIG_DIR="$CLAUDE_CONFIG" \
        PATH="$STUB_DIR:$PATH" \
        "$SCRIPTS_DIR/spawn-claude.sh" --use-wrapper -p ping "$@" 2>&1 || true
}

bad_tokens_of() { cat "$1/.loom/tokens/.bad_tokens" 2>/dev/null || true; }

INCIDENT_LINE="2026-11-13T18:30:25Z agent3-abcdef exhausted: hit your Fable limit"

echo "======================================================"
echo "test-spawn-model-class-mark.sh (#13872)"
echo "======================================================"

echo ""
echo "1. daemon-style --model fable + the incident's 'hit your Fable limit' → class-scoped mark"
WS_A="$TMPROOT/ws-a"; make_ws "$WS_A"
run_spawn "$WS_A" "$INCIDENT_LINE" --model fable >/dev/null
marks="$(bad_tokens_of "$WS_A")"
assert_contains "alpha" "$marks" "the exhausted account is marked bad"
assert_contains "[model-class:fable]" "$marks" \
    "the mark is scoped to fable — the incident's class-less mark is what parked the whole pool"

echo ""
echo "2. the scoped mark blocks only its own class (#8058 end to end)"
sel_opus="$(env -u LOOM_ROLE "$DAEMON_BIN" tokens select --workspace "$WS_A" --model opus --export 2>&1 || true)"
assert_contains "alpha" "$sel_opus" \
    "tokens select --model opus still returns the account whose only mark is [model-class:fable]"
sel_fable="$(env -u LOOM_ROLE "$DAEMON_BIN" tokens select --workspace "$WS_A" --model fable --export 2>&1 || true)"
assert_not_contains "LOOM_TOKEN_NAME" "$sel_fable" \
    "tokens select --model fable does not return it — the scoping cuts both ways"

echo ""
echo "3. account-wide text ('hit your weekly limit') keeps the mark class-less"
WS_B="$TMPROOT/ws-b"; make_ws "$WS_B"
run_spawn "$WS_B" "You've hit your weekly limit. Your limit will reset later." --model fable >/dev/null
marks_b="$(bad_tokens_of "$WS_B")"
assert_contains "alpha" "$marks_b" "the exhausted account is still marked bad"
assert_not_contains "[model-class:" "$marks_b" \
    "a weekly limit is account-wide — widening it to one class would leave a dead account in rotation"

echo ""
echo "4. no model anywhere (session default) keeps the mark class-less"
WS_C="$TMPROOT/ws-c"; make_ws "$WS_C"
run_spawn "$WS_C" "$INCIDENT_LINE" >/dev/null
marks_c="$(bad_tokens_of "$WS_C")"
assert_contains "alpha" "$marks_c" "the exhausted account is still marked bad"
assert_not_contains "[model-class:" "$marks_c" \
    "an unknown model stays class-less — there is no honest class to name"

echo ""
echo "5. 'reached your <Model> limit' still scopes (the pre-#13872 wording)"
WS_D="$TMPROOT/ws-d"; make_ws "$WS_D"
run_spawn "$WS_D" "You've reached your Fable 5 limit." --model fable >/dev/null
assert_contains "[model-class:fable]" "$(bad_tokens_of "$WS_D")" \
    "the wording that always worked keeps working"

echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
[[ "$TESTS_FAILED" -eq 0 ]] || exit 1
exit 0
