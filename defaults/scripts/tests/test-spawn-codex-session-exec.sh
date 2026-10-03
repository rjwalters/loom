#!/usr/bin/env bash
# test-spawn-codex-session-exec.sh — the session-exec invocation's shape
# (issue #8518), split out of test-spawn-codex.sh (over the file-size ratchet
# threshold; new assertions go in a sibling, per file-size-policy.md).
#
# What #8518 fixed: `docker exec` inherits neither the caller's cwd nor its
# environment, so the released invocation started Codex in the image's WORKDIR
# (/home/loom) — never a repository, never a trusted project — and every
# headless dispatch into a session container died with "Not inside a trusted
# directory". The invocation must now carry `--workdir "$PWD"` and
# `--env LOOM_WORKSPACE=…`, forward only Loom's own context variables, and
# never leak the host's HOME/PATH/CODEX_HOME or ambient credentials into the
# account's container (the container owns its CODEX_HOME, ADR-0017 Decision 1).
#
# Hermetic: LOOM_CODEX_NO_EXEC=1 argv-preview mode — never touches docker or
# codex.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPAWN_CODEX="$(cd "$SCRIPT_DIR/.." && pwd)/spawn-codex.sh"

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
        echo "    Expected to contain: '$needle'"
        echo "    Actual: '$haystack'"
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
        echo "    Actual: '$haystack'"
    fi
}

TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

# A profile adopted by a prior `loom-daemon accounts session start` — marked
# with the exact sentinel session_lifecycle::mark_session_managed writes.
PROFILE="$TMPROOT/profiles/acct"
mkdir -p "$PROFILE"
printf '{"token":"stub"}\n' > "$PROFILE/auth.json"
printf '{"schema_version":1,"container_name":"loom-codex-session-acct","adopted_at_unix":0}\n' \
    > "$PROFILE/.session-managed.json"

# The workspace spawn-codex.sh resolves; pinned so the expected string is exact.
WS="$TMPROOT/ws"
mkdir -p "$WS/.loom"

echo "Testing spawn-codex.sh session-exec invocation shape (#8518)..."

out="$(cd "$WS" && env -u CODEX_HOME -u LOOM_CODEX_PROFILE \
    LOOM_SWEEP_NICE=0 LOOM_CODEX_NO_EXEC=1 LOOM_WORKSPACE="$WS" \
    LOOM_CODEX_HOME="$PROFILE" \
    HOME="$TMPROOT/fake-home" \
    bash "$SPAWN_CODEX" -p "hi" 2>&1 || true)"
line="$(printf '%s\n' "$out" | grep '^spawn-codex would-exec:' || true)"

assert_contains "session-exec host --container loom-codex-session-acct --workdir $WS --env LOOM_WORKSPACE=$WS --env CARGO_INCREMENTAL=0" "$line" \
    "the exec carries the caller's cwd as --workdir and LOOM_WORKSPACE, before the container name"
assert_contains " -- codex exec " "$line" \
    "…then the container name, then the codex argv"
assert_contains " hi" "$line" "…and the prompt survives at the end of the argv"
assert_not_contains "HOME=" "$line" \
    "host HOME / CODEX_HOME are never forwarded (the container owns its CODEX_HOME)"
assert_not_contains "PATH=" "$line" \
    "host PATH is never forwarded"

# Loom context variables ride across explicitly; unrelated host env does not.
out="$(cd "$WS" && env -u CODEX_HOME -u LOOM_CODEX_PROFILE \
    LOOM_SWEEP_NICE=0 LOOM_CODEX_NO_EXEC=1 LOOM_WORKSPACE="$WS" \
    LOOM_CODEX_HOME="$PROFILE" \
    LOOM_ROLE=judge LOOM_SWEEP_ID=sweep-1 ANTHROPIC_API_KEY=never-forward-me \
    bash "$SPAWN_CODEX" -p "hi" 2>&1 || true)"
