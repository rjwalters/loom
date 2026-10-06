#!/usr/bin/env bash
# test-spawn-codex-session-down.sh - spawn-codex.sh's SESSION_DOWN terminal
# classification (#10455).
#
# When the account's session container is not running, `session-exec posture`
# reports mode=not-running and `session-exec host` refuses with exit 78. The
# adapter must keep that exit code but report `category=SESSION_DOWN` instead
# of the generic RECOVERABLE, so the role-failure watch can group by cause.
# A running container (any other posture) keeps the classifier's verdict.
#
# Hermetic: a fake daemon stands in for loom-daemon; docker is never run.
#
# Usage:
#   ./.loom/scripts/tests/test-spawn-codex-session-down.sh

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$TEST_DIR/.." && pwd)"
SPAWN_CODEX="$SCRIPTS_DIR/spawn-codex.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'
TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$expected" == "$actual" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg"
        echo "    Expected: '$expected'"
        echo "    Actual:   '$actual'"
    fi
}

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/bin" "$WORK/ws/.loom"

PROFILE="$WORK/profiles/acct"
mkdir -p "$PROFILE"
printf '{"token":"stub"}\n' >"$PROFILE/auth.json"
printf '{"hooks":{}}\n' >"$PROFILE/hooks.json"
printf '' >"$PROFILE/config.toml"
printf '{}\n' >"$PROFILE/loom-codex-hooks.json"
printf '{"schema_version":1,"container_name":"loom-codex-session-acct","adopted_at_unix":0}\n' \
    >"$PROFILE/.session-managed.json"

printf '#!/usr/bin/env bash\nexit 0\n' >"$WORK/bin/docker"
# Safety net: a stray bare-metal codex must never reach the network.
printf '#!/usr/bin/env bash\necho "unexpected bare-metal codex" >&2\nexit 99\n' >"$WORK/bin/codex"
chmod +x "$WORK/bin/docker" "$WORK/bin/codex"

# Fake daemon: posture answers $FAKE_POSTURE; `host` exits $FAKE_HOST_RC.
cat >"$WORK/bin/fake-loom-daemon" <<'FAKE'
#!/usr/bin/env bash
case "${1:-}" in
    --version) echo "loom-daemon 0.0.0-test"; exit 0 ;;
    session-exec)
        case "${2:-}" in
            posture) echo "$FAKE_POSTURE"; exit 0 ;;
            host) echo "fake session-exec host refusal" >&2; exit "${FAKE_HOST_RC:-78}" ;;
            *) exit 0 ;;
        esac ;;
esac
exit 0
FAKE
chmod +x "$WORK/bin/fake-loom-daemon"

run_spawn() {
    SPAWN_ERR="$(cd "$WORK/ws" && env -u CODEX_HOME -u LOOM_CODEX_PROFILE -u LOOM_ROLE \
        LOOM_SWEEP_NICE=0 LOOM_WORKSPACE="$WORK/ws" \
        LOOM_CODEX_HOME="$PROFILE" LOOM_ACCOUNT_NAME=acct \
        LOOM_CODEX_SESSION_DOCKER="$WORK/bin/docker" \
        LOOM_DAEMON_SELF_BIN="$WORK/bin/fake-loom-daemon" \
        PATH="$WORK/bin:$PATH" FAKE_POSTURE="$1" FAKE_HOST_RC="$2" \
        bash "$SPAWN_CODEX" -p "hi" 2>&1 >/dev/null)"
    SPAWN_RC=$?
}
record() { printf '%s\n' "$SPAWN_ERR" | grep '^# LOOM_TERMINAL_RESULT ' || true; }
category() { record | sed -n 's/.* category=\([A-Z_]*\) .*/\1/p'; }

echo "--- a not-running container reports SESSION_DOWN, exit 78 kept ---"
run_spawn "mode=not-running sandbox=workspace-write gh=skip" 78
assert_eq "78" "$SPAWN_RC" "the refusal exit code (78) still passes through"
assert_eq "SESSION_DOWN" "$(category)" "the terminal record is SESSION_DOWN, not RECOVERABLE"

echo "--- a running container is untouched ---"
run_spawn "mode=host sandbox=danger-full-access gh=skip" 78
assert_eq "78" "$SPAWN_RC" "exit code passes through"
assert_eq "RECOVERABLE" "$(category)" \
    "a non-not-running posture keeps the classifier's RECOVERABLE verdict, never SESSION_DOWN"

echo ""
echo "========================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}$TESTS_PASSED${NC}"
if [[ "$TESTS_FAILED" -gt 0 ]]; then
    echo -e "Tests failed: ${RED}$TESTS_FAILED${NC}"
    exit 1
fi
echo "All tests passed"
