#!/usr/bin/env bash
# test-spawn-codex-session-mount-stale.sh - spawn-codex.sh's SESSION_MOUNT_STALE
# terminal classification (#10364).
#
# A host-mode session container mounts each registered repo separately, fixed
# at creation. When it does not mount the tick's working directory (a repo
# registered after it was created), `session-exec host` refuses before exec
# with exit 78 and writes `# LOOM_SESSION_MOUNT_STALE …` into the capture file
# (LOOM_SESSION_STDERR_FILE). The adapter must keep exit 78 but report
# `category=SESSION_MOUNT_STALE`. Without the marker, or with another exit
# code, the classifier's verdict stands, and a not-running container stays
# SESSION_DOWN.
#
# Hermetic: a fake daemon stands in for loom-daemon; docker is never run.
#
# Usage:
#   ./.loom/scripts/tests/test-spawn-codex-session-mount-stale.sh

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

# Fake daemon: posture answers $FAKE_POSTURE; `host` exits $FAKE_HOST_RC and,
# with FAKE_MARKER=1, writes the mount-stale marker as the real one does.
cat >"$WORK/bin/fake-loom-daemon" <<'FAKE'
#!/usr/bin/env bash
case "${1:-}" in
    --version) echo "loom-daemon 0.0.0-test"; exit 0 ;;
    session-exec)
        case "${2:-}" in
            posture) echo "$FAKE_POSTURE"; exit 0 ;;
            host)
                if [[ "${FAKE_MARKER:-0}" == "1" ]]; then
                    line="# LOOM_SESSION_MOUNT_STALE container=loom-codex-session-acct workdir=$PWD session-exec: not mounted"
                    echo "$line" >&2
                    [[ -z "${LOOM_SESSION_STDERR_FILE:-}" ]] || printf '%s\n' "$line" >"$LOOM_SESSION_STDERR_FILE"
                else
                    echo "fake session-exec host refusal" >&2
                fi
                exit "${FAKE_HOST_RC:-78}" ;;
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
        PATH="$WORK/bin:$PATH" FAKE_POSTURE="$1" FAKE_HOST_RC="$2" FAKE_MARKER="$3" \
        bash "$SPAWN_CODEX" -p "hi" 2>&1 >/dev/null)"
    SPAWN_RC=$?
}
record() { printf '%s\n' "$SPAWN_ERR" | grep '^# LOOM_TERMINAL_RESULT ' || true; }
category() { record | sed -n 's/.* category=\([A-Z_]*\) .*/\1/p'; }

HOST="mode=host sandbox=danger-full-access gh=skip"

echo "--- a running container without the workdir mount reports SESSION_MOUNT_STALE ---"
run_spawn "$HOST" 78 1
assert_eq "78" "$SPAWN_RC" "the refusal exit code (78) still passes through"
assert_eq "SESSION_MOUNT_STALE" "$(category)" "the terminal record is SESSION_MOUNT_STALE, not RECOVERABLE"

echo "--- exit 78 without the marker keeps the classifier's verdict ---"
run_spawn "$HOST" 78 0
assert_eq "78" "$SPAWN_RC" "exit code passes through"
case "$(category)" in
    SESSION_MOUNT_STALE) actual="relabelled" ;;
    *) actual="classifier verdict kept" ;;
esac
assert_eq "classifier verdict kept" "$actual" "no marker, no SESSION_MOUNT_STALE"

echo "--- the marker with a non-78 exit is not a refusal ---"
run_spawn "$HOST" 1 1
assert_eq "1" "$SPAWN_RC" "exit code passes through"
case "$(category)" in
    SESSION_MOUNT_STALE) actual="relabelled" ;;
    *) actual="classifier verdict kept" ;;
esac
assert_eq "classifier verdict kept" "$actual" "only exit 78 is the pre-exec refusal"

echo "--- a not-running container stays SESSION_DOWN ---"
run_spawn "mode=not-running sandbox=workspace-write gh=skip" 78 0
assert_eq "SESSION_DOWN" "$(category)" "SESSION_DOWN is unchanged"

echo ""
echo "========================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}$TESTS_PASSED${NC}"
if [[ "$TESTS_FAILED" -gt 0 ]]; then
    echo -e "Tests failed: ${RED}$TESTS_FAILED${NC}"
    exit 1
fi
echo "All tests passed"
