#!/usr/bin/env bash
# test-image.sh — smoke-test a built `loom-worker-session` image (#6899).
#
# Sibling to docker/worker/test-image.sh: takes an already-built image tag
# and asserts the contract docker/session/README.md documents — the checks a
# `docker build` alone cannot catch. Unlike the base image's test script,
# this one actually STARTS a container (the image's whole point is to stay
# running detached) and drives it with `docker exec`, mirroring exactly how
# a real dispatch would use it.
#
# Usage:
#   docker build -f docker/worker/Dockerfile -t loom-worker:dev .
#   docker build -f docker/session/Dockerfile \
#     --build-arg BASE_IMAGE=loom-worker:dev -t loom-worker-session:dev .
#   ./docker/session/test-image.sh loom-worker-session:dev
#
# Exit 0 = every check passed. Exit 1 = at least one check failed (each
# failure is printed with which assertion broke, not just a generic diff).

set -uo pipefail

IMAGE="${1:?usage: test-image.sh <image-tag>}"
CODEX_MIN_VERSION="${CODEX_MIN_VERSION:-0.146.0}"
TMUX_SESSION_NAME="${LOOM_SESSION_TMUX_NAME:-session}"

FAILURES=0
fail() {
    echo "FAIL: $1" >&2
    FAILURES=$((FAILURES + 1))
}
pass() {
    echo "PASS: $1"
}

echo "== Testing image: $IMAGE =="

# 1. codex CLI is present and meets the runtime-adapter floor
# (.loom/docs/runtime-adapters.md). Run as a one-shot container — this does
# NOT need the persistent session to be up, so it runs before the container
# under test is started below.
CODEX_VERSION_OUT=$(docker run --rm --entrypoint bash "$IMAGE" -lc "codex --version" 2>&1)
CODEX_ACTUAL=$(echo "$CODEX_VERSION_OUT" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1 || true)
if [[ -n "$CODEX_ACTUAL" ]]; then
    LOWEST=$(printf '%s\n%s\n' "$CODEX_MIN_VERSION" "$CODEX_ACTUAL" | sort -V | head -1)
    if [[ "$LOWEST" == "$CODEX_MIN_VERSION" ]]; then
        pass "codex --version meets the $CODEX_MIN_VERSION floor: $CODEX_ACTUAL"
    else
        fail "codex $CODEX_ACTUAL is below the $CODEX_MIN_VERSION floor (.loom/docs/runtime-adapters.md)"
    fi
else
    fail "codex --version produced no parseable version: $CODEX_VERSION_OUT"
fi