line="$(printf '%s\n' "$out" | grep '^spawn-codex would-exec:' || true)"
assert_contains "--env LOOM_ROLE=judge" "$line" "LOOM_ROLE is forwarded into the container"
assert_contains "--env LOOM_SWEEP_ID=sweep-1" "$line" "LOOM_SWEEP_ID is forwarded into the container"
assert_not_contains "never-forward-me" "$line" \
    "an ambient provider credential in the host env is never forwarded"

# A non-adopted profile is untouched: bare-metal, no docker, no --workdir.
GOOD="$TMPROOT/profiles/bare"
mkdir -p "$GOOD"
printf '{"token":"stub"}\n' > "$GOOD/auth.json"
out="$(cd "$WS" && env -u CODEX_HOME -u LOOM_CODEX_PROFILE \
    LOOM_SWEEP_NICE=0 LOOM_CODEX_NO_EXEC=1 LOOM_WORKSPACE="$WS" \
    LOOM_CODEX_HOME="$GOOD" \
    bash "$SPAWN_CODEX" -p "hi" 2>&1 || true)"
assert_contains "would-exec: codex exec" "$out" "a non-session-managed profile still dispatches bare-metal"
assert_not_contains "--workdir" "$out" "bare-metal dispatch has no docker --workdir"

# --- Issue #9979: the container is the boundary -------------------------------
# Codex's bubblewrap sandbox cannot start inside a session container, so a
# session dispatch runs `-s danger-full-access` — but ONLY into a container whose
# labels say it was created hardened. A fake docker answers the label probe.
echo ""
echo "Testing the session container boundary (#9979)..."
FAKE_DOCKER="$TMPROOT/fake-docker"
cat > "$FAKE_DOCKER" <<'FAKE'
#!/usr/bin/env bash
# Answers only `docker inspect --format ... <container>` with $FAKE_LABELS.
[[ "$1" == "inspect" ]] || exit 1
[[ -n "${FAKE_LABELS+x}" ]] || { echo "Error: No such object" >&2; exit 1; }
printf '%s\n' "$FAKE_LABELS"
FAKE
chmod +x "$FAKE_DOCKER"

# The HostConfig half of the probe, hardened: privileged|network|pid|ipc|
# capdrop|capadd|secopt|mount sources (see spawn-codex.sh's inspect format).
HC_OK="false|bridge|||ALL,||no-new-privileges,|/src;"

session_run() {
    # $1 = FAKE_LABELS value ("__unset__" for a missing container); rest = env/args
    local labels="$1"; shift
    local -a envs=(LOOM_SWEEP_NICE=0 LOOM_CODEX_NO_EXEC=1 LOOM_WORKSPACE="$WS"
        LOOM_CODEX_HOME="$PROFILE" LOOM_CODEX_SESSION_DOCKER="$FAKE_DOCKER")
    [[ "$labels" != "__unset__" ]] && envs+=(FAKE_LABELS="$labels")
    (cd "$WS" && env -u CODEX_HOME -u LOOM_CODEX_PROFILE -u FAKE_LABELS -u GH_CONFIG_DIR \
        "${envs[@]}" "$@" 2>&1; echo "rc=$?")
}

# Hardened host-mode container: the requested workspace-write becomes
# danger-full-access, and the requested mode is still named in the log.
out="$(session_run "true|container-boundary-v1||$HC_OK|/home/loom/.codex-profile;" bash "$SPAWN_CODEX" -p "hi" --dangerously-skip-permissions)"
line="$(printf '%s\n' "$out" | grep '^spawn-codex would-exec:' || true)"
assert_contains "-s danger-full-access" "$line" "a hardened container runs Codex with its own sandbox off"
assert_not_contains "-s workspace-write" "$line" "the bwrap-dependent mode is never forwarded into the container"
assert_contains "sandbox=danger-full-access source=session-container-boundary requested=workspace-write" "$out" \
    "the audit line keeps the requested mode and names the container boundary"
assert_contains "posture=host" "$out" "the container posture is logged"

