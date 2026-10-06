#!/usr/bin/env bash
# Tests spawn-claude.sh's managed-gh container admission (#9987): a configured
# forge-egress policy that cannot be honoured REFUSES (exit 78) instead of
# restoring ~/.config/gh / GH_TOKEN; only a genuinely unconfigured host keeps
# the legacy credentials. Bash 3.2-safe (no mapfile) is asserted by source grep.
# shellcheck disable=SC2016,SC2034  # check() takes single-quoted expressions to eval; OUT is for debugging
set -uo pipefail

SCRIPTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PASS=0
FAIL=0
ok() { PASS=$((PASS + 1)); echo "  ok: $1"; }
bad() { FAIL=$((FAIL + 1)); echo "  FAIL: $1"; }
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
WS="$T/ws"
mkdir -p "$WS/.loom/tokens" "$T/home/.config/gh" "$T/bin"
chmod 700 "$WS/.loom/tokens"
printf 'fake-token' >"$WS/.loom/tokens/t.token"
chmod 600 "$WS/.loom/tokens/t.token"
ln -s "$SCRIPTS_DIR" "$WS/.loom/scripts"
echo '{"runtimes": {"containment": {"enabled": true}}}' >"$WS/.loom/config.json"

DOCKER_LOG="$T/docker.log"
cat >"$T/bin/docker" <<STUB
#!/usr/bin/env bash
echo "\$*" >>"$DOCKER_LOG"
exit 0
STUB
chmod +x "$T/bin/docker"

# Fake daemon: behaviour chosen by FAKE_ARGS_RC / FAKE_CHECK_RC.
cat >"$T/bin/fake-daemon" <<'STUB'
#!/usr/bin/env bash
[[ "$*" == *"--help"* ]] && exit 0
case "$*" in
    "forge egress container-args")
        [[ "${FAKE_ARGS_RC:-0}" == "0" ]] || { echo "forge-egress: policy.unreadable" >&2; exit "$FAKE_ARGS_RC"; }
        [[ "${FAKE_ARGS_EMPTY:-0}" == "1" ]] && exit 0
        printf '%s\n' -v /managed:/managed:ro -e LOOM_FORGE_EGRESS_POLICY=/managed/policy.json
        ;;
    "forge egress container-check "*)
        echo "$*" >>"$FAKE_CHECK_LOG"
        [[ "${FAKE_CHECK_RC:-0}" == "0" ]] || { echo "forge-egress: toolchain.launcher-python3-missing" >&2; exit "$FAKE_CHECK_RC"; }
        ;;
esac
exit 0
STUB
chmod +x "$T/bin/fake-daemon"

run() { # run <daemon-bin> [ENV=VAL ...]; stdout+stderr, exit code in RC
    local bin="$1"
    shift
    : >"$DOCKER_LOG"
    : >"$T/check.log"
    OUT="$(env HOME="$T/home" LOOM_WORKSPACE="$WS" LOOM_DAEMON_BIN="$bin" PATH="$T/bin:$PATH" \
        FAKE_CHECK_LOG="$T/check.log" LOOM_SWEEP_CPU_QUOTA=0 GH_TOKEN=ghp_secret "$@" \
        "$SCRIPTS_DIR/spawn-claude.sh" -p ping 2>&1)"
    RC=$?
}

echo "managed-gh container admission (#9987)"

run "$T/bin/fake-daemon" FAKE_ARGS_RC=78
check "helper refusal (78) refuses the spawn" '[[ $RC -eq 78 ]]'
check "helper refusal never reaches docker" '[[ ! -s "$DOCKER_LOG" ]]'

run /bin/false LOOM_FORGE_EGRESS_POLICY="$T/policy.json"
check "helper failure (/bin/false) with a policy configured refuses" '[[ $RC -eq 78 ]]'
check "helper failure never reaches docker" '[[ ! -s "$DOCKER_LOG" ]]'

run /bin/false env -u LOOM_FORGE_EGRESS_POLICY
check "no policy + no helper keeps legacy behaviour (docker reached)" '[[ -s "$DOCKER_LOG" ]]'

run "$T/bin/fake-daemon"
check "managed args: docker reached" '[[ $RC -eq 0 && -s "$DOCKER_LOG" ]]'
check "managed args: policy mount passed through" 'grep -q -- "/managed:/managed:ro" "$DOCKER_LOG"'
check "managed args: ~/.config/gh is not mounted" '! grep -q "\.config/gh" "$DOCKER_LOG"'
check "managed args: GH_TOKEN is not forwarded" '! grep -q -- "-e GH_TOKEN" "$DOCKER_LOG"'
check "managed args: container-check ran for the image" 'grep -q "container-check " "$T/check.log"'

run "$T/bin/fake-daemon" FAKE_CHECK_RC=78
check "container-check refusal (python3 / launcher) refuses the spawn" '[[ $RC -eq 78 ]]'
check "container-check refusal never reaches docker" '! grep -q "managed" "$DOCKER_LOG"'

run "$T/bin/fake-daemon" FAKE_ARGS_EMPTY=1 GH_TOKEN=
check "empty helper output on a clean daemon = unconfigured: legacy mount" 'grep -q "\.config/gh" "$DOCKER_LOG"'

check "no mapfile in spawn-claude.sh (Bash 3.2)" '! grep -qE "^[^#]*\bmapfile\b" "$SCRIPTS_DIR/spawn-claude.sh"'

echo "passed=$PASS failed=$FAIL"
[[ $FAIL -eq 0 ]]