# 2. Start the container the way a real session container runs: detached,
# no command override — the image's own ENTRYPOINT (tini + the tmux-server
# entrypoint script) is the whole point under test from here on.
CONTAINER_NAME="loom-session-test-$$"
cleanup() {
    docker rm -f "$CONTAINER_NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if ! docker run -d --name "$CONTAINER_NAME" "$IMAGE" >/dev/null; then
    fail "docker run -d did not start the container at all"
    echo "== $FAILURES failure(s) =="
    exit 1
fi

# 3. tmux session comes up (poll briefly — entrypoint startup is not
# instantaneous) and the container stays running detached (no auto-exit).
TMUX_UP=0
for _ in $(seq 1 20); do
    if docker exec "$CONTAINER_NAME" tmux has-session -t "$TMUX_SESSION_NAME" 2>/dev/null; then
        TMUX_UP=1
        break
    fi
    sleep 0.5
done
if [[ "$TMUX_UP" -eq 1 ]]; then
    pass "tmux session '$TMUX_SESSION_NAME' is live inside the running container"
else
    fail "tmux session '$TMUX_SESSION_NAME' never came up inside the container"
fi

RUNNING=$(docker inspect -f '{{.State.Running}}' "$CONTAINER_NAME" 2>/dev/null || echo false)
if [[ "$RUNNING" == "true" ]]; then
    pass "container stays running detached (no auto-exit)"
else
    fail "container is not running (expected a persistent, still-running container): status=$RUNNING"
fi

# 4. `docker exec` round-trips a REAL exit code, not just 0.
if docker exec "$CONTAINER_NAME" true; then
    pass "docker exec true -> exit 0"
else
    fail "docker exec true did not exit 0"
fi

docker exec "$CONTAINER_NAME" false
FALSE_EXIT=$?
if [[ "$FALSE_EXIT" -eq 1 ]]; then
    pass "docker exec false -> exit 1 (real exit code round-tripped)"
else
    fail "docker exec false exited $FALSE_EXIT, expected 1"
fi

docker exec "$CONTAINER_NAME" bash -c 'exit 42'
ARBITRARY_EXIT=$?
if [[ "$ARBITRARY_EXIT" -eq 42 ]]; then
    pass "docker exec bash -c 'exit 42' -> exit 42 (arbitrary exit code round-tripped)"
else
    fail "docker exec bash -c 'exit 42' exited $ARBITRARY_EXIT, expected 42"
fi

# 5. CODEX_HOME convention: env is set, owned by uid 1000, writable, and
# empty (mount point, not baked content — no profile/secret baked in).
CODEX_HOME_CHECK=$(docker exec "$CONTAINER_NAME" bash -lc '
    echo "HOME_PATH=$CODEX_HOME"
    echo "OWNER_UID=$(stat -c %u "$CODEX_HOME" 2>/dev/null)"
    touch "$CODEX_HOME/.loom-test-write" 2>/dev/null && rm -f "$CODEX_HOME/.loom-test-write" && echo WRITABLE=1
' 2>&1)
if [[ "$CODEX_HOME_CHECK" == *"HOME_PATH=/home/loom/.codex-profile"* \
    && "$CODEX_HOME_CHECK" == *"OWNER_UID=1000"* \
    && "$CODEX_HOME_CHECK" == *"WRITABLE=1"* ]]; then
    pass "CODEX_HOME is set, owned by uid 1000, and writable"
else
    fail "CODEX_HOME check failed: $CODEX_HOME_CHECK"
fi

CODEX_HOME_CONTENTS=$(docker exec "$CONTAINER_NAME" bash -lc 'ls -A "$CODEX_HOME" 2>/dev/null || true')
if [[ -z "$CODEX_HOME_CONTENTS" ]]; then
    pass "CODEX_HOME mount point is empty (no baked profile/secrets)"
else
    fail "CODEX_HOME mount point is NOT empty: $CODEX_HOME_CONTENTS"
fi

# 6. No secrets baked in. Best-effort docker-history scan, same pattern set
# docker/worker/test-image.sh uses, plus Codex/CODEX_HOME-adjacent patterns
# specific to this image (an OpenAI API key shape, a baked auth.json, or
# contents accidentally COPYed into the CODEX_HOME mount point).
HISTORY=$(docker history --no-trunc "$IMAGE" 2>&1 || true)
SECRET_HIT=0
for pattern in \
    CLAUDE_CODE_OAUTH_TOKEN \
    GITHUB_TOKEN \
    GH_TOKEN \
    OPENAI_API_KEY \
    'sk-[A-Za-z0-9]{20,}' \
    'accounts\.env' \
    '\.loom/tokens/.*\.token' \
    'auth\.json' \
    '\.codex-profile/.+'; do
    if echo "$HISTORY" | grep -qE "$pattern"; then
        fail "docker history matched a secret-shaped pattern: $pattern"
        SECRET_HIT=1
    fi
done
if [[ "$SECRET_HIT" -eq 0 ]]; then
    pass "no secret-shaped strings found in docker history"
fi

# 7. Core toolchain inherited from the base image is still present (sanity
# check that this layer did not accidentally shadow/break anything).
for bin in git gh jq tmux curl claude codex node npm; do
    if docker exec "$CONTAINER_NAME" bash -lc "command -v $bin >/dev/null 2>&1"; then
        pass "$bin present on PATH inside the running container"
    else
        fail "$bin missing from PATH inside the running container"
    fi
done

echo "== $FAILURES failure(s) =="
exit $((FAILURES > 0 ? 1 : 0))