# An explicit `-s read-only` is replaced too (not forwarded alongside).
out="$(session_run "true|container-boundary-v1||$HC_OK|/home/loom/.codex-profile;" bash "$SPAWN_CODEX" -p "hi" -s read-only)"
line="$(printf '%s\n' "$out" | grep '^spawn-codex would-exec:' || true)"
assert_contains "-s danger-full-access" "$line" "an explicit -s is replaced inside the container"
assert_not_contains "read-only" "$line" "…and the explicit mode is not forwarded as well"

# Private-clone containers were created hardened (#8787) and qualify.
out="$(session_run "true||private-clone|$HC_OK|/workspace;" bash "$SPAWN_CODEX" -p "hi")"
assert_contains "posture=private-clone" "$out" "a private-clone container qualifies"
assert_contains "-s danger-full-access" "$out" "…and runs Codex with its sandbox off"

# A container created before the hardening (no posture label) is refused: it
# mounted the whole checkout parent and the Claude token pool.
out="$(session_run "true|||$HC_OK|/Users/x/GitHub;" bash "$SPAWN_CODEX" -p "hi")"
assert_contains "rc=78" "$out" "an unhardened container exits 78 (EX_CONFIG)"
assert_contains "created before the container-boundary hardening" "$out" "…naming why"
assert_contains "loom-daemon accounts session stop acct" "$out" "…and the recreate step"
assert_not_contains "would-exec:" "$out" "…before anything is dispatched"

# A missing or stopped container never gets the sandbox dropped: the posture
# is unverifiable, the requested mode stands, and dispatch is refused
# downstream by `session-exec host` (asserted in test-spawn-codex.sh).
out="$(session_run "__unset__" bash "$SPAWN_CODEX" -p "hi" --dangerously-skip-permissions)"
assert_contains "posture not verified (not-running)" "$out" "a missing container is not treated as hardened"
assert_not_contains "danger-full-access" "$out" "…and the sandbox is not dropped for it"
out="$(session_run "false|container-boundary-v1||" bash "$SPAWN_CODEX" -p "hi" --dangerously-skip-permissions)"
assert_not_contains "danger-full-access" "$out" "a stopped container is not treated as hardened either"

# The label is not the posture: a labelled container whose ACTUAL settings
# are not hardened is refused with exit 78 before anything is dispatched.
for bad in \
    "true|bridge|||ALL,||no-new-privileges,|/src;:privileged=true" \
    "false|host|||ALL,||no-new-privileges,|/src;:host-namespace" \
    "false|bridge|host||ALL,||no-new-privileges,|/src;:host-namespace" \
    "false|bridge||host|ALL,||no-new-privileges,|/src;:host-namespace" \
    "false|bridge||||no-new-privileges,|/src;:cap-drop-ALL-missing" \
    "false|bridge|||ALL,|SYS_ADMIN,|no-new-privileges,|/src;:cap-add=SYS_ADMIN" \
    "false|bridge|||ALL,|||/src;:no-new-privileges-missing" \
    "false|bridge|||ALL,||no-new-privileges,seccomp=unconfined,|/src;:security-opt=" \
    "false|bridge|||ALL,||no-new-privileges,apparmor=unconfined,|/src;:security-opt=" \
    "false|bridge|||ALL,||no-new-privileges,|/var/run/docker.sock;:docker-socket-mounted"; do
    hc="${bad%:*}"; why="${bad##*:}"
    for kind in "container-boundary-v1|" "|private-clone"; do
        out="$(session_run "true|$kind|$hc|/home/loom/.codex-profile;" bash "$SPAWN_CODEX" -p "hi" --dangerously-skip-permissions)"
        assert_contains "rc=78" "$out" "labelled ($kind) but $why: exit 78"
        assert_contains "$why" "$out" "…naming the violation ($why)"
        assert_not_contains "would-exec:" "$out" "…before anything is dispatched ($why)"
    done
done
# Docker's own spelling of no-new-privileges via daemon config is accepted.
out="$(session_run "true|container-boundary-v1||false|bridge|||ALL,||no-new-privileges:true,|/src;|/home/loom/.codex-profile;" bash "$SPAWN_CODEX" -p "hi")"
assert_contains "posture=host" "$out" "no-new-privileges:true counts as no-new-privileges"

# Escape hatch: keep the requested Codex sandbox (needs a userns-capable profile).
out="$(session_run "true|||$HC_OK|/Users/x/GitHub;" env LOOM_CODEX_CONTAINER_SANDBOX=codex bash "$SPAWN_CODEX" -p "hi" --dangerously-skip-permissions)"
assert_contains "-s workspace-write" "$out" "LOOM_CODEX_CONTAINER_SANDBOX=codex keeps the requested sandbox"
assert_contains "LOOM_CODEX_CONTAINER_SANDBOX=codex keeps sandbox=workspace-write" "$out" "…with a warning"
out="$(session_run "true|||$HC_OK|/Users/x/GitHub;" env LOOM_CODEX_CONTAINER_SANDBOX=bogus bash "$SPAWN_CODEX" -p "hi")"
assert_contains "rc=78" "$out" "an invalid LOOM_CODEX_CONTAINER_SANDBOX exits 78"

# gh inside a host-mode container authenticates through the daemon's App token
# dir: the PATH is forwarded, never a token value.
out="$(session_run "true|container-boundary-v1||$HC_OK|/home/loom/.codex-profile;$WS/.loom/gh-config;" env GH_CONFIG_DIR="$WS/.loom/gh-config" GH_TOKEN=never-forward-token bash "$SPAWN_CODEX" -p "hi")"
line="$(printf '%s\n' "$out" | grep '^spawn-codex would-exec:' || true)"
assert_contains "--env GH_CONFIG_DIR=$WS/.loom/gh-config" "$line" "GH_CONFIG_DIR is forwarded into a host-mode container that mounts it"
assert_not_contains "never-forward-token" "$line" "a GH_TOKEN value is never forwarded"
out="$(session_run "true|container-boundary-v1||$HC_OK|/home/loom/.codex-profile;/elsewhere;" env GH_CONFIG_DIR="$WS/.loom/gh-config" bash "$SPAWN_CODEX" -p "hi")"
line="$(printf '%s\n' "$out" | grep '^spawn-codex would-exec:' || true)"
assert_not_contains "GH_CONFIG_DIR" "$line" "an unmounted GH_CONFIG_DIR is not forwarded"
assert_contains "gh inside the session will be unauthenticated" "$out" "…and the gap is named"
out="$(session_run "true||private-clone|$HC_OK|/workspace;" env GH_CONFIG_DIR="$WS/.loom/gh-config" bash "$SPAWN_CODEX" -p "hi")"
line="$(printf '%s\n' "$out" | grep '^spawn-codex would-exec:' || true)"
assert_not_contains "GH_CONFIG_DIR" "$line" "a private-clone container keeps its own GH_CONFIG_DIR"

# Bare-metal dispatch is unchanged: the requested sandbox is forwarded as before.
out="$(cd "$WS" && env -u CODEX_HOME -u LOOM_CODEX_PROFILE \
    LOOM_SWEEP_NICE=0 LOOM_CODEX_NO_EXEC=1 LOOM_WORKSPACE="$WS" \
    LOOM_CODEX_HOME="$GOOD" \
    bash "$SPAWN_CODEX" -p "hi" --dangerously-skip-permissions 2>&1 || true)"
assert_contains "-s workspace-write" "$out" "bare-metal dispatch keeps Codex's own sandbox"
assert_not_contains "danger-full-access" "$out" "bare-metal dispatch never drops the sandbox"

echo ""
echo "Tests run: $TESTS_RUN, passed: $TESTS_PASSED, failed: $TESTS_FAILED"
[[ "$TESTS_FAILED" -eq 0 ]]
